//! The dashboard on the demo reports: every tile says something on every engine, statuses follow the evidence, and a
//! live read re-ranks what needs a look.

use culprit_checks::dashboard::{dashboard, family, with_now, without_now};
use culprit_core::{
    Dashboard, DeadlockReport, Engine, LiveState, Report, Session, Severity, TileKey, TileStatus, Timeline,
    TimelineSource,
};

fn report(name: &str) -> Report {
    let text = std::fs::read_to_string(format!("{}/../../fixtures/{name}.report.json", env!("CARGO_MANIFEST_DIR")))
        .expect("fixture");
    serde_json::from_str(&text).expect("report")
}

fn deadlocks(engine: &str) -> DeadlockReport {
    let text = std::fs::read_to_string(format!("{}/../../fixtures/deadlocks.sample.json", env!("CARGO_MANIFEST_DIR")))
        .expect("fixture");
    let all: serde_json::Value = serde_json::from_str(&text).expect("json");
    serde_json::from_value(all[engine].clone()).expect("deadlock report")
}

fn tile(d: &Dashboard, key: TileKey) -> &culprit_core::Tile {
    d.tile(key).expect("tile")
}

fn session(id: &str, state: &str) -> Session {
    Session {
        id: id.into(),
        user: None,
        application: None,
        host: None,
        database: None,
        state: state.into(),
        command: None,
        wait: None,
        wait_ms: None,
        blocked_by: None,
        elapsed_ms: None,
        transaction_ms: None,
        cpu_ms: None,
        reads: None,
        sql: None,
    }
}

fn live(sessions: Vec<Session>, engine: Engine) -> LiveState {
    let blocking = culprit_core::blocking::analyze(engine, &sessions, &[]);
    LiveState {
        taken_at: "2026-10-11T12:00:00Z".into(),
        sessions,
        waits: vec![],
        session_actions: vec![],
        session_actions_note: None,
        blocking,
    }
}

#[test]
fn every_engine_gets_every_tile_with_a_takeaway() {
    for name in ["mssql-demo", "mssql-demo-staging", "pg-demo", "mysql-demo", "mariadb-demo"] {
        let d = dashboard(&report(name));
        assert_eq!(d.tiles.len(), 14, "{name}");
        for t in &d.tiles {
            assert!(t.takeaway.ends_with('.'), "{name} {:?}: one full sentence: {}", t.key, t.takeaway);
            assert!(!t.takeaway.contains("  ") && !t.takeaway.contains(".."), "{name}: {}", t.takeaway);
        }
        // The headline is the most urgent tile's takeaway, or says nothing needs a look.
        match d.attention.first() {
            Some(key) => assert_eq!(d.headline, tile(&d, *key).takeaway),
            None => assert!(d.headline.starts_with("Nothing needs a look")),
        }
        assert!(d.attention.iter().all(|k| matches!(tile(&d, *k).status, TileStatus::Bad | TileStatus::Watch)));
    }
}

#[test]
fn without_query_statistics_the_headline_says_how_to_turn_them_on() {
    let d = dashboard(&report("mssql-demo-staging"));
    assert_eq!(d.attention.first(), Some(&TileKey::Statistics), "{:?}", d.attention);
    assert_eq!(d.headline, tile(&d, TileKey::Statistics).takeaway);
}

#[test]
fn without_query_statistics_the_statistics_tile_leads_with_the_fix() {
    let d = dashboard(&report("mssql-demo-staging"));
    let stats = tile(&d, TileKey::Statistics);
    assert_eq!(stats.status, TileStatus::Bad);
    assert!(stats.takeaway.contains("Query Store is OFF"), "{}", stats.takeaway);
    assert_eq!(stats.fix_sql.as_deref(), Some("ALTER DATABASE CURRENT SET QUERY_STORE = ON;"));
    for key in [TileKey::DbTime, TileKey::TopQueries, TileKey::Plans] {
        assert_eq!(tile(&d, key).status, TileStatus::Unavailable, "{key:?}");
    }
    // Findings still come from tables, indexes and settings.
    let r = report("mssql-demo-staging");
    let count = format!("{} opportunities", r.summary.opportunities);
    assert!(tile(&d, TileKey::Findings).takeaway.starts_with(&count), "{}", tile(&d, TileKey::Findings).takeaway);
}

