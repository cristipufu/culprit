//! Signals that used to raise false flags, through the whole pipeline: each test is one claim a finding made that the
//! engine's numbers didn't support, and what Culprit says now.

use culprit_checks::analyze;
use culprit_core::{
    ColumnInfo, ConstraintIssue, ConstraintKind, FileIo, FileKind, Finding, IndexContention, Pressure, QueryCosts,
    Replica, ReplicaRole, Report, Setting, Severity, Signal, SignalScope, SignalUnit, Snapshot, TableStorage, WaitStat,
};

const MB: f64 = 1024.0 * 1024.0;
const GB: f64 = 1024.0 * MB;

fn fixture(name: &str) -> Snapshot {
    let path = format!("{}/../../fixtures/{name}.json", env!("CARGO_MANIFEST_DIR"));
    serde_json::from_str(&std::fs::read_to_string(path).expect("fixture")).expect("snapshot")
}

/// The snapshot inside a demo report: the scans that carry signals (replicas, files, storage).
fn demo(name: &str) -> Snapshot {
    let path = format!("{}/../../fixtures/{name}.report.json", env!("CARGO_MANIFEST_DIR"));
    let report: Report = serde_json::from_str(&std::fs::read_to_string(path).expect("fixture")).expect("report");
    report.snapshot
}

fn of<'a>(report: &'a Report, check: &str) -> Vec<&'a Finding> {
    report.findings.iter().filter(|f| f.check == check).collect()
}

fn pressure(s: &mut Snapshot) -> &mut Pressure {
    s.pressure.get_or_insert_with(|| Pressure { uptime_hours: Some(72.0), ..Pressure::default() })
}

fn set(s: &mut Snapshot, name: &str, value: &str) {
    s.settings.retain(|x| x.name != name);
    s.settings.push(Setting { name: name.into(), value: value.into(), default_value: None, scope: "server".into() });
}

fn replicas(engine: &str, list: Vec<Replica>) -> Report {
    let mut s = fixture(engine);
    pressure(&mut s).replicas = list;
    analyze(s)
}

fn replica(name: &str, role: ReplicaRole, state: &str) -> Replica {
    Replica { state: Some(state.into()), ..Replica::new(name, role) }
}

/// The heaviest application statement of a demo, to plant numbers on.
fn heaviest(s: &mut Snapshot) -> &mut culprit_core::QueryStat {
    let w = s.workload.as_mut().expect("workload");
    w.queries.sort_by(|a, b| b.total_time_ms.total_cmp(&a.total_time_ms));
    w.queries.iter_mut().find(|q| !q.text.to_ascii_lowercase().contains("pg_catalog")).expect("a query")
}

// ── 1. replica lag ─────────────────────────────────────────────────────────────────────────────────

// On a quiet primary a standby's last replayed commit gets older by the minute with nothing to replay. Scans before
// the collector fixed this reported that age as lag: with nothing received left to replay, it's caught up.
#[test]
fn a_caught_up_standby_of_a_quiet_primary_is_not_behind() {
    let idle = Replica {
        lag_seconds: Some(3_600.0),
        lag_bytes: Some(0.0),
        ..replica("this server", ReplicaRole::ThisServer, "streaming")
    };
    assert!(of(&replicas("postgres-demo", vec![idle]), "replica-lag").is_empty());
    let behind = Replica {
        lag_seconds: Some(900.0),
        lag_bytes: Some(80.0 * MB),
        ..replica("this server", ReplicaRole::ThisServer, "streaming")
    };
    let r = replicas("postgres-demo", vec![behind]);
    assert_eq!(of(&r, "replica-lag")[0].title, "This server is 15 min behind its primary");
}

// A log-shipping standby (restore_command, no primary_conninfo) has no WAL receiver by design: not "stopped".
#[test]
fn a_standby_restoring_from_an_archive_is_not_stopped() {
    let shipping = replica("this server", ReplicaRole::ThisServer, "restoring from archive");
    assert!(of(&replicas("postgres-demo", vec![shipping]), "replica-lag").is_empty());
    let r = replicas("postgres-demo", vec![replica("this server", ReplicaRole::ThisServer, "not receiving")]);
    let f = of(&r, "replica-lag");
    assert_eq!(f[0].severity, Severity::High);
    assert!(f[0].summary.contains("primary_conninfo is set"), "{}", f[0].summary);
}

// ── 2. delayed replicas ────────────────────────────────────────────────────────────────────────────

// SOURCE_DELAY / recovery_min_apply_delay keep a replica behind on purpose: the collector takes the delay out of the
// lag, and the bytes it holds back aren't lag either.
#[test]
fn a_replica_delayed_by_design_is_judged_on_the_lag_beyond_its_delay() {
    let delayed = |lag: f64| Replica {
        lag_seconds: Some(lag),
        delay_seconds: Some(3_600.0),
        ..replica("from 10.0.0.5", ReplicaRole::ThisServer, "replicating")
    };
    assert!(of(&replicas("mysql-demo", vec![delayed(20.0)]), "replica-lag").is_empty());
    let r = replicas("mysql-demo", vec![delayed(700.0)]);
    let f = of(&r, "replica-lag")[0];
    assert_eq!((f.title.as_str(), f.severity), ("This server is 12 min behind its primary", Severity::High));
    assert!(f.summary.contains("1 h late on purpose (SOURCE_DELAY)"), "{}", f.summary);
    let pg = Replica { lag_bytes: Some(5.0 * GB), ..delayed(10.0) };
    assert!(of(&replicas("postgres-demo", vec![pg]), "replica-lag").is_empty(), "WAL held back by the delay");
}

// ── 3. MySQL channels stopped without an error ─────────────────────────────────────────────────────

#[test]
fn a_channel_stopped_by_hand_is_not_a_replication_failure() {
    let promoted =
        replica("from 10.0.0.5", ReplicaRole::ThisServer, "configured, not running (no error; accepts writes)");
    let r = replicas("mysql-demo", vec![promoted]);
    let f = of(&r, "replica-lag")[0];
    assert_eq!(f.severity, Severity::Low);
    assert_eq!(f.title, "This server is still set up as a replica, but replication isn't running");
    assert!(f.fix.as_ref().unwrap().summary.contains("RESET REPLICA ALL"));
    let r =
        replicas("mysql-demo", vec![replica("from 10.0.0.5", ReplicaRole::ThisServer, "stopped by hand (no error)")]);
    assert_eq!(of(&r, "replica-lag")[0].severity, Severity::Medium);
    // Group Replication's idle recovery channel, in a scan from before the collector left it out.
    let gr = replica("group_replication_recovery (from )", ReplicaRole::ThisServer, "stopped");
    assert!(of(&replicas("mysql-demo", vec![gr]), "replica-lag").is_empty());
}

// ── 4. slots and logical consumers ─────────────────────────────────────────────────────────────────

fn slot(name: &str, kind: &str, kept: f64) -> Replica {
    Replica {
        sync: Some(kind.into()),
        logical: Some(kind == "logical"),
        lag_bytes: Some(kept),
        wal_status: Some("extended".into()),
        ..replica(name, ReplicaRole::Slot, "inactive")
    }
}

// A CDC slot (Fivetran, Debezium) is inactive between syncs: dropping it forces a full resync, and with
// max_slot_wal_keep_size set it can't fill the disk.
#[test]
fn an_inactive_logical_slot_belongs_to_a_consumer_and_a_capped_one_cant_fill_the_disk() {
    let r = replicas("postgres-demo", vec![slot("fivetran_slot", "logical", 12.0 * GB)]);
    let f = of(&r, "replica-lag")[0];
    assert_eq!(f.title, "The logical slot fivetran_slot keeps 12.0 GB of WAL while its consumer is away");
    assert_eq!(f.severity, Severity::High, "nothing caps it");
    assert!(f.summary.contains("max_slot_wal_keep_size is -1") && f.summary.contains("copy everything again"));
    let sql = f.fix.as_ref().unwrap().sql.clone().unwrap();
    assert!(sql.contains("-- SELECT pg_drop_replication_slot('fivetran_slot');"), "{sql}");
    assert!(sql.contains("ALTER SYSTEM SET max_slot_wal_keep_size = '50GB';"), "{sql}");

    let capped = Replica { safe_wal_bytes: Some(30.0 * GB), ..slot("fivetran_slot", "logical", 12.0 * GB) };
    let r = replicas("postgres-demo", vec![capped]);
    let f = of(&r, "replica-lag")[0];
    assert_eq!(f.severity, Severity::Low);
    assert!(f.summary.contains("the disk is safe") && !f.summary.contains("until the disk fills"), "{}", f.summary);
    assert!(!f.fix.as_ref().unwrap().sql.clone().unwrap().contains("max_slot_wal_keep_size"), "a cap is never lowered");

    let lost = Replica { wal_status: Some("lost".into()), ..slot("old", "physical", 2.0 * GB) };
    assert!(of(&replicas("postgres-demo", vec![lost]), "replica-lag").is_empty());
}

