use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::plan::Plan;
use crate::provider::SnapshotOptions;

pub const SNAPSHOT_FORMAT: u32 = 1;

#[derive(Serialize, Deserialize, TS, Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[serde(rename_all = "lowercase")]
#[ts(export)]
pub enum Engine {
    Mssql,
    Postgres,
    Mysql,
}

impl Engine {
    pub fn label(self) -> &'static str {
        match self {
            Engine::Mssql => "SQL Server",
            Engine::Postgres => "Postgres",
            Engine::Mysql => "MySQL",
        }
    }
}

impl fmt::Display for Engine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

#[derive(Serialize, Deserialize, TS, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
#[ts(export)]
pub enum Environment {
    Production,
    Staging,
    #[default]
    Development,
    Local,
}

impl Environment {
    pub fn is_production(self) -> bool {
        self == Environment::Production
    }
}

#[derive(Serialize, Deserialize, TS, Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[serde(rename_all = "kebab-case")]
#[ts(export)]
pub enum Capability {
    QueryStore,
    PlanCache,
    PgStatStatements,
    PerformanceSchema,
    QueryPlans,
    GenericPlans,
    TableStats,
    IndexUsage,
    MissingIndexes,
    Statistics,
    WaitStats,
    ResourceUsage,
    Sessions,
    Showplan,
    SlowLog,
}

impl Capability {
    /// The label in `engine`'s own words: estimated plans are SHOWPLAN on SQL Server and EXPLAIN elsewhere.
    pub fn label_for(self, engine: Engine) -> &'static str {
        match (self, engine) {
            (Capability::Showplan, Engine::Postgres | Engine::Mysql) => "Estimated plans (EXPLAIN)",
            _ => self.label(),
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Capability::QueryStore => "Query Store",
            Capability::PlanCache => "Plan cache statistics",
            Capability::PgStatStatements => "pg_stat_statements",
            Capability::PerformanceSchema => "performance_schema statement digests",
            Capability::QueryPlans => "Query plans",
            Capability::GenericPlans => "Generic plans (Postgres 16+)",
            Capability::TableStats => "Table statistics",
            Capability::IndexUsage => "Index usage statistics",
            Capability::MissingIndexes => "Missing-index suggestions",
            Capability::Statistics => "Optimizer statistics",
            Capability::WaitStats => "Wait statistics",
            Capability::ResourceUsage => "Resource usage",
            Capability::Sessions => "Sessions and blocking",
            Capability::Showplan => "Estimated plans (SHOWPLAN)",
            Capability::SlowLog => "Slow query log table",
        }
    }
}

#[derive(Serialize, Deserialize, TS, Clone, Debug)]
#[ts(export)]
pub struct MissingCapability {
    pub capability: Capability,
    pub reason: String,
    pub fix: Option<String>,
}

#[derive(Serialize, Deserialize, TS, Clone, Debug)]
#[ts(export)]
pub struct ServerInfo {
    pub engine: Engine,
    pub product: String,
    pub version: String,
    pub edition: Option<String>,
    pub cloud: Option<String>,
    pub database: String,
    pub database_size_bytes: Option<f64>,
    pub started_at: Option<String>,
    pub read_only_session: bool,
    pub capabilities: Vec<Capability>,
    pub missing: Vec<MissingCapability>,
}

/// Whether a server version string ("5.7.44-log", "10.5.29-MariaDB-ubu2004", "8.0.36") is at least `wanted`. Text
/// that doesn't start with a version counts as new enough, so an unknown build is never refused for its age.
pub fn version_at_least(version: &str, wanted: [u32; 3]) -> bool {
    let numbers: Vec<u32> = version
        .split(|c: char| !c.is_ascii_digit() && c != '.')
        .next()
        .unwrap_or_default()
        .split('.')
        .map_while(|p| p.parse().ok())
        .collect();
    if numbers.is_empty() {
        return true;
    }
    let have = [0, 1, 2].map(|i| numbers.get(i).copied().unwrap_or(0));
    have >= wanted
}