#[test]
fn postgres_says_why_there_are_no_waits() {
    // A scan that recorded no waits (the demo report carries a sampled mix; take it away).
    let mut r = report("pg-demo");
    r.snapshot.waits.clear();
    let d = dashboard(&r);
    let waits = tile(&d, TileKey::Waits);
    assert_eq!(waits.status, TileStatus::Unavailable);
    assert!(waits.takeaway.starts_with("PostgreSQL keeps no history"), "{}", waits.takeaway);
}

#[test]
fn top_queries_and_plans_link_to_queries() {
    let mut r = report("mssql-demo");
    // Query 8 switched plans in the window; give it a share the tile lists (it lists switches over 2% of the time).
    let w = r.snapshot.workload.as_mut().unwrap();
    let total = w.total_time_ms;
    w.queries.iter_mut().find(|q| q.id == "8").unwrap().total_time_ms = total * 0.12;
    let d = dashboard(&r);
    let top = tile(&d, TileKey::TopQueries);
    assert!(
        top.takeaway.starts_with("One query is ") && top.takeaway.contains("% of database time"),
        "{}",
        top.takeaway
    );
    let ids: Vec<&str> = r.snapshot.workload.as_ref().unwrap().queries.iter().map(|q| q.id.as_str()).collect();
    assert!(top.items.iter().all(|i| ids.contains(&i.query_id.as_deref().unwrap())));
    let plans = tile(&d, TileKey::Plans);
    assert!(plans.takeaway.contains("current query plans have something to fix"), "{}", plans.takeaway);
    assert!(plans.items.iter().all(|i| i.query_id.is_some()));
    // The one query that switched plans in the window shows on the database time tile.
    let db = tile(&d, TileKey::DbTime);
    assert_eq!(db.items.len(), 1);
    assert!(db.detail.as_deref().unwrap().contains("switched plans"));
}

#[test]
fn findings_count_by_severity_and_show_the_top_three() {
    let r = report("pg-demo");
    let d = dashboard(&r);
    let f = tile(&d, TileKey::Findings);
    // The demo spends about 13 minutes a day running queries: its worst findings are Medium (a minute a day or more,
    // under ten), so the tile asks to watch rather than act.
    let count = |s: Severity| r.findings.iter().filter(|x| x.severity == s).count() as f64;
    assert_eq!(count(Severity::High), 0.0);
    assert_eq!(f.status, TileStatus::Watch);
    assert_eq!(f.metric("high").and_then(|m| m.value), Some(0.0));
    assert_eq!(f.metric("medium").and_then(|m| m.value), Some(count(Severity::Medium)));
    assert_eq!(f.items.len(), 3);
    assert!(f.items.iter().all(|i| i.finding_id.is_some()));
}

#[test]
fn a_trend_compares_the_last_stretch_with_what_came_before() {
    let mut r = report("mssql-demo");
    let w = r.snapshot.workload.as_mut().unwrap();
    // Eight hourly buckets; the last two (a quarter) run at three times the rate of the six before.
    let starts: Vec<String> = (0..8).map(|h| format!("2026-10-11T{:02}:00:00Z", h + 2)).collect();
    w.window_start = Some(starts[0].clone());
    w.timeline = Some(Timeline {
        source: TimelineSource::QueryStore,
        starts,
        end: "2026-10-11T10:00:00Z".into(),
        time_ms: vec![10_000.0, 10_000.0, 10_000.0, 10_000.0, 10_000.0, 10_000.0, 30_000.0, 30_000.0],
        executions: vec![1.0; 8],
        unknown: None,
    });
    let d = dashboard(&r);
    let db = tile(&d, TileKey::DbTime);
    assert_eq!(db.status, TileStatus::Bad);
    assert_eq!(db.takeaway, "Database time is up 3.0× in the last 2 hours compared with the 6 hours before.");
    assert_eq!(d.attention.first(), Some(&TileKey::DbTime), "a jump in database time comes before findings");
}