// A logical walsender is a consumer of decoded changes, not a copy to fail over to.
#[test]
fn a_lagging_logical_consumer_is_worded_as_one() {
    let consumer = Replica {
        lag_seconds: Some(120.0),
        logical: Some(true),
        ..replica("debezium", ReplicaRole::Replica, "streaming")
    };
    let r = replicas("postgres-demo", vec![consumer]);
    let f = of(&r, "replica-lag")[0];
    assert_eq!(f.title, "The logical consumer debezium is 2 min behind");
    assert!(!f.summary.contains("failing over"), "{}", f.summary);
}

// ── 6. counters whose age is unknown ───────────────────────────────────────────────────────────────

fn updated(s: &mut Snapshot, table: &str, updates: f64, counter_hours: Option<f64>) {
    let t = s.tables.iter_mut().find(|t| t.qualified() == table).expect("table");
    t.updates = Some(updates);
    t.storage =
        Some(TableStorage { hot_updates: Some(0.0), fillfactor: Some(100.0), counter_hours, ..Default::default() });
}

// PostgreSQL 15+ leaves stats_reset empty until the first reset and the counters outlive restarts: no rate then, and
// a backfill keeps the HOT share low long after it ran, so only a measured rate raises it.
#[test]
fn hot_updates_rise_only_on_a_measured_rate() {
    let mut s = fixture("postgres-demo");
    updated(&mut s, "public.orders", 5_000_000.0, None);
    let r = analyze(s.clone());
    let f = of(&r, "hot-updates")[0];
    assert_eq!(f.severity, Severity::Low);
    assert!(f.caveats.iter().any(|c| c.contains("isn't known")), "{:?}", f.caveats);
    updated(&mut s, "public.orders", 5_000_000.0, Some(48.0));
    assert_eq!(of(&analyze(s), "hot-updates")[0].severity, Severity::Medium);
}

// ── 7. connection churn ────────────────────────────────────────────────────────────────────────────

fn churn(engine: &str, rate: f64, setup: impl FnOnce(&mut Snapshot)) -> Report {
    let mut s = fixture(engine);
    pressure(&mut s).signals =
        vec![Signal::new("new-connections", "New connections", rate, SignalUnit::PerSecond, SignalScope::SinceStartup)];
    setup(&mut s);
    analyze(s)
}

#[test]
fn server_wide_connection_counts_say_the_server_and_a_pools_are_not_graded() {
    let r = churn("mssql-demo", 40.0, |_| {});
    assert_eq!(of(&r, "connection-churn")[0].title, "The whole server recorded 40 new connections a second on average");
    // The instance's count is shared: however high, it isn't graded on this database.
    let r = churn("mssql-demo", 400.0, |_| {});
    assert_eq!(of(&r, "connection-churn")[0].severity, Severity::Low);
    assert!(of(&r, "connection-churn")[0].caveats.iter().any(|c| c.contains("whole server")));
    // Azure SQL Database: the counter is the server process's, shared by the elastic pool.
    let r = churn("mssql-demo", 400.0, |s| s.server.cloud = Some("Azure SQL Database".into()));
    assert!(of(&r, "connection-churn").is_empty());
    // MySQL: a thread cache already this big isn't made smaller; on RDS the platform's control, not SQL.
    let r = churn("mysql-demo", 40.0, |s| set(s, "thread_cache_size", "128"));
    let fix = of(&r, "connection-churn")[0].fix.clone().unwrap();
    assert!(fix.sql.is_none() && fix.summary.contains("already keeps 128 threads"), "{}", fix.summary);
    let r = churn("mysql-demo", 40.0, |s| s.server.cloud = Some("Amazon RDS".into()));
    let fix = of(&r, "connection-churn")[0].fix.clone().unwrap();
    assert!(fix.sql.is_none() && fix.summary.contains("DB parameter group"), "{}", fix.summary);
}

// ── 8. one cause, one finding ──────────────────────────────────────────────────────────────────────

// archive.wide: dead rows the bloat estimate counted as empty space, and the update whose WAL its non-HOT rewrites
// explain. Four findings were one table's two problems.
#[test]
fn the_demos_wide_table_is_two_findings_not_four() {
    let r = analyze(demo("pg-demo"));
    let on_wide: Vec<&str> = r
        .findings
        .iter()
        .filter(|f| f.subject.name == "archive.wide" || f.title.contains("of wide"))
        .map(|f| f.check.as_str())
        .collect();
    assert!(!on_wide.contains(&"table-bloat") && !on_wide.contains(&"log-volume"), "{on_wide:?}");
    let dead = of(&r, "dead-rows").into_iter().find(|f| f.subject.name == "archive.wide").expect("dead rows");
    assert!(dead.caveats.iter().any(|c| c.contains("as empty space: these dead rows")), "{:?}", dead.caveats);
    let hot = of(&r, "hot-updates")[0];
    assert!(hot.summary.contains("wrote 174 MB of WAL for them"), "{}", hot.summary);
    assert_eq!(hot.queries.len(), 1);
    assert_eq!(hot.severity, Severity::Low, "the update ran once");
}

// The bloat estimate counts dead rows VACUUM hasn't cleared as empty space: when they are the space, VACUUM is the fix,
// not a rewrite, and with a dead-rows finding the two are one.
#[test]
fn bloat_made_of_dead_rows_is_vacuumed_not_rewritten() {
    let bloated = |rows: f64, dead: f64| {
        let mut s = fixture("postgres-demo");
        let t = s.tables.iter_mut().find(|t| t.name == "orders").unwrap();
        (t.data_bytes, t.rows, t.dead_rows) = (Some(4.0 * GB), Some(rows), Some(dead));
        t.storage = Some(TableStorage { bloat_bytes: Some(2.4 * GB), ..Default::default() });
        analyze(s)
    };
    // Too few dead rows for dead-rows to speak (under 10K), but 90% of the versions: the space is theirs.
    let r = bloated(1_000.0, 9_000.0);
    let fix = of(&r, "table-bloat")[0].fix.clone().unwrap();
    assert_eq!(fix.sql.as_deref(), Some("VACUUM (ANALYZE) public.orders;"));
    // With dead-rows: one finding, with what the estimate saw.
    let r = bloated(1_000_000.0, 1_500_000.0);
    assert!(of(&r, "table-bloat").is_empty());
    let dead = of(&r, "dead-rows").into_iter().find(|f| f.subject.name == "public.orders").expect("dead rows");
    assert!(dead.caveats.iter().any(|c| c.contains("2.4 GB of orders (60%) as empty space")), "{:?}", dead.caveats);
    // Few dead rows: the space is something else, and a rewrite gives it back.
    let r = bloated(1_000_000.0, 1_000.0);
    assert!(of(&r, "table-bloat")[0].fix.clone().unwrap().sql.unwrap().contains("pgstattuple"));
}

// Two statements waiting on orders' locks and orders' own lock waits are one blocking chain.
#[test]
fn a_blocking_chain_is_one_finding() {
    let r = analyze(demo("mssql-demo"));
    assert!(of(&r, "lock-time").is_empty(), "{:?}", of(&r, "lock-time").iter().map(|f| &f.id).collect::<Vec<_>>());
    let hot = of(&r, "hot-rows");
    assert_eq!(hot.len(), 1);
    assert_eq!(hot[0].id, "hot-rows:shop.orders");
    let notes: Vec<_> = hot[0].queries.iter().filter_map(|q| q.note.as_deref()).collect();
    assert!(notes.iter().any(|n| n.starts_with("Waited 2 min on locks, nearly all of its time")), "{notes:?}");
    assert!(hot[0].summary.contains("waited 2 min for these locks"), "{}", hot[0].summary);
    let note = |id: &str| hot[0].queries.iter().find(|q| q.id == id).and_then(|q| q.note.as_deref()).unwrap();
    assert!(note("138").starts_with("Waited 2 min on locks"), "{}", note("138"));
    assert!(note("143").starts_with("Waited 1 min on locks"), "{}", note("143"));
    assert!(hot[0].summary.contains("A read from orders waited 1 min for these locks"), "{}", hot[0].summary);
}

