//! Pressure signals, waits from statement timing, and the "right now" sample. PostgreSQL keeps no wait history, so
//! history comes from per-statement I/O timing, and "right now" samples sessions every second.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use culprit_core::{
    CleanupHolder, Coverage, Engine, OpenTransaction, Pressure, Result, Signal, SignalScope, SignalUnit, WaitSample,
    WaitStat,
};
use tokio_postgres::Client;

use crate::collect::Probe;
use crate::rows::{query, query_one};
use crate::sources;

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

/// Database time by category from statement timing: disk I/O where it is timed, the rest as "Other". WAL write and
/// sync time (pg_stat_wal, pg_stat_io) isn't in it: that is the whole server's, spent mostly by the WAL writer and by
/// commits after a statement's timing ends, so it is no share of statement time. The scan shows it with the files'
/// latency instead.
pub async fn history_waits(
    _client: &Client,
    _probe: &Probe,
    statements_ms: f64,
    statements_io_ms: f64,
) -> Vec<WaitStat> {
    breakdown(statements_ms, statements_io_ms)
}

/// Statement time split into disk I/O and the rest. Nothing else PostgreSQL times is part of statement time per
/// category: CPU, locks and untimed waits stay together, never guessed apart.
fn breakdown(statements_ms: f64, statements_io_ms: f64) -> Vec<WaitStat> {
    if statements_io_ms <= 0.0 {
        return Vec::new();
    }
    let other = (statements_ms - statements_io_ms).max(0.0);
    let mut out = vec![WaitStat {
        category: "IO".into(),
        wait_type: Some("data file reads and writes".into()),
        wait_ms: statements_io_ms,
        tasks: None,
    }];
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

/// The pressure counters, each source read through `Coverage::read` (`crate::sources`).
pub async fn collect(client: &Client, probe: &Probe) -> (Pressure, Coverage) {
    let mut p = Pressure::default();
    let mut coverage = Coverage::default();
    // How long the counters have counted. PostgreSQL keeps statistics across a clean restart, so the server's start
    // time says nothing about it. A reset (of the database's counters or of one table's) stamps the database's
    // stats_reset. Until then it is empty: the counters run from when PostgreSQL last discarded its statistics (a
    // crash, a restore, the first start), which also stamps the shared counters' stats_reset (pg_stat_bgwriter,
    // pg_stat_archiver) with that moment. A later reset of one of those moves only its own stamp, so the earlier of the
    // two is the closer one: still never older than the counters (no "unused" claim stretches past them), and a rate
    // isn't inflated by a span cut short.
    let database = query_one(
        client,
        "SELECT blks_hit, blks_read, temp_bytes, xact_commit, xact_rollback, deadlocks,
                extract(epoch FROM now() - coalesce(stats_reset,
                    (SELECT least(b.stats_reset, a.stats_reset) FROM pg_stat_bgwriter b, pg_stat_archiver a))) / 3600 AS hours
           FROM pg_stat_database WHERE datname = current_database()",
    )
    .await;
    if let Some(Some(r)) = coverage.read(&sources::DATABASE_STATS, database) {
        let hours = r.number("hours").filter(|h| *h > 0.0);
        p.uptime_hours = hours;
        let (hit, read) = (r.number("blks_hit").unwrap_or(0.0), r.number("blks_read").unwrap_or(0.0));
        if hit + read > 0.0 {
            p.signals.push(
                Signal::new(
                    "cache-hit",
                    // shared_buffers only: a read the operating system's cache serves counts as a miss here.
                    "Reads served from shared_buffers",
                    hit / (hit + read) * 100.0,
                    SignalUnit::Percent,
                    SignalScope::SinceStartup,
                )
                .graded("above 99% is healthy for most workloads", 99.0, 95.0, false),
            );
        }
        let temp = r.number("temp_bytes").unwrap_or(0.0);
        match hours {
            // Over an hour of known counting: a rate, graded.
            Some(h) if h >= 1.0 => p.signals.push(
                Signal::new(
                    "temp-bytes",
                    "Temporary files written, per hour",
                    temp / h,
                    SignalUnit::Bytes,
                    SignalScope::SinceStartup,
                )
                .graded(
                    "near zero an hour: queries that sort or hash in memory",
                    100.0 * 1024.0 * 1024.0,
                    1024.0 * 1024.0 * 1024.0,
                    true,
                ),
            ),
            // Under an hour, or since an unknown time: the total, which no rate threshold applies to.
            Some(_) => p.signals.push(Signal::new(
                "temp-bytes",
                "Temporary files written since reset",
                temp,
                SignalUnit::Bytes,
                SignalScope::SinceStartup,
            )),
            None => p.signals.push(Signal::new(
                "temp-bytes",
                "Temporary files written since statistics began",
                temp,
                SignalUnit::Bytes,
                SignalScope::SinceStartup,
            )),
        }
        if let Some(h) = hours {
            p.signals.push(Signal::new(
                "commits",
                "Commits",
                r.number("xact_commit").unwrap_or(0.0) / (h * 3600.0),
                SignalUnit::PerSecond,
                SignalScope::SinceStartup,
            ));
        }
        let (commits, rollbacks) = (r.number("xact_commit").unwrap_or(0.0), r.number("xact_rollback").unwrap_or(0.0));
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
    let checkpoints_sql = if probe.version_num >= 170_000 {
        "SELECT num_timed AS timed, num_requested AS requested FROM pg_stat_checkpointer"
    } else {
        "SELECT checkpoints_timed AS timed, checkpoints_req AS requested FROM pg_stat_bgwriter"
    };
    if let Some(Some(r)) = coverage.read(&sources::CHECKPOINTS, query_one(client, checkpoints_sql).await) {
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
                    "under 10% is healthy; more usually means max_wal_size is too small (a manual CHECKPOINT, a \
                         base backup or CREATE DATABASE also requests one)",
                    10.0,
                    30.0,
                    true,
                ),
            );
        }
    }
    if let Ok(Some(r)) = query_one(
        client,
        "SELECT count(*) FILTER (WHERE backend_type = 'client backend' AND application_name IS DISTINCT FROM 'culprit') AS used,
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
    if probe.privileged
        && let Some(Some(t)) = coverage.read(&sources::OPEN_TRANSACTIONS, cleanup_holder(client).await)
    {
        if t.holder != Some(CleanupHolder::ReplicationSlot) {
            p.signals.push(
                Signal::new(
                    "oldest-transaction",
                    "Oldest transaction holding cleanup",
                    t.seconds,
                    SignalUnit::Seconds,
                    SignalScope::Now,
                )
                .graded("seconds or minutes, not hours", 600.0, 1800.0, true),
            );
        }
        p.oldest_transaction = Some(t);
    }
    (p, coverage)
}

