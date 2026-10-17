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

/// Plan prop a collector sets on a scan a run may never read: a partition a generic plan lists, pruned only when it
/// runs. Its table's size isn't what it reads.
pub const NOT_READ_IN_FULL: &str = "Partition Pruning";

/// Plan prop a collector sets when the rows a scan keeps are the planner's default guess (a parameter compared by range
/// or pattern in a generic plan), so the share kept says nothing.
pub const GUESSED_SELECTIVITY: &str = "Selectivity";

/// Plan prop the analysis sets on a scan whose statement's runs measured what its filter keeps (MySQL and MariaDB count
/// the rows a statement examines and returns): the share of the rows read the runs kept, a number from 0 to 1. What the
/// runs measured outweighs an estimate, and a fixed guess most of all ([`PlanNode::kept_share`]).
pub const MEASURED_KEPT: &str = "Share kept, as its runs measured";

/// Plan prop MySQL's and MariaDB's EXPLAIN carry on a table: the percentage of the rows read that its condition is
/// expected to keep.
const MYSQL_FILTERED: &str = "filtered";

/// An index pays off for a filter that keeps at most this share of the rows a scan reads. Above it, reading through the
/// index touches most of the table's pages anyway, so the scan is the right plan and the fix is to read less. Every
/// rule that suggests an index from a scan uses this bar.
pub const INDEX_KEEPS_AT_MOST: f64 = 0.1;

/// Tables of this many rows or more: reading one in full where a seek would do costs something. Under it, an index is
/// advice only where the statements that want one take real time, and a whole-table read is not said as what makes a
/// cheap plan's runs cost (`insights::whole_read_matters`). The bar every index advice for a table's reads uses.
pub const SCAN_ROWS: f64 = 10_000.0;

/// Actual rows at least this many times away from the estimate are a misestimate…
pub const MISESTIMATE_FACTOR: f64 = 10.0;
/// …once the larger of the two reaches this many rows: 3 rows instead of 40 changes no plan.
pub const MISESTIMATE_MIN_ROWS: f64 = 100.0;

/// Node prop: how many threads ran a parallel operator (SQL Server). Each thread runs it once, and the plan's estimate
/// counts that as one execution, so [`PlanNode::runs`] takes the extra threads off the measured executions.
pub const PARALLEL_THREADS: &str = "ParallelThreads";

/// Node prop: the kind of index an operator reads (`btree`, `brin`, `hash`…), from the catalog
/// ([`crate::insights::mark_snapshot_index_kinds`]): a plan names the index, not what kind it is.
pub const INDEX_KIND: &str = "Index Kind";

