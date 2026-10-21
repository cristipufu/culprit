//! The collected evidence and component identities behind a report. Missing historical metadata stays unknown.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::{ChangeEvent, DatabaseIdentity, Engine, MeasurementContext, Snapshot};

#[derive(Serialize, Deserialize, TS, Clone, Debug, PartialEq, Eq)]
#[ts(export)]
pub struct ComponentVersion {
    pub component: String,
    pub version: Option<String>,
    pub revision: Option<String>,
}

impl ComponentVersion {
    pub fn package(component: &str, version: &str, revision: Option<&str>) -> Self {
        Self {
            component: component.into(),
            version: Some(version.into()),
            revision: revision.filter(|v| !v.trim().is_empty()).map(str::to_string),
        }
    }

    pub fn unknown(component: &str) -> Self {
        Self { component: component.into(), version: None, revision: None }
    }
}

#[derive(Serialize, Deserialize, TS, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
#[ts(export)]
pub enum AnalysisStage {
    Prepare,
    Check,
    Refine,
}

#[derive(Serialize, Deserialize, TS, Clone, Debug, PartialEq, Eq)]
#[ts(export)]
pub struct AnalysisExtension {
    pub stage: AnalysisStage,
    pub id: String,
    pub version: Option<String>,
}

#[derive(Serialize, Deserialize, TS, Clone, Debug, PartialEq, Eq)]
#[ts(export)]
pub struct PipelineProvenance {
    pub prepare: Vec<String>,
    pub checks: Vec<String>,
    pub refine: Vec<String>,
}

#[derive(Serialize, Deserialize, TS, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
#[ts(export)]
pub enum AnalysisOrigin {
    Snapshot,
    Collected,
    RetainedInput,
    PreparedReport,
}

#[derive(Serialize, Deserialize, TS, Clone, Debug, PartialEq, Eq)]
#[ts(export)]
pub struct CollectionProvenance {
    pub collector: ComponentVersion,
    pub started_at: String,
    pub completed_at: Option<String>,
}

#[derive(Serialize, Deserialize, TS, Clone, Debug, PartialEq, Eq)]
#[ts(export)]
pub struct ScanReference {
    pub id: Option<String>,
    pub taken_at: String,
    pub database: Option<DatabaseIdentity>,
}

impl ScanReference {
    pub fn of(snapshot: &Snapshot) -> Self {
        Self {
            id: (!snapshot.id.is_empty()).then(|| snapshot.id.clone()),
            taken_at: snapshot.taken_at.clone(),
            database: Some(snapshot.database_identity()),
        }
    }

    pub fn dated(taken_at: &str) -> Self {
        Self { id: None, taken_at: taken_at.into(), database: None }
    }
}

#[derive(Serialize, Deserialize, TS, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
#[ts(export)]
pub enum BaselinePurpose {
    WorkloadTrend,
    QueryActivity,
    WindowInference,
    IndexAge,
    Escalations,
    Contention,
    Evictions,
    TableGrowth,
    Operations,
}

#[derive(Serialize, Deserialize, TS, Clone, Debug, PartialEq)]
#[ts(export)]
pub struct BaselineReference {
    pub purpose: BaselinePurpose,
    pub scan: ScanReference,
    // Only the measurement actually used; index and operational counters never borrow a workload's clock.
    pub measurement: Option<MeasurementContext>,
}

impl Snapshot {
    /// The historical inputs retained with these measurements. Older baselines keep their date without inventing
    /// a scan id or borrowing the current endpoint. Workload clocks belong only to the workload inputs using them.
    pub fn baseline_references(&self) -> Vec<BaselineReference> {
        let mut references = self.workload.as_ref().map(|w| w.history_inputs.clone()).unwrap_or_default();
        let mut record = |purpose, at: &str, reference: &Option<ScanReference>, measurement| {
            let reference = BaselineReference {
                purpose,
                scan: reference.clone().unwrap_or_else(|| ScanReference::dated(at)),
                measurement,
            };
            if !references.contains(&reference) {
                references.push(reference);
            }
        };
        if let Some(b) = &self.escalations_before {
            record(BaselinePurpose::Escalations, &b.taken_at, &b.reference, None);
        }
        if let Some(b) = &self.contention_before {
            record(BaselinePurpose::Contention, &b.taken_at, &b.reference, None);
        }
        if let Some(b) = &self.evictions_before {
            record(BaselinePurpose::Evictions, &b.taken_at, &b.reference, b.measurement.clone());
        }
        if let Some(b) = &self.sizes_before {
            record(BaselinePurpose::TableGrowth, &b.taken_at, &b.reference, None);
        }
        for before in self.operations.counters.iter().filter_map(|c| c.before.as_ref()) {
            record(BaselinePurpose::Operations, &before.at, &before.reference, None);
        }
        if let Some(before) = self.operations.sqlite.as_ref().and_then(|s| s.before.as_ref()) {
            record(BaselinePurpose::Operations, &before.at, &before.reference, None);
        }
        references
    }
}

#[derive(Serialize, Deserialize, TS, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
#[ts(export)]
pub enum PlanParsingOutcome {
    Reparsed,
    NoNativeText,
    Retained,
}

#[derive(Serialize, Deserialize, TS, Clone, Debug, PartialEq, Eq)]
#[ts(export)]
pub struct PlanParsingProvenance {
    pub query: String,
    pub engine: Engine,
    pub parser: ComponentVersion,
    pub outcome: PlanParsingOutcome,
}

#[derive(Serialize, Deserialize, TS, Clone, Debug)]
#[ts(export)]
pub struct AnalysisProvenance {
    pub analyzer: ComponentVersion,
    pub pipeline: PipelineProvenance,
    pub extensions: Vec<AnalysisExtension>,
    pub origin: AnalysisOrigin,
    pub collection: Option<CollectionProvenance>,
    pub previous_analyzer: Option<ComponentVersion>,
    pub parsing: Vec<PlanParsingProvenance>,
    pub baselines: Vec<BaselineReference>,
}

/// Replay inputs are distinct from the prepared snapshot used by views. A collection is retained only when the
/// caller actually supplied the provider's picture before history enrichment; an ordinary snapshot isn't one.
#[derive(Serialize, Deserialize, TS, Clone, Debug)]
#[ts(export)]
pub struct ReportEvidence {
    pub input: Box<Snapshot>,
    pub collected: Option<Box<Snapshot>>,
    pub recorded_changes: Vec<ChangeEvent>,
}
