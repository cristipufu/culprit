//! Every source Culprit reads on MySQL and MariaDB, in one list. A scan reads each through `Coverage::read`: a source
//! that fails becomes a note, and the capabilities it delivers are locked with why. A source without capabilities adds
//! detail some checks use; without it those checks say less, never something wrong.

use culprit_core::{Capability, Source, source_id};
pub const LIVE_LOCKS: Source =
    Source { name: source_id::LIVE_LOCKS, reads: "InnoDB lock relationships", delivers: &[] };
pub const METADATA_LOCKS: Source = Source {
    name: source_id::METADATA_LOCKS,
    reads: "sys.schema_table_lock_waits (metadata lock dependencies; sys schema and enabled metadata-lock instrumentation required)",
    delivers: &[],
};
pub const REDO_PRESSURE: Source =
    Source { name: source_id::REDO_PRESSURE, reads: "shared InnoDB redo buffer wait counter", delivers: &[] };
pub const CLUSTER_HEALTH: Source = Source {
    name: source_id::CLUSTER_HEALTH,
    reads: "Galera readiness and flow control (where enabled)",
    delivers: &[],
};
pub const SEQUENCE_CAPACITY: Source =
    Source { name: source_id::SEQUENCE_CAPACITY, reads: "auto-increment allocation limits", delivers: &[] };

/// performance_schema's statement digests (or the slow query log table): runs, time, rows per statement.
pub const STATEMENTS: Source = Source {
    name: source_id::WORKLOAD,
    reads: "statement digests",
    delivers: &[Capability::PerformanceSchema, Capability::QueryPlans],
};
/// information_schema.TABLES and the engine's statistics: rows, sizes, writes.
pub const TABLES: Source = Source { name: source_id::TABLES, reads: "tables", delivers: &[Capability::TableStats] };
pub const COLUMNS: Source = Source { name: source_id::COLUMNS, reads: "columns", delivers: &[] };
/// information_schema.STATISTICS with performance_schema's (or userstat's) index usage.
pub const INDEXES: Source = Source { name: source_id::INDEXES, reads: "indexes", delivers: &[Capability::IndexUsage] };
pub const FOREIGN_KEYS: Source = Source { name: source_id::FOREIGN_KEYS, reads: "foreign keys", delivers: &[] };
/// Global variables: the settings checks read.
pub const SETTINGS: Source = Source { name: source_id::SETTINGS, reads: "settings", delivers: &[] };
/// performance_schema's wait summaries.
pub const WAITS: Source = Source { name: source_id::WAITS, reads: "wait totals", delivers: &[] };
/// SHOW GLOBAL STATUS: buffer pool, temp tables, row locks, connections, uptime.
pub const GLOBAL_STATUS: Source = Source { name: source_id::GLOBAL_STATUS, reads: "global status", delivers: &[] };
/// information_schema.INNODB_TRX: the oldest open transaction.
pub const OPEN_TRANSACTIONS: Source =
    Source { name: source_id::OPEN_TRANSACTIONS, reads: "open transactions", delivers: &[] };

/// The connection the signals below are read on.
pub const SIGNALS: Source = Source { name: source_id::SIGNALS, reads: "the signals' connection", delivers: &[] };
/// Errors and warnings per statement digest.
pub const QUERY_ERRORS: Source = Source { name: source_id::QUERY_ERRORS, reads: "statement errors", delivers: &[] };
/// Column histograms (MySQL 8's COLUMN_STATISTICS, MariaDB's column_stats).
pub const HISTOGRAMS: Source = Source { name: source_id::HISTOGRAMS, reads: "column statistics", delivers: &[] };
/// performance_schema's file I/O summaries: read and write latency.
pub const FILE_LATENCY: Source = Source { name: source_id::FILE_LATENCY, reads: "file latency", delivers: &[] };
/// Connections opened, aborted and served from the thread cache.
pub const CONNECTIONS: Source = Source { name: source_id::CONNECTIONS, reads: "connection counters", delivers: &[] };
/// read_only and thread_cache_size, for replication and connection advice.
pub const SERVER_VARIABLES: Source =
    Source { name: source_id::SERVER_VARIABLES, reads: "server variables", delivers: &[] };
/// SHOW REPLICA STATUS (or SLAVE STATUS) and Group Replication.
pub const REPLICATION: Source = Source { name: source_id::REPLICATION, reads: "replica status", delivers: &[] };

pub const MISC: &[Source] = &[SETTINGS, WAITS, GLOBAL_STATUS, OPEN_TRANSACTIONS];
pub const SIGNAL_READS: &[Source] = &[
    QUERY_ERRORS,
    HISTOGRAMS,
    FILE_LATENCY,
    CONNECTIONS,
    SERVER_VARIABLES,
    REPLICATION,
    REDO_PRESSURE,
    CLUSTER_HEALTH,
    SEQUENCE_CAPACITY,
];

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
