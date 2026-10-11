//! What the engine itself knows about tuning (SQL Server's Query Store recommendations, forced plans, hints), statements
//! that differ only in literals, and the index advisor's consolidated view of a report.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::model::Engine;

/// Statements that differ only in literal values: each is compiled and cached on its own.
#[derive(Serialize, Deserialize, TS, Clone, Debug, Default, PartialEq)]
#[ts(export)]
pub struct QueryFamily {
    /// What groups them: SQL Server's query_hash, or the normalized text with value lists collapsed.
    pub key: String,
    /// One member's text, as the database recorded it.
    pub sample: String,
    /// Distinct statements (Query Store queries, pg_stat_statements entries) in the family.
    pub variants: f64,
    pub executions: f64,
    pub total_time_ms: f64,
    /// Plan cache memory the family's single-use plans hold (SQL Server), when known.
    pub cached_bytes: Option<f64>,
    /// A few member ids, to open them in Queries.
    pub query_ids: Vec<String>,
}

/// How the plan cache is used (SQL Server): plans compiled for one statement and never reused.
#[derive(Serialize, Deserialize, TS, Clone, Debug, Default, PartialEq)]
#[ts(export)]
pub struct PlanCacheUse {
    pub single_use_plans: f64,
    pub single_use_bytes: f64,
    pub total_plans: f64,
    pub total_bytes: f64,
}

/// SQL Server's own tuning state from Query Store and automatic tuning.
#[derive(Serialize, Deserialize, TS, Clone, Debug, Default, PartialEq)]
#[ts(export)]
pub struct TuningAdvice {
    pub automatic_tuning: Vec<AutomaticTuningOption>,
    pub recommendations: Vec<TuningRecommendation>,
    /// Plans forced in Query Store, with how often forcing failed.
    pub forced_plans: Vec<ForcedPlan>,
    /// Query Store hints (SQL Server 2022+, Azure SQL).
    pub hints: Vec<QueryHint>,
}

#[derive(Serialize, Deserialize, TS, Clone, Debug, Default, PartialEq)]
#[ts(export)]
pub struct AutomaticTuningOption {
    /// e.g. FORCE_LAST_GOOD_PLAN.
    pub name: String,
    pub desired: String,
    pub actual: String,
    /// Why the actual state differs from the desired one, when it does.
    pub reason: Option<String>,
}

#[derive(Serialize, Deserialize, TS, Clone, Debug, Default, PartialEq)]
#[ts(export)]
pub struct TuningRecommendation {
    pub name: String,
    /// FORCE_LAST_GOOD_PLAN for plan-choice regressions.
    pub kind: String,
    pub reason: Option<String>,
    /// Active, Verifying, Success, Reverted, Expired…
    pub state: Option<String>,
    pub query_id: Option<String>,
    pub regressed_plan_id: Option<String>,
    pub recommended_plan_id: Option<String>,
    /// SQL Server's estimate of how much faster the recommended plan is, 0–1.
    pub estimated_gain: Option<f64>,
    pub error_prone: bool,
    /// The T-SQL SQL Server would run to apply it.
    pub script: Option<String>,
    pub valid_since: Option<String>,
}

#[derive(Serialize, Deserialize, TS, Clone, Debug, Default, PartialEq)]
#[ts(export)]
pub struct ForcedPlan {
    pub query_id: String,
    pub plan_id: String,
    pub failures: f64,
    pub last_failure_reason: Option<String>,
    pub last_execution: Option<String>,
}

#[derive(Serialize, Deserialize, TS, Clone, Debug, Default, PartialEq)]
#[ts(export)]
pub struct QueryHint {
    pub query_id: String,
    pub hint: String,
    pub failures: f64,
    pub last_failure_reason: Option<String>,
}

/// The index advisor's plan for a database, built from one report: indexes to add, indexes to clean up, and tables
/// whose shape costs reads or writes. Every item points at the finding that explains it, when there is one.
#[derive(Serialize, Deserialize, TS, Clone, Debug, Default, PartialEq)]
#[ts(export)]
pub struct IndexAdvice {
    pub connection: String,
    pub engine: Option<Engine>,
    pub taken_at: String,
    /// "in the 6 days since the server last restarted": what the usage counters cover.
    pub counters: String,
    pub candidates: Vec<IndexCandidate>,
    pub cleanups: Vec<IndexCleanup>,
    pub tables: Vec<TableAdvice>,
    /// What this engine or login can't tell the advisor, in plain language.
    pub notes: Vec<String>,
}

