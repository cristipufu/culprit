//! Checks on signals beyond time and reads. Per query: runs that fail or time out, planning and JIT time, log volume,
//! lock time and result size. Per server: slow storage, connection churn, replica lag. Per object: row-lock and
//! last-page contention on indexes, constraints the optimizer can't trust, missing histograms, non-HOT updates and
//! table bloat. Pure functions of the snapshot; each says plainly what its engine measures.

use std::collections::BTreeMap;

use culprit_core::waits::category_key;
use culprit_core::{
    Category, CheckInfo, Confidence, ConstraintIssue, ConstraintKind, Engine, FileIo, FileKind, Finding, Fix,
    IndexStat, Metric, QueryRef, QueryStat, Replica, ReplicaRole, Severity, Snapshot, SubjectKind, TableStat, Unit,
    format, signals::plan_row_bytes,
};

use culprit_core::Check;

use crate::support::*;

mod big_result;
mod cluster_health;
mod connection_churn;
mod failing_query;
mod hot_rows;
mod hot_updates;
mod jit_overhead;
mod large_rows;
mod last_page_insert;
mod lock_escalations;
mod lock_time;
mod log_volume;
mod missing_histogram;
mod operational_errors;
mod planning_time;
mod replica_lag;
mod sequence_capacity;
mod slow_storage;
mod sqlite_wal;
mod storage_capacity;
mod table_bloat;
#[cfg(test)]
mod tests;
mod untrusted_constraint;

pub use big_result::*;
pub use connection_churn::*;
pub use failing_query::*;
pub use hot_rows::*;
pub use hot_updates::*;
pub use jit_overhead::*;
pub use large_rows::*;
pub use last_page_insert::*;
pub use lock_escalations::*;
pub use lock_time::*;
pub use log_volume::*;
pub use missing_histogram::*;
pub use planning_time::*;
pub use replica_lag::*;
pub use slow_storage::*;
pub use table_bloat::*;
pub use untrusted_constraint::*;

/// The checks this area registers (`crate::all()` runs every area's), in the order their findings tie.
pub(crate) const CHECKS: &[&dyn Check] = &[
    &storage_capacity::StorageCapacityCheck,
    &sequence_capacity::SequenceCapacityCheck,
    &operational_errors::OperationalErrorsCheck,
    &cluster_health::ClusterHealthCheck,
    &sqlite_wal::SqliteWalCheck,
    &FailingQueryCheck,
    &PlanningTimeCheck,
    &JitOverheadCheck,
    &LogVolumeCheck,
    &LockTimeCheck,
    &BigResultCheck,
    &HotRowsCheck,
    &LockEscalationsCheck,
    &LastPageInsertCheck,
    &UntrustedConstraintCheck,
    &MissingHistogramCheck,
    &HotUpdatesCheck,
    &TableBloatCheck,
    &LargeRowsCheck,
    &SlowStorageCheck,
    &ConnectionChurnCheck,
    &ReplicaLagCheck,
];

fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    chars.next().map(|c| c.to_uppercase().collect::<String>() + chars.as_str()).unwrap_or_default()
}

/// The SQL that starts the Extended Events session `name`, written the same way by every fix that asks for one: one
/// session catches what every finding of its kind needs, so the fix creates it only where it isn't defined yet and
/// starts it only where it isn't running, and a second finding's fix runs as well as the first. `definition` is what
/// follows `CREATE EVENT SESSION <name> ON <scope>` (its events and target), one clause a line. Per database on Azure
/// SQL Database, where a session can't see other databases; per server elsewhere. [`event_session_drop`] is the line
/// that stops it.
pub(crate) fn event_session_sql(name: &str, azure_sql_database: bool, definition: &str) -> String {
    let (scope, defined, running) = if azure_sql_database {
        ("DATABASE", "sys.database_event_sessions", "sys.dm_xe_database_sessions")
    } else {
        ("SERVER", "sys.server_event_sessions", "sys.dm_xe_sessions")
    };
    let body: String = definition.lines().map(|l| format!("\n    {l}")).collect();
    format!(
        "IF NOT EXISTS (SELECT 1 FROM {defined} WHERE name = N'{name}')\n    \
         CREATE EVENT SESSION {name} ON {scope}{body};\n\
         IF NOT EXISTS (SELECT 1 FROM {running} WHERE name = N'{name}')\n    \
         ALTER EVENT SESSION {name} ON {scope} STATE = START;"
    )
}

