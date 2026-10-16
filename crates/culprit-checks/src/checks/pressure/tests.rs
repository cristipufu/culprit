use super::*;
use crate::waits::snapshot_with;
use culprit_core::{GrantStat, OpenTransaction, ResourceSample, Signal, SignalScope, SignalUnit, WaitStat};

fn with_signals(engine: Engine, signals: Vec<Signal>) -> Snapshot {
    let mut s = snapshot_with(engine);
    s.pressure = Some(Pressure {
        uptime_hours: Some(72.0),
        signals,
        grants: vec![],
        oldest_transaction: None,
        ..Pressure::default()
    });
    s
}

fn sig(key: &str, value: f64) -> Signal {
    Signal::new(key, key, value, SignalUnit::Count, SignalScope::SinceStartup)
}

/// A signal the collector graded (it does for a server's own counters under load).
fn graded(key: &str, value: f64) -> Signal {
    sig(key, value).graded("under 10% is healthy", 10.0, 25.0, true)
}

fn fixture(name: &str) -> Snapshot {
    let text =
        std::fs::read_to_string(format!("{}/../../fixtures/{name}", env!("CARGO_MANIFEST_DIR"))).expect("fixture");
    serde_json::from_str(&text).expect("snapshot")
}

#[test]
fn over_sized_grants_are_named_with_their_query() {
    let mut s = with_signals(Engine::Mssql, vec![sig("memory-grants-pending", 6.0)]);
    let hash = s.workload.as_mut().unwrap().queries[0].query_hash.clone().unwrap_or_else(|| "0xABC".into());
    s.workload.as_mut().unwrap().queries[0].query_hash = Some(hash.clone());
    s.pressure.as_mut().unwrap().grants = vec![GrantStat {
        query_hash: hash.to_lowercase(),
        executions: 40.0,
        avg_granted_kb: 2.0 * 1024.0 * 1024.0,
        avg_used_kb: 40.0 * 1024.0,
        max_granted_kb: 3.0 * 1024.0 * 1024.0,
        last_granted_kb: None,
        last_used_kb: None,
    }];
    let f = memory_grant(&s);
    assert_eq!(f.len(), 1);
    assert!(f[0].title.contains("reserves 2.0 GB of memory per run and uses 40.0 MB"), "{}", f[0].title);
    assert_eq!(f[0].severity, Severity::High);
    // The query never waited for memory itself: the grant claims none of its time.
    assert_eq!(f[0].impact.db_time_share, None);
    let q = memory_queue(&s);
    assert_eq!(q[0].title, "6 queries in this database were waiting for a memory grant when the scan ran");
    // A few queries seen queueing once is no record of queueing: the waste alone grades it.
    s.pressure.as_mut().unwrap().signals.retain(|x| x.key != "memory-grants-pending");
    s.pressure.as_mut().unwrap().signals.push(sig("memory-grants-pending", 3.0));
    assert_eq!(memory_grant(&s)[0].severity, Severity::Medium);
    // Recent runs that use their grant (memory grant feedback shrank it) are fixed already.
    s.pressure.as_mut().unwrap().grants[0].last_granted_kb = Some(100.0 * 1024.0);
    s.pressure.as_mut().unwrap().grants[0].last_used_kb = Some(60.0 * 1024.0);
    assert!(memory_grant(&s).is_empty());
}

// C10: a grant or a slow reader claims only the time it explains, not the whole query's.
#[test]
fn grants_and_network_waits_claim_only_their_own_time() {
    let mut s = with_signals(Engine::Mssql, vec![]);
    let total = s.workload.as_ref().unwrap().total_time_ms;
    let q = &mut s.workload.as_mut().unwrap().queries[0];
    q.query_hash = Some("0xABC".into());
    let query_ms = q.total_time_ms;
    q.waits = vec![WaitStat { category: "Memory".into(), wait_type: None, wait_ms: query_ms * 0.1, tasks: None }];
    s.pressure.as_mut().unwrap().grants = vec![GrantStat {
        query_hash: "0xabc".into(),
        executions: 40.0,
        avg_granted_kb: 2.0 * 1024.0 * 1024.0,
        avg_used_kb: 40.0 * 1024.0,
        max_granted_kb: 3.0 * 1024.0 * 1024.0,
        last_granted_kb: None,
        last_used_kb: None,
    }];
    let f = memory_grant(&s);
    let share = f[0].impact.db_time_share.unwrap();
    assert!((share - query_ms * 0.1 / total).abs() < 1e-9, "{share}");

    let mut s = snapshot_with(Engine::Mssql);
    let total = s.workload.as_ref().unwrap().total_time_ms;
    let q = &mut s.workload.as_mut().unwrap().queries[0];
    let network = q.total_time_ms * 0.3;
    q.waits = vec![WaitStat { category: "Network".into(), wait_type: None, wait_ms: network, tasks: None }];
    let f = network_wait(&s);
    assert!((f[0].impact.db_time_share.unwrap() - network / total).abs() < 1e-9);
}

