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
    assert_eq!(of(&r, "connection-churn")[0].title, "The server accepts 40 new connections a second: use a pool");
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
        hot[0].summary.contains("20 times for locks on the rows and pages of orders, 12 min in all"),
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
    assert_eq!(titles, vec!["Writes to shop's log average 12 ms: every commit waits for them"]);
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
    assert!(of(&r, "large-rows")[0].queries.is_empty());
    // One ticket by id reading 280 pages pulls its large values.
    let q = heaviest(&mut s);
    q.text = "SELECT * FROM shop.support_tickets WHERE id = @P1".into();
    q.logical_reads = Some(280.0 * 5_000.0);
    assert_eq!(of(&analyze(s), "large-rows")[0].queries.len(), 1);
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
    analyze(s)
}

#[test]
fn selects_into_a_table_or_variable_and_exports_are_not_big_results() {
    assert!(
        of(&big_read("SELECT o.* INTO #recent FROM dbo.orders o WHERE o.created_at > @P1", 100.0, 24.0), "big-result")
            .is_empty()
    );
    assert!(of(&big_read("SELECT @total = SUM(amount) FROM dbo.orders", 100.0, 24.0), "big-result").is_empty());
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
    assert_eq!(of(&failing(Some("1062 duplicate key")), "failing-query")[0].severity, Severity::High);
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
        analyze(s)
    };
    assert_eq!(of(&lock(24.0), "lock-time")[0].severity, Severity::Low, "2 minutes in a day");
    assert_eq!(of(&lock(4.0), "lock-time")[0].severity, Severity::Medium, "12 minutes a day");
    let r = lock(0.03);
    assert_eq!(of(&r, "lock-time")[0].severity, Severity::Low, "two minutes of statistics make no daily figure");
}

// ── 17. advice that can be applied ─────────────────────────────────────────────────────────────────

fn jit(setup: impl FnOnce(&mut Snapshot)) -> culprit_core::Fix {
    let mut s = fixture("postgres-demo");
    let q = heaviest(&mut s);
    q.text = "SELECT count(*) FROM orders WHERE status = $1".into();
    q.executions = 100.0;
    q.total_time_ms = 4_000.0;
    q.mean_time_ms = 40.0;
    q.costs = Some(QueryCosts { jit_ms: Some(2_500.0), jit_functions: Some(800.0), ..Default::default() });
    setup(&mut s);
    of(&analyze(s), "jit-overhead")[0].fix.clone().unwrap()
}

#[test]
fn jit_advice_raises_the_current_threshold_where_the_server_is_configured() {
    assert!(jit(|_| {}).sql.unwrap().contains("ALTER SYSTEM SET jit_above_cost = 1000000;"));
    // Already above the threshold the old advice set: ten times higher, never lower.
    let raised = jit(|s| set(s, "jit_above_cost", "5000000")).sql.unwrap();
    assert!(raised.contains("jit_above_cost = 50000000;"), "{raised}");
    // RDS refuses ALTER SYSTEM: the parameter group.
    let rds = jit(|s| s.server.cloud = Some("Amazon RDS".into()));
    assert!(!rds.sql.unwrap().contains("ALTER SYSTEM") && rds.summary.contains("DB parameter group"));
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

// ── 19. tables without indexes ─────────────────────────────────────────────────────────────────────

#[test]
fn updates_to_a_table_without_indexes_rewrite_no_index() {
    let mut s = fixture("postgres-demo");
    updated(&mut s, "public.orders", 5_000_000.0, Some(48.0));
    s.indexes.retain(|i| i.table != "orders");
    assert!(of(&analyze(s), "hot-updates").is_empty());
}