// Lock waits on two indexes of one table are one table's rows fought over.
#[test]
fn hot_rows_are_one_finding_per_table() {
    let mut s = fixture("mssql-demo");
    s.server.started_at = Some("2026-10-11T00:13:03Z".into());
    let waits = |ms: f64| IndexContention { row_lock_waits: 10.0, row_lock_wait_ms: ms, ..Default::default() };
    let mut n = 0;
    for i in s.indexes.iter_mut() {
        i.contention = None;
        if i.table == "orders" && n < 2 {
            i.contention = Some(waits(if n == 0 { 400_000.0 } else { 300_000.0 }));
            n += 1;
        }
    }
    let r = analyze(s);
    let hot = of(&r, "hot-rows");
    assert_eq!(hot.len(), 1, "{:?}", hot.iter().map(|f| &f.id).collect::<Vec<_>>());
    assert!(
        hot[0].title.contains("in orders") && hot[0].summary.contains("waited on pk_orders and"),
        "{}",
        hot[0].summary
    );
    assert!(
        hot[0].summary.contains("20 times for locks on the rows and pages of orders, 11 min 40 s in all"),
        "{}",
        hot[0].summary
    );
    assert_eq!(hot[0].severity, Severity::High, "about 23 minutes a day over 12 hours of counting");
}

// Inserting large values logs them in full: log-volume's advice (batch, drop indexes) can't shrink that.
#[test]
fn the_log_of_large_values_belongs_to_large_rows() {
    let mut s = fixture("mssql-demo");
    let t = s.tables.iter_mut().find(|t| t.name == "support_tickets").expect("demo table");
    (t.rows, t.in_row_bytes, t.lob_bytes, t.overflow_bytes, t.data_bytes) =
        (Some(9_728.0), Some(31.0 * MB), Some(2_900.0 * MB), Some(0.0), Some(2_931.0 * MB));
    let (schema, table) = (t.schema.clone(), t.name.clone());
    for (ordinal, name) in ["body", "attachments"].into_iter().enumerate() {
        s.columns.push(ColumnInfo {
            schema: schema.clone(),
            table: table.clone(),
            name: name.into(),
            data_type: "varbinary(max)".into(),
            nullable: true,
            default: None,
            generated: false,
            ordinal: 200 + ordinal as u32,
        });
    }
    let q = heaviest(&mut s);
    q.text = "INSERT INTO shop.support_tickets (body, attachments) VALUES (@P1, @P2)".into();
    q.plan = None;
    q.executions = 2_000.0;
    q.rows = Some(2_000.0);
    q.costs =
        Some(QueryCosts { log_bytes: Some(2_000.0 * 300.0 * 1024.0), log_share: Some(0.9), ..Default::default() });
    let id = q.id.clone();
    let r = analyze(s);
    assert!(of(&r, "log-volume").iter().all(|f| f.queries[0].id != id), "folded into large-rows");
    let large = of(&r, "large-rows")[0];
    assert!(large.queries.iter().any(|q| q.id == id));
    assert!(large.summary.contains("the large values are logged in full"), "{}", large.summary);
}

// ── 9. NOT VALID check constraints ─────────────────────────────────────────────────────────────────

#[test]
fn a_not_valid_check_is_about_the_data_unless_the_planner_reads_it() {
    let check = |child| ConstraintIssue {
        schema: "public".into(),
        table: "orders".into(),
        name: "orders_total_check".into(),
        kind: ConstraintKind::Check,
        disabled: false,
        inheritance_child: child,
    };
    let mut s = fixture("postgres-demo");
    s.constraints = vec![check(false)];
    let r = analyze(s.clone());
    let f = of(&r, "untrusted-constraint")[0];
    assert!(f.summary.contains("often deliberate") && f.summary.contains("SET NOT NULL"), "{}", f.summary);
    assert!(!f.summary.contains("can't use it to skip"), "{}", f.summary);
    assert_eq!(f.severity, Severity::Low);
    s.constraints = vec![check(true)];
    assert!(of(&analyze(s.clone()), "untrusted-constraint")[0].summary.contains("inheritance child"));
    s.constraints = vec![check(false)];
    set(&mut s, "constraint_exclusion", "on");
    assert!(of(&analyze(s), "untrusted-constraint")[0].summary.contains("can't use it to skip"));
}

// ── 11. storage latency ────────────────────────────────────────────────────────────────────────────

fn file(name: &str, db: &str, kind: FileKind, reads: (f64, f64, f64), writes: (f64, f64)) -> FileIo {
    FileIo {
        name: name.into(),
        kind,
        database: Some(db.into()),
        reads: reads.0,
        read_ms: Some(reads.1),
        read_bytes: Some(reads.2),
        writes: writes.0,
        write_ms: Some(writes.1),
        write_bytes: None,
    }
}

#[test]
fn storage_latency_is_weighted_per_kind_and_tempdbs_log_holds_no_commit() {
    let mut s = fixture("mssql-demo");
    pressure(&mut s).files = vec![
        // tempdb's log is slower, but SQL Server doesn't harden it at commit: it mustn't hide the user log.
        file("templog", "tempdb", FileKind::Log, (0.0, 0.0, 0.0), (50_000.0, 2_000_000.0)),
        file("shop_log", "shop", FileKind::Log, (0.0, 0.0, 0.0), (50_000.0, 600_000.0)),
        // A busy fast data file and a quiet one a backup read in 1 MB chunks: 5 ms weighted, not 300 ms.
        file("shop", "shop", FileKind::Data, (100_000.0, 200_000.0, 100_000.0 * 8_192.0), (0.0, 0.0)),
        file("shop_archive", "shop", FileKind::Data, (2_000.0, 600_000.0, 2_000.0 * MB), (0.0, 0.0)),
    ];
    let r = analyze(s.clone());
    let titles: Vec<&str> = of(&r, "slow-storage").iter().map(|f| f.title.as_str()).collect();
    assert_eq!(titles, vec!["Recorded log writes to shop's log averaged 12 ms"]);
    // Large reads on their own: bulk, Low, and said.
    pressure(&mut s).files =
        vec![file("shop_archive", "shop", FileKind::Data, (2_000.0, 600_000.0, 2_000.0 * MB), (0.0, 0.0))];
    let r = analyze(s.clone());
    let f = of(&r, "slow-storage")[0];
    assert_eq!(f.severity, Severity::Low);
    assert!(f.summary.contains("backups, CHECKDB, read-ahead"), "{}", f.summary);
    // Managed Instance General Purpose: file size sets the IOPS.
    s.server.cloud = Some("Azure SQL Managed Instance".into());
    assert!(of(&analyze(s), "slow-storage")[0].fix.as_ref().unwrap().summary.contains("come from the file's size"));
}

// ── 12. large rows ─────────────────────────────────────────────────────────────────────────────────

fn tickets(engine: &str) -> Snapshot {
    let mut s = fixture(engine);
    let name = if engine == "postgres-demo" { "events" } else { "support_tickets" };
    let t = s.tables.iter_mut().find(|t| t.name == name).expect("demo table");
    (t.rows, t.in_row_bytes, t.lob_bytes, t.overflow_bytes, t.data_bytes) =
        (Some(9_728.0), Some(31.0 * MB), Some(2_900.0 * MB), Some(0.0), Some(2_931.0 * MB));
    s
}

#[test]
fn large_rows_need_live_large_rows() {
    assert_eq!(of(&analyze(tickets("postgres-demo")), "large-rows").len(), 1);
    // A statistics reset zeroed the live-row count; the planner still counts a million rows: 3 KB a row.
    let mut s = tickets("postgres-demo");
    let t = s.tables.iter_mut().find(|t| t.name == "events").unwrap();
    t.storage = Some(TableStorage { planner_rows: Some(1_000_000.0), ..Default::default() });
    assert!(of(&analyze(s), "large-rows").is_empty());
    // A queue whose deleted payloads wait for VACUUM: dead versions, not large rows.
    let mut s = tickets("postgres-demo");
    s.tables.iter_mut().find(|t| t.name == "events").unwrap().dead_rows = Some(990_000.0);
    assert!(of(&analyze(s), "large-rows").is_empty());
}