/// The comment that ends a fix's SQL starting [`event_session_sql`]'s session: how to stop and remove it once it has
/// caught what was needed. A comment, so running the fix's SQL never drops what it just started.
pub(crate) fn event_session_drop(name: &str, azure_sql_database: bool) -> String {
    let scope = if azure_sql_database { "DATABASE" } else { "SERVER" };
    format!("-- Once it has caught what you need, stop and remove it: DROP EVENT SESSION {name} ON {scope};")
}

fn engine_name(snapshot: &Snapshot) -> &'static str {
    match snapshot.server.engine {
        Engine::Mysql if snapshot.server.product.to_ascii_lowercase().contains("mariadb") => "MariaDB",
        Engine::Mysql => "MySQL",
        Engine::Postgres => "PostgreSQL",
        Engine::Mssql => "SQL Server",
        Engine::Sqlite => "SQLite",
    }
}

fn is_mariadb(snapshot: &Snapshot) -> bool {
    snapshot.server.is_mariadb()
}

/// "in the last 7 days" for windowed sources, "since its statistics began 1277 days ago" otherwise: spans in the words
/// every window is said in ([`culprit_core::span_words`]), never "1277.0 days".
fn window_phrase(snapshot: &Snapshot) -> String {
    match snapshot.workload.as_ref() {
        Some(w) if w.log.is_some() => format!("over {}", w.window_phrase()),
        Some(w) if w.is_windowed() => format!("in the last {}", culprit_core::span_words(w.measurement_hours())),
        Some(w) if w.measurement_hours() > 0.0 => {
            format!("since its statistics began {} ago", culprit_core::span_words(w.measurement_hours()))
        }
        _ => "since its statistics began".into(),
    }
}

/// When statement `q`'s counts were made: "on Oct 11, 13:56–14:19 UTC" for runs that came in bursts
/// ([`culprit_core::Cadence`]), otherwise the window ([`window_phrase`]).
fn ran_phrase(snapshot: &Snapshot, q: &QueryStat) -> String {
    snapshot
        .workload
        .as_ref()
        .map(|w| w.cadence(q))
        .filter(|c| c.is_burst())
        .and_then(|c| c.when())
        .unwrap_or_else(|| window_phrase(snapshot))
}

/// "since SQL Server started 3 days ago", for server counters.
fn since_start(snapshot: &Snapshot) -> String {
    let name = engine_name(snapshot);
    match snapshot.pressure.as_ref().and_then(|p| p.uptime_hours) {
        Some(h) => {
            format!("since {name} started or its statistics were reset, {} ago", format::duration(h * 3_600_000.0))
        }
        None => format!("since {name} started"),
    }
}

/// The statement's verb, a `WITH` list resolved to the statement it leads to.
fn verb(text: &str) -> String {
    main_statement(text).split(|c: char| !c.is_ascii_alphabetic()).next().unwrap_or("").to_ascii_lowercase()
}

fn is_read(q: &QueryStat) -> bool {
    matches!(verb(&q.text).as_str(), "select" | "with" | "values" | "table")
}