#[test]
fn deadlocks_are_grouped_and_postgres_only_counts_them() {
    let mut r = report("mssql-demo");
    r.snapshot.deadlocks = Some(deadlocks("mssql"));
    r.snapshot.taken_at = "2026-10-11T14:00:00Z".into();
    r = culprit_checks::analyze(r.snapshot);
    let d = dashboard(&r);
    let t = tile(&d, TileKey::Deadlocks);
    assert_eq!(t.status, TileStatus::Info, "two historical events follow the check's Low assessment");
    // The sample covers six hours, and the tile says so.
    assert!(t.takeaway.starts_with("2 deadlocks in the last 6 hours"), "{}", t.takeaway);
    assert!(!t.items.is_empty());

    let mut r = report("pg-demo");
    r.snapshot.deadlocks = Some(DeadlockReport {
        source: "pg_stat_database".into(),
        since_hours: 24.0,
        counted: Some(12.0),
        counted_since: Some("2026-10-05T12:00:00Z".into()),
        ..Default::default()
    });
    r.snapshot.taken_at = "2026-10-11T12:00:00Z".into();
    r = culprit_checks::analyze(r.snapshot);
    let t = dashboard(&r).tile(TileKey::Deadlocks).cloned().unwrap();
    assert_eq!(t.status, TileStatus::Info, "two deadlocks a day are historical context under the check's rule");
    assert_eq!(
        t.takeaway,
        "PostgreSQL counted 12 deadlocks in the 6.0 days since its statistics were reset; it keeps no details to explain them."
    );
    r.snapshot.deadlocks.as_mut().unwrap().counted = Some(72.0);
    r = culprit_checks::analyze(r.snapshot);
    assert_eq!(
        tile(&dashboard(&r), TileKey::Deadlocks).status,
        TileStatus::Watch,
        "a substantial measured historical rate retains its warning"
    );
}

#[test]
fn a_live_read_puts_blocking_first() {
    let mut d = dashboard(&report("pg-demo"));
    assert_eq!(tile(&d, TileKey::Now).status, TileStatus::Info, "not read yet");

    let mut head = session("57", "idle in transaction");
    head.transaction_ms = Some(720_000.0);
    let waiters: Vec<Session> = (0..6)
        .map(|i| {
            let mut s = session(&format!("{}", 60 + i), "active");
            s.blocked_by = Some("57".into());
            s.wait_ms = Some(40_000.0);
            s.elapsed_ms = Some(40_000.0);
            s
        })
        .collect();
    with_now(&mut d, &live([vec![head], waiters].concat(), Engine::Postgres));
    let now = tile(&d, TileKey::Now);
    assert_eq!(now.status, TileStatus::Bad);
    assert_eq!(d.attention.first(), Some(&TileKey::Now));
    assert_eq!(d.headline, now.takeaway);
    assert_eq!(now.metric("blocked").and_then(|m| m.value), Some(6.0));

    with_now(&mut d, &live(vec![session("1", "idle")], Engine::Postgres));
    assert_eq!(tile(&d, TileKey::Now).takeaway, "Nothing is running right now.");
    without_now(&mut d, "The server closed the connection");
    let now = tile(&d, TileKey::Now);
    assert_eq!(now.status, TileStatus::Unavailable);
    assert_eq!(now.takeaway, "Couldn't read sessions: the server closed the connection.");
}

#[test]
fn brief_locks_and_recent_idle_transactions_do_not_raise_live_alarms() {
    let mut d = dashboard(&report("pg-demo"));
    let head = session("1", "idle");
    let mut waiting = session("2", "active");
    waiting.blocked_by = Some("1".into());
    waiting.wait_ms = Some(10.0);
    with_now(&mut d, &live(vec![head, waiting], Engine::Postgres));
    assert_eq!(tile(&d, TileKey::Now).status, TileStatus::Info);
    let mut idle = session("1", "idle in transaction");
    idle.transaction_ms = Some(90_000.0);
    with_now(&mut d, &live(vec![idle.clone()], Engine::Postgres));
    assert_eq!(tile(&d, TileKey::Now).status, TileStatus::Good);
    idle.transaction_ms = Some(720_000.0);
    with_now(&mut d, &live(vec![idle], Engine::Postgres));
    assert_eq!(tile(&d, TileKey::Now).status, TileStatus::Watch);
    assert!(tile(&d, TileKey::Now).takeaway.contains("transaction opened"));
}

#[test]
fn mariadb_is_called_mariadb() {
    let mut r = report("mysql-demo");
    r.snapshot.server.product = "MariaDB 11.8.9".into();
    assert_eq!(family(&r.snapshot.server), "MariaDB");
    r.snapshot.workload = None;
    r.snapshot.waits.clear();
    r.snapshot.server.missing.push(culprit_core::MissingCapability {
        capability: culprit_core::Capability::PerformanceSchema,
        reason: "performance_schema is OFF on this server.".into(),
        fix: Some("Set performance_schema=ON in the server configuration and restart.".into()),
    });
    let d = dashboard(&r);
    let waits = tile(&d, TileKey::Waits);
    assert_eq!(waits.status, TileStatus::Unavailable);
    assert!(waits.takeaway.starts_with("MariaDB isn't counting waits"), "{}", waits.takeaway);
    let stats = tile(&d, TileKey::Statistics);
    assert_eq!(stats.status, TileStatus::Bad);
    assert!(stats.takeaway.contains("performance_schema is OFF"), "{}", stats.takeaway);
}