// A count(*) or a filter on an unindexed column reads the rows' own pages, not their large values.
#[test]
fn a_scan_of_the_in_row_pages_is_not_a_whole_row_read() {
    let mut s = tickets("mssql-demo");
    s.workload.as_mut().unwrap().window_hours = 24.0;
    let q = heaviest(&mut s);
    q.text = "SELECT count(*) FROM shop.support_tickets WHERE status = @P1".into();
    q.plan = None;
    q.executions = 5_000.0;
    q.rows = Some(5_000.0);
    // 31 MB in-row is 3,968 pages: a scan reads about that many.
    q.logical_reads = Some(4_000.0 * 5_000.0);
    let r = analyze(s.clone());
    let large = of(&r, "large-rows")[0];
    assert!(large.queries.is_empty());
    // With no statements listed, the verify doesn't point at "the statements below".
    assert!(large.verify.iter().all(|v| !v.contains("statements below")), "{:?}", large.verify);
    // One ticket by id reading 280 pages pulls its large values.
    let q = heaviest(&mut s);
    q.text = "SELECT * FROM shop.support_tickets WHERE id = @P1".into();
    q.logical_reads = Some(280.0 * 5_000.0);
    let r = analyze(s);
    let large = of(&r, "large-rows")[0];
    assert_eq!(large.queries.len(), 1);
    assert!(large.verify[0].contains("measure how much data the application receives"), "{:?}", large.verify);
    assert!(large.verify[0].contains("time per run"), "{:?}", large.verify);
    assert!(large.verify[0].contains("need not change the table's stored size"), "{:?}", large.verify);
}

#[test]
fn large_row_verification_separates_projection_from_storage_changes() {
    let mut s = tickets("mssql-demo");
    let q = heaviest(&mut s);
    q.text = "SELECT * FROM shop.support_tickets WHERE id = @P1".into();
    q.plan = None;
    (q.executions, q.rows, q.logical_reads) = (5_000.0, Some(5_000.0), Some(280.0 * 5_000.0));
    let before = analyze(s.clone());
    let stored = |f: &Finding| {
        f.metrics
            .iter()
            .filter(|m| matches!(m.label.as_str(), "Table data" | "Average row"))
            .map(|m| m.value)
            .collect::<Vec<_>>()
    };
    let before_size = stored(of(&before, "large-rows")[0]);
    // Selecting just the id makes each read cheaper but leaves all stored values in the table.
    let q = heaviest(&mut s);
    q.text = "SELECT id FROM shop.support_tickets WHERE id = @P1".into();
    q.logical_reads = Some(3.0 * 5_000.0);
    let after = analyze(s.clone());
    let large = of(&after, "large-rows")[0];
    assert_eq!(stored(large), before_size);
    assert!(large.verify[0].contains("in-row pages read"), "{:?}", large.verify);
    assert!(large.verify[1].starts_with("If you make stored values smaller"), "{:?}", large.verify);
    assert!(large.verify[1].contains("percentage in large values need not fall"), "{:?}", large.verify);
    // Only changing the stored values can remove this table-size finding.
    let t = s.tables.iter_mut().find(|t| t.name == "support_tickets").unwrap();
    (t.lob_bytes, t.data_bytes) = (Some(16.0 * MB), Some(47.0 * MB));
    assert!(of(&analyze(s), "large-rows").is_empty());

    // PostgreSQL keeps running totals: a rewrite has a new entry, and elapsed time is compared over scans.
    let mut s = tickets("postgres-demo");
    let q = heaviest(&mut s);
    q.text = "SELECT * FROM public.events WHERE id = $1".into();
    q.plan = None;
    (q.executions, q.rows, q.logical_reads) = (5_000.0, Some(5_000.0), Some(280.0 * 5_000.0));
    let r = analyze(s);
    let large = of(&r, "large-rows")[0];
    assert!(large.verify[0].contains("What changed"), "{:?}", large.verify);
    assert!(large.verify[0].contains("A rewritten statement has a new entry"), "{:?}", large.verify);
}

#[test]
fn sequential_key_pacing_is_verified_by_throughput_not_zero_wait_growth() {
    let mut s = demo("mssql-demo");
    let id = "last-page-insert:archive.hot.pk_hot";
    let r = analyze(s.clone());
    let f = r.findings.iter().find(|f| f.id == id).expect("demo's contended insert index");
    assert!(f.verify[0].contains("OPTIMIZE_FOR_SEQUENTIAL_KEY is ON"), "{:?}", f.verify);
    assert!(f.verify[0].contains("completed inserts per second"), "{:?}", f.verify);
    assert!(f.verify[0].contains("comparable writer concurrency"), "{:?}", f.verify);
    assert!(f.verify[1].contains("BTREE_INSERT_FLOW_CONTROL"), "{:?}", f.verify);
    assert!(f.verify[1].contains("cumulative wait totals need not stop growing"), "{:?}", f.verify);
    fn hot(s: &mut Snapshot) -> &mut IndexContention {
        s.indexes.iter_mut().find(|i| i.schema == "archive" && i.name == "pk_hot").unwrap().contention.as_mut().unwrap()
    }
    hot(&mut s).optimize_for_sequential_key = Some(true);
    assert!(analyze(s.clone()).findings.iter().all(|f| f.id != id));
    // Older versions get key/batching advice; a mostly-updated hot page gets row-change verification.
    hot(&mut s).optimize_for_sequential_key = None;
    let r = analyze(s.clone());
    let f = r.findings.iter().find(|f| f.id == id).unwrap();
    assert!(f.verify[0].contains("inserts per second"), "{:?}", f.verify);
    assert!(!f.verify.iter().any(|v| v.contains("BTREE_INSERT_FLOW_CONTROL")), "{:?}", f.verify);
    hot(&mut s).leaf_inserts = Some(1.0);
    let r = analyze(s);
    let f = r.findings.iter().find(|f| f.id == id).unwrap();
    assert!(f.verify[0].contains("row changes per second"), "{:?}", f.verify);
}

#[test]
fn an_explicit_recompile_hint_is_the_planning_time_fix() {
    // The saved demo's query 249: 300 compiles / 300 runs and 6.9 seconds spent compiling.
    let mut s = demo("mssql-demo");
    let id = "planning-time:249";
    // Only an entry whose recorded compiles all fall within the period can support this comparison.
    let c = s.workload.as_mut().unwrap().queries.iter_mut().find(|q| q.id == "249").unwrap().costs.as_mut().unwrap();
    c.compile_history = Some(culprit_core::signals::CompileHistory {
        count: 300.0,
        total_ms: 6900.0,
        within_window: true,
        ..Default::default()
    });
    let r = analyze(s.clone());
    let f = r.findings.iter().find(|f| f.id == id).expect("demo's explicit RECOMPILE");
    assert!(f.summary.contains("hint explicitly asks SQL Server for a new plan"), "{}", f.summary);
    let fix = f.fix.as_ref().unwrap();
    assert!(fix.summary.contains("Test removing OPTION (RECOMPILE)"), "{}", fix.summary);
    assert!(fix.sql.is_none(), "forced parameterization cannot override RECOMPILE");
    assert!(f.verify.iter().any(|v| v.contains("representative parameter values")), "{:?}", f.verify);
    // Hint text in a comment/literal is not evidence of a compile request. Keep the generic path there.
    let q = s.workload.as_mut().unwrap().queries.iter_mut().find(|q| q.id == "249").unwrap();
    q.text = q.text.replace("OPTION (RECOMPILE)", "/* OPTION (RECOMPILE) */ AND N'OPTION (RECOMPILE)' <> N''");
    let r = analyze(s);
    let f = r.findings.iter().find(|f| f.id == id).unwrap();
    assert!(f.fix.as_ref().unwrap().sql.as_ref().unwrap().contains("SET PARAMETERIZATION FORCED"));
}

// ── 13. results that never reach the client ────────────────────────────────────────────────────────

fn big_read(text: &str, executions: f64, window_hours: f64) -> Report {
    let mut s = fixture("mssql-demo");
    s.workload.as_mut().unwrap().window_hours = window_hours;
    let q = heaviest(&mut s);
    q.text = text.into();
    q.plan = None;
    q.executions = executions;
    q.rows = Some(executions * 200_000.0);
    // Runs over the whole window, not the fixture's four hours: a burst would be judged on its own day.
    (q.first_seen, q.last_seen) = (None, None);
    analyze(s)
}

