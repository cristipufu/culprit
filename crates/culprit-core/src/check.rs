use crate::finding::{CheckInfo, Finding};
use crate::model::Snapshot;
use serde::{Deserialize, Serialize};
use ts_rs::TS;

/// Whether the inputs let a check assess its subject. Missing evidence is never a clean bill of health.
#[derive(Clone, Debug)]
pub enum CheckEvidence {
    Available,
    Unavailable(String),
    Insufficient(String),
    NotApplicable(String),
}

#[derive(Serialize, Deserialize, TS, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
#[ts(export)]
pub enum CheckStatus {
    Findings,
    Assessed,
    Unavailable,
    InsufficientEvidence,
    NotApplicable,
}

#[derive(Serialize, Deserialize, TS, Clone, Debug)]
#[ts(export)]
pub struct CheckAssessment {
    pub check: String,
    pub title: String,
    pub status: CheckStatus,
    pub reason: Option<String>,
}

/// A check is a pure function from a snapshot to findings. It never touches a live connection.
pub trait Check: Send + Sync {
    fn info(&self) -> CheckInfo;
    fn run(&self, snapshot: &Snapshot) -> Vec<Finding>;
    /// The extension's version, when its author records one. It never inherits Culprit's own package version.
    fn version(&self) -> Option<&str> {
        None
    }
    /// Embedders can declare positive evidence too. Existing plugins still run, without claiming a negative result.
    fn evidence(&self, _snapshot: &Snapshot) -> CheckEvidence {
        CheckEvidence::Insufficient(
            "This check does not declare the evidence needed to establish a negative result.".into(),
        )
    }
}
