//! Detector facts used by cross-finding decisions. Human metric labels and prose are presentation, never identifiers.
use serde::{Deserialize, Serialize};
use ts_rs::TS;

#[derive(Serialize, Deserialize, TS, Clone, Debug, Default, PartialEq)]
#[serde(default)]
#[ts(export)]
pub struct FindingFacts {
    pub histogram_columns: Vec<String>,
    pub index_keys: Vec<String>,
    pub network_wait_share: Option<f64>,
    pub lock_wait: Option<FindingLockWait>,
    pub plan_change: Option<FindingPlanChange>,
    pub recommended_plan: Option<FindingPlanRecommendation>,
    pub usual_fast_plan: Option<String>,
    pub bloat: Option<FindingBloat>,
    pub log_write: Option<FindingLogWrite>,
    pub large_rows: Option<FindingLargeRows>,
}
impl FindingFacts {
    pub fn is_empty(&self) -> bool {
        self == &Self::default()
    }
}

#[derive(Serialize, Deserialize, TS, Clone, Debug, PartialEq)]
#[ts(export)]
pub struct FindingLockWait {
    pub ms: f64,
    pub share: f64,
    pub versioning_spares_read: bool,
}

#[derive(Serialize, Deserialize, TS, Clone, Debug, PartialEq)]
#[ts(export)]
pub struct FindingPlanChange {
    pub from: String,
    pub to: String,
}

#[derive(Serialize, Deserialize, TS, Clone, Debug, PartialEq)]
#[ts(export)]
pub struct FindingPlanRecommendation {
    pub plan: String,
    pub cpu_saved_share: Option<f64>,
}

#[derive(Serialize, Deserialize, TS, Clone, Debug, PartialEq)]
#[ts(export)]
pub struct FindingBloat {
    pub bytes: f64,
    pub share: f64,
}

#[derive(Serialize, Deserialize, TS, Clone, Debug, PartialEq)]
#[ts(export)]
pub struct FindingLogWrite {
    pub bytes: f64,
    pub per_row: Option<f64>,
}

#[derive(Serialize, Deserialize, TS, Clone, Debug, PartialEq)]
#[ts(export)]
pub struct FindingLargeRows {
    pub rows: f64,
    pub size: f64,
    pub large: f64,
}