#[test]
fn selects_into_a_table_or_variable_and_exports_are_not_big_results() {
    assert!(
        of(&big_read("SELECT o.* INTO #recent FROM dbo.orders o WHERE o.created_at > @P1", 100.0, 24.0), "big-result")
            .is_empty()
    );
    assert!(of(&big_read("SELECT @total = SUM(amount) FROM dbo.orders", 100.0, 24.0), "big-result").is_empty());
    // A write behind a WITH list counts the rows it changed, not rows sent to the client.
    for write in [
        "WITH b AS (SELECT id FROM jobs WHERE done ORDER BY id LIMIT 10000) DELETE FROM jobs USING b WHERE jobs.id = b.id",
        "WITH cte AS (SELECT TOP (5000) * FROM dbo.queue ORDER BY id) DELETE FROM cte",
        "WITH s(id) AS (SELECT id FROM src) UPDATE dst SET seen = true FROM s WHERE dst.id = s.id",
    ] {
        let r = big_read(write, 100.0, 24.0);
        assert!(of(&r, "big-result").is_empty(), "{write}");
    }
    assert_eq!(
        of(&big_read("SELECT id, amount FROM dbo.orders WHERE status = @P1", 100.0, 24.0), "big-result").len(),
        1
    );
    // mysqldump's read, and a whole-table read once a day for a month: an export.
    let r = big_read("SELECT SQL_NO_CACHE * FROM orders", 100.0, 24.0);
    let f = of(&r, "big-result")[0];
    assert_eq!(f.severity, Severity::Low);
    assert!(f.summary.contains("an export, a dump or an ETL job"), "{}", f.summary);
    let r = big_read("SELECT * FROM dbo.orders", 30.0, 720.0);
    assert!(of(&r, "big-result")[0].summary.contains("about once a day"), "{}", of(&r, "big-result")[0].summary);
}

// ── 15. counts since the start without a rate ──────────────────────────────────────────────────────

#[test]
fn counts_since_the_start_rise_only_on_a_measured_rate() {
    // The demo's server started 44 seconds before the scan: no rate, whatever the totals.
    let mut s = fixture("mssql-demo");
    s.indexes.iter_mut().for_each(|i| i.contention = None);
    let i = s.indexes.iter_mut().find(|i| i.table == "orders").unwrap();
    i.contention = Some(IndexContention {
        row_lock_waits: 20.0,
        row_lock_wait_ms: 900_000.0,
        page_latch_waits: 50_000.0,
        page_latch_wait_ms: 900_000.0,
        sequential_key: true,
        optimize_for_sequential_key: Some(false),
        ..Default::default()
    });
    let r = analyze(s.clone());
    for check in ["hot-rows", "last-page-insert"] {
        let f = of(&r, check)[0];
        assert_eq!(f.severity, Severity::Low, "{check}");
        assert!(
            f.caveats
                .iter()
                .any(|c| c.contains("only the minute since the server last restarted: too little time to make a rate")),
            "{check}: {:?}",
            f.caveats
        );
    }
    // Twelve hours of counting: 15 minutes of waiting is 30 a day.
    s.server.started_at = Some("2026-10-11T00:13:03Z".into());
    let r = analyze(s);
    assert_eq!(of(&r, "hot-rows")[0].severity, Severity::High);
    assert_eq!(of(&r, "last-page-insert")[0].severity, Severity::High);
}

// MySQL counts errors per digest from when it first saw it: only a failed run in its recent history says it still
// happens. NOWAIT's 3572 is a try-lock the application asked for.
#[test]
fn old_mysql_failures_are_history_and_nowait_is_not_a_failure() {
    let failing = |top_error: Option<&str>| {
        let mut s = fixture("mysql-demo");
        let q = heaviest(&mut s);
        q.text = "UPDATE `orders` SET `status` = ? WHERE `id` = ?".into();
        q.executions = 1_000.0;
        q.costs =
            Some(QueryCosts { failed: Some(300.0), top_error: top_error.map(str::to_string), ..Default::default() });
        analyze(s)
    };
    let recent = failing(Some("1062 duplicate key"));
    let f = of(&recent, "failing-query")[0];
    assert_eq!(f.severity, Severity::High);
    let caveats = f.caveats.join(" ");
    assert!(caveats.contains("from when each statement's entry began counting"), "{caveats}");
    assert!(caveats.contains("until the counters are reset or the server restarts"), "{caveats}");
    assert!(!caveats.contains("since it started"), "{caveats}");
    let r = failing(None);
    let f = of(&r, "failing-query")[0];
    assert_eq!(f.severity, Severity::Low);
    assert!(f.caveats.iter().any(|c| c.contains("may be old")), "{:?}", f.caveats);
    assert!(of(&failing(Some("3572 lock not available (NOWAIT)")), "failing-query").is_empty());
}

// ── 16. lock time ──────────────────────────────────────────────────────────────────────────────────

// One two-minute incident in a quiet database is most of its database time but a few minutes a day.
#[test]
fn a_short_lock_incident_in_a_quiet_database_is_not_high() {
    let lock = |window_hours: f64| {
        let mut s = fixture("mysql-demo");
        s.workload.as_mut().unwrap().window_hours = window_hours;
        let q = heaviest(&mut s);
        q.text = "UPDATE `stock` SET `qty` = `qty` - ? WHERE `sku` = ?".into();
        q.executions = 4.0;
        q.total_time_ms = 120_000.0;
        q.waits = vec![WaitStat { category: "Lock".into(), wait_type: None, wait_ms: 119_000.0, tasks: None }];
        // Runs over the whole window, not the fixture's two minutes: a burst would be graded on its total.
        (q.first_seen, q.last_seen) = (None, None);
        analyze(s)
    };
    assert_eq!(of(&lock(24.0), "lock-time")[0].severity, Severity::Low, "2 minutes in a day");
    assert_eq!(of(&lock(4.0), "lock-time")[0].severity, Severity::Medium, "12 minutes a day");
    let r = lock(0.03);
    assert_eq!(of(&r, "lock-time")[0].severity, Severity::Low, "two minutes of statistics make no daily figure");
}

// A read on SQL Server with READ_COMMITTED_SNAPSHOT off waits for writers' locks because reads take shared locks
// there: the change that removes the wait is row versioning, as config:read-committed-snapshot says. A read that locks
// what it reads (UPDLOCK) waits with row versioning on too, and a write isn't spared by it.
#[test]
fn a_read_blocked_by_writers_is_told_of_row_versioning() {
    let fix = |text: &str, rcsi: &str| {
        let mut s = fixture("mssql-demo");
        s.settings.retain(|x| x.name != "is_read_committed_snapshot_on");
        s.settings.push(Setting {
            name: "is_read_committed_snapshot_on".into(),
            value: rcsi.into(),
            default_value: None,
            scope: "database".into(),
        });
        let q = heaviest(&mut s);
        q.text = text.into();
        q.executions = 40.0;
        q.total_time_ms = 120_000.0;
        q.waits = vec![WaitStat { category: "Lock".into(), wait_type: None, wait_ms: 110_000.0, tasks: None }];
        let id = format!("lock-time:{}", q.id);
        let r = analyze(s);
        let f = r.findings.iter().find(|f| f.id == id);
        f.and_then(|f| f.fix.as_ref()).map(|x| x.summary.clone()).unwrap_or_default()
    };
    let read = "SELECT id, status FROM dbo.orders WHERE customer_id = @p0";
    let said = fix(read, "0");
    assert!(
        said.starts_with("This is a read, and READ_COMMITTED_SNAPSHOT is off for this database, so under read committed it waits for any write holding a lock on the rows it reads. With row versioning on (config:read-committed-snapshot has the change), it would read the last committed version of the rows instead of waiting. Without it, find what holds the locks while it waits (Live shows the blocking chain)."),
        "{said}"
    );
    for (text, rcsi) in [
        (read, "1"),
        ("SELECT id, status FROM dbo.orders WITH (UPDLOCK) WHERE customer_id = @p0", "0"),
        ("UPDATE dbo.orders SET status = @p1 WHERE customer_id = @p0", "0"),
    ] {
        let said = fix(text, rcsi);
        assert!(
            said.starts_with("Find what holds the locks while it waits (Live shows the blocking chain).")
                && !said.contains("row versioning"),
            "{text}: {said}"
        );
    }
}