/// Sessions of this database (and physical walsenders, which carry a standby's feedback for every database) whose
/// snapshot or transaction id holds VACUUM's horizon back. A session idle in a read-committed transaction has neither,
/// and a session in another database doesn't hold this one's tables back, so neither is listed. Autovacuum's own
/// workers are ignored by VACUUM.
const HOLDING_SESSION_SQL: &str = "
SELECT pid::text AS session, extract(epoch FROM now() - coalesce(xact_start, backend_start)) AS seconds,
       application_name AS program, state, greatest(age(backend_xmin), age(backend_xid))::float8 AS xid_age
  FROM pg_stat_activity
 WHERE (backend_xmin IS NOT NULL OR backend_xid IS NOT NULL)
   AND (datname = current_database() OR (datname IS NULL AND backend_type = 'walsender'))
   AND backend_type <> 'autovacuum worker' AND pid <> pg_backend_pid()
   AND application_name IS DISTINCT FROM 'culprit'
 ORDER BY greatest(age(backend_xmin), age(backend_xid)) DESC LIMIT 1";

/// A replication slot's xmin (a standby with hot_standby_feedback) holds the horizon for every database, active or not.
const HOLDING_SLOT_SQL: &str = "
SELECT slot_name AS session, active, age(xmin)::float8 AS xid_age
  FROM pg_replication_slots WHERE xmin IS NOT NULL ORDER BY age(xmin) DESC LIMIT 1";

/// A prepared transaction in this database holds the horizon until someone commits or rolls it back.
const HOLDING_PREPARED_SQL: &str = "
SELECT gid AS session, extract(epoch FROM now() - prepared) AS seconds, owner, age(transaction)::float8 AS xid_age
  FROM pg_prepared_xacts WHERE database = current_database() ORDER BY age(transaction) DESC LIMIT 1";