#[test]
fn slow_log_lock_time_is_not_called_where_time_goes() {
    // MariaDB without performance_schema: the slow query log keeps only each statement's lock time.
    let mut r = report("mysql-demo");
    r.snapshot.server.product = "MariaDB 10.5.29".into();
    r.snapshot.server.capabilities.retain(|c| *c != culprit_core::Capability::PerformanceSchema);
    let workload = r.snapshot.workload.as_mut().expect("workload");
    workload.source = "slow query log".into();
    workload.windowed = true;
    r.snapshot.waits = vec![culprit_core::WaitStat {
        category: "Lock".into(),
        wait_type: Some("lock time".into()),
        wait_ms: 9.0,
        tasks: None,
    }];
    let d = dashboard(&r);
    let waits = tile(&d, TileKey::Waits);
    assert!(waits.takeaway.starts_with("MariaDB times no waits without performance_schema"), "{}", waits.takeaway);
    assert!(!waits.takeaway.contains("Most time goes to"), "{}", waits.takeaway);
    assert_eq!(waits.status, TileStatus::Info, "9 ms of lock time is a sliver of database time");
}

#[test]
fn storage_connections_replication_and_failures_say_what_they_saw() {
    use culprit_core::signals::{FileIo, FileKind, QueryCosts, Replica, ReplicaRole};
    let mut r = report("pg-demo");
    // Nothing collected: a tile names the engine's reason only when the scan proves it, otherwise what's missing.
    if let Some(p) = r.snapshot.pressure.as_mut() {
        p.files.clear();
        p.replicas.clear();
    }
    let d = dashboard(&r);
    assert_eq!(tile(&d, TileKey::Storage).takeaway, "This scan has no storage timings.");
    assert!(tile(&d, TileKey::Failures).takeaway.contains("doesn't count failed runs"));
    r.snapshot.settings.retain(|s| s.name != "track_io_timing");
    r.snapshot.settings.push(culprit_core::Setting {
        name: "track_io_timing".into(),
        value: "off".into(),
        default_value: Some("off".into()),
        scope: "server".into(),
    });
    let d = dashboard(&r);
    assert!(tile(&d, TileKey::Storage).takeaway.contains("track_io_timing"), "{}", tile(&d, TileKey::Storage).takeaway);

    // A slow WAL, a lagging replica and an inactive slot, as a scan would collect them.
    let p = r.snapshot.pressure.get_or_insert_with(Default::default);
    p.uptime_hours = Some(48.0);
    p.files = vec![
        FileIo {
            name: "relation".into(),
            kind: FileKind::Data,
            database: None,
            reads: 50_000.0,
            read_ms: Some(50_000.0 * 0.4),
            read_bytes: None,
            writes: 0.0,
            write_ms: None,
            write_bytes: None,
            scope: None,
            stats_reset: None,
            write_operation: None,
        },
        FileIo {
            name: "wal".into(),
            kind: FileKind::Log,
            database: None,
            reads: 0.0,
            read_ms: None,
            read_bytes: None,
            writes: 20_000.0,
            write_ms: Some(20_000.0 * 40.0),
            write_bytes: None,
            scope: None,
            stats_reset: None,
            write_operation: None,
        },
    ];
    p.replicas = vec![Replica {
        name: "standby-1".into(),
        role: ReplicaRole::Replica,
        state: Some("streaming".into()),
        sync: Some("async".into()),
        lag_seconds: Some(90.0),
        lag_bytes: None,
        unsent_bytes: None,
        error: None,
        ..Replica::new("", ReplicaRole::Replica)
    }];
    r = culprit_checks::analyze(r.snapshot);
    let d = dashboard(&r);
    let storage = tile(&d, TileKey::Storage);
    assert_eq!(
        storage.items.first().map(|i| i.label.as_str()),
        Some("WAL (whole server)"),
        "the slowest file first, with its scope"
    );
    assert_eq!(storage.items[0].status, Some(TileStatus::Info), "a shared historical average is context");
    assert!(storage.metric("log-write-ms").is_some());
    let replication = tile(&d, TileKey::Replication);
    assert_eq!(replication.items[0].status, Some(TileStatus::Watch), "90 s behind");
    assert!(replication.items[0].value.as_deref().is_some_and(|v| v.contains("behind")));

    // Engines that count unfinished runs: the tile ranks statements by how often they don't finish.
    let mut m = report("mysql-demo");
    let w = m.snapshot.workload.as_mut().expect("workload");
    // Only the planted counts: the demo report carries its own.
    for q in w.queries.iter_mut() {
        q.costs = None;
    }
    let q = &mut w.queries[0];
    q.executions = 100.0;
    q.costs = Some(QueryCosts { failed: Some(30.0), ..Default::default() });
    m = culprit_checks::analyze(m.snapshot);
    let d = dashboard(&m);
    let failures = tile(&d, TileKey::Failures);
    assert_eq!(failures.items.len(), 1);
    assert_eq!(failures.items[0].value.as_deref(), Some("30% of runs"));
    assert_eq!(failures.items[0].status, Some(TileStatus::Info), "a historical ratio alone cannot become red");
    m.snapshot.workload.as_mut().unwrap().queries[0].costs.as_mut().unwrap().top_error =
        Some("1205 lock wait timeout".into());
    m = culprit_checks::analyze(m.snapshot);
    assert!(
        matches!(tile(&dashboard(&m), TileKey::Failures).items[0].status, Some(TileStatus::Watch | TileStatus::Bad)),
        "recent failures retain the assessed warning"
    );
    for t in &d.tiles {
        assert!(t.takeaway.ends_with('.'), "{:?}: {}", t.key, t.takeaway);
    }
}

