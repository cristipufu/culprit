//! Every source Culprit reads on MySQL and MariaDB, in one list. A scan reads each through `Coverage::read`: a source
//! that fails becomes a note, and the capabilities it delivers are locked with why. A source without capabilities adds
//! detail some checks use; without it those checks say less, never something wrong.

use culprit_core::{Capability, Source};
pub const LIVE_LOCKS: Source = Source { name: "live_locks", reads: "InnoDB lock relationships", delivers: &[] };
pub const METADATA_LOCKS: Source = Source {
    name: "metadata_locks",
    reads: "sys.schema_table_lock_waits (metadata lock dependencies; sys schema and enabled metadata-lock instrumentation required)",
    delivers: &[],
};
pub const REDO_PRESSURE: Source =
    Source { name: "redo_pressure", reads: "shared InnoDB redo buffer wait counter", delivers: &[] };
pub const CLUSTER_HEALTH: Source =
    Source { name: "cluster_health", reads: "Galera readiness and flow control (where enabled)", delivers: &[] };
pub const SEQUENCE_CAPACITY: Source =
    Source { name: "sequence_capacity", reads: "auto-increment allocation limits", delivers: &[] };

/// performance_schema's statement digests (or the slow query log table): runs, time, rows per statement.
pub const STATEMENTS: Source = Source {
    name: "workload",
    reads: "statement digests",
    delivers: &[Capability::PerformanceSchema, Capability::QueryPlans],
};
/// information_schema.TABLES and the engine's statistics: rows, sizes, writes.
pub const TABLES: Source = Source { name: "tables", reads: "tables", delivers: &[Capability::TableStats] };
pub const COLUMNS: Source = Source { name: "columns", reads: "columns", delivers: &[] };
/// information_schema.STATISTICS with performance_schema's (or userstat's) index usage.
pub const INDEXES: Source = Source { name: "indexes", reads: "indexes", delivers: &[Capability::IndexUsage] };
pub const FOREIGN_KEYS: Source = Source { name: "foreign_keys", reads: "foreign keys", delivers: &[] };
/// Global variables: the settings checks read.
pub const SETTINGS: Source = Source { name: "settings", reads: "settings", delivers: &[] };
/// performance_schema's wait summaries.
pub const WAITS: Source = Source { name: "waits", reads: "wait totals", delivers: &[] };
/// SHOW GLOBAL STATUS: buffer pool, temp tables, row locks, connections, uptime.
pub const GLOBAL_STATUS: Source = Source { name: "global status", reads: "global status", delivers: &[] };
/// information_schema.INNODB_TRX: the oldest open transaction.
pub const OPEN_TRANSACTIONS: Source = Source { name: "open transactions", reads: "open transactions", delivers: &[] };

/// The connection the signals below are read on.
pub const SIGNALS: Source = Source { name: "signals", reads: "the signals' connection", delivers: &[] };
/// Errors and warnings per statement digest.
pub const QUERY_ERRORS: Source = Source { name: "query errors", reads: "statement errors", delivers: &[] };
/// Column histograms (MySQL 8's COLUMN_STATISTICS, MariaDB's column_stats).
pub const HISTOGRAMS: Source = Source { name: "histograms", reads: "column statistics", delivers: &[] };
/// performance_schema's file I/O summaries: read and write latency.
pub const FILE_LATENCY: Source = Source { name: "file latency", reads: "file latency", delivers: &[] };
/// Connections opened, aborted and served from the thread cache.
pub const CONNECTIONS: Source = Source { name: "connections", reads: "connection counters", delivers: &[] };
/// read_only and thread_cache_size, for replication and connection advice.
pub const SERVER_VARIABLES: Source = Source { name: "server variables", reads: "server variables", delivers: &[] };
/// SHOW REPLICA STATUS (or SLAVE STATUS) and Group Replication.
pub const REPLICATION: Source = Source { name: "replication", reads: "replica status", delivers: &[] };

/// Every source above, for listings (`culprit sources`).
pub const ALL: &[Source] = &[
    REDO_PRESSURE,
    CLUSTER_HEALTH,
    SEQUENCE_CAPACITY,
    LIVE_LOCKS,
    METADATA_LOCKS,
    STATEMENTS,
    TABLES,
    COLUMNS,
    INDEXES,
    FOREIGN_KEYS,
    SETTINGS,
    WAITS,
    GLOBAL_STATUS,
    OPEN_TRANSACTIONS,
    SIGNALS,
    QUERY_ERRORS,
    HISTOGRAMS,
    FILE_LATENCY,
    CONNECTIONS,
    SERVER_VARIABLES,
    REPLICATION,
];