// The SQL Server demo's fresh reading: an update and a read of orders waited on locks between 13:56 and 14:19, as two
// lock-time findings with the same fix. Statements that waited on one table in the same hours are one episode of
// blocking: one finding lists both and says what each waited; one that waited at another time stays its own.
#[test]
fn one_lock_episode_on_a_table_is_one_finding() {
    let mut s = fixture("mssql-demo");
    s.settings.retain(|x| x.name != "is_read_committed_snapshot_on");
    s.settings.push(Setting {
        name: "is_read_committed_snapshot_on".into(),
        value: "0".into(),
        default_value: None,
        scope: "database".into(),
    });
    let w = s.workload.as_mut().expect("workload");
    let mut set = |id: &str, text: &str, runs: f64, lock: f64, first: &str, last: &str| {
        let q = w.queries.iter_mut().find(|q| q.id == id).expect("demo query");
        (q.text, q.executions, q.total_time_ms, q.mean_time_ms) =
            (text.into(), runs, lock + 50.0, (lock + 50.0) / runs);
        q.waits = vec![WaitStat { category: "Lock".into(), wait_type: None, wait_ms: lock, tasks: None }];
        (q.first_seen, q.last_seen) = (Some(first.into()), Some(last.into()));
        q.plan = None;
        q.plans.clear();
    };
    set(
        "7",
        "UPDATE [shop].[orders] SET [status] = @P1 WHERE [id] = @P2",
        4.0,
        115_000.0,
        "2026-10-11T10:56:00Z",
        "2026-10-11T11:19:00Z",
    );
    set(
        "9",
        "SELECT [status] FROM [shop].[orders] WHERE [id] = @P1",
        3.0,
        72_000.0,
        "2026-10-11T10:57:00Z",
        "2026-10-11T11:19:00Z",
    );
    set(
        "10",
        "UPDATE [shop].[orders] SET [total] = @P1 WHERE [id] = @P2",
        2.0,
        20_000.0,
        "2026-10-11T08:00:00Z",
        "2026-10-11T08:01:00Z",
    );
    let report = analyze(s);
    let locks: Vec<&Finding> = report.findings.iter().filter(|f| f.check == "lock-time").collect();
    let episode = locks.iter().find(|f| f.id == "lock-time:7").expect("the longest wait keeps the finding");
    let ids: Vec<&str> = episode.queries.iter().map(|q| q.id.as_str()).collect();
    assert_eq!(ids, vec!["7", "9"], "{:?}", episode.queries);
    assert_eq!(episode.impact.label, "3 min waiting on locks", "both statements' waits are counted");
    assert!(
        episode.summary.contains(
            "A read from orders (query 9) waited on locks on shop.orders on Oct 11, 10:57–11:19 UTC too, 1 min (nearly all of its time): likely the same episode of blocking, behind the same holder or one another."
        ),
        "{}",
        episode.summary
    );
    // The read's own way out (row versioning) stays said.
    let fix = &episode.fix.as_ref().unwrap().summary;
    assert!(
        fix.contains("Query 9, a read, waits for writes on the same table while READ_COMMITTED_SNAPSHOT is off"),
        "{fix}"
    );
    assert!(locks.iter().all(|f| f.id != "lock-time:9"), "folded in");
    assert!(locks.iter().any(|f| f.id == "lock-time:10"), "another hour, another episode");
}

// ── 17. advice that can be applied ─────────────────────────────────────────────────────────────────

fn jit_finding(setup: impl FnOnce(&mut Snapshot)) -> Finding {
    let mut s = fixture("postgres-demo");
    let q = heaviest(&mut s);
    q.text = "SELECT count(*) FROM orders WHERE status = $1".into();
    q.executions = 100.0;
    q.total_time_ms = 4_000.0;
    q.mean_time_ms = 40.0;
    q.costs = Some(QueryCosts { jit_ms: Some(2_500.0), jit_functions: Some(800.0), ..Default::default() });
    setup(&mut s);
    of(&analyze(s), "jit-overhead")[0].clone()
}

fn jit(setup: impl FnOnce(&mut Snapshot)) -> culprit_core::Fix {
    jit_finding(setup).fix.unwrap()
}

#[test]
fn jit_advice_raises_the_current_threshold_where_the_server_is_configured() {
    // With no captured threshold, a default is no evidence of the application's current setting.
    let unknown = jit(|_| {});
    let sql = unknown.sql.unwrap();
    assert!(sql.contains("ALTER ROLE <app_role> SET jit = off;") && !sql.contains("ALTER SYSTEM"), "{sql}");
    assert!(unknown.summary.contains("does not establish that threshold"), "{}", unknown.summary);
    assert!(!unknown.summary.contains("100K today"), "{}", unknown.summary);
    let default = jit(|s| set(s, "jit_above_cost", "100000"));
    assert!(default.sql.unwrap().contains("ALTER SYSTEM SET jit_above_cost = 1000000;"));
    // Already above the threshold the old advice set: ten times higher, never lower.
    let raised = jit(|s| set(s, "jit_above_cost", "5000000")).sql.unwrap();
    assert!(raised.contains("jit_above_cost = 50000000;"), "{raised}");
    // RDS refuses ALTER SYSTEM: the parameter group.
    let rds = jit(|s| {
        s.server.cloud = Some("Amazon RDS".into());
        set(s, "jit_above_cost", "100000");
    });
    assert!(!rds.sql.unwrap().contains("ALTER SYSTEM") && rds.summary.contains("DB parameter group"));
    // A captured -1 disables threshold-driven JIT: raising it would enable something the scan found disabled.
    assert!(!jit(|s| set(s, "jit_above_cost", "-1")).sql.unwrap().contains("ALTER SYSTEM"));
}

#[test]
fn jit_counters_do_not_prove_the_current_plan_or_a_bad_row_estimate_enabled_it() {
    let f = jit_finding(|s| {
        set(s, "jit_above_cost", "100000");
        let plan = s
            .workload
            .as_mut()
            .unwrap()
            .queries
            .iter_mut()
            .find(|q| q.costs.as_ref().is_some_and(|c| c.jit_ms.is_some()))
            .unwrap()
            .plan
            .as_mut()
            .unwrap();
        plan.total_cost = 250.0;
    });
    assert!(f.summary.contains("jit_above_cost is 100000 in Culprit's session"), "{}", f.summary);
    assert!(f.summary.contains("chosen at plan time"), "{}", f.summary);
    assert!(f.summary.contains("another setting") && f.summary.contains("cached plan"), "{}", f.summary);
    let fix = f.fix.unwrap();
    assert!(fix.summary.contains("first check the application's effective threshold"), "{}", fix.summary);
    assert!(!fix.summary.contains("row estimate that inflates"), "{}", fix.summary);
    assert!(!f.summary.contains("usually because"), "{}", f.summary);
}

// ── 18. MariaDB's guess ────────────────────────────────────────────────────────────────────────────

#[test]
fn mariadb_without_column_statistics_assumes_no_filtering() {
    let mut s = fixture("mysql-demo");
    s.server.product = "MariaDB 11.8.9".into();
    let table = s.tables.iter().find(|t| t.rows.unwrap_or(0.0) >= 1_000.0).unwrap().clone();
    let q = s.workload.as_mut().unwrap().queries.iter_mut().find(|q| q.plan.is_some()).unwrap();
    q.text = format!("SELECT * FROM `{}` WHERE `flavor` = ?", table.name);
    let plan = q.plan.as_mut().unwrap();
    plan.root.object = Some(format!("`{}`.`{}`", table.schema, table.name));
    plan.root.predicate = Some(format!("(`{}`.`{}`.`flavor` = 'x')", table.schema, table.name));
    plan.root.props.insert("filtered".into(), "100".into());
    s.histograms = Some(vec![]);
    let r = analyze(s);
    let f = of(&r, "missing-histogram").into_iter().find(|f| f.title.contains("flavor")).expect("finding");
    assert!(f.summary.contains("assumes the condition keeps every row") && !f.summary.contains("10%"), "{}", f.summary);
}

// The fresh MySQL reading: "a histogram for promo_code and created_at". The count's range on ix_orders_status_created
// takes created_at from the index (a key part, filtered 100: MySQL dives into the index), and the promo_code read
// reads orders whole the only way it can (no index serves its filter or its ORDER BY): neither estimate is a guess a
// histogram would change.
#[test]
fn a_histogram_is_advised_only_for_a_guess_the_optimizer_chooses_with() {
    let mut s = fixture("mysql-demo");
    s.histograms = Some(vec![]);
    assert!(of(&analyze(s.clone()), "missing-histogram").is_empty());
    // With an index on the column it sorts by, MySQL chooses between reading that index in order until the LIMIT and
    // scanning and sorting, and the guessed share of rows that match promo_code weighs that choice.
    let mut by_total = s.indexes.iter().find(|i| i.name == "ix_orders_status").expect("index").clone();
    by_total.name = "ix_orders_total".into();
    by_total.columns = vec!["total".into()];
    s.indexes.push(by_total);
    let r = analyze(s);
    let f = of(&r, "missing-histogram");
    assert_eq!(f.len(), 1, "{:?}", f.iter().map(|f| &f.title).collect::<Vec<_>>());
    assert_eq!(f[0].title, "MySQL guesses how many orders rows match promo_code: no histogram");
    assert_eq!(f[0].queries.iter().map(|q| &q.id[..4]).collect::<Vec<_>>(), vec!["19bd"]);
}