// C4: the demo's 5.9 s of Memory waits over 4.6 hours is a share of almost nothing.
#[test]
fn memory_waits_need_an_amount_not_only_a_share() {
    let s = fixture("mssql-demo.json");
    let memory: f64 = s.waits.iter().filter(|w| w.category == "Memory").map(|w| w.wait_ms).sum();
    assert!(memory > 5_000.0 && memory < 6_000.0);
    assert!(memory_queue(&s).is_empty(), "6% of waiting, but about a second an hour");

    // Twenty minutes an hour, a quarter of all waiting, in Query Store's window: a real queue.
    let mut s = fixture("mssql-demo.json");
    let hours = s.workload.as_ref().unwrap().window_hours;
    let other: f64 = s.waits.iter().filter(|w| w.category != "Memory").map(|w| w.wait_ms).sum();
    let planted = (20.0 * MINUTE_MS * hours).max(other / 3.0);
    s.waits.retain(|w| w.category != "Memory");
    s.waits.push(WaitStat {
        category: "Memory".into(),
        wait_type: Some("Memory".into()),
        wait_ms: planted,
        tasks: None,
    });
    let f = memory_queue(&s);
    assert_eq!(f[0].severity, Severity::High);
    // Query Store's Memory category mixes grants and allocations: no grant story without grant evidence.
    assert!(f[0].summary.contains("the totals don't say which"), "{}", f[0].summary);
    assert!(f[0].fix.as_ref().unwrap().summary.starts_with("Sample Waits"));
}

// C4: one query seen waiting once is a moment to watch, like THREADPOOL; RESOURCE_SEMAPHORE since the start
// tells the grant story, at most Medium.
#[test]
fn a_single_pending_grant_is_low_and_grant_history_is_at_most_medium() {
    let s = with_signals(Engine::Mssql, vec![sig("memory-grants-pending", 1.0)]);
    let f = memory_queue(&s);
    assert_eq!((f[0].severity, f[0].confidence), (Severity::Low, Confidence::Low));
    assert!(f[0].caveats[0].starts_with("Seen once"));
    let mut s = with_signals(Engine::Mssql, vec![sig("grant-wait-ms", 72.0 * 30.0 * MINUTE_MS)]);
    let f = memory_queue(&s);
    // The instance's totals, with nothing of this database's queueing: said, not graded on this database.
    assert_eq!(f[0].severity, Severity::Low);
    // On Azure SQL Database the instance is the database: the same history is this database's, at most Medium.
    s.server.platform = Some(culprit_core::Platform::AzureSqlDatabase);
    let f = memory_queue(&s);
    assert_eq!(f[0].severity, Severity::Medium);
    assert!(
        f[0].title.starts_with("Queries waited 36 h for memory grants since SQL Server started 3 days ago"),
        "{}",
        f[0].title
    );
    assert!(f[0].summary.contains("RESOURCE_SEMAPHORE"));
    assert!(memory_queue(&with_signals(Engine::Mssql, vec![sig("grant-wait-ms", 30_000.0)])).is_empty());
}

#[test]
fn compile_pressure_needs_load_and_ratio() {
    let mut s = with_signals(Engine::Mssql, vec![graded("compiles-per-batch", 31.0), sig("batch-requests", 120.0)]);
    // The instance's counters, and this database's statements show no literal values: Low, and no fix aimed here.
    let f = compile_pressure(&s);
    assert_eq!(f[0].severity, Severity::Low);
    assert!(f[0].fix.as_ref().unwrap().sql.is_none());
    // This database sends one statement with different literal values: it's a likely source.
    s.query_families = vec![culprit_core::QueryFamily {
        key: "0xAB".into(),
        sample: "SELECT * FROM orders WHERE id = 42".into(),
        variants: 900.0,
        executions: 900.0,
        total_time_ms: 9_000.0,
        cached_bytes: None,
        query_ids: Vec::new(),
    }];
    let f = compile_pressure(&s);
    assert!(f[0].title.contains("31% of batches (about 2.2K a minute)"), "{}", f[0].title);
    // An average since the start: Medium at most.
    assert_eq!(f[0].severity, Severity::Medium);
    // The fix cuts compiles; caching one-off plans as stubs wouldn't.
    let sql = f[0].fix.as_ref().unwrap().sql.clone().unwrap();
    assert!(sql.contains("PARAMETERIZATION FORCED") && !sql.contains("optimize for ad hoc"), "{sql}");
    assert!(
        compile_pressure(&with_signals(
            Engine::Mssql,
            vec![graded("compiles-per-batch", 31.0), sig("batch-requests", 1.0)]
        ))
        .is_empty()
    );
    assert!(compile_pressure(&with_signals(Engine::Mssql, vec![graded("compiles-per-batch", 18.0)])).is_empty());
}