impl ServerInfo {
    pub fn has(&self, capability: Capability) -> bool {
        self.capabilities.contains(&capability)
    }

    /// The server's name in sentences: the engine's, except MariaDB, which the MySQL provider also serves.
    pub fn name(&self) -> &'static str {
        if self.engine == Engine::Mysql && self.product.starts_with("MariaDB") {
            "MariaDB"
        } else {
            self.engine.label()
        }
    }
}

#[derive(Serialize, Deserialize, TS, Clone, Debug)]
#[ts(export)]
pub struct Workload {
    pub source: String,
    /// The source keeps history by time, so a scan can choose its window (Query Store). Cumulative sources
    /// (pg_stat_statements, the plan cache) cover "since the last reset" and ignore the requested window.
    #[serde(default)]
    pub windowed: bool,
    pub window_start: Option<String>,
    pub window_end: Option<String>,
    pub window_hours: f64,
    pub total_time_ms: f64,
    pub total_cpu_ms: Option<f64>,
    pub total_executions: f64,
    pub query_count: f64,
    pub queries: Vec<QueryStat>,
    /// Database time across the window, oldest first. `None` when the source keeps no history and Culprit has no
    /// earlier scans of this connection to compare with.
    #[serde(default)]
    pub timeline: Option<Timeline>,
}

impl Workload {
    pub fn per_day(&self, value: f64) -> f64 {
        if self.window_hours <= 0.0 {
            return value;
        }
        value * 24.0 / self.window_hours
    }

    pub fn share(&self, time_ms: f64) -> f64 {
        if self.total_time_ms <= 0.0 {
            return 0.0;
        }
        (time_ms / self.total_time_ms).clamp(0.0, 1.0)
    }

    pub fn query(&self, id: &str) -> Option<&QueryStat> {
        self.queries.iter().find(|q| q.id == id)
    }
}

#[derive(Serialize, Deserialize, TS, Clone, Debug)]
#[ts(export)]
pub struct QueryStat {
    pub id: String,
    pub query_hash: Option<String>,
    pub text: String,
    /// Parameter declarations sent with the statement, e.g. `@P1 nvarchar(4000)`; they explain implicit conversions.
    pub parameters: Option<String>,
    pub object_name: Option<String>,
    pub executions: f64,
    pub total_time_ms: f64,
    pub mean_time_ms: f64,
    pub max_time_ms: Option<f64>,
    pub stddev_time_ms: Option<f64>,
    pub cpu_time_ms: Option<f64>,
    pub rows: Option<f64>,
    pub logical_reads: Option<f64>,
    pub physical_reads: Option<f64>,
    pub temp_writes: Option<f64>,
    pub first_seen: Option<String>,
    pub last_seen: Option<String>,
    pub plans: Vec<PlanVariant>,
    pub plan: Option<Plan>,
    pub waits: Vec<WaitStat>,
    /// This query's part of each bucket of the workload's timeline (only the heaviest queries carry one).
    #[serde(default)]
    pub trend: Option<QueryTrend>,
    /// How run times spread beyond the average, as far as the engine keeps it.
    #[serde(default)]
    pub spread: Option<Spread>,
    /// Failed runs, planning, JIT, log volume and result size, as far as the engine keeps them.
    #[serde(default)]
    pub costs: Option<crate::signals::QueryCosts>,
}

/// How a statement's run times spread. Every engine keeps something different: SQL Server and Postgres the fastest
/// run, MySQL percentiles and a run-time histogram (8.0.19+). Missing values are `None`, never zero.
#[derive(Serialize, Deserialize, TS, Clone, Debug, Default, PartialEq)]
#[ts(export)]
pub struct Spread {
    pub min_ms: Option<f64>,
    pub p50_ms: Option<f64>,
    pub p95_ms: Option<f64>,
    pub p99_ms: Option<f64>,
    /// `[low_ms, high_ms, runs]` per bucket, fastest first (MySQL).
    #[serde(default)]
    pub histogram: Vec<[f64; 3]>,
    /// Time spent waiting for locks across all runs (MySQL).
    #[serde(default)]
    pub lock_ms: Option<f64>,
}