#[derive(Serialize, Deserialize, TS, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
#[ts(export)]
pub enum CandidateSource {
    /// SQL Server's optimizer recorded a missing-index request.
    Optimizer,
    /// A captured plan reads the table in full and filters on columns no index leads with.
    PlanScan,
    /// A foreign key whose child columns no index leads with.
    ForeignKey,
}

#[derive(Serialize, Deserialize, TS, Clone, Debug, PartialEq)]
#[ts(export)]
pub struct IndexCandidate {
    pub schema: String,
    pub table: String,
    pub keys: Vec<String>,
    pub include: Vec<String>,
    pub source: CandidateSource,
    /// One plain sentence: why this index.
    pub reason: String,
    /// Copy-only DDL; Culprit never runs it.
    pub sql: Option<String>,
    /// An existing index that already leads with these keys and should be widened instead.
    pub widens: Option<String>,
    pub queries: Vec<AdviceQuery>,
    /// The optimizer requests merged into this candidate (SQL Server).
    pub requests: Vec<MergedRequest>,
    /// Share of database time of the queries that asked for it.
    pub db_time_share: Option<f64>,
    pub lookups: Option<f64>,
    pub estimated_improvement: Option<f64>,
    pub table_rows: Option<f64>,
    pub writes_per_day: Option<f64>,
    pub write_cost: Option<String>,
    pub verify: Vec<String>,
    pub finding_id: Option<String>,
}

#[derive(Serialize, Deserialize, TS, Clone, Debug, PartialEq)]
#[ts(export)]
pub struct AdviceQuery {
    pub id: String,
    pub text: String,
    pub share: Option<f64>,
    pub runs_per_day: Option<f64>,
}

#[derive(Serialize, Deserialize, TS, Clone, Debug, PartialEq)]
#[ts(export)]
pub struct MergedRequest {
    pub equality: Vec<String>,
    pub inequality: Vec<String>,
    pub include: Vec<String>,
    pub lookups: f64,
    pub improvement: f64,
}

#[derive(Serialize, Deserialize, TS, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
#[ts(export)]
pub enum CleanupKind {
    /// Same keys as another index.
    Duplicate,
    /// Its keys are the leading part of a wider index.
    Prefix,
    /// Shares its leading key with another index; one wider index could serve both.
    Overlap,
    /// Maintained on every write, never read.
    Unused,
}

#[derive(Serialize, Deserialize, TS, Clone, Debug, PartialEq)]
#[ts(export)]
pub struct IndexCleanup {
    pub kind: CleanupKind,
    pub schema: String,
    pub table: String,
    pub index: String,
    pub keys: Vec<String>,
    /// The index that makes this one unnecessary (duplicate, prefix, overlap).
    pub other: Option<String>,
    pub other_keys: Vec<String>,
    pub size_bytes: Option<f64>,
    pub reads: Option<f64>,
    pub writes: Option<f64>,
    pub reason: String,
    pub sql: Option<String>,
    pub caveats: Vec<String>,
    pub finding_id: Option<String>,
}

#[derive(Serialize, Deserialize, TS, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
#[ts(export)]
pub enum TableAdviceKind {
    /// Read in full over and over, with no index on what the queries filter by.
    ScanHeavy,
    /// A heap whose reads follow forwarding pointers.
    ForwardedFetches,
    /// Many indexes on a table that is written a lot.
    OverIndexed,
}

#[derive(Serialize, Deserialize, TS, Clone, Debug, PartialEq)]
#[ts(export)]
pub struct TableAdvice {
    pub kind: TableAdviceKind,
    pub schema: String,
    pub table: String,
    pub reason: String,
    pub rows: Option<f64>,
    pub indexes: Option<f64>,
    pub writes_per_day: Option<f64>,
    pub full_scans_per_day: Option<f64>,
    pub rows_scanned_per_day: Option<f64>,
    pub forwarded_per_day: Option<f64>,
    pub sql: Option<String>,
    pub queries: Vec<AdviceQuery>,
    pub finding_id: Option<String>,
}
