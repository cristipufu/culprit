use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::insights::PlanInsights;
use crate::model::{Engine, Properties};

#[derive(Serialize, Deserialize, TS, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
#[ts(export)]
pub enum PlanKind {
    Estimated,
    Generic,
    Actual,
}

#[derive(Serialize, Deserialize, TS, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
#[ts(export)]
pub enum NodeKind {
    Scan,
    Seek,
    Lookup,
    Join,
    Sort,
    Aggregate,
    Spool,
    Parallelism,
    Modify,
    Compute,
    Other,
}

#[derive(Serialize, Deserialize, TS, Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[serde(rename_all = "kebab-case")]
#[ts(export)]
pub enum PlanWarning {
    ImplicitConversion,
    Spill,
    NoJoinPredicate,
    MissingStatistics,
    MemoryGrant,
    Misestimate,
    ResidualPredicate,
}

impl PlanWarning {
    pub fn label(self) -> &'static str {
        match self {
            PlanWarning::ImplicitConversion => "Implicit conversion",
            PlanWarning::Spill => "Spills to disk",
            PlanWarning::NoJoinPredicate => "No join predicate",
            PlanWarning::MissingStatistics => "Missing statistics",
            PlanWarning::MemoryGrant => "Memory grant issue",
            PlanWarning::Misestimate => "Row estimate off",
            PlanWarning::ResidualPredicate => "Filters after reading",
        }
    }
}

#[derive(Serialize, Deserialize, TS, Clone, Debug)]
#[ts(export)]
pub struct PlanNode {
    pub id: u32,
    pub op: String,
    pub kind: NodeKind,
    pub detail: Option<String>,
    pub object: Option<String>,
    pub index: Option<String>,
    pub seek_predicate: Option<String>,
    pub predicate: Option<String>,
    pub est_rows: f64,
    pub est_executions: f64,
    pub act_rows: Option<f64>,
    pub act_executions: Option<f64>,
    pub rows_read: Option<f64>,
    pub table_rows: Option<f64>,
    pub subtree_cost: f64,
    pub self_cost: f64,
    pub act_time_ms: Option<f64>,
    /// CPU, reads, per-thread rows and spills, when the plan carries runtime counters (actual plans, live progress).
    #[serde(default)]
    pub runtime: Option<crate::actual::NodeRuntime>,
    pub warnings: Vec<PlanWarning>,
    pub props: Properties,
    pub children: Vec<PlanNode>,
}

/// An index pays off for a filter that keeps at most this share of the rows a scan reads. Above it, reading through the
/// index touches most of the table's pages anyway, so the scan is the right plan and the fix is to read less. Every
/// rule that suggests an index from a scan uses this bar.
pub const INDEX_KEEPS_AT_MOST: f64 = 0.1;

/// Actual rows at least this many times away from the estimate are a misestimate…
pub const MISESTIMATE_FACTOR: f64 = 10.0;
/// …once the larger of the two reaches this many rows: 3 rows instead of 40 changes no plan.
pub const MISESTIMATE_MIN_ROWS: f64 = 100.0;

/// Node prop: how many threads ran a parallel operator (SQL Server). Each thread runs it once, and the plan's estimate
/// counts that as one execution, so [`PlanNode::runs`] takes the extra threads off the measured executions.
pub const PARALLEL_THREADS: &str = "ParallelThreads";