/// Where a timeline's buckets come from.
#[derive(Serialize, Deserialize, TS, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "kebab-case")]
#[ts(export)]
pub enum TimelineSource {
    /// Query Store's runtime-stats intervals, folded into equal buckets.
    #[default]
    QueryStore,
    /// Differences between Culprit's own saved scans of a source that only keeps running totals.
    Scans,
    /// Runs logged one by one (MySQL and MariaDB's slow query log table), bucketed by when they ran.
    SlowLog,
}

/// Database time and executions per bucket. Buckets are equal for Query Store and fall between scans otherwise.
#[derive(Serialize, Deserialize, TS, Clone, Debug, Default)]
#[ts(export)]
pub struct Timeline {
    pub source: TimelineSource,
    /// Start of each bucket (UTC, ISO 8601). Bucket `i` ends where bucket `i + 1` starts; the last one ends at `end`.
    pub starts: Vec<String>,
    pub end: String,
    pub time_ms: Vec<f64>,
    pub executions: Vec<f64>,
}

impl Timeline {
    pub fn len(&self) -> usize {
        self.starts.len()
    }

    pub fn is_empty(&self) -> bool {
        self.starts.is_empty()
    }

    /// Start and end of bucket `i`.
    pub fn bounds(&self, i: usize) -> (&str, &str) {
        let end = self.starts.get(i + 1).unwrap_or(&self.end);
        (self.starts[i].as_str(), end.as_str())
    }
}

/// One query's database time and executions in each bucket of the workload timeline. `None` marks a bucket the
/// source can't account for (an earlier scan didn't track this query, or its counters were reset).
#[derive(Serialize, Deserialize, TS, Clone, Debug, Default)]
#[ts(export)]
pub struct QueryTrend {
    pub time_ms: Vec<Option<f64>>,
    pub executions: Vec<Option<f64>>,
}

impl QueryTrend {
    /// Average duration in bucket `i`, when the query ran in it.
    pub fn mean_ms(&self, i: usize) -> Option<f64> {
        let time = (*self.time_ms.get(i)?)?;
        let runs = (*self.executions.get(i)?)?;
        (runs > 0.0).then(|| time / runs)
    }
}

impl QueryStat {
    pub fn rows_per_execution(&self) -> Option<f64> {
        let rows = self.rows?;
        (self.executions > 0.0).then(|| rows / self.executions)
    }

    pub fn reads_per_execution(&self) -> Option<f64> {
        let reads = self.logical_reads?;
        (self.executions > 0.0).then(|| reads / self.executions)
    }
}

#[derive(Serialize, Deserialize, TS, Clone, Debug)]
#[ts(export)]
pub struct PlanVariant {
    pub id: String,
    pub executions: f64,
    pub total_time_ms: f64,
    pub mean_time_ms: f64,
    pub first_seen: Option<String>,
    pub last_seen: Option<String>,
    pub is_forced: bool,
    #[serde(default)]
    pub min_time_ms: Option<f64>,
    #[serde(default)]
    pub max_time_ms: Option<f64>,
    #[serde(default)]
    pub stddev_time_ms: Option<f64>,
    /// The parameter values this plan was compiled for, as the engine reports them (SQL Server showplan).
    #[serde(default)]
    pub compiled: Option<String>,
    /// SQL Server 2022 parameter-sensitive plans: the variant query this plan belongs to.
    #[serde(default)]
    pub variant_query: Option<String>,
}

