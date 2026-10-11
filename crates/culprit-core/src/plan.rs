use serde::{Deserialize, Serialize};
use ts_rs::TS;

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
    pub warnings: Vec<PlanWarning>,
    pub props: Properties,
    pub children: Vec<PlanNode>,
}

impl PlanNode {
    pub fn walk<'a>(&'a self, out: &mut Vec<&'a PlanNode>) {
        out.push(self);
        for child in &self.children {
            child.walk(out);
        }
    }

    /// Estimated rows across all executions of the operator, the number comparable to `act_rows`.
    pub fn est_rows_total(&self) -> f64 {
        self.est_rows * self.est_executions.max(1.0)
    }

    /// How far off the estimate was, as a factor ≥ 1, signed: positive means more rows than estimated.
    pub fn misestimate(&self) -> Option<f64> {
        let actual = self.act_rows?;
        let estimated = self.est_rows_total().max(1.0);
        let actual = actual.max(1.0);
        Some(if actual >= estimated { actual / estimated } else { -(estimated / actual) })
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
