//! Pressure signals and the "right now" wait sample: global status counters, InnoDB metrics and the oldest open
//! transaction, plus two readings of performance_schema's wait totals a few seconds apart.

use std::collections::{BTreeMap, HashMap};
use std::time::{Duration, Instant};

use culprit_core::{Engine, OpenTransaction, Pressure, Result, Signal, SignalScope, SignalUnit, WaitSample, WaitStat};
use mysql_async::{Conn, Value};

use crate::connect::TAG;
use crate::rows::{query, query_one, query_with};

const PICOS_PER_MS: f64 = 1e9;

/// The shared category for a performance_schema wait event.
pub fn category(event: &str) -> &'static str {
    let e = event.to_ascii_lowercase();
    if e.starts_with("wait/io/file/innodb/innodb_log_file") || e.starts_with("wait/io/file/innodb/innodb_redo") {
        return "Log";
    }
    if e.starts_with("wait/io/file") {
        return "IO";
    }
    if e.starts_with("wait/io/table") {
        return "Table IO";
    }
    if e.starts_with("wait/io/socket") {
        return "Network";
    }
    if e.starts_with("wait/lock") {
        return "Lock";
    }
    if e.starts_with("wait/synch") {
        return "Latch";
    }
    "Other"
}

/// Event name → (wait ms, count) from performance_schema's server-wide wait totals.
/// Wait totals since startup. `wait/io/table/sql/handler` is left out: it times row reads inside the storage engine,
/// the statements' own work rather than waiting, and would otherwise dwarf every real wait.
async fn wait_totals(conn: &mut Conn) -> Result<HashMap<String, (f64, f64)>> {
    Ok(query(
        conn,
        "SELECT EVENT_NAME AS event, COUNT_STAR AS tasks, SUM_TIMER_WAIT AS wait_ps
           FROM performance_schema.events_waits_summary_global_by_event_name
          WHERE SUM_TIMER_WAIT > 0 AND EVENT_NAME NOT IN ('idle', 'wait/io/table/sql/handler')",
    )
    .await?
    .iter()
    .map(|r| (r.string("event"), (r.number("wait_ps").unwrap_or(0.0) / PICOS_PER_MS, r.number("tasks").unwrap_or(0.0))))
    .collect())
}

/// Server-wide wait totals since startup, folded into categories, each with its biggest event.
pub async fn waits(conn: &mut Conn) -> Result<Vec<WaitStat>> {
    Ok(fold(wait_totals(conn).await?.into_iter().map(|(e, (ms, n))| (e, ms, n))))
}

fn fold(items: impl IntoIterator<Item = (String, f64, f64)>) -> Vec<WaitStat> {
    let mut by_category: BTreeMap<&'static str, (f64, f64, String, f64)> = BTreeMap::new();
    for (event, ms, tasks) in items {
        if ms <= 0.0 {
            continue;
        }
        let entry = by_category.entry(category(&event)).or_default();
        entry.0 += ms;
        entry.1 += tasks;
        if ms > entry.3 {
            entry.2 = event;
            entry.3 = ms;
        }
    }
    let mut out: Vec<WaitStat> = by_category
        .into_iter()
        .map(|(category, (ms, tasks, top, _))| WaitStat {
            category: category.into(),
            wait_type: Some(top),
            wait_ms: ms,
            tasks: Some(tasks),
        })
        .collect();
    out.sort_by(|a, b| b.wait_ms.total_cmp(&a.wait_ms));
    out
}

async fn status(conn: &mut Conn) -> Result<HashMap<String, f64>> {
    crate::vars::status(
        conn,
        &[
            "Uptime",
            "Innodb_buffer_pool_reads",
            "Innodb_buffer_pool_read_requests",
            "Created_tmp_disk_tables",
            "Created_tmp_tables",
            "Questions",
            "Threads_running",
            "Threads_connected",
            "Max_used_connections",
            "Innodb_row_lock_waits",
            "Innodb_row_lock_time_avg",
        ],
    )
    .await
}

