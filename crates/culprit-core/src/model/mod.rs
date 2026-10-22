use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::provider::SnapshotOptions;

#[cfg(test)]
mod tests;
mod workload;

pub use workload::{
    Cadence, PER_DAY_MIN_HOURS, PlanVariant, QueryStat, QueryTrend, RATE_MIN_HOURS, ScanSpan, Spread,
    StatementActivity, StatementCost, Timeline, TimelineSource, Workload, WorkloadKind, span_when, span_words,
};

pub const SNAPSHOT_FORMAT: u32 = 1;

#[derive(Serialize, Deserialize, TS, Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[serde(rename_all = "lowercase")]
#[ts(export)]
pub enum Engine {
    Mssql,
    Postgres,
    Mysql,
    /// A SQLite database file: no server, no statement statistics. Culprit reads its schema, settings and plans.
    Sqlite,
}

/// A unit enum's name as it serializes (`query-store`, `engine-built`): serde's own, so a label table can't drift from it.
pub fn wire_name<T: Serialize>(value: &T) -> String {
    serde_json::to_value(value).ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default()
}

impl Engine {
    /// Every engine, for tables kept per engine (see [`Engine::abilities`]).
    pub const ALL: [Engine; 4] = [Engine::Mssql, Engine::Postgres, Engine::Mysql, Engine::Sqlite];

    /// A server Culprit talks to: sessions, waits, deadlocks and server counters exist. A file the application opens
    /// itself (SQLite) has none of these.
    pub fn has_server(self) -> bool {
        self.abilities().sessions
    }
}

/// What an engine can answer (see [`Engine::abilities`]).
#[derive(Serialize, Deserialize, TS, Clone, Copy, Debug, PartialEq, Eq)]
#[ts(export)]
pub struct EngineAbilities {
    /// Sessions, blocking and the Live view.
    pub sessions: bool,
    /// Sampling what the server waits on right now.
    pub wait_sample: bool,
    /// Deadlocks it records (graphs or a counter).
    pub deadlocks: bool,
    /// Running a statement once and reporting what each step actually did (rows, timings).
    pub measures_runs: bool,
    /// Row and cost estimates in its plans.
    pub plan_estimates: bool,
    /// Statements that change data, from the editor.
    pub writes: bool,
    /// Two periods of statement history to compare (What changed).
    pub periods: bool,
    /// The actual plan of a statement's most recent run, kept by the engine (SQL Server 2019+).
    pub last_actual_plan: bool,
    /// Live per-step counters of a running statement, sent every time (SQL Server); elsewhere the plan only when asked.
    pub operator_progress: bool,
    /// A sign-in: a password, a token. A file opened from disk takes none.
    pub sign_in: bool,
}

/// One of an engine's [`EngineAbilities`], to ask about it or for the reason it lacks it ([`crate::EngineSpec::refusal`]).
#[derive(Serialize, Deserialize, TS, Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum Ability {
    Sessions,
    WaitSample,
    Deadlocks,
    MeasuresRuns,
    PlanEstimates,
    Writes,
    Periods,
    LastActualPlan,
    OperatorProgress,
    SignIn,
}

impl Ability {
    pub const ALL: [Ability; 10] = [
        Ability::Sessions,
        Ability::WaitSample,
        Ability::Deadlocks,
        Ability::MeasuresRuns,
        Ability::PlanEstimates,
        Ability::Writes,
        Ability::Periods,
        Ability::LastActualPlan,
        Ability::OperatorProgress,
        Ability::SignIn,
    ];
}