// C3: on Azure the counters cover the server behind the database (its elastic pool's too) and the collector
// doesn't grade them; the check doesn't either.
#[test]
fn shared_compile_counters_raise_nothing() {
    let shared = Signal {
        healthy: Some("covers every database on the server behind this one, so it isn't graded here".into()),
        ..sig("compiles-per-batch", 45.0)
    };
    let mut s = with_signals(Engine::Mssql, vec![shared, sig("batch-requests", 300.0)]);
    s.server.cloud = Some("Azure SQL Database".into());
    assert!(compile_pressure(&s).is_empty());
    // Under an hour since the start there is no rate to judge.
    let mut s = with_signals(Engine::Mssql, vec![graded("compiles-per-batch", 45.0), sig("batch-requests", 300.0)]);
    s.pressure.as_mut().unwrap().uptime_hours = Some(0.4);
    assert!(compile_pressure(&s).is_empty());
}

fn transaction(session: &str, seconds: f64) -> OpenTransaction {
    OpenTransaction {
        session: session.into(),
        seconds,
        program: Some("billing-worker".into()),
        state: Some("idle in transaction".into()),
        holder: Some(CleanupHolder::Session),
        ..OpenTransaction::default()
    }
}

#[test]
fn open_transactions_are_explained_per_engine() {
    for (engine, held, needle) in [
        (Engine::Mssql, Some(3.0 * 1024.0 * 1024.0 * 1024.0), "this database's version store holds 3.0 GB"),
        (Engine::Postgres, None, "VACUUM can't clean up"),
        (Engine::Mysql, None, "InnoDB can't purge old row versions"),
    ] {
        let mut s = with_signals(engine, vec![]);
        s.pressure.as_mut().unwrap().oldest_transaction =
            Some(OpenTransaction { held_bytes: held, ..transaction("92", 3.0 * 3600.0) });
        let f = held_cleanup(&s);
        assert!(f[0].title.contains(needle), "{engine:?}: {}", f[0].title);
        assert!(f[0].summary.contains("billing-worker"));
        assert_eq!(f[0].severity, Severity::High);
    }
}

// C2: InnoDB's history length is the whole server's and counts undo logs: it never makes a seconds-old
// transaction the culprit, and it isn't called this transaction's row versions.
#[test]
fn a_young_mysql_transaction_is_not_blamed_for_the_servers_history() {
    let mut s = with_signals(Engine::Mysql, vec![sig("history-length", 2_100_000.0)]);
    s.pressure.as_mut().unwrap().oldest_transaction =
        Some(OpenTransaction { held_versions: Some(2_100_000.0), ..transaction("104", 2.0) });
    assert!(held_cleanup(&s).is_empty());
    s.pressure.as_mut().unwrap().oldest_transaction =
        Some(OpenTransaction { database: Some("billing".into()), ..transaction("104", 2.0 * 3600.0) });
    let f = held_cleanup(&s);
    assert!(f[0].summary.contains("2.1M undo logs"), "{}", f[0].summary);
    assert!(f[0].summary.contains("It runs in database billing."));
    assert!(!f[0].title.contains("row versions for it"));
}

// C2: a SQL Server writer holds the log and its locks; the version store is named only for a transaction that
// reads or makes row versions.
#[test]
fn sql_server_says_what_the_transaction_holds() {
    let mut s = with_signals(Engine::Mssql, vec![]);
    s.pressure.as_mut().unwrap().oldest_transaction =
        Some(OpenTransaction { wrote: Some(true), state: Some("running".into()), ..transaction("71", 3600.0) });
    let f = held_cleanup(&s);
    assert!(f[0].title.contains("uncommitted changes"), "{}", f[0].title);
    assert!(f[0].summary.contains("can't reuse the transaction log"));
    assert!(!f[0].summary.contains("version store"), "{}", f[0].summary);
}

