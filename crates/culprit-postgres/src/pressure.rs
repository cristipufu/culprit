//! Pressure signals, waits from statement timing, and the "right now" sample. PostgreSQL keeps no wait history, so
//! history comes from per-statement I/O timing and WAL timing, and "right now" samples sessions every second.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use culprit_core::{Engine, OpenTransaction, Pressure, Result, Signal, SignalScope, SignalUnit, WaitSample, WaitStat};
use tokio_postgres::Client;

use crate::collect::Probe;
use crate::rows::{query, query_one};

/// pg_stat_statements I/O time columns per version (milliseconds; zero unless track_io_timing is on).
pub fn statements_io_column(version_num: u32) -> &'static str {
    if version_num >= 170_000 {
        "(shared_blk_read_time + shared_blk_write_time + local_blk_read_time + local_blk_write_time + temp_blk_read_time + temp_blk_write_time)"
    } else if version_num >= 150_000 {
        "(blk_read_time + blk_write_time + temp_blk_read_time + temp_blk_write_time)"
    } else {
        "(blk_read_time + blk_write_time)"
    }
}

/// Time spent writing and syncing WAL, in ms (zero unless track_wal_io_timing is on).
async fn wal_ms(client: &Client, version_num: u32) -> Option<f64> {
    let sql = if version_num >= 180_000 {
        "SELECT coalesce(sum(write_time + fsync_time), 0) AS ms FROM pg_stat_io WHERE object = 'wal'"
    } else if version_num >= 140_000 {
        "SELECT wal_write_time + wal_sync_time AS ms FROM pg_stat_wal"
    } else {
        return None;
    };
    query_one(client, sql).await.ok().flatten()?.number("ms")
}

/// Database time by category from statement timing: disk I/O and WAL where they are timed, the rest as "Other".
pub async fn history_waits(client: &Client, probe: &Probe, statements_ms: f64, statements_io_ms: f64) -> Vec<WaitStat> {
    let wal = wal_ms(client, probe.version_num).await.unwrap_or(0.0);
    if statements_io_ms <= 0.0 && wal <= 0.0 {
        return Vec::new();
    }
    let other = (statements_ms - statements_io_ms - wal).max(0.0);
    let mut out = Vec::new();
    if statements_io_ms > 0.0 {
        out.push(WaitStat {
            category: "IO".into(),
            wait_type: Some("data file reads and writes".into()),
            wait_ms: statements_io_ms,
            tasks: None,
        });
    }
    if wal > 0.0 {
        out.push(WaitStat {
            category: "Log".into(),
            wait_type: Some("WAL write and sync".into()),
            wait_ms: wal,
            tasks: None,
        });
    }
    if other > 0.0 {
        out.push(WaitStat {
            category: "Other".into(),
            wait_type: Some("CPU, locks and untimed waits".into()),
            wait_ms: other,
            tasks: None,
        });
    }
    out.sort_by(|a, b| b.wait_ms.total_cmp(&a.wait_ms));
    out
}