impl EngineAbilities {
    pub fn has(&self, ability: Ability) -> bool {
        match ability {
            Ability::Sessions => self.sessions,
            Ability::WaitSample => self.wait_sample,
            Ability::Deadlocks => self.deadlocks,
            Ability::MeasuresRuns => self.measures_runs,
            Ability::PlanEstimates => self.plan_estimates,
            Ability::Writes => self.writes,
            Ability::Periods => self.periods,
            Ability::LastActualPlan => self.last_actual_plan,
            Ability::OperatorProgress => self.operator_progress,
            Ability::SignIn => self.sign_in,
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
    /// The statements and their times from a log the application's framework writes, for an engine that keeps none.
    QueryLog,
}

impl Capability {
    /// The label in `engine`'s own words: estimated plans are SHOWPLAN on SQL Server and EXPLAIN elsewhere.
    pub fn label_for(self, engine: Engine) -> &'static str {
        match (self, engine) {
            (Capability::Showplan, Engine::Postgres | Engine::Mysql) => "Estimated plans (EXPLAIN)",
            (Capability::Showplan, Engine::Sqlite) => "Estimated plans (EXPLAIN QUERY PLAN)",
            _ => self.label(),
        }
    }

    /// Its stable id, as it serializes (`query-store`).
    pub fn id(self) -> String {
        wire_name(&self)
    }

    /// A source of statement statistics: without it a scan has no workload, and its reason says why.
    pub fn is_workload_source(self) -> bool {
        matches!(
            self,
            Capability::QueryStore
                | Capability::PlanCache
                | Capability::PgStatStatements
                | Capability::PerformanceSchema
                | Capability::SlowLog
                | Capability::QueryLog
        )
    }

    pub fn label(self) -> &'static str {
        match self {
            Capability::QueryStore => "Query Store",
            Capability::PlanCache => "Plan cache statistics",
            Capability::PgStatStatements => "pg_stat_statements",
            Capability::PerformanceSchema => "performance_schema statement digests",
            Capability::QueryPlans => "Query plans",
            Capability::GenericPlans => "Generic plans (PostgreSQL 12+)",
            Capability::TableStats => "Table statistics",
            Capability::IndexUsage => "Index usage statistics",
            Capability::MissingIndexes => "Missing-index suggestions",
            Capability::Statistics => "Optimizer statistics",
            Capability::WaitStats => "Wait statistics",
            Capability::ResourceUsage => "Resource usage",
            Capability::Sessions => "Sessions and blocking",
            Capability::Showplan => "Estimated plans (SHOWPLAN)",
            Capability::SlowLog => "Slow query log table",
            Capability::QueryLog => "Application query log",
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
    /// The managed service it runs on, as a person names it ("Amazon RDS"). For sentences; [`ServerInfo::platform`]
    /// is what code asks.
    pub cloud: Option<String>,
    /// The managed service it runs on (`None`: a server you run yourself). Older reports have only `cloud`.
    #[serde(default)]
    pub platform: Option<crate::platform::Platform>,
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

    /// The managed service it runs on: the one the provider detected, or for an older report the one its label names.
    pub fn platform(&self) -> Option<crate::platform::Platform> {
        self.platform.or_else(|| self.cloud.as_deref().and_then(crate::platform::Platform::from_label))
    }

    /// Whether it runs on `platform`.
    pub fn on(&self, platform: crate::platform::Platform) -> bool {
        self.platform() == Some(platform)
    }

    /// Whether it runs on a managed service at all: no superuser, no configuration file, no operating system.
    pub fn is_managed(&self) -> bool {
        self.platform().is_some() || self.cloud.is_some()
    }

    /// The server's name in sentences: the engine's, except MariaDB, which the MySQL provider also serves.
    pub fn name(&self) -> &'static str {
        if self.is_mariadb() { "MariaDB" } else { self.engine.label() }
    }

    /// MariaDB, which Culprit reads through the MySQL provider: where the two differ (histograms, userstat, ignored
    /// indexes), the rules ask this.
    pub fn is_mariadb(&self) -> bool {
        self.engine == Engine::Mysql && self.product.starts_with("MariaDB")
    }
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
    /// Bytes of the table's own rows (its heap or clustered index) kept in the rows' pages. With `lob_bytes` and
    /// `overflow_bytes`, how its data splits between the rows and the large values kept apart from them. `None` where
    /// the engine doesn't say.
    #[serde(default)]
    pub in_row_bytes: Option<f64>,
    /// Bytes of large values kept apart from the rows: SQL Server's LOB pages (`nvarchar(max)`, `varbinary(max)`,
    /// `xml`…), PostgreSQL's TOAST table.
    #[serde(default)]
    pub lob_bytes: Option<f64>,
    /// Bytes of variable-length values pushed off a row that outgrew its page (SQL Server row-overflow pages).
    #[serde(default)]
    pub overflow_bytes: Option<f64>,
    /// The partitioned table this one is a partition of (`schema.table`, PostgreSQL). Its rows, sizes and counters are
    /// the partition's own; judge the table as a whole at the parent. `None` for a table that isn't a partition.
    #[serde(default)]
    pub partition_of: Option<String>,
    /// The latest its usage counters (scans, reads, writes) may have started over (UTC), when a scan saw them shrink
    /// since the scan before without the server restarting: a statistics reset for the table, a database taken offline,
    /// restored or failed over. What they count covers the time since then at least. `None`: as old as the server's.
    #[serde(default)]
    pub counters_since: Option<String>,
    // The engine's table kind (SQLite: "table" or "virtual"). Absent in older scans and where not collected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    // The table's original CREATE statement, where collected. Virtual tables must keep their module and options.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub definition: Option<String>,
}

impl TableStat {
    pub fn qualified(&self) -> String {
        format!("{}.{}", self.schema, self.name)
    }