#[derive(Serialize, Deserialize, TS, Clone, Debug)]
#[ts(export)]
pub struct TableStat {
    pub schema: String,
    pub name: String,
    pub rows: Option<f64>,
    pub total_bytes: Option<f64>,
    pub data_bytes: Option<f64>,
    pub index_bytes: Option<f64>,
    pub has_primary_key: bool,
    pub is_heap: Option<bool>,
    pub seq_scans: Option<f64>,
    pub seq_rows_read: Option<f64>,
    pub index_scans: Option<f64>,
    pub inserts: Option<f64>,
    pub updates: Option<f64>,
    pub deletes: Option<f64>,
    pub dead_rows: Option<f64>,
    pub modifications_since_analyze: Option<f64>,
    pub last_vacuum: Option<String>,
    pub last_analyze: Option<String>,
    pub forwarded_records: Option<f64>,
    pub xid_age: Option<f64>,
    /// Reads of a heap that followed a forwarding pointer to the row's new page, since the table's metadata was cached
    /// (SQL Server).
    #[serde(default)]
    pub forwarded_fetches: Option<f64>,
    /// How updates land on its pages and how much is empty space (PostgreSQL).
    #[serde(default)]
    pub storage: Option<crate::signals::TableStorage>,
}

impl TableStat {
    pub fn qualified(&self) -> String {
        format!("{}.{}", self.schema, self.name)
    }
}

#[derive(Serialize, Deserialize, TS, Clone, Debug)]
#[ts(export)]
pub struct IndexStat {
    pub schema: String,
    pub table: String,
    pub name: String,
    pub kind: String,
    pub columns: Vec<String>,
    pub include: Vec<String>,
    pub filter: Option<String>,
    pub is_unique: bool,
    pub is_primary_key: bool,
    pub is_clustered: bool,
    pub is_disabled: bool,
    pub size_bytes: Option<f64>,
    pub reads: Option<f64>,
    pub writes: Option<f64>,
    pub seeks: Option<f64>,
    pub scans: Option<f64>,
    pub lookups: Option<f64>,
    pub last_read: Option<String>,
    pub definition: Option<String>,
    /// Lock and latch waits on this index (SQL Server).
    #[serde(default)]
    pub contention: Option<crate::signals::IndexContention>,
}

impl IndexStat {
    pub fn qualified_table(&self) -> String {
        format!("{}.{}", self.schema, self.table)
    }

    pub fn key_names(&self) -> Vec<&str> {
        self.columns.iter().map(|c| strip_direction(c)).collect()
    }
}

pub fn strip_direction(column: &str) -> &str {
    column.strip_suffix(" DESC").unwrap_or(column)
}

#[derive(Serialize, Deserialize, TS, Clone, Debug)]
#[ts(export)]
pub struct ColumnInfo {
    pub schema: String,
    pub table: String,
    pub name: String,
    /// Engine type as the database spells it, with length/precision: `varchar(20)`, `numeric(12,2)`, `timestamptz`.
    pub data_type: String,
    pub nullable: bool,
    pub default: Option<String>,
    /// Identity / auto-increment / serial-backed column.
    pub generated: bool,
    pub ordinal: u32,
}

#[derive(Serialize, Deserialize, TS, Clone, Debug)]
#[ts(export)]
pub struct ForeignKey {
    pub name: String,
    pub schema: String,
    pub table: String,
    pub columns: Vec<String>,
    pub ref_schema: String,
    pub ref_table: String,
    pub ref_columns: Vec<String>,
}

#[derive(Serialize, Deserialize, TS, Clone, Debug)]
#[ts(export)]
pub struct MissingIndex {
    pub schema: String,
    pub table: String,
    pub equality: Vec<String>,
    pub inequality: Vec<String>,
    pub include: Vec<String>,
    pub seeks: f64,
    pub scans: f64,
    pub avg_query_cost: f64,
    pub avg_impact_pct: f64,
    pub last_used: Option<String>,
    pub query_hashes: Vec<String>,
}