pub async fn collect(client: &Client, probe: &Probe) -> (Pressure, Vec<String>) {
    let mut p = Pressure::default();
    let mut notes = Vec::new();
    let database = query_one(
        client,
        "SELECT blks_hit, blks_read, temp_bytes, xact_commit, xact_rollback, deadlocks,
                extract(epoch FROM now() - coalesce(stats_reset, pg_postmaster_start_time())) / 3600 AS hours
           FROM pg_stat_database WHERE datname = current_database()",
    )
    .await;
    match database {
        Ok(Some(r)) => {
            let hours = r.number("hours").filter(|h| *h > 0.0);
            p.uptime_hours = hours;
            let (hit, read) = (r.number("blks_hit").unwrap_or(0.0), r.number("blks_read").unwrap_or(0.0));
            if hit + read > 0.0 {
                p.signals.push(
                    Signal::new(
                        "cache-hit",
                        "Reads served from memory",
                        hit / (hit + read) * 100.0,
                        SignalUnit::Percent,
                        SignalScope::SinceStartup,
                    )
                    .graded("above 99% is healthy for most workloads", 99.0, 95.0, false),
                );
            }
            if let Some(h) = hours {
                // Under an hour since the reset, a rate per hour would be an extrapolation: report the total.
                let (label, per) = if h >= 1.0 {
                    ("Temporary files written, per hour", h)
                } else {
                    ("Temporary files written since reset", 1.0)
                };
                p.signals.push(
                    Signal::new(
                        "temp-bytes",
                        label,
                        r.number("temp_bytes").unwrap_or(0.0) / per,
                        SignalUnit::Bytes,
                        SignalScope::SinceStartup,
                    )
                    .graded(
                        "near zero an hour: queries that sort or hash in memory",
                        100.0 * 1024.0 * 1024.0,
                        1024.0 * 1024.0 * 1024.0,
                        true,
                    ),
                );
                p.signals.push(Signal::new(
                    "commits",
                    "Commits",
                    r.number("xact_commit").unwrap_or(0.0) / (h * 3600.0),
                    SignalUnit::PerSecond,
                    SignalScope::SinceStartup,
                ));
            }
            let (commits, rollbacks) =
                (r.number("xact_commit").unwrap_or(0.0), r.number("xact_rollback").unwrap_or(0.0));
            if commits + rollbacks > 0.0 {
                p.signals.push(
                    Signal::new(
                        "rollbacks",
                        "Transactions rolled back",
                        rollbacks / (commits + rollbacks) * 100.0,
                        SignalUnit::Percent,
                        SignalScope::SinceStartup,
                    )
                    .graded("under 5% is healthy", 5.0, 20.0, true),
                );
            }
            p.signals.push(
                Signal::new(
                    "deadlocks",
                    "Deadlocks",
                    r.number("deadlocks").unwrap_or(0.0),
                    SignalUnit::Count,
                    SignalScope::SinceStartup,
                )
                .graded("0 is healthy", 1.0, 10.0, true),
            );
        }
        Ok(None) => {}
        Err(e) => notes.push(format!("Database statistics: {}", e.message)),
    }
    let checkpoints_sql = if probe.version_num >= 170_000 {
        "SELECT num_timed AS timed, num_requested AS requested FROM pg_stat_checkpointer"
    } else {
        "SELECT checkpoints_timed AS timed, checkpoints_req AS requested FROM pg_stat_bgwriter"
    };
    match query_one(client, checkpoints_sql).await {
        Ok(Some(r)) => {
            let (timed, requested) = (r.number("timed").unwrap_or(0.0), r.number("requested").unwrap_or(0.0));
            if timed + requested > 0.0 {
                p.signals.push(Signal::new(
                    "checkpoints",
                    "Checkpoints",
                    timed + requested,
                    SignalUnit::Count,
                    SignalScope::SinceStartup,
                ));
                p.signals.push(
                    Signal::new(
                        "checkpoints-requested",
                        "Checkpoints forced early",
                        requested / (timed + requested) * 100.0,
                        SignalUnit::Percent,
                        SignalScope::SinceStartup,
                    )
                    .graded(
                        "under 10% is healthy; more means max_wal_size is too small",
                        10.0,
                        30.0,
                        true,
                    ),
                );
            }
        }
        Ok(None) => {}
        Err(e) => notes.push(format!("Checkpoints: {}", e.message)),
    }
    if let Ok(Some(r)) = query_one(
        client,
        "SELECT count(*) FILTER (WHERE backend_type = 'client backend') AS used,
                current_setting('max_connections')::float8 - current_setting('superuser_reserved_connections')::float8 AS max
           FROM pg_stat_activity",
    )
    .await
        && let (Some(used), Some(max)) = (r.number("used"), r.number("max").filter(|m| *m > 0.0))
    {
        p.signals.push(
            Signal::new("connections", "Connections in use", used / max * 100.0, SignalUnit::Percent, SignalScope::Now)
                .graded("under 80% of max_connections is healthy", 80.0, 90.0, true),
        );
    }
    if probe.privileged {
        match query_one(
            client,
            "SELECT pid::text AS session, extract(epoch FROM now() - xact_start) AS seconds, application_name AS program, state
               FROM pg_stat_activity
              WHERE xact_start IS NOT NULL AND backend_type = 'client backend' AND pid <> pg_backend_pid()
                AND application_name <> 'culprit'
              ORDER BY xact_start LIMIT 1",
        )
        .await
        {
            Ok(Some(r)) => {
                let seconds = r.number("seconds").unwrap_or(0.0);
                p.oldest_transaction = Some(OpenTransaction {
                    session: r.string("session"),
                    seconds,
                    program: r.text("program").filter(|s| !s.is_empty()),
                    state: r.text("state"),
                    held_bytes: None,
                    held_versions: None,
                });
                p.signals.push(
                    Signal::new("oldest-transaction", "Oldest open transaction", seconds, SignalUnit::Seconds, SignalScope::Now)
                        .graded("seconds or minutes, not hours", 600.0, 1800.0, true),
                );
            }
            Ok(None) => {}
            Err(e) => notes.push(format!("Open transactions: {}", e.message)),
        }
    }
    (p, notes)
}

/// The shared category for a PostgreSQL wait event. A session with no wait event is running on CPU.
pub fn category(event_type: Option<&str>, event: Option<&str>) -> &'static str {
    let event = event.unwrap_or("");
    match event_type {
        None => "CPU",
        Some("IO") if event.starts_with("Wal") || event.starts_with("WAL") => "Log",
        Some("IO") => "IO",
        Some("LWLock") if event.starts_with("WAL") || event.starts_with("Wal") => "Log",
        Some("LWLock") | Some("BufferPin") => "Latch",
        Some("Lock") => "Lock",
        Some("Client") => "Network",
        Some("IPC")
            if event.starts_with("BgWorker")
                || event.starts_with("ExecuteGather")
                || event.starts_with("Parallel")
                || event.starts_with("Hash")
                || event.starts_with("MessageQueue") =>
        {
            "Parallelism"
        }
        _ => "Other",
    }
}