    pub fn is_virtual(&self) -> bool {
        self.kind.as_deref().is_some_and(|kind| kind.eq_ignore_ascii_case("virtual"))
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
    /// Engine-specific availability. `is_disabled` remains the compatibility flag for optimizer unavailability.
    #[serde(default)]
    pub state: Option<IndexState>,
    /// Whether writes maintain it; unknown for an old invalid PostgreSQL index without indisready evidence.
    #[serde(default)]
    pub maintained: Option<bool>,
    /// Whether this index has a sqlite_stat1 entry; unknown on other engines or an unread catalog.
    #[serde(default)]
    pub has_statistics: Option<bool>,
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
    /// The latest it may have been created (UTC), set by a scan only when something says it may be new: an earlier
    /// scan saw its table without it, or the database dated a change to its table that no earlier scan was there to
    /// see, or its usage counters started over (`counters_since`). Its usage counters cover the time since at least.
    /// `None`: as old as the counters, as far as anyone knows.
    #[serde(default)]
    pub created_at_latest: Option<String>,
    /// The partitioned index this one is a partition of (`schema.index`, PostgreSQL): every partition of a table gets its
    /// own copy of the parent's index. Judge it (unused, duplicate) at the parent. `None` for an index of its own.
    #[serde(default)]
    pub partition_of: Option<String>,
    /// Per key column, what the index compares it by when that isn't the column type's default: an operator class
    /// (`text_pattern_ops`, `jsonb_path_ops`) and/or a collation (`COLLATE "C"`), empty for a key that uses the
    /// defaults. Two indexes on the same columns with different ones serve different queries, so neither duplicates
    /// the other. `None` when every key uses the defaults, or the engine has no such thing.
    #[serde(default)]
    pub opclasses: Option<Vec<String>>,
    /// The latest its usage counters may have started over (UTC), when a scan saw them shrink or vanish since the scan
    /// before without the server restarting: a database taken offline, restored or failed over (SQL Server keeps index
    /// usage per database), an ALTER or OPTIMIZE of its table (MySQL), a statistics reset. No reads says nothing about
    /// the time before it; `created_at_latest` moves to it too, so a check that waits for a young index waits for these
    /// counters as well. `None`: as old as the server's counters, as far as anyone knows.
    #[serde(default)]
    pub counters_since: Option<String>,
    // Predicate relationships retained before shared-copy literal redaction; absent on ordinary collected indexes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub filter_facts: Option<IndexFilterFacts>,
}

impl IndexStat {
    pub fn same_filter(&self, other: &Self) -> bool {
        match (&self.filter_facts, &other.filter_facts) {
            (Some(a), Some(b)) => a.fingerprint == b.fingerprint,
            (None, None) => self.filter == other.filter,
            _ => false,
        }
    }
    pub fn qualified_table(&self) -> String {
        format!("{}.{}", self.schema, self.table)
    }