/// Whatever holds this database's VACUUM horizon furthest back: a session, a replication slot or a prepared
/// transaction, by how many transactions it is behind.
async fn cleanup_holder(client: &Client) -> Result<Option<OpenTransaction>> {
    let mut found: Vec<OpenTransaction> = Vec::new();
    if let Some(r) = query_one(client, HOLDING_SESSION_SQL).await? {
        found.push(OpenTransaction {
            session: r.string("session"),
            seconds: r.number("seconds").unwrap_or(0.0),
            program: r.text("program").filter(|s| !s.is_empty()),
            state: r.text("state"),
            holder: Some(CleanupHolder::Session),
            xid_age: r.number("xid_age"),
            ..OpenTransaction::default()
        });
    }
    // Slots and prepared transactions are rarer; a view this login can't read leaves them out.
    if let Ok(Some(r)) = query_one(client, HOLDING_SLOT_SQL).await {
        found.push(OpenTransaction {
            session: r.string("session"),
            state: Some(if r.flag("active") { "active slot" } else { "inactive slot" }.into()),
            holder: Some(CleanupHolder::ReplicationSlot),
            xid_age: r.number("xid_age"),
            ..OpenTransaction::default()
        });
    }
    if let Ok(Some(r)) = query_one(client, HOLDING_PREPARED_SQL).await {
        found.push(OpenTransaction {
            session: r.string("session"),
            seconds: r.number("seconds").unwrap_or(0.0),
            program: r.text("owner").filter(|s| !s.is_empty()),
            state: Some("prepared".into()),
            holder: Some(CleanupHolder::PreparedTransaction),
            xid_age: r.number("xid_age"),
            ..OpenTransaction::default()
        });
    }
    Ok(oldest_holder(found))
}

/// The holder furthest behind; on a tie the first found (a session before a slot before a prepared transaction).
fn oldest_holder(found: Vec<OpenTransaction>) -> Option<OpenTransaction> {
    found.into_iter().reduce(|a, b| if b.xid_age.unwrap_or(0.0) > a.xid_age.unwrap_or(0.0) { b } else { a })
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
                AND datname = current_database() AND application_name IS DISTINCT FROM 'culprit'
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
            wait_ms: n / samples * elapsed * 1000.0,
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
            "This database's pg_stat_activity sampled every second for {seconds} s; observations are weighted over the measured sample duration"
        ),
        total_ms: busy / samples * elapsed * 1000.0,
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

    fn category_ms(waits: &[WaitStat], category: &str) -> f64 {
        waits.iter().filter(|w| w.category == category).map(|w| w.wait_ms).sum()
    }

    // The aptabase reading: pg_stat_wal's write and sync time (4,143 s) stood for 99.98% of statement time, because it
    // was capped at the time statements didn't spend on I/O. It is the server's WAL writer and commits, outside
    // statement time: never a share of it.
    #[test]
    fn wal_time_is_no_share_of_statement_time() {
        let waits = breakdown(4_143_706.0, 750.6);
        assert_eq!(category_ms(&waits, "Log"), 0.0);
        assert_eq!(category_ms(&waits, "IO"), 750.6);
        assert!((category_ms(&waits, "Other") - 4_142_955.4).abs() < 0.01);
        assert!(waits.iter().all(|w| !w.wait_type.as_deref().unwrap_or_default().contains("WAL")));
        // Nothing timed per category: no breakdown, rather than all of it as "Other".
        assert!(breakdown(100_000.0, 0.0).is_empty(), "nothing timed, nothing to break down");
    }

    #[test]
    fn the_holder_furthest_behind_holds_the_horizon() {
        let holder = |session: &str, kind, age| OpenTransaction {
            session: session.into(),
            holder: Some(kind),
            xid_age: Some(age),
            ..OpenTransaction::default()
        };
        let oldest = oldest_holder(vec![
            holder("4242", CleanupHolder::Session, 1_200.0),
            holder("standby_1", CleanupHolder::ReplicationSlot, 9_000_000.0),
            holder("tx-17", CleanupHolder::PreparedTransaction, 300.0),
        ])
        .unwrap();
        assert_eq!((oldest.session.as_str(), oldest.holder), ("standby_1", Some(CleanupHolder::ReplicationSlot)));
        assert!(oldest_holder(Vec::new()).is_none());
    }
}