#[derive(Serialize, Deserialize, TS, Clone, Debug)]
#[ts(export)]
pub struct StatisticsInfo {
    pub schema: String,
    pub table: String,
    pub name: String,
    pub last_updated: Option<String>,
    pub rows: Option<f64>,
    pub rows_sampled: Option<f64>,
    pub modifications: Option<f64>,
    pub auto_created: bool,
}

#[derive(Serialize, Deserialize, TS, Clone, Debug)]
#[ts(export)]
pub struct WaitStat {
    pub category: String,
    pub wait_type: Option<String>,
    pub wait_ms: f64,
    pub tasks: Option<f64>,
}

#[derive(Serialize, Deserialize, TS, Clone, Debug)]
#[ts(export)]
pub struct Setting {
    pub name: String,
    pub value: String,
    pub default_value: Option<String>,
    pub scope: String,
}

#[derive(Serialize, Deserialize, TS, Clone, Debug)]
#[ts(export)]
pub struct ResourceSample {
    pub at: String,
    pub cpu_pct: Option<f64>,
    pub data_io_pct: Option<f64>,
    pub log_write_pct: Option<f64>,
    pub memory_pct: Option<f64>,
    pub workers_pct: Option<f64>,
}

#[derive(Serialize, Deserialize, TS, Clone, Debug)]
#[ts(export)]
pub struct CollectorNote {
    pub collector: String,
    pub message: String,
}

#[derive(Serialize, Deserialize, TS, Clone, Debug)]
#[ts(export)]
pub struct Snapshot {
    pub format: u32,
    pub id: String,
    pub connection: String,
    pub environment: Environment,
    pub taken_at: String,
    pub duration_ms: f64,
    pub server: ServerInfo,
    pub workload: Option<Workload>,
    pub tables: Vec<TableStat>,
    pub indexes: Vec<IndexStat>,
    #[serde(default)]
    pub columns: Vec<ColumnInfo>,
    pub foreign_keys: Vec<ForeignKey>,
    pub missing_indexes: Vec<MissingIndex>,
    pub statistics: Vec<StatisticsInfo>,
    pub waits: Vec<WaitStat>,
    pub resources: Vec<ResourceSample>,
    pub settings: Vec<Setting>,
    pub notes: Vec<CollectorNote>,
    /// Pressure signals from cheap counters (compiles, memory grants, cache hits, open transactions).
    #[serde(default)]
    pub pressure: Option<crate::waits::Pressure>,
    /// What the scan asked for; the workload's own window says what the source could actually cover.
    #[serde(default)]
    pub options: Option<SnapshotOptions>,
    /// Statements that differ only in literal values (Query Store query_hash, pg_stat_statements entries).
    #[serde(default)]
    pub query_families: Vec<crate::advice::QueryFamily>,
    /// Single-use plans in the plan cache (SQL Server).
    #[serde(default)]
    pub plan_cache: Option<crate::advice::PlanCacheUse>,
    /// Query Store's tuning recommendations, forced plans and hints (SQL Server).
    #[serde(default)]
    pub tuning: Option<crate::advice::TuningAdvice>,
    /// Deadlocks the engine recorded in the last day (graphs on SQL Server and MySQL, a counter on Postgres).
    #[serde(default)]
    pub deadlocks: Option<crate::deadlock::DeadlockReport>,
    /// Constraints the optimizer can't rely on: unchecked against existing rows or switched off.
    #[serde(default)]
    pub constraints: Vec<crate::signals::ConstraintIssue>,
    /// Column histograms (MySQL 8, MariaDB); `None` when the engine has none or they couldn't be read.
    #[serde(default)]
    pub histograms: Option<Vec<crate::signals::ColumnHistogram>>,
}

impl Snapshot {
    pub fn table(&self, schema: &str, name: &str) -> Option<&TableStat> {
        self.tables.iter().find(|t| t.schema.eq_ignore_ascii_case(schema) && t.name.eq_ignore_ascii_case(name))
    }

