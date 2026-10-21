//! Index-set shape: indexes that overlap a better-used one, tables carrying many indexes under heavy writes, and tables
//! read in full over and over. Every engine where the statistics exist.

use culprit_core::{
    Capability, Category, CheckInfo, Confidence, Engine, Finding, Fix, IndexStat, Metric, QueryStat, ServerInfo,
    Severity, Snapshot, SubjectKind, Unit, format,
};

use crate::indexes::{
    backs_foreign_key, is_unused, measured_writes, redundant_indexes, same_keys, scan_groups, too_new,
};
use culprit_core::Check;

use crate::support::*;

mod over_indexed;
mod overlapping_index;
mod scan_heavy;

pub use over_indexed::*;
pub use overlapping_index::*;
pub use scan_heavy::*;

/// The checks this area registers (`crate::all()` runs every area's), in the order their findings tie.
pub(crate) const CHECKS: &[&dyn Check] = &[&OverlappingIndexCheck, &OverIndexedCheck, &ScanHeavyCheck];

/// Reversible first step: SQL Server disables, MySQL makes invisible, MariaDB ignores (10.6+), Postgres can only drop
/// (keep the definition). An index that is already disabled, hidden or invalid is ready to drop.
pub(crate) fn hide_index_sql(server: &ServerInfo, i: &IndexStat) -> String {
    let engine = server.engine;
    if i.is_disabled {
        return drop_index_sql(engine, &i.schema, &i.table, &i.name);
    }
    if server.is_mariadb() {
        return match mariadb_can_ignore(server) {
            true => format!(
                "ALTER TABLE {} ALTER INDEX {} IGNORED;",
                qualified(engine, &i.schema, &i.table),
                quote(engine, &i.name)
            ),
            false => drop_index_sql(engine, &i.schema, &i.table, &i.name),
        };
    }
    match engine {
        Engine::Mssql => {
            format!("ALTER INDEX {} ON {} DISABLE;", quote(engine, &i.name), qualified(engine, &i.schema, &i.table))
        }
        Engine::Postgres => drop_index_sql(engine, &i.schema, &i.table, &i.name),
        Engine::Sqlite => drop_index_sql(engine, &i.schema, &i.table, &i.name),
        // Invisible indexes arrived in MySQL 8.0; 5.7 can only drop (keep the definition).
        Engine::Mysql if !culprit_core::version_at_least(&server.version, [8, 0, 0]) => {
            drop_index_sql(engine, &i.schema, &i.table, &i.name)
        }
        Engine::Mysql => format!(
            "ALTER TABLE {} ALTER INDEX {} INVISIBLE;",
            qualified(engine, &i.schema, &i.table),
            quote(engine, &i.name)
        ),
    }
}
