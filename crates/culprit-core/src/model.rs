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
}

impl Capability {
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

impl ServerInfo {
    pub fn has(&self, capability: Capability) -> bool {
        self.capabilities.contains(&capability)
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
    /// What the scan asked for; the workload's own window says what the source could actually cover.
    #[serde(default)]
    pub options: Option<SnapshotOptions>,
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
