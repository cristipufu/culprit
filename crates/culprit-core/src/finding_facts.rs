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
    // A retained fast-plan baseline for verifying recovery after it leaves the selected window.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub plan_baseline: Option<PlanBaseline>,
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

/// No SQL or parameters: safe to keep in a digest between scans.
#[derive(Serialize, Deserialize, TS, Clone, Debug, PartialEq)]
#[ts(export)]
pub struct PlanBaseline {
    pub query: String,
    pub mean_ms: f64,
    // Last observed run of the regressed plan. Recovery needs fresh runs after this observation.
    pub observed_until: String,
}

pub const PLAN_VARIANT_MIN_RUNS: f64 = 5.0;

impl PlanBaseline {
    pub(crate) fn recovery_problem(&self, snapshot: &crate::Snapshot) -> Option<String> {
        let unknown = || {
            Some("Fresh, sufficiently sampled plan runs have not established recovery against the earlier fast-plan baseline.".into())
        };
        let Some(w) = snapshot.workload.as_ref().filter(|w| w.kind() == Some(crate::WorkloadKind::QueryStore)) else {
            return unknown();
        };
        if let Some(reason) = snapshot.source_problem(&["query_store"]) {
            return Some(reason);
        }
        if snapshot.source_outcome("query_store").is_some_and(|o| o.state == crate::SourceState::NotApplicable) {
            return unknown();
        }
        let Some(q) = w.queries.iter().find(|q| q.id == self.query) else { return unknown() };
        let Some((start, end)) = w
            .window_start
            .as_deref()
            .and_then(crate::format::epoch_seconds)
            .zip(w.window_end.as_deref().and_then(crate::format::epoch_seconds))
        else {
            return unknown();
        };
        let Some(observed) = crate::format::epoch_seconds(&self.observed_until) else { return unknown() };
        if !self.mean_ms.is_finite() || self.mean_ms < 0.0 || q.plans.is_empty() {
            return unknown();
        }
        let recovered = q.plans.iter().all(|p| {
            let span = p
                .first_seen
                .as_deref()
                .and_then(crate::format::epoch_seconds)
                .zip(p.last_seen.as_deref().and_then(crate::format::epoch_seconds));
            p.executions.is_finite()
                && p.executions >= PLAN_VARIANT_MIN_RUNS
                && p.mean_time_ms.is_finite()
                && p.mean_time_ms >= 0.0
                && span.is_some_and(|(first, last)| first > observed && last >= first && first >= start && last <= end)
                && (p.mean_time_ms < self.mean_ms * 2.0 || p.mean_time_ms - self.mean_ms < 5.0)
        });
        if recovered { None } else { unknown() }
    }
}