/// The oldest open transaction, with the connecting program's name when performance_schema has it.
fn oldest_sql(with_program: bool) -> String {
    let (column, join) = match with_program {
        true => (
            "a.ATTR_VALUE AS program",
            "LEFT JOIN performance_schema.session_connect_attrs a
                    ON a.PROCESSLIST_ID = t.trx_mysql_thread_id AND a.ATTR_NAME = 'program_name'",
        ),
        false => ("NULL AS program", ""),
    };
    format!(
        "SELECT CAST(t.trx_mysql_thread_id AS CHAR) AS session, TIMESTAMPDIFF(SECOND, t.trx_started, NOW()) AS seconds,
                p.COMMAND AS command, {column}
           FROM information_schema.INNODB_TRX t
           LEFT JOIN information_schema.PROCESSLIST p ON p.ID = t.trx_mysql_thread_id
           {join}
          WHERE t.trx_mysql_thread_id <> CONNECTION_ID()
          ORDER BY t.trx_started LIMIT 1"
    )
}

/// Culprit's own connections: those named `program_name = culprit` where performance_schema shows connection
/// attributes, otherwise this session and any running a tagged statement.
async fn own_sessions(conn: &mut Conn) -> f64 {
    let named = "SELECT COUNT(DISTINCT PROCESSLIST_ID) AS n FROM performance_schema.session_connect_attrs
                  WHERE ATTR_NAME = 'program_name' AND ATTR_VALUE = 'culprit'";
    if let Ok(rows) = query(conn, named).await
        && let Some(n) = rows.first().and_then(|r| r.number("n")).filter(|n| *n >= 1.0)
    {
        return n;
    }
    let tagged = "SELECT COUNT(*) AS n FROM information_schema.PROCESSLIST WHERE ID = CONNECTION_ID() OR INFO LIKE ?";
    query_with(conn, tagged, vec![Value::from(format!("%{TAG}%"))])
        .await
        .ok()
        .and_then(|rows| rows.first().and_then(|r| r.number("n")))
        .unwrap_or(1.0)
}

/// Statements running right now, counted the way Live counts them: other sessions' work, never Culprit's own (a scan
/// runs several collectors at once, each on its own session, so `Threads_running - 1` still counts some of them).
async fn running_statements(conn: &mut Conn) -> Option<f64> {
    let sql = "SELECT COUNT(*) AS n FROM information_schema.PROCESSLIST
                WHERE ID <> CONNECTION_ID() AND COMMAND NOT IN ('Sleep', 'Daemon', 'Binlog Dump', 'Binlog Dump GTID')
                  AND USER <> 'system user' AND (INFO IS NULL OR INFO NOT LIKE ?)";
    let rows = query_with(conn, sql, vec![Value::from(format!("%{TAG}%"))]).await.ok()?;
    rows.first().and_then(|r| r.number("n"))
}