    pub fn key_names(&self) -> Vec<&str> {
        self.columns.iter().map(|c| strip_direction(c)).collect()
    }
}

#[derive(Serialize, Deserialize, TS, Clone, Debug)]
#[ts(export)]
pub struct IndexFilterFacts {
    pub fingerprint: String,
    pub matching_queries: Vec<String>,
    // Only column names and redacted values; positive coverage is retained separately by statement identity.
    pub equalities: Option<Vec<(String, String)>>,
}

#[derive(Serialize, Deserialize, TS, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
#[ts(export)]
pub enum IndexState {
    Active,
    Disabled,
    Invalid,
    Invisible,
    Ignored,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub delete_rule: Option<String>,
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

/// The setting a provider adds when the connection reached a replica (a readable secondary, a standby, a replicating
/// MySQL): its value says which kind. A replica's usage counters count only the reads run on it, so they say nothing
/// about what the primary's workload uses.
pub const REPLICA_SETTING: &str = "replica";

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
    #[serde(default)]
    pub operations: crate::operations::OperationalStats,
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
    /// The lock escalations an earlier scan counted (SQL Server), set from the scans Culprit keeps or a previous
    /// digest: what tells escalations since then from old ones. `None` without an earlier scan to compare with.
    #[serde(default)]
    pub escalations_before: Option<crate::signals::EscalationBaseline>,
    /// Per-index lock counters read by an earlier scan of this database; absent without comparable history.
    #[serde(default)]
    pub contention_before: Option<crate::signals::ContentionBaseline>,
    /// pg_stat_statements' eviction rounds an earlier scan counted, set from the scans Culprit keeps: what tells rounds
    /// since then from old ones, since the count outlives restarts and a higher limit. `None` without an earlier scan
    /// that read them.
    #[serde(default)]
    pub evictions_before: Option<crate::signals::CountBaseline>,
    /// How big each table's data was at an earlier scan of the same database, set from the scans Culprit keeps: what
    /// tells how fast a table grows. `None` without an earlier scan.
    #[serde(default)]
    pub sizes_before: Option<crate::signals::SizeBaseline>,
    // Collection outcomes, separate from informational notes. Missing on scans saved by older versions.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sources: Vec<crate::coverage::SourceOutcome>,
    // Endpoint identity captured with the scan, never borrowed from today's connection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<crate::measurement::DatabaseIdentity>,
}

impl Snapshot {
    /// Why this scan has no query statistics ("Query Store is OFF on this database."), or `None` when it has them.
    pub fn no_workload_reason(&self) -> Option<String> {
        if self.workload.is_some() {
            return None;
        }
        Some(
            self.server
                .missing
                .iter()
                .find(|m| m.capability.is_workload_source())
                .map(|m| m.reason.clone())
                .unwrap_or_else(|| "This scan has no query history.".into()),
        )
    }

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
    #[serde(default)]
    pub notes: Vec<String>,
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

impl LiveState {
    pub fn blocking_incomplete(&self) -> bool {
        !self.notes.is_empty()
            || self.sessions.iter().any(|s| {
                s.blocked_by.is_none()
                    && s.wait
                        .as_deref()
                        .is_some_and(|w| w.to_ascii_lowercase().contains("lock") || w.starts_with("LCK_"))
            })
    }
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