/// Set on an operator whose scalar functions (`UserDefinedFunctions`) are all in its seek keys: they run once per
/// seek, not once per row.
pub const UDF_IN_SEEK_ONLY: &str = "UserDefinedFunctionsInSeekOnly";

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
    /// estimates: MySQL's and MariaDB's own share (`filtered`), or the rows estimated against the rows read. None when
    /// the plan doesn't say how many rows it read, or when the share is the optimizer's default guess.
    pub fn kept_share(&self) -> Option<f64> {
        // What the statement's runs measured, where the analysis put it on the scan ([`MEASURED_KEPT`]).
        if self.act_rows.is_none()
            && let Some(kept) = self.props.get(MEASURED_KEPT).and_then(|k| k.trim().parse::<f64>().ok())
        {
            return Some(kept.clamp(0.0, 1.0));
        }
        // A default guess for a parameter compared by range or pattern isn't a share anything can be judged on.
        if self.props.contains_key(GUESSED_SELECTIVITY) {
            return None;
        }
        // MySQL estimates the share its condition keeps of each scan; one of its fixed guesses says nothing.
        if self.act_rows.is_none()
            && let Some(filtered) = self.props.get(MYSQL_FILTERED).and_then(|f| f.trim().parse::<f64>().ok())
        {
            let conditions = self.predicate.as_deref().map_or(1, crate::insights::condition_count);
            return (!crate::insights::mysql_guessed_filtered(filtered, conditions))
                .then_some((filtered / 100.0).clamp(0.0, 1.0));
        }
        let kept = self.act_rows.unwrap_or_else(|| self.est_rows_total());
        let read = self.rows_read_total()?;
        (read > 0.0).then(|| (kept / read).min(1.0))
    }

    /// Rows this operator read over its runs: what the engine reports, else, for a scan that reads its table in full,
    /// the table's rows for every run. None when neither is known, or when the collector marked the scan as one a run
    /// may never read ([`NOT_READ_IN_FULL`]: a partition a generic plan lists but a run would prune).
    pub fn rows_read_total(&self) -> Option<f64> {
        self.rows_read.or_else(|| {
            (self.kind == NodeKind::Scan && !self.props.contains_key(NOT_READ_IN_FULL))
                .then(|| self.table_rows.map(|t| t * self.runs()))
                .flatten()
        })
    }

    /// Whether the operator reads its whole table on each run: a scan of a table, unless the collector marked it as one
    /// a run may never read ([`NOT_READ_IN_FULL`]) or the plan's own estimate of the rows it reads is under half the
    /// table's (a scan under a TOP or a LIMIT that stops at its first rows, in index order).
    pub fn reads_table_whole(&self) -> bool {
        if self.kind != NodeKind::Scan || self.object.is_none() || self.props.contains_key(NOT_READ_IN_FULL) {
            return false;
        }
        match (self.rows_read, self.table_rows) {
            (Some(read), Some(table)) if table > 0.0 => read >= 0.5 * table * self.runs(),
            _ => true,
        }
    }

    /// Whether the operator reads a BRIN index, by the catalog's mark ([`INDEX_KIND`]).
    pub fn reads_brin(&self) -> bool {
        self.props.get(INDEX_KIND).is_some_and(|k| k.eq_ignore_ascii_case("brin"))
    }

    /// How far off the estimate was for the executions that happened, as a factor ≥ 1, signed: positive means more
    /// rows than estimated. None without actual rows, or when the operator never ran (nothing to compare).
    ///
    /// PostgreSQL's BitmapAnd and BitmapOr combine bitmaps and count no rows: they always report 0 actual, which is no
    /// measurement.
    ///
    /// A bitmap scan of a BRIN index reports page ranges' worth of rows, not rows: no measurement either.
    pub fn misestimate(&self) -> Option<f64> {
        let actual = self.act_rows?;
        if self.act_executions.is_some_and(|runs| runs <= 0.0)
            || matches!(self.op.as_str(), "BitmapAnd" | "BitmapOr")
            || (self.op == "Bitmap Index Scan" && self.reads_brin())
        {
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

/// How a step's cost rests on a default guess for a parameter ([`Plan::guess`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Guess {
    /// The step, or a step below it, was planned on the guess: the rows it works on are the guess.
    Rows,
    /// A loop runs the step once for each row of an input planned on the guess: how many times it runs is the guess.
    Runs,
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

    /// The step with the largest share of the plan's cost: where the plan spends most of its effort. `None` when no
    /// step carries a cost (SQLite's plans have the access path only), so nothing is called the most expensive by
    /// accident. An exchange (PostgreSQL's Gather and Gather Merge, SQL Server's Parallelism) is never it: its cost is
    /// mostly the charge for passing each row between threads, not work on the data.
    pub fn hottest(&self) -> Option<&PlanNode> {
        if !self.weighed() {
            return None;
        }
        self.nodes()
            .into_iter()
            .filter(|n| n.kind != NodeKind::Parallelism)
            .max_by(|a, b| a.self_cost.total_cmp(&b.self_cost))
    }

    /// A step's share of the plan's cost, at most 1: its own cost over what the plan's costs add up to
    /// ([`crate::plan_diff::plan_cost_basis`]), never over the root's alone, which under a PostgreSQL Limit is a
    /// fraction of what its children are costed at.
    pub fn cost_share(&self, node: &PlanNode) -> f64 {
        (node.self_cost / crate::plan_diff::plan_cost_basis(self)).clamp(0.0, 1.0)
    }

    /// A step's share of the plan's cost where the plan can state one, which is what every "N% of the plan's cost" a
    /// person reads comes from, on every surface (findings, briefs, exports, the plan viewer, the CLI's tree, MCP).
    /// `None` where the plan states no shares at all ([`Plan::states_shares`]: a plan without costs, a cheap plan, a
    /// plan beside whose steps one doing work carries no cost), for a step with no cost of its own, and for a step whose
    /// cost rests on a default guess for a parameter rather than on statistics ([`Plan::guess`]). `culprit_checks::
    /// no_share_reason` says why there is none, and the app mirrors this rule (`stepShare` in
    /// `app/src/lib/plan-format.ts`).
    pub fn step_share(&self, node: &PlanNode) -> Option<f64> {
        (self.states_shares() && node.self_cost > 0.0 && self.guess(node).is_none()).then(|| self.cost_share(node))
    }

    /// Whether the plan states any step's share of its cost: it weighs its steps with costs, it does real work
    /// ([`crate::insights::does_real_work`]: half of a point lookup's cost isn't where its statement's time goes), and
    /// it puts a cost on every step doing work ([`Plan::uncosted_steps`]: next to MySQL's filesort a scan reads as
    /// "100%"). A plan insight's share of the work follows the same rule ([`crate::insights::stated_share`]).
    pub fn states_shares(&self) -> bool {
        self.weighed() && crate::insights::does_real_work(self) && self.uncosted_steps().is_empty()
    }

    /// How the cost of `node` rests on a default guess for a parameter ([`crate::GUESSED_SELECTIVITY`]), where it does:
    /// the step or one below it was planned on the guess (the rows it works on are the guess), or a loop runs it once for
    /// each row of an input planned on one (how many times it runs is the guess: the inner side of a nested loop over
    /// `= ANY($1)`, which the planner takes to hold 10 values). `None` where its cost rests on statistics.
    pub fn guess(&self, node: &PlanNode) -> Option<Guess> {
        self.guesses().into_iter().find(|(id, _)| *id == node.id).map(|(_, g)| g)
    }

    /// Whether any step of the plan was planned on a default guess for a parameter: then what the plan costs in all, and
    /// whether it is cheap, rests on that guess too.
    pub fn has_guess(&self) -> bool {
        self.nodes().iter().any(|n| n.props.contains_key(crate::GUESSED_SELECTIVITY))
    }

    /// Every step whose cost rests on a guess, with how ([`Plan::guess`]).
    pub fn guesses(&self) -> Vec<(u32, Guess)> {
        // Planned on the guess: the step itself, or any step below it.
        fn mark(n: &PlanNode, planned: &mut Vec<u32>) -> bool {
            let mut any = n.props.contains_key(crate::GUESSED_SELECTIVITY);
            for child in &n.children {
                any |= mark(child, planned);
            }
            if any {
                planned.push(n.id);
            }
            any
        }
        fn visit(n: &PlanNode, repeated: bool, planned: &[u32], out: &mut Vec<(u32, Guess)>) {
            if planned.contains(&n.id) {
                out.push((n.id, Guess::Rows));
            } else if repeated {
                out.push((n.id, Guess::Runs));
            }
            let nested_loop = n.op.to_ascii_lowercase().starts_with("nested loop");
            let mut earlier_guessed = false;
            for (i, child) in n.children.iter().enumerate() {
                // An InitPlan (a CTE among them) runs once for the statement, whatever its parent repeats.
                let once = child.props.get("Parent Relationship").is_some_and(|r| r == "InitPlan" || r == "CTE");
                // A nested loop's later inputs run once for each row of the earlier ones; so does any input the plan
                // counts more runs for than its parent (a MySQL join's later tables, a correlated subplan).
                let per_row = !once && i > 0 && (nested_loop || child.est_executions > n.est_executions.max(1.0));
                visit(child, repeated || (per_row && earlier_guessed), planned, out);
                earlier_guessed |= planned.contains(&child.id);
            }
        }
        let mut planned = Vec::new();
        if !mark(&self.root, &mut planned) {
            return Vec::new();
        }
        let mut out = Vec::new();
        visit(&self.root, false, &planned, &mut out);
        out
    }

    /// Steps doing work that carry no cost of their own in a plan that weighs its steps: MySQL and MariaDB put none on a
    /// filesort or an internal temporary table (nor on the grouping or de-duplication that uses one) unless they give a
    /// sort's cost. Beside one, a step's share of what the plan costs isn't its share of the statement's work. Only
    /// theirs: SQL Server and PostgreSQL cost every step, so a step whose own cost works out at nothing there (a Stream
    /// Aggregate whose subtree cost rounds to its input's, a Memoize) costs too little to matter, not nothing.
    pub fn uncosted_steps(&self) -> Vec<&PlanNode> {
        if !self.weighed() || self.engine != Engine::Mysql {
            return Vec::new();
        }
        let temporary = |n: &PlanNode| {
            [Some(n.op.as_str()), n.detail.as_deref()]
                .into_iter()
                .flatten()
                .any(|t| t.to_ascii_lowercase().contains("temporary"))
        };
        self.nodes()
            .into_iter()
            .filter(|n| {
                n.self_cost <= 0.0
                    && (matches!(n.kind, NodeKind::Sort | NodeKind::Aggregate | NodeKind::Spool) || temporary(n))
            })
            .collect()
    }

    /// Whether the plan weighs its steps with costs: what "share of the cost" and estimated rows mean on it.
    pub fn weighed(&self) -> bool {
        self.total_cost > 0.0 || self.nodes().iter().any(|n| n.self_cost > 0.0 || n.subtree_cost > 0.0)
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
