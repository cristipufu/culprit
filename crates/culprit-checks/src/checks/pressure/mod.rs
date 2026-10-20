//! Pressure checks: memory grants, compile load, worker and connection limits, open transactions that hold cleanup
//! back, results the application reads slowly, cloud tier limits, forced checkpoints and row-lock waits. Pure functions
//! of the snapshot's waits, pressure signals and resource samples.

use culprit_core::waits::category_key;
use culprit_core::{
    Category, CheckInfo, CleanupHolder, Confidence, Engine, Finding, Fix, GrantStat, Metric, Pressure, QueryStat,
    Severity, SignalStatus, SignalUnit, Snapshot, SubjectKind, Unit, format,
};

use culprit_core::Check;

use crate::support::*;

mod checkpoint_pressure;
mod compile_pressure;
mod health;
mod held_cleanup;
mod memory_grant;
mod memory_queue;
mod network_wait;
mod row_lock_waits;
#[cfg(test)]
mod tests;
mod tier_limit;
mod worker_pressure;

pub use checkpoint_pressure::*;
pub use compile_pressure::*;
pub use held_cleanup::*;
pub use memory_grant::*;
pub use memory_queue::*;
pub use network_wait::*;
pub use row_lock_waits::*;
pub use tier_limit::*;
pub use worker_pressure::*;

/// The checks this area registers (`crate::all()` runs every area's), in the order their findings tie.
pub(crate) const CHECKS: &[&dyn Check] = &[
    &MemoryGrantCheck,
    &MemoryQueueCheck,
    &CompilePressureCheck,
    &WorkerPressureCheck,
    &HeldCleanupCheck,
    &NetworkWaitCheck,
    &TierLimitCheck,
    &CheckpointPressureCheck,
    &RowLockWaitsCheck,
];

const MB: f64 = 1024.0;
/// A minute of waiting an hour: below it, a share of waiting is a share of very little.
const MINUTE_MS: f64 = 60_000.0;

fn pressure(snapshot: &Snapshot) -> Option<&Pressure> {
    snapshot.pressure.as_ref()
}

fn signal(snapshot: &Snapshot, key: &str) -> Option<f64> {
    pressure(snapshot)?.signal(key).map(|s| s.value)
}

/// Hours the since-startup counters cover, when that is at least an hour: a rate over less is no rate (see
/// [`counter_days_for_rate`]).
fn counter_hours(snapshot: &Snapshot) -> Option<f64> {
    pressure(snapshot)?.uptime_hours.filter(|h| *h >= culprit_core::RATE_MIN_HOURS)
}

/// Normalize counters and their periods before checks; queue/cleanup grades then follow the final findings.
pub(crate) use health::{grade_findings, grade_on_evidence};

/// How long the database-level waits (`snapshot.waits`) cover, and whether that is a measured window (Query Store's
/// intervals) rather than totals since the server started. `None` when unknown or under an hour.
fn waits_window(snapshot: &Snapshot) -> (Option<f64>, bool) {
    let windowed = snapshot.server.engine == Engine::Mssql && app_queries(snapshot).any(|q| !q.waits.is_empty());
    let hours = if windowed {
        snapshot.workload.as_ref().map(|w| w.measurement_hours())
    } else {
        pressure(snapshot).and_then(|p| p.uptime_hours)
    };
    (hours.filter(|h| *h >= culprit_core::RATE_MIN_HOURS), windowed)
}

/// A single sighting at the moment of the scan, with nothing on record: worth watching, not acting on.
const SEEN_ONCE: &str = "Seen once, when the scan ran, with nothing on record before it: sample Waits → Right now a few \
                         times before acting on it.";

fn uptime_phrase(snapshot: &Snapshot) -> String {
    let engine = snapshot.server.name();
    match pressure(snapshot).and_then(|p| p.uptime_hours) {
        Some(h) if h >= 47.0 => format!("since {engine} started {} days ago", (h / 24.0).round()),
        Some(h) if h >= 1.5 => format!("since {engine} started {} hours ago", h.round()),
        Some(h) => format!("since {engine} started {} minutes ago", (h * 60.0).round().max(1.0)),
        None => format!("since {engine} started"),
    }
}

fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    chars.next().map(|c| c.to_uppercase().collect::<String>() + chars.as_str()).unwrap_or_default()
}

/// Server-level findings key on `server`, never the connection's name: finding ids travel into exports, PR comments
/// and agent briefs, which must not name the connection. They only need to be stable within one connection's reports.
fn server_key(_: &Snapshot) -> String {
    "server".to_string()
}

fn kb(value: f64) -> String {
    format::bytes(value * 1024.0)
}

fn by_hash<'a>(snapshot: &'a Snapshot, hash: &str) -> Option<&'a QueryStat> {
    app_queries(snapshot).find(|q| q.query_hash.as_deref().is_some_and(|h| h.eq_ignore_ascii_case(hash)))
}

/// A PostgreSQL size setting ("1GB", "512MB", "64kB", or a bare number of MB as `max_wal_size` reports it) in MB.
fn size_in_mb(text: &str) -> Option<f64> {
    let text = text.trim();
    let digits = text.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(text.len());
    let number: f64 = text[..digits].parse().ok()?;
    let factor = match text[digits..].trim().to_ascii_lowercase().as_str() {
        "" | "mb" => 1.0,
        "kb" => 1.0 / 1024.0,
        "gb" => 1024.0,
        "tb" => 1024.0 * 1024.0,
        _ => return None,
    };
    Some(number * factor)
}
