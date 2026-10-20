//! The per-database dashboard, composed from one report: a takeaway, the numbers behind it and the rows to open for
//! each of fourteen tiles. `now` (sessions right now) comes from a live read and is added with [`with_now`].
//!
//! Every tile says something on every engine: when an engine, login or scan can't show a signal, the tile is
//! `unavailable` and its takeaway says why, in the engine's own terms.

use std::collections::{BTreeMap, HashMap, HashSet};

use culprit_core::signals::{FileIo, FileKind, Replica, ReplicaRole, merge_logs};
use culprit_core::{
    Capability, Dashboard, Engine, Finding, ItemKind, LiveState, QueryStat, Report, ServerInfo, Severity, Signal,
    SignalStatus, SignalUnit, Snapshot, Tile, TileItem, TileKey, TileMetric, TileStatus, WaitScope, Workload, format,
};

use crate::support::{app_queries, query_handle, statement_handle, top_findings_share, window_phrase};

mod database_time;
mod deadlocks;
mod findings;
mod health;
mod indexes;
mod plans;
mod right_now;
mod signal_tiles;
#[cfg(test)]
mod tests;
mod top_queries;
mod waits;

use database_time::*;
use deadlocks::*;
use findings::*;
use health::*;
use indexes::*;
use plans::*;
pub use right_now::*;
use signal_tiles::*;
use top_queries::*;
use waits::*;

/// Below this many hours, per-day figures would be forecasts from a sliver of data (the UI's `PER_DAY_MIN_HOURS`).
const PER_DAY_MIN_HOURS: f64 = 20.0;

/// Findings about the query statistics themselves: a store that is off, full, forgetting or capturing too little.
const STATISTICS_FINDINGS: [&str; 10] = [
    "config:query-store",
    "config:query-store-size",
    "config:query-store-capture",
    "config:query-store-waits",
    "config:pg_stat_statements.track",
    "config:pg_stat_statements.max",
    "config:performance_schema",
    "config:statements_digest",
    "config:performance_schema_digests_size",
    "config:max_digest_length",
];

/// Most urgent first when two tiles share a status.
const PRIORITY: [TileKey; 14] = [
    TileKey::Now,
    TileKey::DbTime,
    TileKey::Findings,
    TileKey::Statistics,
    TileKey::Failures,
    TileKey::Replication,
    TileKey::Deadlocks,
    TileKey::Storage,
    TileKey::Health,
    TileKey::Connections,
    TileKey::Waits,
    TileKey::Plans,
    TileKey::Indexes,
    TileKey::TopQueries,
];

/// The dashboard for one report. The `now` tile says sessions weren't read until [`with_now`] adds a live read.
pub fn dashboard(report: &Report) -> Dashboard {
    let s = &report.snapshot;
    let tiles = vec![
        db_time(s),
        findings(report),
        top_queries(s),
        waits(s),
        not_read(s),
        health(report),
        deadlocks(report),
        indexes(report),
        plans(s),
        storage(report),
        connections(report),
        replication(report),
        failures(report),
        statistics(report),
    ];
    let mut d = Dashboard {
        connection: s.connection.clone(),
        engine: s.server.engine,
        product: s.server.product.clone(),
        database: s.server.database.clone(),
        environment: s.environment,
        report_id: s.id.clone(),
        taken_at: s.taken_at.clone(),
        coverage: s.workload.as_ref().map(|w| format!("{}, {}", w.source, window_phrase(w))),
        headline: String::new(),
        attention: Vec::new(),
        tiles,
    };
    summarize(&mut d);
    d
}

/// Put a live read into the dashboard and re-rank what needs a look.
pub fn with_now(d: &mut Dashboard, live: &LiveState) {
    let tile = now(live);
    match d.tiles.iter_mut().find(|t| t.key == TileKey::Now) {
        Some(slot) => *slot = tile,
        None => d.tiles.push(tile),
    }
    summarize(d);
}

/// Sessions couldn't be read: the tile says why and the rest of the dashboard stands.
pub fn without_now(d: &mut Dashboard, reason: &str) {
    let mut tile =
        Tile::new(TileKey::Now, TileStatus::Unavailable, format!("Couldn't read sessions: {}", sentence(reason)));
    tile.detail = Some("Everything else here comes from the latest scan.".into());
    match d.tiles.iter_mut().find(|t| t.key == TileKey::Now) {
        Some(slot) => *slot = tile,
        None => d.tiles.push(tile),
    }
    summarize(d);
}

fn summarize(d: &mut Dashboard) {
    // Without query statistics, turning them on comes first: only a problem happening right now is more urgent.
    let no_workload = d.tiles.iter().any(|t| t.key == TileKey::DbTime && t.status == TileStatus::Unavailable);
    let rank = |k: TileKey| match k {
        TileKey::Statistics if no_workload => 1,
        TileKey::Now => 0,
        _ => PRIORITY.iter().position(|p| *p == k).unwrap_or(PRIORITY.len()) + 1,
    };
    let mut attention: Vec<&Tile> =
        d.tiles.iter().filter(|t| matches!(t.status, TileStatus::Bad | TileStatus::Watch)).collect();
    attention.sort_by(|a, b| b.status.cmp(&a.status).then(rank(a.key).cmp(&rank(b.key))));
    d.attention = attention.iter().map(|t| t.key).collect();
    d.headline = match attention.first() {
        Some(t) => t.takeaway.clone(),
        None => {
            let missing = d.tiles.iter().filter(|t| t.status == TileStatus::Unavailable).count();
            match missing {
                0 => "Nothing needs a look right now.".into(),
                1 => "Nothing needs a look in what this scan can see; one tile has no data here.".into(),
                n => format!("Nothing needs a look in what this scan can see; {n} tiles have no data here."),
            }
        }
    };
}

