//! Index-set shape: indexes that overlap a better-used one, tables carrying many indexes under heavy writes, and tables
//! read in full over and over. Every engine where the statistics exist.

use culprit_core::{
    Capability, Category, CheckInfo, Confidence, Engine, Finding, Fix, IndexStat, Metric, QueryStat, Severity,
    Snapshot, SubjectKind, Unit, format,
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
pub(crate) fn hide_index_sql(snapshot: &Snapshot, i: &IndexStat) -> String {
    let server = &snapshot.server;
    let engine = server.engine;
    if i.is_disabled {
        return drop_index_for_snapshot(snapshot, &i.schema, &i.table, &i.name);
    }
    engine
        .spec()
        .hide_index(server, &i.schema, &i.table, &i.name)
        .unwrap_or_else(|| drop_index_for_snapshot(snapshot, &i.schema, &i.table, &i.name))
}
