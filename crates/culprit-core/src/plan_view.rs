//! Derived plan data for renderers. Collection facts remain on Plan/PlanNode; this view is recomputed by analysis.
use crate::{ActualSummary, PlanKind, plan::Guess};
use serde::{Deserialize, Serialize};
use ts_rs::TS;

#[derive(Serialize, Deserialize, TS, Clone, Debug)]
#[ts(export)]
pub struct PlanView {
    pub kind: PlanKind,
    pub query: Option<PlanMeasurementRef>,
    pub focus_checks: Vec<String>,
    pub cost_basis: f64,
    pub weighed: bool,
    pub states_shares: bool,
    pub real_work: bool,
    pub hottest_cost: Option<u32>,
    pub hottest_time: Option<u32>,
    pub timed_total_ms: Option<f64>,
    pub cost_note: Option<String>,
    pub time_note: Option<String>,
    pub actual: Option<ActualSummary>,
    pub nodes: Vec<PlanOperatorView>,
    pub insights: Vec<PlanInsightShare>,
}

#[derive(Serialize, Deserialize, TS, Clone, Debug)]
#[ts(export)]
pub struct PlanMeasurementRef {
    pub query_id: String,
    pub plan_id: Option<String>,
}

#[derive(Serialize, Deserialize, TS, Clone, Debug)]
#[ts(export)]
pub struct PlanOperatorView {
    pub id: u32,
    pub runs: f64,
    pub estimated_rows: f64,
    pub estimated_rows_for_runs: f64,
    pub rows_out: f64,
    pub rows_read: Option<f64>,
    pub misestimate: Option<f64>,
    pub guess: Option<Guess>,
    pub uncosted: bool,
    pub layout_own_share: f64,
    pub layout_subtree_share: f64,
    pub cost_share: Option<f64>,
    pub subtree_cost_share: Option<f64>,
    pub own_ms: Option<f64>,
    pub time_share: Option<f64>,
    pub subtree_time_share: Option<f64>,
    pub no_share_reason: Option<String>,
    pub finding_no_share_reason: Option<String>,
}

#[derive(Serialize, Deserialize, TS, Clone, Debug)]
#[ts(export)]
pub struct PlanInsightShare {
    pub rule: String,
    pub nodes: Vec<u32>,
    pub share: Option<f64>,
}

// Saved projections are output, not evidence. Ignore their schema on input and derive a current view from Plan facts.
pub(crate) mod output {
    use super::PlanView;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    pub fn serialize<S: Serializer>(view: &Option<PlanView>, serializer: S) -> Result<S::Ok, S::Error> {
        view.serialize(serializer)
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<PlanView>, D::Error> {
        let _ = serde::de::IgnoredAny::deserialize(deserializer)?;
        Ok(None)
    }
}