pub async fn collect(conn: &mut Conn) -> (Pressure, Vec<String>) {
    let running = running_statements(conn).await;
    let mut p = Pressure::default();
    let mut notes = Vec::new();
    match status(conn).await {
        Ok(s) => {
            let get = |k: &str| s.get(k).copied();
            let uptime_s = get("Uptime").filter(|u| *u > 0.0);
            p.uptime_hours = uptime_s.map(|u| u / 3600.0);
            if let (Some(reads), Some(requests)) =
                (get("Innodb_buffer_pool_reads"), get("Innodb_buffer_pool_read_requests").filter(|r| *r > 0.0))
            {
                p.signals.push(
                    Signal::new(
                        "buffer-pool-hit",
                        "Reads served from memory",
                        (1.0 - reads / requests) * 100.0,
                        SignalUnit::Percent,
                        SignalScope::SinceStartup,
                    )
                    .graded("above 99% is healthy for most workloads", 99.0, 95.0, false),
                );
            }
            if let (Some(disk), Some(all)) =
                (get("Created_tmp_disk_tables"), get("Created_tmp_tables").filter(|t| *t > 0.0))
            {
                p.signals.push(
                    Signal::new(
                        "tmp-disk-tables",
                        "Temporary tables on disk",
                        disk / all * 100.0,
                        SignalUnit::Percent,
                        SignalScope::SinceStartup,
                    )
                    .graded("under 10% of temporary tables is healthy", 10.0, 25.0, true),
                );
            }
            if let (Some(q), Some(u)) = (get("Questions"), uptime_s) {
                p.signals.push(Signal::new(
                    "questions",
                    "Statements",
                    q / u,
                    SignalUnit::PerSecond,
                    SignalScope::SinceStartup,
                ));
            }
            // The process list without Culprit's sessions; the counter (minus this session) if it can't be read.
            if let Some(running) = running.or_else(|| get("Threads_running").map(|r| (r - 1.0).max(0.0))) {
                p.signals.push(Signal::new(
                    "threads-running",
                    "Statements running now",
                    running,
                    SignalUnit::Count,
                    SignalScope::Now,
                ));
            }
            if let (Some(waits), Some(u)) = (get("Innodb_row_lock_waits"), uptime_s) {
                // Under an hour of uptime, a rate per hour would be an extrapolation: report the plain count.
                let hours = u / 3600.0;
                let (label, value, unit) = if hours >= 1.0 {
                    ("Row-lock waits", waits / hours, SignalUnit::PerHour)
                } else {
                    ("Row-lock waits since start", waits, SignalUnit::Count)
                };
                p.signals.push(Signal::new("row-lock-waits", label, value, unit, SignalScope::SinceStartup));
            }
            if let Some(avg) = get("Innodb_row_lock_time_avg") {
                p.signals.push(
                    Signal::new(
                        "row-lock-avg",
                        "Average row-lock wait",
                        avg,
                        SignalUnit::Milliseconds,
                        SignalScope::SinceStartup,
                    )
                    .graded("under 50 ms is healthy", 50.0, 500.0, true),
                );
            }
            if let Ok(Some(r)) = query_one(conn, "SELECT @@max_connections AS max").await
                && let Some(max) = r.number("max").filter(|m| *m > 0.0)
            {
                // The application's connections: Culprit's own (this scan opens several) don't count.
                let own = own_sessions(conn).await;
                if let Some(used) = get("Threads_connected").map(|u| (u - own).max(0.0)) {
                    p.signals.push(
                        Signal::new(
                            "connections",
                            "Connections in use",
                            used / max * 100.0,
                            SignalUnit::Percent,
                            SignalScope::Now,
                        )
                        .graded("under 80% of max_connections is healthy", 80.0, 90.0, true),
                    );
                }
                if let Some(peak) = get("Max_used_connections") {
                    p.signals.push(
                        Signal::new(
                            "max-used-connections",
                            "Busiest moment",
                            peak / max * 100.0,
                            SignalUnit::Percent,
                            SignalScope::SinceStartup,
                        )
                        .graded("under 90% of max_connections is healthy", 90.0, 100.0, true),
                    );
                }
            }
        }
        Err(e) => notes.push(format!("Global status: {}", e.message)),
    }
    let history = query_one(
        conn,
        "SELECT `COUNT` AS n FROM information_schema.INNODB_METRICS WHERE NAME = 'trx_rseg_history_len'",
    )
    .await
    .ok()
    .flatten()
    .and_then(|r| r.number("n"));
    if let Some(h) = history {
        p.signals.push(Signal::new("history-length", "Undo history", h, SignalUnit::Count, SignalScope::Now).graded(
            "under 100K old row versions is healthy",
            100_000.0,
            1_000_000.0,
            true,
        ));
    }
    // The program name comes from performance_schema's connection attributes, which a server without it lacks.
    let oldest = match query_one(conn, &oldest_sql(true)).await {
        Ok(found) => Ok(found),
        Err(_) => query_one(conn, &oldest_sql(false)).await,
    };
    match oldest {
        Ok(Some(r)) => {
            let seconds = r.number("seconds").unwrap_or(0.0);
            p.oldest_transaction = Some(OpenTransaction {
                session: r.string("session"),
                seconds,
                program: r.text("program").filter(|s| !s.is_empty()),
                state: Some(if r.text("command").as_deref() == Some("Sleep") {
                    "idle in transaction".into()
                } else {
                    "running".into()
                }),
                held_bytes: None,
                held_versions: history,
            });
            p.signals.push(
                Signal::new(
                    "oldest-transaction",
                    "Oldest open transaction",
                    seconds,
                    SignalUnit::Seconds,
                    SignalScope::Now,
                )
                .graded("seconds or minutes, not hours", 600.0, 1800.0, true),
            );
        }
        Ok(None) => {}
        Err(e) => notes.push(format!("Open transactions: {}", e.message)),
    }
    (p, notes)
}