// C2: what holds PostgreSQL's horizon may be a replication slot or a prepared transaction, each with its own fix.
#[test]
fn postgres_slots_and_prepared_transactions_get_their_own_fix() {
    let mut s = with_signals(Engine::Postgres, vec![]);
    s.pressure.as_mut().unwrap().oldest_transaction = Some(OpenTransaction {
        session: "standby_1".into(),
        state: Some("inactive slot".into()),
        holder: Some(CleanupHolder::ReplicationSlot),
        xid_age: Some(150_000_000.0),
        ..OpenTransaction::default()
    });
    let f = held_cleanup(&s);
    assert_eq!(f[0].title, "Replication slot standby_1 holds VACUUM back by 150.0M transactions");
    assert_eq!(f[0].severity, Severity::High);
    let sql = f[0].fix.as_ref().unwrap().sql.clone().unwrap();
    assert!(sql.contains("pg_drop_replication_slot('standby_1')"), "{sql}");
    s.pressure.as_mut().unwrap().oldest_transaction = Some(OpenTransaction {
        session: "tx-17".into(),
        seconds: 7200.0,
        state: Some("prepared".into()),
        holder: Some(CleanupHolder::PreparedTransaction),
        xid_age: Some(40_000.0),
        ..OpenTransaction::default()
    });
    let f = held_cleanup(&s);
    assert!(f[0].title.starts_with("Prepared transaction 'tx-17' has waited 2 h "), "{}", f[0].title);
    assert_eq!(f[0].fix.as_ref().unwrap().sql.as_deref(), Some("ROLLBACK PREPARED 'tx-17';"));
    // A slot only a few transactions behind holds nothing worth a finding.
    s.pressure.as_mut().unwrap().oldest_transaction = Some(OpenTransaction {
        session: "standby_2".into(),
        holder: Some(CleanupHolder::ReplicationSlot),
        xid_age: Some(800.0),
        ..OpenTransaction::default()
    });
    assert!(held_cleanup(&s).is_empty());
}

#[test]
fn connection_limits_for_postgres_and_mysql() {
    let f = worker_pressure(&with_signals(Engine::Postgres, vec![sig("connections", 92.0)]));
    assert_eq!(f[0].title, "Connections are at 92% of max_connections");
    assert!(worker_pressure(&with_signals(Engine::Mysql, vec![sig("connections", 40.0)])).is_empty());
}

// C14: one peak since the start is a moment in the past: Low unless it was within the last day, and a peak above
// today's limit says the limit was lowered.
#[test]
fn a_connection_peak_since_startup_is_not_a_current_emergency() {
    let peak = |pct: f64, ago: f64| {
        worker_pressure(&with_signals(
            Engine::Mysql,
            vec![sig("connections", 20.0), sig("max-used-connections", pct), sig("max-used-connections-ago", ago)],
        ))
    };
    let f = peak(104.0, 9.0 * 86_400.0);
    assert_eq!(f[0].severity, Severity::Low);
    assert!(f[0].title.starts_with("MySQL reached its connection limit once, 9.0 days ago"), "{}", f[0].title);
    assert!(f[0].summary.contains("the limit was lowered after that"));
    assert_eq!(peak(100.0, 3600.0)[0].severity, Severity::Medium);
}

// C14: THREADPOOL totals since the start are said as totals, at most Medium; a queue seen now is High; one or two
// requests seen once are Low.
#[test]
fn threadpool_waits_since_startup_are_totals_not_rates() {
    let f = worker_pressure(&with_signals(Engine::Mssql, vec![sig("threadpool-wait-ms", 72.0 * 120_000.0)]));
    assert_eq!(f[0].severity, Severity::Medium);
    assert!(
        f[0].title.starts_with("Requests waited 2 h 24 min for a worker thread since SQL Server started 3 days ago"),
        "{}",
        f[0].title
    );
    assert!(f[0].summary.contains("the counters don't say when"));
    let f = worker_pressure(&with_signals(Engine::Mssql, vec![sig("threadpool-waiting", 2.0)]));
    assert_eq!((f[0].severity, f[0].confidence), (Severity::Low, Confidence::Low));
    let f = worker_pressure(&with_signals(Engine::Mssql, vec![sig("threadpool-waiting", 6.0)]));
    assert_eq!(f[0].severity, Severity::High);
    assert!(f[0].title.contains("THREADPOOL"));
    // Under an hour since the start, a total is no rate: nothing to grade unless someone is waiting now.
    let mut s = with_signals(Engine::Mssql, vec![sig("threadpool-wait-ms", 900_000.0)]);
    s.pressure.as_mut().unwrap().uptime_hours = Some(0.5);
    assert!(worker_pressure(&s).is_empty());
}

