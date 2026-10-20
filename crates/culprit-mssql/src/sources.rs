//! Every source Culprit reads on SQL Server, in one list. A scan reads each through [`read`], which is
//! `Coverage::read` for this driver's errors: a source that fails becomes a note, and the capabilities it delivers are
//! locked with why (a permission error says so, with the grant this login needs). A source without capabilities adds
//! detail some checks use; without it those checks say less, never something wrong.

use culprit_core::{Capability, Coverage, ErrorCode, Source};
pub const LOG_CAPACITY: Source =
    Source { name: "log_capacity", reads: "database log space and log reuse reason", delivers: &[] };
pub const FILE_CAPACITY: Source =
    Source { name: "file_capacity", reads: "data file space and growth limits", delivers: &[] };
pub const TEMP_CAPACITY: Source = Source { name: "temp_capacity", reads: "tempdb shared space", delivers: &[] };
pub const SEQUENCE_CAPACITY: Source =
    Source { name: "sequence_capacity", reads: "sequence and identity allocation limits", delivers: &[] };

use crate::conn::{is_permission_error, map_error};

/// Query Store's runtime statistics, plans and waits per query, by interval.
pub const QUERY_STORE: Source =
    Source { name: "query_store", reads: "Query Store", delivers: &[Capability::QueryStore] };
/// sys.dm_exec_query_stats: totals per cached plan, where Query Store is off.
pub const PLAN_CACHE: Source =
    Source { name: "plan_cache", reads: "plan cache statistics", delivers: &[Capability::PlanCache] };
/// Row counts, sizes and heap or clustered storage per table.
pub const TABLES: Source = Source { name: "tables", reads: "table statistics", delivers: &[Capability::TableStats] };
/// Keys, sizes and sys.dm_db_index_usage_stats per index.
pub const INDEXES: Source = Source { name: "indexes", reads: "index statistics", delivers: &[Capability::IndexUsage] };
pub const COLUMNS: Source = Source { name: "columns", reads: "columns", delivers: &[] };
pub const FOREIGN_KEYS: Source = Source { name: "foreign_keys", reads: "foreign keys", delivers: &[] };
/// Statistics objects: when each was updated and how many rows changed since.
pub const STATISTICS: Source = Source { name: "statistics", reads: "statistics", delivers: &[Capability::Statistics] };
/// The optimizer's missing-index requests.
pub const MISSING_INDEXES: Source =
    Source { name: "missing_indexes", reads: "missing-index requests", delivers: &[Capability::MissingIndexes] };
/// Wait totals: this database's (Azure SQL Database) or the instance's.
pub const WAITS: Source = Source { name: "waits", reads: "wait statistics", delivers: &[Capability::WaitStats] };
/// sys.dm_db_resource_stats on Azure SQL Database: CPU, I/O, log and workers against the tier's limits.
pub const RESOURCES: Source =
    Source { name: "resources", reads: "resource statistics", delivers: &[Capability::ResourceUsage] };
/// Heaps' forwarded fetches, for the index advisor.
pub const FORWARDED_FETCHES: Source = Source { name: "forwarded_fetches", reads: "forwarded fetches", delivers: &[] };
/// Statements that differ only in their literals.
pub const QUERY_FAMILIES: Source = Source { name: "query_families", reads: "query families", delivers: &[] };
/// How much of the plan cache single-use plans take.
pub const PLAN_CACHE_USE: Source = Source { name: "plan cache use", reads: "plan cache use", delivers: &[] };
/// Automatic tuning's recommendations and forced plans.
pub const TUNING: Source = Source { name: "tuning", reads: "automatic tuning", delivers: &[] };
/// Database, scoped and server settings.
pub const SETTINGS: Source = Source { name: "settings", reads: "settings", delivers: &[] };
/// Performance counters: batches, compiles, page life.
pub const PERFORMANCE_COUNTERS: Source =
    Source { name: "performance counters", reads: "performance counters", delivers: &[] };
/// The last hour's resource use on Azure SQL Database, for the health signals.
pub const RESOURCE_USE: Source = Source { name: "resource use", reads: "resource use", delivers: &[] };
/// Queries waiting for a memory grant now.
pub const PENDING_GRANTS: Source = Source { name: "memory grants", reads: "pending memory grants", delivers: &[] };
/// Signal, thread pool and memory waits, for the health signals.
pub const WAIT_TOTALS: Source = Source { name: "wait totals", reads: "wait totals", delivers: &[] };
/// Memory granted and used per cached query.
pub const GRANTS: Source = Source { name: "memory grants per query", reads: "memory grants per query", delivers: &[] };
/// The oldest open transaction, and what it holds back.
pub const OPEN_TRANSACTIONS: Source = Source { name: "open transactions", reads: "open transactions", delivers: &[] };

/// CPU, reads and waits per query beyond time (Query Store's runtime statistics).
pub const QUERY_COSTS: Source = Source { name: "query costs", reads: "statement costs", delivers: &[] };
/// This database's compilation counts and timings for retained Query Store entries.
pub const QUERY_COMPILATION: Source =
    Source { name: "query compilation", reads: "Query Store compilation statistics", delivers: &[] };
/// Row and page lock waits, latch waits and escalations per index.
pub const INDEX_CONTENTION: Source = Source { name: "index contention", reads: "index contention", delivers: &[] };
/// Foreign keys and CHECK constraints not trusted.
pub const CONSTRAINTS: Source = Source { name: "constraints", reads: "constraints", delivers: &[] };
/// sys.dm_io_virtual_file_stats: read and write latency per file.
pub const FILE_LATENCY: Source = Source { name: "file latency", reads: "file latency", delivers: &[] };
/// Logins and connection resets, for connection churn.
pub const LOGINS: Source = Source { name: "logins", reads: "login counters", delivers: &[] };

/// Every source above, for listings (`culprit sources`).
pub const ALL: &[Source] = &[
    LOG_CAPACITY,
    FILE_CAPACITY,
    TEMP_CAPACITY,
    SEQUENCE_CAPACITY,
    QUERY_STORE,
    PLAN_CACHE,
    TABLES,
    INDEXES,
    COLUMNS,
    FOREIGN_KEYS,
    STATISTICS,
    MISSING_INDEXES,
    WAITS,
    RESOURCES,
    FORWARDED_FETCHES,
    QUERY_FAMILIES,
    PLAN_CACHE_USE,
    TUNING,
    SETTINGS,
    PERFORMANCE_COUNTERS,
    RESOURCE_USE,
    PENDING_GRANTS,
    WAIT_TOTALS,
    GRANTS,
    OPEN_TRANSACTIONS,
    QUERY_COSTS,
    QUERY_COMPILATION,
    INDEX_CONTENTION,
    CONSTRAINTS,
    FILE_LATENCY,
    LOGINS,
];

/// `Coverage::read` for tiberius errors: a permission error is one, with `grant` (what this login needs) as its fix.
pub(crate) fn read<T>(
    coverage: &mut Coverage,
    source: &Source,
    result: tiberius::Result<T>,
    grant: impl FnOnce() -> String,
) -> Option<T> {
    coverage.read(
        source,
        result.map_err(|err| {
            let denied = is_permission_error(&err);
            let mut error = map_error(err);
            if denied {
                error.code = ErrorCode::PermissionDenied;
                error.hint = Some(grant());
            }
            error
        }),
    )
}