    pub fn indexes_on<'a>(&'a self, schema: &'a str, table: &'a str) -> impl Iterator<Item = &'a IndexStat> + 'a {
        self.indexes
            .iter()
            .filter(move |i| i.schema.eq_ignore_ascii_case(schema) && i.table.eq_ignore_ascii_case(table))
    }

    pub fn setting(&self, name: &str) -> Option<&Setting> {
        self.settings.iter().find(|s| s.name.eq_ignore_ascii_case(name))
    }

    pub fn queries(&self) -> &[QueryStat] {
        self.workload.as_ref().map(|w| w.queries.as_slice()).unwrap_or_default()
    }
}

#[derive(Serialize, Deserialize, TS, Clone, Debug)]
#[ts(export)]
pub struct Session {
    pub id: String,
    pub user: Option<String>,
    pub application: Option<String>,
    pub host: Option<String>,
    pub database: Option<String>,
    pub state: String,
    pub command: Option<String>,
    pub wait: Option<String>,
    pub wait_ms: Option<f64>,
    pub blocked_by: Option<String>,
    pub elapsed_ms: Option<f64>,
    pub transaction_ms: Option<f64>,
    pub cpu_ms: Option<f64>,
    pub reads: Option<f64>,
    pub sql: Option<String>,
}

#[derive(Serialize, Deserialize, TS, Clone, Debug)]
#[ts(export)]
pub struct LiveState {
    pub taken_at: String,
    pub sessions: Vec<Session>,
    pub waits: Vec<WaitStat>,
    /// What Live may do to other sessions on this connection; empty on production connections.
    #[serde(default)]
    pub session_actions: Vec<SessionAction>,
    /// Why `session_actions` is empty, in plain language.
    #[serde(default)]
    pub session_actions_note: Option<String>,
    /// Who blocks whom, as chains under each head blocker, when anyone waits on a lock.
    #[serde(default)]
    pub blocking: Option<crate::blocking::Blocking>,
}

/// A numeric session id from `live()` (SQL Server spid, Postgres pid, MySQL thread id). Ids are spliced into `KILL`, so
/// anything but digits is refused.
pub fn session_number(session: &str) -> crate::error::Result<u64> {
    let session = session.trim();
    if session.is_empty() || session.len() > 19 || !session.bytes().all(|b| b.is_ascii_digit()) {
        return Err(crate::error::Error::invalid(format!("'{session}' is not a session id")));
    }
    session.parse().map_err(|_| crate::error::Error::invalid(format!("'{session}' is not a session id")))
}

/// Something Live can do to another session. Never offered on production connections.
#[derive(Serialize, Deserialize, TS, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
#[ts(export)]
pub enum SessionAction {
    /// Stop the statement the session is running; the session stays connected.
    Cancel,
    /// Close the session; its open transaction rolls back.
    Terminate,
}

#[derive(Serialize, Deserialize, TS, Clone, Debug)]
#[ts(export)]
pub struct ResultSet {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<serde_json::Value>>,
    pub truncated: bool,
    pub elapsed_ms: f64,
    pub messages: Vec<String>,
}

pub type Properties = BTreeMap<String, String>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_versions_compare_by_number() {
        assert!(!version_at_least("5.7.44", [8, 0, 18]));
        assert!(!version_at_least("8.0.17-log", [8, 0, 18]));
        assert!(version_at_least("8.0.18", [8, 0, 18]));
        assert!(version_at_least("8.4.3", [8, 0, 18]));
        assert!(version_at_least("10.5.29-MariaDB-ubu2004", [10, 5, 0]));
        assert!(!version_at_least("10.5.29-MariaDB-ubu2004", [10, 6, 0]));
        assert!(version_at_least("8.0", [8, 0, 0]));
        // Unknown builds are never refused for their age.
        assert!(version_at_least("", [8, 0, 18]));
        assert!(version_at_least("unknown", [8, 0, 18]));
    }
}