#[test]
fn the_top_share_is_the_same_number_as_the_findings_summary() {
    for name in ["mssql-demo", "pg-demo", "mysql-demo", "mariadb-demo"] {
        let r = report(name);
        let d = dashboard(&r);
        let takeaway = &tile(&d, TileKey::Findings).takeaway;
        // Recompute the summary from the same findings: both must name the same share.
        let summary = culprit_checks::summarize(&r.snapshot, &r.findings);
        if let Some(share) = summary.top_share.filter(|s| *s >= 0.05) {
            let text = culprit_core::format::percent(share);
            assert!(takeaway.contains(&text), "{name}: dashboard '{takeaway}' vs summary {text}");
            assert!(summary.headline.contains(&text), "{name}: {}", summary.headline);
        }
    }
}

#[test]
fn healthy_signals_are_shown_before_ungraded_ones() {
    // As on an Azure SQL database: more ungraded counters than the tile has squares, and a few healthy signals.
    let mut r = report("mssql-demo");
    let p = r.snapshot.pressure.get_or_insert_with(Default::default);
    p.signals.clear();
    for n in 0..9 {
        p.signals.push(culprit_core::waits::Signal::new(
            &format!("counter-{n}"),
            &format!("Counter {n}"),
            1.0,
            culprit_core::waits::SignalUnit::Count,
            culprit_core::waits::SignalScope::SinceStartup,
        ));
    }
    for n in 0..3 {
        let mut s = culprit_core::waits::Signal::new(
            &format!("healthy-{n}"),
            &format!("Healthy {n}"),
            1.0,
            culprit_core::waits::SignalUnit::Count,
            culprit_core::waits::SignalScope::SinceStartup,
        );
        s.status = culprit_core::waits::SignalStatus::Good;
        p.signals.push(s);
    }
    let d = dashboard(&r);
    let health = tile(&d, TileKey::Health);
    let shown: Vec<&str> = health
        .items
        .iter()
        .filter(|i| i.id.starts_with("healthy-") || i.id.starts_with("counter-"))
        .map(|i| i.id.as_str())
        .collect();
    assert_eq!(&shown[..3], &["healthy-0", "healthy-1", "healthy-2"], "{shown:?}");
    assert_eq!(health.status, TileStatus::Good);
}