/// What each busy session is doing, from the process list: the state text and whether InnoDB has it waiting for a lock.
const SESSIONS_SQL: &str = "SELECT p.STATE AS state, t.trx_state AS trx_state
   FROM information_schema.PROCESSLIST p
   LEFT JOIN information_schema.INNODB_TRX t ON t.trx_mysql_thread_id = p.ID
  WHERE p.ID <> CONNECTION_ID() AND p.COMMAND NOT IN ('Sleep', 'Daemon', 'Binlog Dump', 'Binlog Dump GTID')
    AND p.USER <> 'system user' AND (p.INFO IS NULL OR p.INFO NOT LIKE '%/* culprit */%')";

/// The shared category for a process-list state (`Waiting for table metadata lock`, `Sending to client`, …).
pub fn state_category(state: &str, lock_wait: bool) -> &'static str {
    let s = state.to_ascii_lowercase();
    if lock_wait || s.contains("lock") {
        "Lock"
    } else if s.contains("commit") || s.contains("binlog") || s.contains("semi-sync") {
        "Log"
    } else if s.contains(" net") || s.contains("to client") {
        "Network"
    } else if s.contains("on disk") || s.contains("ondisk") || s.contains("opening tables") || s.contains("flushing") {
        "IO"
    } else if s.contains("sleep") {
        "Other"
    } else {
        "Running"
    }
}