/// A read whose rows go to the client. `SELECT … INTO #t` (or a new table), `SELECT … INTO OUTFILE` and `SELECT @v = …`
/// / `SELECT @v := …` start with SELECT but keep their rows on the server.
fn returns_rows(q: &QueryStat) -> bool {
    if !is_read(q) {
        return false;
    }
    let lower = q.text.to_ascii_lowercase().replace(['\n', '\r', '\t'], " ");
    let words: Vec<&str> = lower.split_whitespace().collect();
    // `INTO` before the first FROM, outside parentheses and strings, is SELECT INTO; MySQL also writes it last.
    let mut depth = 0i32;
    for w in &words {
        depth += w.matches('(').count() as i32 - w.matches(')').count() as i32;
        if depth > 0 {
            continue;
        }
        match *w {
            "from" => break,
            "into" => return false,
            _ => {}
        }
    }
    if lower.contains(" into outfile") || lower.contains(" into dumpfile") || lower.contains(" into @") {
        return false;
    }
    // The first item of the select list assigns a variable: `SELECT @total = SUM(x)` (SQL Server), `@v := …` (MySQL).
    let mut list = words.iter().skip(1).skip_while(|w| matches!(**w, "distinct" | "all" | "top")).peekable();
    if let Some(first) = list.next() {
        let first = if first.starts_with('(') || first.chars().all(|c| c.is_ascii_digit()) {
            list.next().copied().unwrap_or(first)
        } else {
            first
        };
        if first.starts_with('@') && !first.starts_with("@@") {
            let rest = &lower[lower.find(first).unwrap_or(0) + first.len()..];
            let assigns = first.contains(":=")
                || first.ends_with('=')
                || rest.trim_start().starts_with('=')
                || rest.trim_start().starts_with(":=");
            if assigns {
                return false;
            }
        }
    }
    true
}

/// A read that copies a whole table out: mysqldump's `SELECT SQL_NO_CACHE * FROM t`, or an export or ETL job reading
/// without a filter a few times a day. Its rows are the point; paging them only spreads the same rows over more runs.
fn looks_like_export(snapshot: &Snapshot, q: &QueryStat) -> bool {
    let lower = q.text.to_ascii_lowercase();
    if lower.contains("sql_no_cache") {
        return true;
    }
    let unfiltered = !lower.contains(" where ") && !lower.contains(" limit ") && !lower.contains(" top ");
    // A few a day at the pace it ran: a burst's own total, never spread over the window.
    let few_a_day = snapshot
        .workload
        .as_ref()
        .filter(|w| w.measurement_hours() >= culprit_core::PER_DAY_MIN_HOURS)
        .and_then(|w| w.rate_per_day_of(q, q.executions))
        .is_some_and(|d| d <= 4.0);
    unfiltered && few_a_day
}

fn is_write(q: &QueryStat) -> bool {
    matches!(verb(&q.text).as_str(), "insert" | "update" | "delete" | "merge" | "copy" | "replace" | "upsert")
}

pub(crate) fn lock_ms(q: &QueryStat) -> f64 {
    q.waits.iter().filter(|w| category_key(&w.category) == "lock").map(|w| w.wait_ms).sum()
}

/// A total wait to the second under an hour: "1 min 29 s", where `format::duration` would round 88,960 ms down to
/// "1 min" next to an average that says otherwise, and 152 s up to "3 min".
pub(crate) fn total_wait(ms: f64) -> String {
    let seconds = (ms / 1_000.0).round();
    if !(60.0..3_600.0).contains(&seconds) {
        return format::duration(ms);
    }
    let (minutes, rest) = ((seconds / 60.0).floor(), seconds % 60.0);
    if rest == 0.0 { format!("{minutes:.0} min") } else { format!("{minutes:.0} min {rest:.0} s") }
}

fn severity_score(f: &mut Finding) {
    f.impact.score = f.impact.score.max(base_score(f.severity));
}

/// Attach the query's share of database time, keep a score at least as high as the severity's.
fn with_query(snapshot: &Snapshot, f: &mut Finding, q: &QueryStat) {
    if let Some(impact) = query_impact(snapshot, &[q]) {
        apply_query_impact(f, &impact);
    }
    f.queries = query_refs(&[q]);
    f.last_seen = q.last_seen.clone();
}

/// "40%", or "almost all" when rounding would claim 100%.
fn share_words(share: f64) -> String {
    if share >= 0.97 { "almost all".into() } else { format::percent(share) }
}

fn per_run(total: f64, runs: f64) -> f64 {
    if runs > 0.0 { total / runs } else { 0.0 }
}