/// Sessions sampled every second for `seconds`: each observation of a busy session counts as one second of its time.
pub async fn sample(client: &Client, seconds: u32) -> Result<WaitSample> {
    let started = Instant::now();
    let taken_at = query_one(client, &format!("SELECT {} AS now", crate::rows::iso("now()")))
        .await?
        .and_then(|r| r.text("now"))
        .unwrap_or_default();
    let commits = |r: Option<crate::rows::Row>| r.and_then(|r| r.number("commits"));
    let commit_sql =
        "SELECT xact_commit + xact_rollback AS commits FROM pg_stat_database WHERE datname = current_database()";
    let before = commits(query_one(client, commit_sql).await?);
    let mut observed: BTreeMap<(String, String), f64> = BTreeMap::new();
    let mut samples = 0.0;
    let mut busy = 0.0;
    // N seconds need N + 1 samples, one at each end.
    let ticks = seconds.max(1) + 1;
    for tick in 0..ticks {
        let rows = query(
            client,
            "SELECT wait_event_type AS type, wait_event AS event, count(*) AS n
               FROM pg_stat_activity
              WHERE state = 'active' AND backend_type = 'client backend' AND pid <> pg_backend_pid()
                AND application_name <> 'culprit'
              GROUP BY 1, 2",
        )
        .await?;
        samples += 1.0;
        for r in rows {
            let n = r.number("n").unwrap_or(0.0);
            busy += n;
            let key = (r.text("type").unwrap_or_default(), r.text("event").unwrap_or_else(|| "Running".into()));
            *observed.entry(key).or_default() += n;
        }
        if tick + 1 < ticks {
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }
    let after = commits(query_one(client, commit_sql).await?);
    let elapsed = started.elapsed().as_secs_f64().max(1.0);
    // Each observation stands for one second of one session's time.
    let mut by_category: BTreeMap<&'static str, (f64, String, f64)> = BTreeMap::new();
    for ((event_type, event), n) in &observed {
        let key = category(Some(event_type.as_str()).filter(|t| !t.is_empty()), Some(event.as_str()));
        let entry = by_category.entry(key).or_default();
        entry.0 += n;
        if *n > entry.2 {
            entry.1 = if event_type.is_empty() { "Running".into() } else { format!("{event_type}: {event}") };
            entry.2 = *n;
        }
    }
    let mut categories: Vec<WaitStat> = by_category
        .into_iter()
        .map(|(category, (n, top, _))| WaitStat {
            category: category.into(),
            wait_type: Some(top),
            wait_ms: n * 1000.0,
            tasks: Some(n),
        })
        .collect();
    categories.sort_by(|a, b| b.wait_ms.total_cmp(&a.wait_ms));
    let mut signals = Vec::new();
    if let (Some(a), Some(b)) = (before, after) {
        signals.push(Signal::new(
            "commits",
            "Transactions",
            ((b - a) / elapsed).max(0.0),
            SignalUnit::PerSecond,
            SignalScope::Sampled,
        ));
    }
    let mut notes = Vec::new();
    if busy == 0.0 {
        notes.push("No session was running a statement at any of the samples: the database was idle.".into());
    }
    Ok(WaitSample {
        engine: Engine::Postgres,
        taken_at,
        seconds: elapsed,
        method: format!(
            "pg_stat_activity sampled every second for {seconds} s; each busy session seen counts as one second of its time"
        ),
        total_ms: busy * 1000.0,
        categories,
        signals,
        active_sessions: Some(if samples > 0.0 { busy / samples } else { 0.0 }),
        notes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_fold_into_shared_categories() {
        assert_eq!(category(None, None), "CPU");
        assert_eq!(category(Some("IO"), Some("DataFileRead")), "IO");
        assert_eq!(category(Some("IO"), Some("WalSync")), "Log");
        assert_eq!(category(Some("LWLock"), Some("WALWrite")), "Log");
        assert_eq!(category(Some("LWLock"), Some("BufferMapping")), "Latch");
        assert_eq!(category(Some("Lock"), Some("transactionid")), "Lock");
        assert_eq!(category(Some("Client"), Some("ClientWrite")), "Network");
        assert_eq!(category(Some("IPC"), Some("ExecuteGather")), "Parallelism");
        assert_eq!(category(Some("Timeout"), Some("PgSleep")), "Other");
    }

    #[test]
    fn io_columns_follow_the_version() {
        assert!(statements_io_column(170_000).contains("shared_blk_read_time"));
        assert!(statements_io_column(150_000).contains("temp_blk_read_time"));
        assert_eq!(statements_io_column(130_000), "(blk_read_time + blk_write_time)");
    }
}