/// Readings of performance_schema's wait totals `seconds` apart, with the process list sampled once a second in
/// between. The instruments time waits precisely but most are off by default (and all of them are without
/// performance_schema, MariaDB's default); the samples show what busy sessions were doing either way.
pub async fn sample(conn: &mut Conn, seconds: u32) -> Result<WaitSample> {
    let started = Instant::now();
    let taken_at = query_one(conn, "SELECT DATE_FORMAT(UTC_TIMESTAMP(), '%Y-%m-%dT%H:%i:%sZ') AS now")
        .await?
        .and_then(|r| r.text("now"))
        .unwrap_or_default();
    let instruments_on =
        query_one(conn, "SELECT @@performance_schema AS ps").await.ok().flatten().is_some_and(|r| r.flag("ps"));
    let first = if instruments_on { wait_totals(conn).await.ok() } else { None };
    let status_before = status(conn).await.unwrap_or_default();
    let mut observed: BTreeMap<(&'static str, String), f64> = BTreeMap::new();
    let ticks = seconds.max(1) + 1;
    for tick in 0..ticks {
        for r in query(conn, SESSIONS_SQL).await.unwrap_or_default() {
            let state = r.text("state").filter(|s| !s.is_empty()).unwrap_or_else(|| "executing".into());
            let lock_wait = r.text("trx_state").is_some_and(|t| t.eq_ignore_ascii_case("LOCK WAIT"));
            let label = if lock_wait && !state.to_ascii_lowercase().contains("lock") {
                format!("row lock wait ({state})")
            } else {
                state.clone()
            };
            *observed.entry((state_category(&state, lock_wait), label)).or_default() += 1.0;
        }
        if tick + 1 < ticks {
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }
    let second = if instruments_on { wait_totals(conn).await.ok() } else { None };
    let status_after = status(conn).await.unwrap_or_default();
    let elapsed = started.elapsed().as_secs_f64().max(1.0);
    let instrumented = match (&first, &second) {
        (Some(first), Some(second)) => fold(second.iter().filter_map(|(event, (ms, n))| {
            let (ms0, n0) = first.get(event).copied().unwrap_or((0.0, 0.0));
            let delta = ms - ms0;
            (delta > 0.0).then(|| (event.clone(), delta, (n - n0).max(0.0)))
        })),
        _ => Vec::new(),
    };
    let samples = f64::from(ticks);
    let busy: f64 = observed.values().sum();
    let mut notes = Vec::new();
    let (categories, method) = if instrumented.iter().map(|c| c.wait_ms).sum::<f64>() > 0.0 {
        (
            instrumented,
            format!(
                "Difference of two readings of performance_schema.events_waits_summary_global_by_event_name {seconds} s apart"
            ),
        )
    } else {
        // Each observation stands for one second of one session's time.
        let mut by_category: BTreeMap<&'static str, (f64, String, f64)> = BTreeMap::new();
        for ((category, label), n) in &observed {
            let entry = by_category.entry(category).or_default();
            entry.0 += n;
            if *n > entry.2 {
                entry.1 = label.clone();
                entry.2 = *n;
            }
        }
        let mut sampled: Vec<WaitStat> = by_category
            .into_iter()
            .map(|(category, (n, top, _))| WaitStat {
                category: category.into(),
                wait_type: Some(top),
                wait_ms: n * 1000.0,
                tasks: Some(n),
            })
            .collect();
        sampled.sort_by(|a, b| b.wait_ms.total_cmp(&a.wait_ms));
        notes.push(match first {
            Some(_) => "No enabled performance_schema wait instrument recorded anything, so these come from sampling what each busy session was doing once a second.".to_string(),
            None => "performance_schema is off, so the server times no waits; these come from sampling what each busy session was doing once a second.".to_string(),
        });
        (
            sampled,
            format!(
                "The process list sampled once a second for {seconds} s: each observation is one second of one session's time"
            ),
        )
    };
    let total_ms: f64 = categories.iter().map(|c| c.wait_ms).sum();
    let rate = |k: &str| Some(((status_after.get(k)? - status_before.get(k)?) / elapsed).max(0.0));
    let mut signals = Vec::new();
    if let Some(q) = rate("Questions") {
        signals.push(Signal::new("questions", "Statements", q, SignalUnit::PerSecond, SignalScope::Sampled));
    }
    if let Some(w) = rate("Innodb_row_lock_waits") {
        signals.push(Signal::new(
            "row-lock-waits",
            "Row-lock waits",
            w * 3600.0,
            SignalUnit::PerHour,
            SignalScope::Sampled,
        ));
    }
    if total_ms == 0.0 && notes.is_empty() {
        notes.push("No enabled wait instrument recorded anything and no session was busy. Many instruments are off by default; enable them in performance_schema.setup_instruments to see more.".into());
    }
    Ok(WaitSample {
        engine: Engine::Mysql,
        taken_at,
        seconds: elapsed,
        method,
        total_ms,
        categories,
        signals,
        active_sessions: Some(busy / samples),
        notes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_list_states_fold_into_categories() {
        assert_eq!(state_category("Waiting for table metadata lock", false), "Lock");
        assert_eq!(state_category("Updating", true), "Lock");
        assert_eq!(state_category("Sending to client", false), "Network");
        assert_eq!(state_category("waiting for handler commit", false), "Log");
        assert_eq!(state_category("Copying to tmp table on disk", false), "IO");
        assert_eq!(state_category("User sleep", false), "Other");
        assert_eq!(state_category("Sending data", false), "Running");
    }

    #[test]
    fn events_fold_into_categories_with_their_biggest_event() {
        assert_eq!(category("wait/io/file/innodb/innodb_log_file"), "Log");
        assert_eq!(category("wait/io/file/innodb/innodb_data_file"), "IO");
        assert_eq!(category("wait/io/table/sql/handler"), "Table IO");
        assert_eq!(category("wait/lock/metadata/sql/mdl"), "Lock");
        let folded = fold(vec![
            ("wait/io/file/innodb/innodb_data_file".into(), 30.0, 3.0),
            ("wait/io/file/sql/binlog".into(), 50.0, 1.0),
            ("wait/lock/table/sql/handler".into(), 5.0, 2.0),
        ]);
        assert_eq!(folded[0].category, "IO");
        assert_eq!(folded[0].wait_ms, 80.0);
        assert_eq!(folded[0].wait_type.as_deref(), Some("wait/io/file/sql/binlog"));
    }
}
