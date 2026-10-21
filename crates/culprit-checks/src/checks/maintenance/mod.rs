//! Maintenance checks: statistics gone stale, heaps, dead rows autovacuum hasn't reached, and transaction ID age.
//! What they share (autovacuum's thresholds for a table) is here; each check is a file of its own.

use culprit_core::{
    Capability, Category, CheckInfo, Confidence, Engine, Finding, Fix, Metric, Severity, Snapshot, SubjectKind, Unit,
    format,
};

use culprit_core::Check;

use crate::support::*;

mod dead_rows;
mod heap_table;
mod multixact_age;
mod stale_stats;
mod xid_age;

pub use dead_rows::*;
pub use heap_table::*;
pub use stale_stats::*;
pub use xid_age::*;

/// The checks this area registers (`crate::all()` runs every area's), in the order their findings tie.
pub(crate) const CHECKS: &[&dyn Check] =
    &[&StaleStatsCheck, &HeapTableCheck, &DeadRowsCheck, &XidAgeCheck, &multixact_age::MultixactAgeCheck];

fn short_date(iso: &str) -> String {
    iso.get(..10).unwrap_or(iso).to_string()
}

pub(super) fn cleanup_holder_phrase(h: &culprit_core::OpenTransaction) -> String {
    let kind = match h.holder.unwrap_or(culprit_core::CleanupHolder::Session) {
        culprit_core::CleanupHolder::PreparedTransaction => format!("prepared transaction '{}'", h.session),
        culprit_core::CleanupHolder::ReplicationSlot => format!("replication slot '{}'", h.session),
        culprit_core::CleanupHolder::Session => format!("session {}", h.session),
    };
    match h.xid_age.filter(|a| *a > 0.0) {
        Some(age) => format!("{kind}, {} transactions behind", format::count(age)),
        None => kind,
    }
}

/// Dead rows past this many times a table's autovacuum trigger mean autovacuum is behind on it, not about to run.
const DEAD_MARGIN: f64 = 2.0;

/// The rows changed (`analyze`) or dead (`vacuum`) at which autovacuum visits `t`: the table's own setting, else the
/// server's, else PostgreSQL's default. `None` with autovacuum off for it.
fn autovacuum_trigger(snapshot: &Snapshot, t: &culprit_core::TableStat, live: f64, kind: &str) -> Option<f64> {
    let off = t.storage.as_ref().is_some_and(|s| s.autovacuum_off())
        || snapshot.setting("autovacuum").is_some_and(|s| !matches!(s.value.trim(), "on" | "true" | "1"));
    if off {
        return None;
    }
    let threshold = autovacuum_setting(snapshot, t, &format!("autovacuum_{kind}_threshold")).unwrap_or(50.0);
    let factor = autovacuum_setting(snapshot, t, &format!("autovacuum_{kind}_scale_factor"))
        .unwrap_or(if kind == "analyze" { 0.1 } else { 0.2 });
    let trigger = threshold + factor * live;
    let cap = (kind == "vacuum").then(|| autovacuum_setting(snapshot, t, "autovacuum_vacuum_max_threshold")).flatten();
    Some(cap.filter(|c| *c >= 0.0).map_or(trigger, |c| trigger.min(c)))
}