/// Whether the workload window's start is only a lower bound: pg_stat_statements before PostgreSQL 14 keeps no reset
/// time, so the window is dated from the server's start, but the counts survive a clean restart and may be far older.
fn window_start_unknown(snapshot: &Snapshot) -> bool {
    snapshot.server.engine == Engine::Postgres
        && !culprit_core::version_at_least(&snapshot.server.version, [14, 0, 0])
        && snapshot.workload.as_ref().is_some_and(|w| !w.is_windowed())
}

/// [`at_least_per_day_of`] for statement `q`'s `total` (at the rate it ran while it ran: a burst on its own total),
/// false where the window's start is unknown (see [`window_start_unknown`]).
fn window_at_least(snapshot: &Snapshot, q: &QueryStat, total: f64, per_day: f64) -> bool {
    !window_start_unknown(snapshot) && at_least_per_day_of(snapshot, q, total, per_day)
}

/// [`window_at_least`] for the runs of several statements together, each at the pace it ran
/// ([`culprit_core::Workload::rate_per_day_of`]): a burst on its own total.
fn runs_at_least(snapshot: &Snapshot, queries: &[&QueryStat], per_day: f64) -> bool {
    let Some(w) = snapshot.workload.as_ref() else { return false };
    if window_start_unknown(snapshot) || w.window_unknown() {
        return false;
    }
    let runs: f64 = queries.iter().map(|q| q.executions).sum();
    let rate: Option<f64> = queries.iter().map(|q| w.rate_per_day_of(q, q.executions)).sum();
    rate.is_some_and(|r| r >= per_day)
        || (w.measurement_hours() > 0.0 && w.measurement_hours() <= 24.0 && runs >= per_day)
}

/// [`rate_per_day_of`] statement `q`'s `value` for a threshold (a burst's own total, never extrapolated to a day),
/// `None` where the window's start is unknown (see [`window_start_unknown`]): a window that may be days too short
/// would make the rate days too high.
fn window_rate_per_day(snapshot: &Snapshot, q: &QueryStat, value: f64) -> Option<f64> {
    if window_start_unknown(snapshot) {
        return None;
    }
    rate_per_day_of(snapshot, &[q], value)
}

/// The grade of a count kept since the server started (or its statistics were reset), the one way every check here
/// grades one: such a count keeps no time for each event, so a burst months ago reads the same as one this morning.
/// It escalates only on a daily rate measured over an hour or more of counters ([`counter_rate_per_day`]) reaching
/// `high` or `medium`; without one it is history, Low.
fn since_start_severity(rate_per_day: Option<f64>, high: f64, medium: f64) -> Severity {
    match rate_per_day {
        Some(r) if r >= high => Severity::High,
        Some(r) if r >= medium => Severity::Medium,
        _ => Severity::Low,
    }
}

/// The caveat on a since-start count that has no measured rate: what was counted, over how long, and why it isn't
/// graded higher.
fn history_caveat(snapshot: &Snapshot, what: &str) -> String {
    let engine = engine_name(snapshot);
    match counter_days(snapshot) {
        // Counting for less than an hour: too little for a rate, whatever the total.
        Some(_) => format!(
            "{engine} has counted these {what} only {}: too little time to make a rate, so the total doesn't raise it. A scan after an hour or more of counting can.",
            since_phrase(snapshot).trim_start_matches("in ")
        ),
        None => format!(
            "{engine} counts {what} from when it started, with no time for each, and this scan doesn't know when that was: they may come from one burst long ago. Culprit raises it only on a rate measured over an hour or more of counting."
        ),
    }
}

const GB: f64 = 1024.0 * 1024.0 * 1024.0;
const MB: f64 = 1024.0 * 1024.0;

fn index_label(i: &IndexStat) -> String {
    format!("{}.{}", i.schema, i.table)
}

/// The metric `lock-escalations` (and `hot-rows`) report escalations under; the digest reads it back.
pub(crate) const ESCALATIONS_METRIC: &str = "Escalations since SQL Server started";

/// The share of a table's row versions that are dead (not yet vacuumed), when the engine counts them.
pub(crate) fn dead_share(t: &TableStat) -> Option<f64> {
    let dead = t.dead_rows.filter(|d| *d > 0.0)?;
    Some(dead / (dead + t.rows.unwrap_or(0.0).max(0.0)))
}