// ── 19. tables without indexes ─────────────────────────────────────────────────────────────────────

#[test]
fn updates_to_a_table_without_indexes_rewrite_no_index() {
    let mut s = fixture("postgres-demo");
    updated(&mut s, "public.orders", 5_000_000.0, Some(48.0));
    s.indexes.retain(|i| i.table != "orders");
    assert!(of(&analyze(s), "hot-updates").is_empty());
}

// ── 20. log volume ─────────────────────────────────────────────────────────────────────────────────

/// The PostgreSQL demo with its heaviest statement an insert into events that wrote `bytes` of WAL (99% of what
/// statements wrote) for `rows` rows, in the `hours` since its statistics were reset.
fn wal(bytes: f64, rows: f64, hours: f64) -> Snapshot {
    let mut s = fixture("postgres-demo");
    s.workload.as_mut().unwrap().window_hours = hours;
    let q = heaviest(&mut s);
    q.text = "INSERT INTO events (kind, payload) VALUES ($1, $2)".into();
    q.plan = None;
    q.executions = rows;
    q.rows = Some(rows);
    q.costs = Some(QueryCosts { log_bytes: Some(bytes), log_share: Some(0.99), ..Default::default() });
    s
}

// The aptabase reading: 2.9 GB of WAL in the 1,277 days since pg_stat_statements was reset is 2.3 MB a day, yet it
// fired on the total and ranked second on the statement's share of database time.
#[test]
fn a_few_megabytes_of_wal_a_day_is_no_log_finding() {
    let r = analyze(wal(2.9 * GB, 450_000.0, 1_277.0 * 24.0));
    assert!(of(&r, "log-volume").is_empty(), "{:?}", of(&r, "log-volume").iter().map(|f| &f.title).collect::<Vec<_>>());
    // 290 MB a day of 6.8 KB rows is one, ranked by the log it writes a day, not its time: Low, below any Medium.
    let r = analyze(wal(2.9 * GB, 450_000.0, 240.0));
    let f = of(&r, "log-volume")[0];
    assert_eq!(f.severity, Severity::Low);
    assert!(f.impact.score < 22.0, "{}", f.impact.score);
    assert!(f.summary.contains("since its statistics began 10 days ago"), "{}", f.summary);
    // Over years, the window is said in days, never "1277.0 days".
    let r = analyze(wal(1_277.0 * 200.0 * MB, 1_277.0 * 200.0 * MB / 6_900.0, 1_277.0 * 24.0));
    let f = of(&r, "log-volume")[0];
    assert!(f.summary.contains("since its statistics began 1277 days ago"), "{}", f.summary);
}

// "Drop indexes nobody reads" only where the table has one, after a week of counters.
#[test]
fn log_advice_drops_an_index_only_where_one_is_unread() {
    let mut s = wal(2.9 * GB, 450_000.0, 240.0);
    let fix = of(&analyze(s.clone()), "log-volume")[0].fix.clone().unwrap();
    assert!(!fix.summary.contains("nobody reads"), "{}", fix.summary);
    s.pressure.get_or_insert_with(Pressure::default).uptime_hours = Some(240.0);
    let mut idle = s.indexes.iter().find(|i| i.name == "events_customer_idx").expect("an index on events").clone();
    idle.name = "events_idle_idx".into();
    idle.columns = vec!["kind".into()];
    idle.reads = Some(0.0);
    idle.size_bytes = Some(8.0 * MB);
    s.indexes.push(idle);
    let fix = of(&analyze(s), "log-volume")[0].fix.clone().unwrap();
    assert!(fix.summary.contains("events has one nobody reads: drop it (see Indexes)"), "{}", fix.summary);
}

#[test]
fn a_write_to_a_gone_table_gets_no_log_fix_but_an_active_or_unknown_table_does() {
    let mut s = wal(2.9 * GB, 450_000.0, 240.0);
    let q = s
        .workload
        .as_mut()
        .unwrap()
        .queries
        .iter_mut()
        .find(|q| q.costs.as_ref().is_some_and(|c| c.log_bytes.is_some()))
        .unwrap();
    q.first_seen = Some("2026-10-01T12:00:00Z".into());
    q.last_seen = Some("2026-10-01T13:00:00Z".into());
    q.plan = None;
    let id = q.id.clone();
    let before = analyze(s.clone());
    let f = of(&before, "log-volume").into_iter().find(|f| f.queries.iter().any(|r| r.id == id)).unwrap();
    assert!(!f.fix.as_ref().unwrap().summary.starts_with("Nothing to change"));
    s.tables.retain(|t| t.name != "events");
    let report = analyze(s.clone());
    let f = of(&report, "log-volume").into_iter().find(|f| f.queries.iter().any(|r| r.id == id)).unwrap();
    assert!(f.fix.as_ref().unwrap().summary.contains("isn't in the database's catalog"), "{:?}", f.fix);
    assert!(f.fix.as_ref().unwrap().sql.is_none());
    assert!(f.verify[0].starts_with("Nothing to check"));
    s.tables.clear();
    let report = analyze(s);
    let f = of(&report, "log-volume").into_iter().find(|f| f.queries.iter().any(|r| r.id == id)).unwrap();
    assert!(
        !f.fix.as_ref().unwrap().summary.contains("isn't in the database's catalog"),
        "an unread catalog proves no absence"
    );
}

// A staging SQL Server reading: 192 inserts writing 412 KB of log each into a table of 284 KB rows. The bytes per row went unsaid
// (fewer than 1,000 rows), so the log wasn't tied to the large values, and the fix said to update fewer columns.
#[test]
fn a_few_inserts_of_large_values_log_the_values() {
    // support_tickets: 9,728 rows in 2.9 GB, about 300 KB a row, nearly all of it large values.
    let mut s = tickets("mssql-demo");
    let q = heaviest(&mut s);
    q.text = "INSERT INTO shop.support_tickets (body, attachments) VALUES (@P1, @P2)".into();
    q.plan = None;
    q.executions = 192.0;
    q.rows = Some(192.0);
    q.costs = Some(QueryCosts { log_bytes: Some(192.0 * 412.0 * 1024.0), log_share: Some(0.6), ..Default::default() });
    let id = q.id.clone();
    let r = analyze(s.clone());
    assert!(of(&r, "log-volume").iter().all(|f| f.queries[0].id != id), "one cause, one finding: large-rows");
    let large = of(&r, "large-rows")[0];
    let note = large.queries.iter().find(|q| q.id == id).and_then(|q| q.note.clone());
    assert_eq!(note.as_deref(), Some("Writes about 412 KB of transaction log a row."));
    // A table too small for a large-rows finding of its own: log-volume stays, with large-rows' fix.
    let t = s.tables.iter_mut().find(|t| t.name == "support_tickets").unwrap();
    (t.rows, t.in_row_bytes, t.lob_bytes, t.data_bytes) = (Some(500.0), Some(MB), Some(150.0 * MB), Some(151.0 * MB));
    let r = analyze(s);
    let f = of(&r, "log-volume").into_iter().find(|f| f.queries[0].id == id).expect("log-volume");
    assert_eq!(f.title, "An insert into support_tickets writes 412 KB of transaction log per row it changes");
    let fix = f.fix.clone().unwrap();
    assert!(fix.summary.contains("COMPRESS()"), "{}", fix.summary);
    assert!(!fix.summary.contains("update only the columns"), "{}", fix.summary);
}

