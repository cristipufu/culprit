//! Why analysis changed a detector's finding. Decisions describe evidence and policy, independently of rendered prose.
use serde::{Deserialize, Serialize};
use ts_rs::TS;

#[derive(Serialize, Deserialize, TS, Clone, Debug, PartialEq)]
#[ts(export)]
pub struct RefinementDecision {
    pub rule: String,
    pub finding: Option<String>,
    pub outcome: RefinementOutcome,
    pub queries: Vec<String>,
    pub tables: Vec<String>,
}

#[derive(Serialize, Deserialize, TS, Clone, Debug, PartialEq)]
#[serde(tag = "kind", rename_all = "kebab-case")]
#[ts(export)]
pub enum RefinementOutcome {
    Historical {
        basis: HistoryBasis,
        last_seen: Option<String>,
        quiet_seconds: Option<f64>,
    },
    Quiet {
        last_seen: String,
        seconds: f64,
    },
    LastExecution {
        last_seen: Option<String>,
    },
    InsufficientEvidence {
        missing_queries: Vec<String>,
    },
    FixWithheld {
        basis: FixWithholdingBasis,
    },
    HistoricalVerification,
    Absorbed {
        into: String,
    },
    EvidenceTransferred {
        into: String,
        queries: Vec<String>,
    },
    ColumnsCovered {
        by: Vec<String>,
        columns: Vec<String>,
    },
    Covered {
        by: Vec<String>,
    },
    Presentation {
        aspect: PresentationAspect,
    },
    /// Compatibility with an extension that implements the original mutating Refine interface. Its policy details
    /// were not supplied; this entry records invocation, not an inferred explanation of its changes.
    RuleApplied,
}

#[derive(Serialize, Deserialize, TS, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
#[ts(export)]
pub enum HistoryBasis {
    SourceMarked,
    SingleExecution,
    SingleLoggedExecution,
    Stopped,
    TablesAbsent,
}

#[derive(Serialize, Deserialize, TS, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
#[ts(export)]
pub enum FixWithholdingBasis {
    StatementsStopped,
    InsufficientMeasuredWork,
}

#[derive(Serialize, Deserialize, TS, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
#[ts(export)]
pub enum PresentationAspect {
    AbsoluteCost,
    ObservedActivity,
    DatabaseNaming,
    RepeatedCaveats,
}