#[test]
fn the_headline_counts_statements_that_stopped_not_findings() {
    // Two findings on one statement that stopped running are one statement: "1 more statement … it".
    let report = report("pg-demo");
    let mut findings = report.findings.clone();
    let id = findings[0].queries.first().expect("a statement").id.clone();
    let mut second = findings[0].clone();
    second.id = format!("{}:second", second.id);
    findings.push(second);
    for f in findings.iter_mut().filter(|f| f.queries.first().is_some_and(|r| r.id == id)) {
        f.inactive = true;
    }
    let headline = culprit_checks::summarize(&report.snapshot, &findings).headline;
    assert!(headline.ends_with(" 1 more statement stopped running: a deploy may have changed it."), "{headline}");

    // A finding that names no statement (an index the optimizer stopped asking for) is counted on its own.
    let mut unnamed = findings[0].clone();
    (unnamed.id, unnamed.queries) = ("missing-index:unnamed".into(), Vec::new());
    findings.push(unnamed);
    let headline = culprit_checks::summarize(&report.snapshot, &findings).headline;
    assert!(
        headline.ends_with(
            " 1 more statement stopped running, and 1 more finding rests on statements that did: a deploy may have \
             changed them."
        ),
        "{headline}"
    );
}

#[test]
fn postgres_storage_waits_and_plan_counts_keep_their_own_meaning() {
    use culprit_core::{FileIo, FileKind, FileScope, FileWriteOperation};
    let mut r = report("pg-demo");
    r.snapshot.server.version = "15.17".into();
    let f = |name: &str, kind, reset: &str| FileIo {
        name: name.into(),
        kind,
        database: None,
        reads: 2000.0,
        read_ms: Some(1000.0),
        read_bytes: None,
        writes: 2000.0,
        write_ms: Some(6000.0),
        write_bytes: None,
        scope: None,
        stats_reset: Some(reset.into()),
        write_operation: None,
    };
    r.snapshot.pressure.get_or_insert_with(Default::default).files = vec![
        f("Tables and indexes", FileKind::Data, "2026-10-19T00:00:00Z"),
        f("WAL", FileKind::Log, "2026-10-18T00:00:00Z"),
    ];
    let r = culprit_checks::analyze(r.snapshot);
    let files = &r.snapshot.pressure.as_ref().unwrap().files;
    assert_eq!(files[0].scope, Some(FileScope::Database));
    assert_eq!(files[1].scope, Some(FileScope::Server));
    assert_eq!(files[1].write_operation, Some(FileWriteOperation::Flush));
    let d = dashboard(&r);
    let storage = tile(&d, TileKey::Storage);
    let coverage = storage.detail.as_ref().unwrap();
    assert!(coverage.contains("this database") && coverage.contains("whole server"), "{coverage}");
    assert!(coverage.contains("2026-10-19") && coverage.contains("2026-10-18"), "{coverage}");
    assert!(storage.items.iter().any(|i| i.value.as_deref().is_some_and(|v| v.contains("per flush"))));
    let waits = tile(&d, TileKey::Waits);
    assert!(waits.metrics.iter().any(|m| m.label == "Statement execution time"));
    assert!(waits.detail.as_deref().unwrap().contains("not a measurement of time spent waiting"));
    assert!(tile(&d, TileKey::Plans).detail.as_deref().unwrap().contains("historical or once-only"));
    assert!(tile(&d, TileKey::Statistics).metrics.iter().any(|m| m.label == "All queries with plans"));
    // From PostgreSQL 16, pg_stat_io's data timing is also server-wide.
    let mut s = r.snapshot;
    s.server.version = "16.5".into();
    s.pressure.as_mut().unwrap().files[0].scope = None;
    assert_eq!(culprit_checks::analyze(s).snapshot.pressure.unwrap().files[0].scope, Some(FileScope::Server));
}

#[test]
fn saved_postgres_io_that_exceeds_execution_time_is_not_an_exclusive_share() {
    use culprit_core::{Capability, WaitStat};
    let mut s = report("pg-demo").snapshot;
    let w = s.workload.as_mut().unwrap();
    w.total_time_ms = 100.0;
    w.queries[0].waits = vec![WaitStat {
        category: "IO".into(),
        wait_type: Some("data file reads and writes".into()),
        wait_ms: 150.0,
        tasks: None,
    }];
    s.waits = w.queries[0].waits.clone();
    let r = culprit_checks::analyze(s);
    assert!(r.snapshot.waits.is_empty());
    assert!(!r.snapshot.server.capabilities.contains(&Capability::WaitStats));
    assert_eq!(r.snapshot.workload.as_ref().unwrap().queries[0].waits[0].wait_ms, 150.0);
    assert!(
        r.snapshot
            .notes
            .iter()
            .any(|n| n.collector == "waits" && n.message.contains("exceeds statement execution time"))
    );
}
