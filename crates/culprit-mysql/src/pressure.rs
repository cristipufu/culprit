//! Pressure signals and the "right now" wait sample: global status counters, InnoDB metrics and the oldest open
//! transaction, plus two readings of performance_schema's wait totals a few seconds apart.

use std::collections::{BTreeMap, HashMap};
use std::time::{Duration, Instant};

use culprit_core::{Engine, OpenTransaction, Pressure, Result, Signal, SignalScope, SignalUnit, WaitSample, WaitStat};
use mysql_async::Conn;

use crate::rows::{query, query_one};

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
async fn wait_totals(conn: &mut Conn) -> Result<HashMap<String, (f64, f64)>> {
    Ok(query(
        conn,
        "SELECT EVENT_NAME AS event, COUNT_STAR AS tasks, SUM_TIMER_WAIT AS wait_ps
           FROM performance_schema.events_waits_summary_global_by_event_name
          WHERE SUM_TIMER_WAIT > 0 AND EVENT_NAME <> 'idle'",
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
    Ok(query(
        conn,
        "SELECT VARIABLE_NAME AS name, VARIABLE_VALUE AS value FROM performance_schema.global_status
          WHERE VARIABLE_NAME IN ('Uptime', 'Innodb_buffer_pool_reads', 'Innodb_buffer_pool_read_requests',
            'Created_tmp_disk_tables', 'Created_tmp_tables', 'Questions', 'Threads_running', 'Threads_connected',
            'Max_used_connections', 'Innodb_row_lock_waits', 'Innodb_row_lock_time_avg')",
    )
    .await?
    .iter()
    .filter_map(|r| Some((r.string("name"), r.text("value")?.parse::<f64>().ok()?)))
    .collect())
}

pub async fn collect(conn: &mut Conn) -> (Pressure, Vec<String>) {
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
            if let Some(running) = get("Threads_running") {
                // Culprit's own session is one of them.
                p.signals.push(Signal::new(
                    "threads-running",
                    "Statements running now",
                    (running - 1.0).max(0.0),
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
                if let Some(used) = get("Threads_connected") {
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
    match query_one(
        conn,
        "SELECT CAST(t.trx_mysql_thread_id AS CHAR) AS session, TIMESTAMPDIFF(SECOND, t.trx_started, NOW()) AS seconds,
                p.COMMAND AS command, a.ATTR_VALUE AS program
           FROM information_schema.INNODB_TRX t
           LEFT JOIN information_schema.PROCESSLIST p ON p.ID = t.trx_mysql_thread_id
           LEFT JOIN performance_schema.session_connect_attrs a
                  ON a.PROCESSLIST_ID = t.trx_mysql_thread_id AND a.ATTR_NAME = 'program_name'
          WHERE t.trx_mysql_thread_id <> CONNECTION_ID()
          ORDER BY t.trx_started LIMIT 1",
    )
    .await
    {
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

/// Two readings of performance_schema's wait totals `seconds` apart.
pub async fn sample(conn: &mut Conn, seconds: u32) -> Result<WaitSample> {
    let started = Instant::now();
    let taken_at = query_one(conn, "SELECT DATE_FORMAT(UTC_TIMESTAMP(), '%Y-%m-%dT%H:%i:%sZ') AS now")
        .await?
        .and_then(|r| r.text("now"))
        .unwrap_or_default();
    let first = wait_totals(conn).await?;
    let status_before = status(conn).await.unwrap_or_default();
    tokio::time::sleep(Duration::from_secs(u64::from(seconds))).await;
    let second = wait_totals(conn).await?;
    let status_after = status(conn).await.unwrap_or_default();
    let elapsed = started.elapsed().as_secs_f64().max(1.0);
    let deltas = second.iter().filter_map(|(event, (ms, n))| {
        let (ms0, n0) = first.get(event).copied().unwrap_or((0.0, 0.0));
        let delta = ms - ms0;
        (delta > 0.0).then(|| (event.clone(), delta, (n - n0).max(0.0)))
    });
    let categories = fold(deltas);
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
    let running = match (status_before.get("Threads_running"), status_after.get("Threads_running")) {
        (Some(a), Some(b)) => Some(((a + b) / 2.0 - 1.0).max(0.0)),
        _ => None,
    };
    let mut notes = Vec::new();
    if total_ms == 0.0 {
        notes.push("No enabled wait instrument recorded anything. Many are off by default; enable them in performance_schema.setup_instruments to see more.".into());
    }
    Ok(WaitSample {
        engine: Engine::Mysql,
        taken_at,
        seconds: elapsed,
        method: format!(
            "Difference of two readings of performance_schema.events_waits_summary_global_by_event_name {seconds} s apart"
        ),
        total_ms,
        categories,
        signals,
        active_sessions: running,
        notes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

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
