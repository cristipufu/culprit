use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::model::{Capability, Engine, Snapshot};

#[derive(Serialize, Deserialize, TS, Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[serde(rename_all = "lowercase")]
#[ts(export)]
pub enum Category {
    Indexes,
    Queries,
    Maintenance,
    Configuration,
}

impl Category {
    pub fn label(self) -> &'static str {
        match self {
            Category::Indexes => "Indexes",
            Category::Queries => "Queries",
            Category::Maintenance => "Maintenance",
            Category::Configuration => "Configuration",
        }
    }
}

#[derive(Serialize, Deserialize, TS, Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[serde(rename_all = "lowercase")]
#[ts(export)]
pub enum Severity {
    Low,
    Medium,
    High,
}

#[derive(Serialize, Deserialize, TS, Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[serde(rename_all = "lowercase")]
#[ts(export)]
pub enum Confidence {
    Low,
    Medium,
    High,
}

#[derive(Serialize, Deserialize, TS, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
#[ts(export)]
pub enum SubjectKind {
    Query,
    Table,
    Index,
    Database,
    Setting,
}

#[derive(Serialize, Deserialize, TS, Clone, Debug)]
#[ts(export)]
pub struct Subject {
    pub kind: SubjectKind,
    pub name: String,
}

#[derive(Serialize, Deserialize, TS, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
#[ts(export)]
pub enum Unit {
    Ms,
    Count,
    Bytes,
    Ratio,
    Rows,
    Pages,
    Days,
    Text,
}

#[derive(Serialize, Deserialize, TS, Clone, Debug)]
#[ts(export)]
pub struct Metric {
    pub label: String,
    pub value: f64,
    pub unit: Unit,
    pub text: Option<String>,
}

impl Metric {
    pub fn new(label: impl Into<String>, value: f64, unit: Unit) -> Self {
        Metric { label: label.into(), value, unit, text: None }
    }

    pub fn text(label: impl Into<String>, text: impl Into<String>) -> Self {
        Metric { label: label.into(), value: 0.0, unit: Unit::Text, text: Some(text.into()) }
    }
}

#[derive(Serialize, Deserialize, TS, Clone, Debug, Default)]
#[ts(export)]
pub struct Impact {
    /// 0–100, the ranking key across all findings.
    pub score: f64,
    /// Share of total database time spent in the affected queries, 0–1.
    pub db_time_share: Option<f64>,
    /// Database time per day spent in the affected queries.
    pub time_ms_per_day: Option<f64>,
    /// Short human label for the impact column, e.g. "31% of DB time" or "6.1 GB unused".
    pub label: String,
}

#[derive(Serialize, Deserialize, TS, Clone, Debug)]
#[ts(export)]
pub struct QueryRef {
    pub id: String,
    pub plan_node: Option<u32>,
    pub note: Option<String>,
}

#[derive(Serialize, Deserialize, TS, Clone, Debug, PartialEq)]
#[ts(export)]
pub struct Fix {
    pub summary: String,
    pub sql: Option<String>,
    pub cost: Option<String>,
}

#[derive(Serialize, Deserialize, TS, Clone, Debug)]
#[ts(export)]
pub struct Finding {
    pub id: String,
    pub check: String,
    pub category: Category,
    pub severity: Severity,
    pub confidence: Confidence,
    pub title: String,
    pub summary: String,
    pub subject: Subject,
    pub impact: Impact,
    pub metrics: Vec<Metric>,
    pub queries: Vec<QueryRef>,
    pub fix: Option<Fix>,
    pub verify: Vec<String>,
    pub caveats: Vec<String>,
    /// When the affected queries last ran, for query findings.
    #[serde(default)]
    pub last_seen: Option<String>,
    /// The affected queries stopped running well before the end of the window: a deploy may have fixed or replaced them.
    #[serde(default)]
    pub inactive: bool,
}

#[derive(Serialize, Deserialize, TS, Clone, Debug)]
#[ts(export)]
pub struct CheckInfo {
    pub id: String,
    pub title: String,
    pub category: Category,
    pub engines: Vec<Engine>,
    pub requires: Vec<Capability>,
    pub description: String,
}

#[derive(Serialize, Deserialize, TS, Clone, Debug)]
#[ts(export)]
pub struct LockedCheck {
    pub check: CheckInfo,
    pub missing: Vec<Capability>,
}

#[derive(Serialize, Deserialize, TS, Clone, Debug)]
#[ts(export)]
pub struct Summary {
    pub opportunities: u32,
    pub by_category: Vec<CategoryCount>,
    pub by_severity: Vec<SeverityCount>,
    /// Share of database time touched by the three highest-ranked query findings.
    pub top_share: Option<f64>,
    pub headline: String,
    /// Why the scan has no statement statistics ("Query Store is OFF on this database."), `None` when it has them: one
    /// sentence every surface shows instead of deciding for itself which capability explains it.
    #[serde(default)]
    pub no_workload: Option<String>,
}

#[derive(Serialize, Deserialize, TS, Clone, Debug)]
#[ts(export)]
pub struct CategoryCount {
    pub category: Category,
    pub count: u32,
}

#[derive(Serialize, Deserialize, TS, Clone, Debug)]
#[ts(export)]
pub struct SeverityCount {
    pub severity: Severity,
    pub count: u32,
}

/// A snapshot together with everything derived from it. This is what a scan produces and what gets stored.
#[derive(Serialize, Deserialize, TS, Clone, Debug)]
#[ts(export)]
pub struct Report {
    pub snapshot: Snapshot,
    pub findings: Vec<Finding>,
    pub locked: Vec<LockedCheck>,
    pub summary: Summary,
}