impl PlanNode {
    pub fn walk<'a>(&'a self, out: &mut Vec<&'a PlanNode>) {
        out.push(self);
        for child in &self.children {
            child.walk(out);
        }
    }

    /// Estimated rows across all estimated executions of the operator. Compare actual rows with
    /// [`PlanNode::est_rows_for_runs`], which scales the estimate to the executions that happened.
    pub fn est_rows_total(&self) -> f64 {
        self.est_rows * self.est_executions.max(1.0)
    }

    /// How many times the operator ran, counted the way its estimate counts executions: measured when the plan has
    /// actual counts (0 when it never ran; a parallel operator that ran once on each thread counts once), else as
    /// estimated.
    pub fn runs(&self) -> f64 {
        match self.act_executions {
            Some(runs) if runs <= 0.0 => 0.0,
            Some(runs) => {
                let threads =
                    self.props.get(PARALLEL_THREADS).and_then(|t| t.parse::<f64>().ok()).unwrap_or(1.0).max(1.0);
                (runs - (threads - 1.0)).max(1.0)
            }
            None => self.est_executions.max(1.0),
        }
    }

    /// The estimate for the executions that happened: rows per execution as estimated, times the times it actually
    /// ran. The number comparable to `act_rows`: the inner side of a nested loop estimated right per seek isn't blamed
    /// for an outer side that returned more rows than expected. Without actual counts, the same as `est_rows_total`.
    pub fn est_rows_for_runs(&self) -> f64 {
        match self.act_executions {
            Some(_) => self.est_rows * self.runs(),
            None => self.est_rows_total(),
        }
    }

    /// The share of the rows this operator read that it kept, from actual counts when the plan has them, else from
    /// estimates and the table's size. None when the plan doesn't say how many rows it read.
    pub fn kept_share(&self) -> Option<f64> {
        let kept = self.act_rows.unwrap_or_else(|| self.est_rows_total());
        let read = self.rows_read.or_else(|| self.table_rows.map(|t| t * self.runs()))?;
        (read > 0.0).then(|| (kept / read).min(1.0))
    }

    /// How far off the estimate was for the executions that happened, as a factor ≥ 1, signed: positive means more
    /// rows than estimated. None without actual rows, or when the operator never ran (nothing to compare).
    pub fn misestimate(&self) -> Option<f64> {
        let actual = self.act_rows?;
        if self.act_executions.is_some_and(|runs| runs <= 0.0) {
            return None;
        }
        let estimated = self.est_rows_for_runs().max(1.0);
        let actual = actual.max(1.0);
        Some(if actual >= estimated { actual / estimated } else { -(estimated / actual) })
    }

    /// The operators at or below this one whose actual rows are [`MISESTIMATE_FACTOR`] or more away from the estimate
    /// for the runs that happened, as (node id, signed factor). Fewer rows than estimated is no evidence where the
    /// operator may have been stopped early, because its estimate is for a full run: below a Limit or Top, on the inner
    /// side of a nested-loop semi or anti join (it stops at the first match), on either input of a merge join (it stops
    /// when the other runs out) and in a subquery (EXISTS stops at its first row); a sort or hash build between them
    /// reads its input to the end. Nor is it where SQL Server pushed a hash join's bitmap into a scan (`PROBE(…)`): the
    /// estimates of the scan and of what passes its rows on leave the bitmap out. The one rule behind the Misestimate
    /// warning on every engine and the misestimate insight.
    pub fn misestimates(&self) -> Vec<(u32, f64)> {
        fn probed(n: &PlanNode) -> bool {
            n.predicate.as_deref().is_some_and(|p| p.contains("PROBE(")) || n.children.iter().any(probed)
        }
        fn visit(n: &PlanNode, stopped: bool, out: &mut Vec<(u32, f64)>) {
            if let Some(factor) = n.misestimate() {
                let larger = n.act_rows.unwrap_or(0.0).max(n.est_rows_for_runs());
                let explained = factor < 0.0 && (stopped || probed(n));
                if factor.abs() >= MISESTIMATE_FACTOR && larger >= MISESTIMATE_MIN_ROWS && !explained {
                    out.push((n.id, factor));
                }
            }
            let op = n.op.to_ascii_lowercase();
            let detail = n.detail.as_deref().unwrap_or_default().to_ascii_lowercase();
            let limits = op == "top" || op.starts_with("limit");
            let semi = op.starts_with("nested loop")
                && [op.as_str(), detail.as_str()].iter().any(|t| t.contains("semi") || t.contains("anti"));
            let last = n.children.len().saturating_sub(1);
            for (i, child) in n.children.iter().enumerate() {
                let subquery =
                    child.props.get("Parent Relationship").is_some_and(|r| r == "SubPlan" || r == "InitPlan");
                // A hash join builds its first input (SQL Server) or its Hash child (PostgreSQL, MySQL) in full.
                let reads_to_the_end = n.kind == NodeKind::Sort || op == "hash" || (op == "hash match" && i == 0);
                let child_stopped = if subquery || limits || op.contains("merge join") || (semi && i == last && i > 0) {
                    true
                } else if reads_to_the_end {
                    false
                } else {
                    stopped
                };
                visit(child, child_stopped, out);
            }
        }
        let mut out = Vec::new();
        visit(self, false, &mut out);
        out
    }

    /// Set the Misestimate warning on exactly the operators [`PlanNode::misestimates`] finds. Every plan reader calls
    /// this once the tree is built.
    pub fn flag_misestimates(&mut self) {
        fn mark(n: &mut PlanNode, ids: &[u32]) {
            n.warnings.retain(|w| *w != PlanWarning::Misestimate);
            if ids.contains(&n.id) {
                n.warnings.push(PlanWarning::Misestimate);
            }
            n.children.iter_mut().for_each(|c| mark(c, ids));
        }
        let ids: Vec<u32> = self.misestimates().into_iter().map(|(id, _)| id).collect();
        mark(self, &ids);
    }
}

#[derive(Serialize, Deserialize, TS, Clone, Debug)]
#[ts(export)]
pub struct PlanIndexHint {
    pub table: String,
    pub equality: Vec<String>,
    pub inequality: Vec<String>,
    pub include: Vec<String>,
    pub impact_pct: f64,
}

#[derive(Serialize, Deserialize, TS, Clone, Debug)]
#[ts(export)]
pub struct Plan {
    pub engine: Engine,
    pub kind: PlanKind,
    pub statement: Option<String>,
    pub total_cost: f64,
    pub root: PlanNode,
    pub warnings: Vec<PlanWarning>,
    pub index_hints: Vec<PlanIndexHint>,
    pub props: Properties,
    pub raw: Option<String>,
    /// Problems found in this plan, attached where plans leave the engine (`insights::annotate`). Absent in saved
    /// reports and older files.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub insights: Option<PlanInsights>,
}

impl Plan {
    pub fn nodes(&self) -> Vec<&PlanNode> {
        let mut out = Vec::new();
        self.root.walk(&mut out);
        out
    }

    pub fn has_warning(&self, warning: PlanWarning) -> bool {
        self.warnings.contains(&warning) || self.nodes().iter().any(|n| n.warnings.contains(&warning))
    }

    /// The node with the highest self cost: where the plan spends most of its effort.
    pub fn hottest(&self) -> &PlanNode {
        self.nodes().into_iter().max_by(|a, b| a.self_cost.total_cmp(&b.self_cost)).unwrap_or(&self.root)
    }

    /// Fill `self_cost` from subtree costs, for engines that only report cumulative cost.
    pub fn compute_self_costs(&mut self) {
        fn visit(node: &mut PlanNode) {
            let children: f64 = node.children.iter().map(|c| c.subtree_cost).sum();
            node.self_cost = (node.subtree_cost - children).max(0.0);
            node.children.iter_mut().for_each(visit);
        }
        visit(&mut self.root);
    }
}