#[test]
fn network_waits_name_big_result_sets() {
    let mut s = snapshot_with(Engine::Mssql);
    let q = &mut s.workload.as_mut().unwrap().queries[0];
    q.waits = vec![WaitStat {
        category: "Network".into(),
        wait_type: Some("Network IO".into()),
        wait_ms: q.total_time_ms * 0.6,
        tasks: None,
    }];
    q.rows = Some(q.executions * 80_000.0);
    let f = network_wait(&s);
    assert!(f[0].title.contains("60% of its time while sending 80K rows per call"), "{}", f[0].title);
}

fn samples(pick: impl Fn(usize) -> (f64, f64)) -> Vec<ResourceSample> {
    (0..20)
        .map(|i| {
            let (log, workers) = pick(i);
            ResourceSample {
                at: format!("2026-10-11T10:{:02}:00Z", i * 3),
                cpu_pct: Some(30.0),
                data_io_pct: Some(10.0),
                log_write_pct: Some(log),
                memory_pct: Some(80.0),
                workers_pct: Some(workers),
            }
        })
        .collect()
}

#[test]
fn tier_limits_from_resource_samples() {
    let mut s = snapshot_with(Engine::Mssql);
    s.resources = samples(|i| (if i % 5 < 2 { 99.0 } else { 40.0 }, 5.0));
    let f = tier_limit(&s);
    assert_eq!(f.len(), 1);
    // Twenty samples three minutes apart: the span said as it is.
    assert!(f[0].title.starts_with("Log writes hit the tier's limit 40% of the last 57 minutes"), "{}", f[0].title);
    assert!(f[0].summary.contains("wait rather than fail"));
    // Minutes of samples (after a failover or a resume) aren't a real part of the hour: no finding.
    s.resources.truncate(8);
    assert!(tier_limit(&s).is_empty());
}

// C18: at the worker limit Azure refuses requests (error 10928); the finding says so in its title and summary.
#[test]
fn the_worker_limit_refuses_requests() {
    let mut s = snapshot_with(Engine::Mssql);
    s.resources = samples(|i| (10.0, if i % 2 == 0 { 100.0 } else { 60.0 }));
    let f = tier_limit(&s);
    assert!(f[0].title.ends_with("new requests are refused (error 10928)"), "{}", f[0].title);
    assert!(f[0].summary.contains("refuses new requests with error 10928"));
    assert!(!f[0].summary.contains("rather than fail"));
}

#[test]
fn forced_checkpoints_name_every_cause() {
    let f = checkpoint_pressure(&with_signals(
        Engine::Postgres,
        vec![sig("checkpoints-requested", 62.0), sig("checkpoints", 50.0)],
    ));
    assert!(f[0].title.starts_with("62% of checkpoints start early, most likely because WAL fills max_wal_size"));
    assert!(f[0].summary.contains("a manual CHECKPOINT, a base backup or CREATE DATABASE"));
}

// C15: 61 waits an hour of half a second each is half a minute an hour: not a finding. What counts is the time,
// the server's, since startup: at most Medium.
#[test]
fn row_lock_waits_are_graded_on_time_and_said_to_be_server_wide() {
    let hourly = |n: f64| Signal { unit: SignalUnit::PerHour, ..sig("row-lock-waits", n) };
    let quiet = with_signals(Engine::Mysql, vec![hourly(61.0), sig("row-lock-avg", 510.0)]);
    assert!(row_lock_waits(&quiet).is_empty());
    let mut s = with_signals(
        Engine::Mysql,
        vec![hourly(340.0), sig("row-lock-avg", 1200.0), sig("row-lock-time", 72.0 * 340.0 * 1200.0)],
    );
    let f = row_lock_waits(&s);
    assert!(f[0].title.starts_with("Transactions on this server waited"), "{}", f[0].title);
    assert!(f[0].title.contains("since MySQL started 3 days ago, 1.2 s each on average"), "{}", f[0].title);
    assert!(f[0].summary.contains("across the whole server"));
    assert!(f[0].severity <= Severity::Medium);
    // 340 waits an hour of 1.2 s is nearly seven minutes an hour, but no statement of this database waits on
    // locks: the server's other databases are the likelier source, so it stays Low.
    assert!(app_queries(&s).all(|q| q.waits.iter().all(|w| category_key(&w.category) != "lock")));
    assert_eq!(f[0].severity, Severity::Low);
    assert!(f[0].caveats[0].contains("whole server"));
    // Under an hour of counters there is no rate.
    s.pressure.as_mut().unwrap().uptime_hours = Some(0.5);
    assert!(row_lock_waits(&s).is_empty());
}
