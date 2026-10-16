//! Every source Culprit reads on a SQLite file, in one list. The file is opened read-only and holds everything: its
//! catalog, its planner statistics and its pages. A file whose catalog can't be read has nothing to analyze, so that
//! one source fails the scan; any other that can't be read becomes a note, and the capability it would have delivered
//! is listed as missing with why.

use culprit_core::{Capability, Source};

/// sqlite_schema and the table, column, index and foreign key pragmas.
pub const CATALOG: Source =
    Source { name: "catalog", reads: "the file's catalog", delivers: &[Capability::TableStats] };
/// sqlite_stat1: the planner's row counts and index selectivity, where ANALYZE has run.
pub const PLANNER_STATISTICS: Source =
    Source { name: "statistics", reads: "planner statistics", delivers: &[Capability::Statistics] };
/// dbstat: every page of the file, for sizes and counted rows, read whole only within a time and size budget.
pub const SIZES: Source = Source { name: "sizes", reads: "table and index sizes", delivers: &[] };
/// The pragmas that shape how the file is read and written (journal mode, page size, foreign keys…).
pub const SETTINGS: Source = Source { name: "settings", reads: "file settings", delivers: &[] };
/// The application's own log of its statements (Rails, Django, EF Core, Ecto, JSON lines), read by `culprit-querylog`.
pub const QUERY_LOG: Source =
    Source { name: "query log", reads: "the application's query log", delivers: &[Capability::QueryLog] };

/// Every source above, for listings (`culprit sources`).
pub const ALL: &[Source] = &[CATALOG, PLANNER_STATISTICS, SIZES, SETTINGS, QUERY_LOG];
