//! Every source Culprit reads on PostgreSQL, in one list. A scan reads each through `Coverage::read`: a source that
//! fails becomes a note, and the capabilities it delivers are locked with why. A source without capabilities adds
//! detail some checks use (a table's HOT updates, a replica's lag); without it those checks say less, never something
//! wrong.

use culprit_core::{Capability, Source, source_id};
pub const MULTIXACTS: Source =
    Source { name: source_id::MULTIXACTS, reads: "database and table MultiXact ages", delivers: &[] };
pub const ARCHIVER: Source =
    Source { name: source_id::ARCHIVER, reads: "shared WAL archive failure counters", delivers: &[] };
pub const SUBSCRIPTION_ERRORS: Source =
    Source { name: source_id::SUBSCRIPTION_ERRORS, reads: "logical subscription error counters (15+)", delivers: &[] };
pub const STANDBY_CONFLICTS: Source =
    Source { name: source_id::STANDBY_CONFLICTS, reads: "standby query cancellation counters", delivers: &[] };
pub const SEQUENCE_CAPACITY: Source =
    Source { name: source_id::SEQUENCE_CAPACITY, reads: "visible sequence allocation limits", delivers: &[] };

/// Statement statistics: runs, time, rows and buffers per normalized statement.
pub const STATEMENTS: Source =
    Source { name: source_id::WORKLOAD, reads: "pg_stat_statements", delivers: &[Capability::PgStatStatements] };
/// pg_stat_user_tables and the catalog's sizes: rows, scans, writes, dead rows, analyze and vacuum times.
pub const TABLES: Source = Source {
    name: source_id::TABLES,
    reads: "table statistics",
    delivers: &[Capability::TableStats, Capability::Statistics],
};
/// pg_index with pg_stat_user_indexes: keys, kinds, sizes and reads per index.
pub const INDEXES: Source =
    Source { name: source_id::INDEXES, reads: "index statistics", delivers: &[Capability::IndexUsage] };
pub const COLUMNS: Source = Source { name: source_id::COLUMNS, reads: "columns", delivers: &[] };
pub const FOREIGN_KEYS: Source = Source { name: source_id::FOREIGN_KEYS, reads: "foreign keys", delivers: &[] };
/// pg_settings: the settings checks read, and what changed them.
pub const SETTINGS: Source = Source { name: source_id::SETTINGS, reads: "settings", delivers: &[] };
/// pg_stat_database: cache hits, temp files, commits and rollbacks, deadlocks, and how long they have counted.
pub const DATABASE_STATS: Source = Source { name: source_id::DATABASE_STATS, reads: "pg_stat_database", delivers: &[] };
/// pg_stat_checkpointer (17+) or pg_stat_bgwriter: checkpoints timed and requested.
pub const CHECKPOINTS: Source = Source { name: source_id::CHECKPOINTS, reads: "checkpoint counters", delivers: &[] };
/// pg_stat_activity: the oldest transaction holding cleanup back.
pub const OPEN_TRANSACTIONS: Source =
    Source { name: source_id::OPEN_TRANSACTIONS, reads: "open transactions", delivers: &[] };
pub const CLEANUP_SLOTS: Source = Source {
    name: source_id::CLEANUP_SLOTS,
    reads: "replication slots retaining row or catalog cleanup",
    delivers: &[],
};
pub const CLEANUP_PREPARED: Source =
    Source { name: source_id::CLEANUP_PREPARED, reads: "prepared transactions retaining cleanup", delivers: &[] };
/// Planning, JIT and WAL per statement (pg_stat_statements' newer columns).
pub const QUERY_COSTS: Source = Source { name: source_id::QUERY_COSTS, reads: "statement costs", delivers: &[] };
/// HOT and new-page updates, fillfactor and the bloat estimate per table.
pub const TABLE_STORAGE: Source = Source { name: source_id::TABLE_STORAGE, reads: "table storage", delivers: &[] };
/// CHECK constraints added NOT VALID.
pub const CONSTRAINTS: Source = Source { name: source_id::CONSTRAINTS, reads: "constraints", delivers: &[] };
/// The settings advice depends on (wal_compression, constraint_exclusion, max_slot_wal_keep_size).
pub const ADVICE_SETTINGS: Source =
    Source { name: source_id::ADVICE_SETTINGS, reads: "the settings advice depends on", delivers: &[] };
/// pg_stat_io, pg_stat_database and pg_stat_wal: read and write latency.
pub const FILE_LATENCY: Source = Source { name: source_id::FILE_LATENCY, reads: "file latency", delivers: &[] };
/// Sessions opened and abandoned (14+).
pub const SESSIONS: Source = Source { name: source_id::SESSIONS, reads: "session statistics", delivers: &[] };
/// Replicas, this server's own lag as a standby, and replication slots (10+).
pub const REPLICATION: Source = Source { name: source_id::REPLICATION, reads: "replication", delivers: &[] };

pub const HYPERTABLES: Source = Source {
    name: source_id::TIMESCALE_HYPERTABLES,
    reads: "TimescaleDB hypertables, dimensions and aggregate storage sizes",
    delivers: &[Capability::TableStats, Capability::Statistics, Capability::IndexUsage],
};
pub const CHUNKS: Source = Source {
    name: source_id::TIMESCALE_CHUNKS,
    reads: "TimescaleDB managed chunks and compressed storage sizes",
    delivers: &[],
};
pub const HYPERTABLE_INDEXES: Source =
    Source { name: source_id::TIMESCALE_INDEXES, reads: "TimescaleDB logical hypertable index sizes", delivers: &[] };

/// Sources sharing the auxiliary connection. A session failure accounts for all of them.
pub const MISC: &[Source] =
    &[SETTINGS, DATABASE_STATS, CHECKPOINTS, OPEN_TRANSACTIONS, CLEANUP_SLOTS, CLEANUP_PREPARED];

/// Every source above, for listings (`culprit sources`).
pub const ALL: &[Source] = &[
    HYPERTABLES,
    CHUNKS,
    HYPERTABLE_INDEXES,
    MULTIXACTS,
    ARCHIVER,
    SUBSCRIPTION_ERRORS,
    STANDBY_CONFLICTS,
    SEQUENCE_CAPACITY,
    STATEMENTS,
    TABLES,
    INDEXES,
    COLUMNS,
    FOREIGN_KEYS,
    SETTINGS,
    DATABASE_STATS,
    CHECKPOINTS,
    OPEN_TRANSACTIONS,
    CLEANUP_SLOTS,
    CLEANUP_PREPARED,
    QUERY_COSTS,
    TABLE_STORAGE,
    CONSTRAINTS,
    ADVICE_SETTINGS,
    FILE_LATENCY,
    SESSIONS,
    REPLICATION,
];