// A staging SQL Server reading: an update setting some of workflow_executions' large values logged 90 KB a row against rows of
// 284 KB, outside the "about the average row" band the merge asked for, so log-volume repeated large-rows' fix as a
// second Medium finding, and large-rows' list of statements left out the table's costliest writer.
#[test]
fn an_update_of_some_large_values_is_the_large_rows_finding_too() {
    let mut s = tickets("mssql-demo");
    let w = s.workload.as_mut().unwrap();
    // A read of whole tickets, which large-rows lists.
    let reader = w.queries.iter_mut().find(|q| q.id == "10").expect("demo query");
    reader.text = "SELECT * FROM shop.support_tickets WHERE id = @P1".into();
    reader.plan = None;
    (reader.executions, reader.rows, reader.logical_reads) = (5_000.0, Some(5_000.0), Some(280.0 * 5_000.0));
    // An update of two of a ticket's columns, a few pages a run: not a whole-row statement.
    let writer = w.queries.iter_mut().find(|q| q.id == "7").expect("demo query");
    writer.text = "UPDATE shop.support_tickets SET body = @P1, status = @P2 WHERE id = @P3".into();
    writer.plan = None;
    (writer.executions, writer.rows, writer.logical_reads) = (1_000.0, Some(1_000.0), Some(3_000.0));
    writer.costs =
        Some(QueryCosts { log_bytes: Some(1_000.0 * 90.0 * 1024.0), log_share: Some(0.5), ..Default::default() });
    let r = analyze(s);
    assert!(of(&r, "log-volume").iter().all(|f| f.queries[0].id != "7"), "one cause, one finding: large-rows");
    let large = of(&r, "large-rows")[0];
    let note = large.queries.iter().find(|q| q.id == "7").and_then(|q| q.note.clone());
    assert_eq!(note.as_deref(), Some("Writes about 90.0 KB of transaction log a row."));
    assert!(large.queries.iter().any(|q| q.id == "10"), "{:?}", large.queries);
    // The count is of every statement listed (a staging SQL Server reading counted 3 of the 4 below).
    assert!(large.summary.contains(" The 2 statements below read or write them "), "{}", large.summary);
    assert!(
        large
            .summary
            .contains("An update of support_tickets wrote 87.9 MB of transaction log for them, about 90.0 KB a row"),
        "{}",
        large.summary
    );
}

// ── 21. what lock escalations name ─────────────────────────────────────────────────────────────────

// A staging SQL Server reading: inserts into another table that check this one in an EXISTS, and an update of another table whose
// plan seeks this one, were listed as "the writes to it", and the table's own single-row update was left out. Its fix
// led with a DELETE TOP (1000) loop for a table written a row at a time.
#[test]
fn an_escalation_lists_the_writes_to_its_own_table() {
    let mut s = fixture("mssql-demo");
    s.server.started_at = Some("2026-10-11T00:13:03Z".into());
    s.indexes.iter_mut().for_each(|i| i.contention = None);
    let i = s.indexes.iter_mut().find(|i| i.name == "pk_order_items").unwrap();
    i.contention = Some(IndexContention { lock_escalations: 2.0, ..Default::default() });
    let w = s.workload.as_mut().unwrap();
    let exists = w.queries.iter_mut().find(|q| q.id == "7").unwrap();
    exists.text = "INSERT INTO [shop].[orders] ([customer_id]) SELECT @P1 WHERE EXISTS (SELECT 1 FROM [shop].[order_items] WHERE [order_id] = @P2)".into();
    exists.plan = None;
    // An update of orders whose plan seeks order_items: its modifying step names orders.
    let through = w.queries.iter_mut().find(|q| q.id == "9").unwrap();
    through.text = "UPDATE o SET [total] = @P1 FROM [shop].[orders] o JOIN [shop].[order_items] i ON i.[order_id] = o.[id] WHERE i.[id] = @P2".into();
    let plan = through.plan.as_mut().expect("the demo's plan reading order_items");
    plan.root = culprit_core::PlanNode {
        op: "Clustered Index Update".into(),
        kind: culprit_core::NodeKind::Modify,
        object: Some("[culprit_demo].[shop].[orders]".into()),
        index: Some("pk_orders".into()),
        children: vec![plan.root.clone()],
        ..plan.root.clone()
    };
    let own = w.queries.iter_mut().find(|q| q.id == "5").unwrap();
    own.text = "UPDATE [shop].[order_items] SET [quantity] = @P1 WHERE [id] = @P2".into();
    own.plan = None;
    // 252 rows in 257 runs reads as a row a run, as a statement that changes exactly one does ("1.0 rows" did).
    own.rows = Some(own.executions * 252.0 / 257.0);
    let r = analyze(s);
    let f =
        of(&r, "lock-escalations").into_iter().find(|f| f.id == "lock-escalations:shop.order_items").expect("finding");
    let listed: Vec<(&str, Option<&str>)> = f.queries.iter().map(|q| (q.id.as_str(), q.note.as_deref())).collect();
    assert_eq!(listed, vec![("5", Some("Changes about 1 row a run."))]);
    let fix = f.fix.clone().unwrap();
    let sql = fix.sql.unwrap_or_default();
    assert!(!sql.contains("DELETE TOP") && sql.contains("CREATE EVENT SESSION culprit_lock_escalation"), "{sql}");
    assert!(
        fix.summary.starts_with(
            "None of the statements in the query history changes thousands of rows of order_items at once"
        ),
        "{}",
        fix.summary
    );
}

// The fresh SQL Server reading: escalations on support_tickets, which no statement in the query history writes, were
// blamed on "jobs, migrations and maintenance", and "may be old" three minutes after a restart. The demo's lookup of
// tickets by priority returns hundreds of rows a run through key lookups its plan prefetches, which keeps their row
// locks until the statement ends: the read is the likely one.
#[test]
fn an_escalation_with_no_write_names_the_read_that_keeps_its_locks() {
    let mut s = fixture("mssql-demo");
    s.indexes.iter_mut().for_each(|i| i.contention = None);
    let i = s.indexes.iter_mut().find(|i| i.name == "ix_support_tickets_priority").unwrap();
    i.contention = Some(IndexContention { lock_escalations: 5.0, ..Default::default() });
    let r = analyze(s);
    let f = of(&r, "lock-escalations")
        .into_iter()
        .find(|f| f.id == "lock-escalations:shop.support_tickets")
        .expect("finding");
    assert!(
        f.summary.contains(
            "5 times since it started, on Oct 11 at 12:13 UTC, a minute before this scan, so none is older than that."
        ) && !f.summary.contains("may be old"),
        "{}",
        f.summary
    );
    assert!(!f.summary.contains("jobs, migrations and maintenance"), "{}", f.summary);
    assert!(
        f.summary.ends_with(
            "The likeliest is a read from support_tickets, which returns about 973 rows a run on average: its plan prefetches its key lookups, which keeps their row locks until the statement ends."
        ),
        "{}",
        f.summary
    );
    let listed: Vec<(&str, Option<&str>)> = f.queries.iter().map(|q| (q.id.as_str(), q.note.as_deref())).collect();
    assert_eq!(
        listed,
        vec![(
            "10",
            Some(
                "Returns about 973 rows a run. Its plan prefetches its key lookups, which keeps their row locks until the statement ends."
            )
        )]
    );
    let fix = f.fix.clone().unwrap();
    assert!(
        fix.summary
            .starts_with("Start the event session below (culprit_lock_escalation) to learn which statement escalates")
    );
    assert!(fix.summary.contains("READ_COMMITTED_SNAPSHOT lets reads under read committed take no row locks"));
    assert!(!fix.summary.contains("UPDATE loop"), "{}", fix.summary);
    assert!(fix.sql.unwrap().starts_with("-- SQL Server doesn't record which statement escalated"));
}

// ── 22. totals of waiting ──────────────────────────────────────────────────────────────────────────

// A staging SQL Server reading "1 min in all" for 88,960 ms next to "23 ms each": a total under an hour is said to the second.
#[test]
fn a_total_wait_is_said_to_the_second() {
    let mut s = fixture("mssql-demo");
    s.server.started_at = Some("2026-10-11T00:13:03Z".into());
    s.indexes.iter_mut().for_each(|i| i.contention = None);
    let i = s.indexes.iter_mut().find(|i| i.name == "pk_orders").unwrap();
    i.contention = Some(IndexContention { row_lock_waits: 3_901.0, row_lock_wait_ms: 88_960.0, ..Default::default() });
    let r = analyze(s);
    let hot = of(&r, "hot-rows")[0];
    // A staging scan's brief named the index as if it were a column: it is said as the table's index.
    assert!(
        hot.summary.starts_with("Since its index pk_orders was loaded into memory, sessions waited"),
        "{}",
        hot.summary
    );
    assert!(
        hot.summary
            .contains("3.9K times for locks on the rows and pages of orders, 1 min 29 s in all, 23 ms each on average"),
        "{}",
        hot.summary
    );
    assert_eq!(hot.impact.label, "1 min 29 s waiting on locks");
}