/// "SQL Server", "PostgreSQL", "MySQL" or "MariaDB": the name people know the server by.
pub fn family(server: &ServerInfo) -> &'static str {
    if server.product.to_ascii_lowercase().contains("mariadb") {
        return "MariaDB";
    }
    match server.engine {
        Engine::Mssql => "SQL Server",
        Engine::Postgres => "PostgreSQL",
        Engine::Mysql => "MySQL",
        Engine::Sqlite => "SQLite",
    }
}

fn last(hours: f64) -> String {
    if (hours - 24.0).abs() < 0.5 { "the last day".into() } else { format!("the last {}", span(hours)) }
}

fn span(hours: f64) -> String {
    if hours >= 47.0 {
        return format!("{} days", (hours / 24.0).round());
    }
    if hours >= 1.5 {
        return format!("{} hours", hours.round());
    }
    let minutes = (hours * 60.0).round().max(1.0);
    if minutes == 1.0 { "1 minute".into() } else { format!("{minutes} minutes") }
}

fn no_workload(key: TileKey, what: &str) -> Tile {
    let mut t = Tile::new(key, TileStatus::Unavailable, format!("No query history, so {what}."));
    t.detail = Some("Query statistics shows how to get them.".into());
    t
}

fn capitalized(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(c) => c.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

fn severity_status(s: Severity) -> TileStatus {
    match s {
        Severity::High => TileStatus::Bad,
        Severity::Medium => TileStatus::Watch,
        Severity::Low => TileStatus::Info,
    }
}

fn plural(n: usize, one: &str, many: &str) -> String {
    if n == 1 { format!("1 {one}") } else { format!("{n} {many}") }
}

fn missing_reason(s: &Snapshot, capability: Capability) -> Option<&culprit_core::MissingCapability> {
    s.server.missing.iter().find(|m| m.capability == capability)
}

fn setting<'a>(s: &'a Snapshot, name: &str) -> Option<&'a str> {
    s.settings.iter().find(|x| x.name.eq_ignore_ascii_case(name)).map(|x| x.value.as_str())
}

fn is_off(value: Option<&str>) -> bool {
    value.is_some_and(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "off" | "0" | "false" | "no"))
}

/// A reason as the end of a sentence: first letter lowered, one full stop.
fn sentence(text: &str) -> String {
    let text = text.trim().trim_end_matches('.');
    let mut chars = text.chars();
    let lowered = match (chars.next(), chars.clone().next()) {
        // Keep acronyms and names ("Query Store is OFF", "SQL Server …") as they are.
        (Some(c), Some(n)) if c.is_uppercase() && n.is_lowercase() && !text.starts_with("Query ") => {
            c.to_lowercase().chain(chars).collect()
        }
        _ => text.to_string(),
    };
    format!("{lowered}.")
}

/// A signal's value as a person reads it, with its unit.
pub fn signal_value(s: &Signal) -> String {
    match s.unit {
        SignalUnit::Percent => format::percent_value(s.value),
        SignalUnit::PerSecond => format!("{}/s", format::count(s.value)),
        SignalUnit::PerHour => format!("{}/h", format::count(s.value)),
        SignalUnit::Count => format::count(s.value),
        SignalUnit::Bytes => format::bytes(s.value),
        SignalUnit::Milliseconds => format::duration(s.value),
        SignalUnit::Seconds => format::duration(s.value * 1000.0),
    }
}

fn signal_status(s: SignalStatus) -> TileStatus {
    match s {
        SignalStatus::Bad => TileStatus::Bad,
        SignalStatus::Watch => TileStatus::Watch,
        SignalStatus::Good => TileStatus::Good,
        SignalStatus::Info => TileStatus::Info,
    }
}

/// The first SQL statement in a fix sentence ("Run CREATE EXTENSION pg_stat_statements; in this database…"): an
/// upper-case verb up to its semicolon. "Set performance_schema = ON in the configuration" is advice, not SQL.
fn fix_statement(fix: &str) -> Option<String> {
    let start = ["CREATE ", "ALTER ", "GRANT ", "SET ", "UPDATE ", "CALL ", "EXEC ", "INSTALL "]
        .iter()
        .filter_map(|verb| {
            let mut from = 0;
            while let Some(at) = fix[from..].find(verb) {
                let i = from + at;
                if i == 0 || !fix.as_bytes()[i - 1].is_ascii_alphanumeric() {
                    return Some(i);
                }
                from = i + verb.len();
            }
            None
        })
        .min()?;
    let end = start + fix[start..].find(';')? + 1;
    Some(fix[start..end].trim().to_string())
}
