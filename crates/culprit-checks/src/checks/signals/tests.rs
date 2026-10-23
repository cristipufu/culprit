use super::*;
use crate::waits::snapshot_with;
use culprit_core::{
    ColumnHistogram, IndexContention, Pressure, QueryCosts, Signal, SignalScope, SignalUnit, TableStorage, WaitStat,
};

fn first_query(s: &mut Snapshot) -> &mut QueryStat {
    let w = s.workload.as_mut().unwrap();
    w.queries.sort_by(|a, b| b.total_time_ms.total_cmp(&a.total_time_ms));
    w.queries.iter_mut().find(|q| !crate::support::is_monitoring_query(q)).unwrap()
}

#[test]
fn failing_runs_name_the_timeout() {
    let mut s = snapshot_with(Engine::Mssql);
    let q = first_query(&mut s);
    q.text = "SELECT id FROM dbo.orders WHERE customer_id = @P1".into();
    q.executions = 1_000.0;
    q.costs = Some(QueryCosts {
        aborted: Some(60.0),
        failed: Some(0.0),
        aborted_mean_ms: Some(30_050.0),
        ..Default::default()
    });
    let f = failing_query(&s);
    assert_eq!(f.len(), 1);
    assert_eq!(f[0].title, "A read from orders times out 6% of the time, at about 30 s");
    assert_eq!(f[0].severity, Severity::Medium);
    assert!(f[0].fix.as_ref().unwrap().summary.contains("command timeout"));
    // A handful of failures is noise.
    first_query(&mut s).costs = Some(QueryCosts { failed: Some(3.0), ..Default::default() });
    assert!(failing_query(&s).is_empty());
}

#[test]
fn mysql_errors_carry_the_code_not_the_message() {
    let mut s = snapshot_with(Engine::Mysql);
    let q = first_query(&mut s);
    q.text = "UPDATE `orders` SET `status` = ? WHERE `id` = ?".into();
    q.executions = 200.0;
    q.query_hash = Some("abc123".into());
    q.costs =
        Some(QueryCosts { failed: Some(30.0), top_error: Some("1205 lock wait timeout".into()), ..Default::default() });
    let f = failing_query(&s);
    assert_eq!(f[0].title, "An update of orders fails 15% of the time (1205 lock wait timeout)");
    assert_eq!(f[0].severity, Severity::High);
    let fix = f[0].fix.as_ref().unwrap();
    assert!(fix.summary.contains("innodb_lock_wait_timeout"));
    assert!(fix.sql.as_ref().unwrap().contains("DIGEST = 'abc123'"));
}

#[test]
fn planning_share_needs_reuse_and_volume() {
    let mut s = snapshot_with(Engine::Postgres);
    let q = first_query(&mut s);
    q.text = "SELECT * FROM orders o JOIN customers c ON c.id = o.customer_id WHERE o.id = $1".into();
    q.executions = 10_000.0;
    q.total_time_ms = 20_000.0;
    q.mean_time_ms = 2.0;
    q.costs = Some(QueryCosts { plan_ms: Some(30_000.0), plans: Some(10_000.0), ..Default::default() });
    let f = planning_time(&s);
    assert_eq!(f.len(), 1);
    assert_eq!(f[0].title, "Planning takes 60% of the time of a read from orders");
    assert!(f[0].summary.contains("a new plan for every run"));
}

#[test]
fn jit_longer_than_the_query() {
    let mut s = snapshot_with(Engine::Postgres);
    let q = first_query(&mut s);
    q.text = "SELECT count(*) FROM orders WHERE status = $1".into();
    q.executions = 100.0;
    q.total_time_ms = 4_000.0;
    q.mean_time_ms = 40.0;
    q.costs = Some(QueryCosts { jit_ms: Some(2_500.0), jit_functions: Some(800.0), ..Default::default() });
    let f = jit_overhead(&s);
    assert_eq!(f[0].title, "JIT compilation takes longer than running a read from orders");
    assert!(s.setting("jit_above_cost").is_none());
    assert!(f[0].summary.contains("Culprit did not read jit_above_cost."));
    let sql = f[0].fix.as_ref().unwrap().sql.as_ref().unwrap();
    assert!(sql.contains("ALTER ROLE <app_role> SET jit = off;"), "{sql}");
    assert!(!sql.contains("ALTER SYSTEM"), "an unknown threshold must not be guessed: {sql}");
}

#[test]
fn log_volume_per_row_and_share() {
    let mut s = snapshot_with(Engine::Postgres);
    let q = first_query(&mut s);
    q.text = "UPDATE orders SET note = $1 WHERE id = $2".into();
    q.rows = Some(20_000.0);
    q.costs = Some(QueryCosts {
        log_bytes: Some(200.0 * MB),
        log_share: Some(0.2),
        log_full_pages: Some(20_000.0),
        ..Default::default()
    });
    // Whether wal_compression is on decides both what the image count means and the advice: it was assumed off.
    let f = log_volume(&s);
    assert_eq!(f[0].title, "An update of orders writes 10.2 KB of WAL per row it changes");
    assert!(f[0].summary.contains("20K of its records are full-page images"), "{}", f[0].summary);
    assert!(f[0].fix.as_ref().unwrap().sql.is_none(), "unknown wal_compression: no setting to change");
    set(&mut s, "wal_compression", "off");
    let f = log_volume(&s);
    assert!(f[0].summary.contains("Up to 78% of it is full-page images"), "{}", f[0].summary);
    assert_eq!(
        f[0].fix.as_ref().unwrap().sql.as_deref(),
        Some("ALTER SYSTEM SET wal_compression = 'on';\nSELECT pg_reload_conf();")
    );
    // Already compressed: the checkpoint interval is the lever, not the setting it already has.
    set(&mut s, "wal_compression", "lz4");
    let f = log_volume(&s);
    assert!(f[0].fix.as_ref().unwrap().sql.is_none());
    assert!(f[0].fix.as_ref().unwrap().summary.contains("wal_compression already shrinks"));
    // On a managed service the advice is the platform's control.
    set(&mut s, "wal_compression", "off");
    s.server.cloud = Some("Amazon RDS".into());
    let fix = log_volume(&s)[0].fix.clone().unwrap();
    assert!(fix.sql.is_none() && fix.summary.contains("DB parameter group"), "{}", fix.summary);
    // Reads are never log findings.
    first_query(&mut s).text = "SELECT 1".into();
    assert!(log_volume(&s).is_empty());
}

// A share of the log needs a measured volume a day: 30% of a few quiet minutes is nothing.
#[test]
fn a_log_share_needs_a_daily_volume() {
    let mut s = snapshot_with(Engine::Postgres);
    let q = first_query(&mut s);
    q.text = "UPDATE orders SET note = $1 WHERE id = $2".into();
    q.rows = None;
    q.costs = Some(QueryCosts { log_bytes: Some(0.4 * GB), log_share: Some(0.6), ..Default::default() });
    s.workload.as_mut().unwrap().window_hours = 0.5;
    assert!(log_volume(&s).is_empty(), "0.4 GB in half an hour is a gigabyte a day only by stretching it");
    // 2 GB inside half an hour are 2 GB inside that day: nothing is stretched.
    first_query(&mut s).costs.as_mut().unwrap().log_bytes = Some(2.0 * GB);
    assert_eq!(log_volume(&s).len(), 1);
    s.workload.as_mut().unwrap().window_hours = 24.0;
    assert_eq!(log_volume(&s)[0].severity, Severity::Medium);
    // pg_stat_statements before 14 can't say when its counts began: the window may be days longer.
    s.server.version = "13.16".into();
    assert!(log_volume(&s).is_empty());
}

fn set(s: &mut Snapshot, name: &str, value: &str) {
    s.settings.retain(|x| x.name != name);
    s.settings.push(culprit_core::Setting {
        name: name.into(),
        value: value.into(),
        default_value: None,
        scope: "server".into(),
    });
}

#[test]
fn lock_time_share_of_the_query() {
    let mut s = snapshot_with(Engine::Mysql);
    let q = first_query(&mut s);
    q.text = "UPDATE `stock` SET `qty` = `qty` - ? WHERE `sku` = ?".into();
    q.executions = 1_000.0;
    q.total_time_ms = 100_000.0;
    q.waits =
        vec![WaitStat { category: "Lock".into(), wait_type: Some("lock time".into()), wait_ms: 60_000.0, tasks: None }];
    let f = lock_time(&s);
    assert_eq!(f[0].title, "An update of stock waits on locks for 60% of its time, 60 ms per run");
    assert!(f[0].summary.contains("8.0.28"));
}

#[test]
fn big_results_with_bytes_from_the_plan_width() {
    let mut s = snapshot_with(Engine::Postgres);
    let q = first_query(&mut s);
    q.text = "SELECT id, note FROM orders WHERE created_at > $1".into();
    q.executions = 200.0;
    q.rows = Some(200.0 * 80_000.0);
    q.costs = Some(QueryCosts { row_bytes: Some(150.0), ..Default::default() });
    let f = big_result(&s);
    assert_eq!(f[0].title, "A read from orders returns 80K rows (≈ 11.4 MB) per run");
}

fn with_pressure(engine: Engine, f: impl FnOnce(&mut Pressure)) -> Snapshot {
    let mut s = snapshot_with(engine);
    let mut p = Pressure { uptime_hours: Some(72.0), ..Pressure::default() };
    f(&mut p);
    s.pressure = Some(p);
    s
}

#[test]
fn slow_tempdb_and_log() {
    let file = |name: &str, db: &str, kind, reads: f64, read_ms: f64, writes: f64, write_ms: f64| FileIo {
        name: name.into(),
        kind,
        database: Some(db.into()),
        reads,
        read_ms: Some(read_ms),
        read_bytes: None,
        writes,
        write_ms: Some(write_ms),
        write_bytes: None,
        scope: None,
        stats_reset: None,
        write_operation: None,
    };
    let s = with_pressure(Engine::Mssql, |p| {
        p.files = vec![
            file("tempdev", "tempdb", FileKind::Temp, 10_000.0, 450_000.0, 100.0, 100.0),
            file("shop", "shop", FileKind::Data, 10_000.0, 30_000.0, 0.0, 0.0),
            file("shop_log", "shop", FileKind::Log, 0.0, 0.0, 50_000.0, 600_000.0),
        ];
    });
    let titles: Vec<String> = slow_storage(&s).into_iter().map(|f| f.title).collect();
    assert_eq!(
        titles,
        vec!["Recorded reads from tempdb averaged 45 ms", "Recorded log writes to shop's log averaged 12 ms"]
    );
}

#[test]
fn churn_thresholds_differ_per_engine() {
    let signal = |rate| {
        Signal::new("new-connections", "New connections", rate, SignalUnit::PerSecond, SignalScope::SinceStartup)
    };
    let pg = with_pressure(Engine::Postgres, |p| p.signals = vec![signal(3.0)]);
    assert_eq!(connection_churn(&pg)[0].title, "This database recorded 3 new connections a second on average");
    let mssql = with_pressure(Engine::Mssql, |p| p.signals = vec![signal(3.0)]);
    assert!(connection_churn(&mssql).is_empty());
    let mysql = with_pressure(Engine::Mysql, |p| p.signals = vec![signal(40.0)]);
    assert_eq!(
        connection_churn(&mysql)[0].fix.as_ref().unwrap().sql.as_deref(),
        Some("SET PERSIST thread_cache_size = 64;")
    );
}

#[test]
fn hot_rows_and_last_page() {
    let mut s = snapshot_with(Engine::Mssql);
    let i = &mut s.indexes[0];
    let (table, index) = (i.table.clone(), i.name.clone());
    i.contention = Some(IndexContention {
        row_lock_waits: 40.0,
        row_lock_wait_ms: 120_000.0,
        page_latch_waits: 50_000.0,
        page_latch_wait_ms: 900_000.0,
        lock_escalations: 2.0,
        sequential_key: true,
        optimize_for_sequential_key: Some(false),
        ..Default::default()
    });
    let hot = hot_rows(&s);
    assert_eq!(hot[0].title, format!("{table} has 40 recorded historical lock waits"));
    assert_eq!(hot[0].actionability, culprit_core::Actionability::Information);
    assert!(hot[0].summary.contains("escalated"));
    let page = last_page_insert(&s);
    assert_eq!(page[0].title, format!("Inserts into {table} queue for the last page of {index}"));
    assert!(page[0].fix.as_ref().unwrap().sql.as_ref().unwrap().contains("OPTIMIZE_FOR_SEQUENTIAL_KEY = ON"));
}

fn escalating(escalations: f64, attempts: f64, lock_wait_ms: f64) -> IndexContention {
    IndexContention {
        lock_escalations: escalations,
        lock_escalation_attempts: attempts,
        row_lock_waits: if lock_wait_ms > 0.0 { 10.0 } else { 0.0 },
        row_lock_wait_ms: lock_wait_ms,
        ..Default::default()
    }
}

/// The demo with no contention anywhere but on `dbo.<table>`'s two indexes.
/// The demo's server started 44 seconds before its scan: too little counting for any rate. These tests start it 12
/// hours earlier, at 00:13 UTC on the day of the scan.
fn escalations_on(table: &str, first: IndexContention, second: IndexContention) -> Snapshot {
    let mut s = snapshot_with(Engine::Mssql);
    s.server.started_at = Some("2026-10-11T00:13:03Z".into());
    s.indexes.iter_mut().for_each(|i| i.contention = None);
    let mut a = s.indexes[0].clone();
    a.schema = "dbo".into();
    a.table = table.into();
    a.name = format!("pk_{table}");
    a.contention = Some(first);
    let mut b = a.clone();
    b.name = format!("ix_{table}_created_at");
    b.contention = Some(second);
    s.indexes.extend([a, b]);
    s
}

#[test]
fn lock_escalations_add_up_per_table_and_name_the_write_that_changes_the_most_rows() {
    let mut s = escalations_on("orders", escalating(3.0, 6.0, 30_000.0), escalating(2.0, 3.0, 0.0));
    let q = first_query(&mut s);
    q.text = "DELETE FROM dbo.orders WHERE created_at < @P1".into();
    q.plan = None;
    q.executions = 4.0;
    q.rows = Some(40_000.0);
    let id = q.id.clone();
    let f = lock_escalations(&s);
    assert_eq!(f.len(), 1);
    assert_eq!(f[0].id, "lock-escalations:dbo.orders");
    assert_eq!(f[0].title, "Row locks on orders escalated to a table lock 5 times since SQL Server started (Oct 11)");
    // Attempts aren't reported: the counter moves without any statement behind it.
    assert!(!f[0].summary.contains("tried"), "{}", f[0].summary);
    assert!(
        f[0].summary.contains("the likeliest is a delete from orders, which changes about 10K rows a run."),
        "{}",
        f[0].summary
    );
    assert_eq!(f[0].queries[0].id, id);
    assert_eq!(f[0].severity, Severity::High);
    assert_eq!(f[0].metrics[0].label, ESCALATIONS_METRIC);
    assert_eq!(f[0].metrics[0].value, 5.0);
    assert!(f[0].fix.as_ref().unwrap().sql.as_ref().unwrap().contains("DELETE TOP (1000) FROM [dbo].[orders]"));
}

// On a server, every index of a table a few dozen rows long can show an escalation attempt an hour that no
// statement made: the counter moves with SQL Server's own background work. Only escalations are reported.
#[test]
fn lock_escalation_attempts_alone_are_not_a_finding() {
    let s = escalations_on("audit_archive", escalating(0.0, 24.0, 0.0), escalating(0.0, 24.0, 0.0));
    assert!(lock_escalations(&s).is_empty());
    let s = escalations_on("orders", escalating(0.0, 0.0, 90_000.0), IndexContention::default());
    assert!(lock_escalations(&s).is_empty());
}

#[test]
fn a_table_with_no_statement_writing_to_it_says_where_to_look() {
    let s = escalations_on("audit_archive", escalating(2.0, 0.0, 0.0), IndexContention::default());
    let f = lock_escalations(&s);
    assert_eq!(
        f[0].title,
        "Row locks on audit_archive escalated to a table lock 2 times since SQL Server started (Oct 11)"
    );
    assert!(f[0].summary.contains("none of the statements in the query history writes to it"), "{}", f[0].summary);
    assert!(f[0].queries.is_empty());
    assert_eq!(f[0].severity, Severity::High);
}

#[test]
fn a_single_lock_escalation_is_high() {
    let s = escalations_on("orders", escalating(1.0, 1.0, 0.0), IndexContention::default());
    let f = lock_escalations(&s);
    assert_eq!(f[0].title, "Row locks on orders escalated to a table lock once since SQL Server started (Oct 11)");
    assert_eq!(f[0].severity, Severity::High);
}

/// `s` with the counts of an earlier scan of it, taken at 06:00 on the day it was taken.
fn with_earlier_scan(mut s: Snapshot, counts: &[(&str, f64)], started_at: Option<&str>) -> Snapshot {
    let mut earlier = s.clone();
    earlier.taken_at = "2026-10-11T06:00:00Z".into();
    if let Some(at) = started_at {
        earlier.server.started_at = Some(at.into());
    }
    for i in earlier.indexes.iter_mut().filter_map(|i| i.contention.as_mut()) {
        i.lock_escalations = 0.0;
    }
    for (table, n) in counts {
        let i = earlier.indexes.iter_mut().find(|i| i.table == *table && i.contention.is_some()).expect("table");
        i.contention.as_mut().unwrap().lock_escalations = *n;
    }
    s.escalations_before = culprit_core::EscalationBaseline::from_scan(&earlier, &s);
    s
}

// SQL Server counts escalations since it started, with no time for each: without an earlier scan, Culprit can't
// tell whether they're recent. The finding says when the count began, and only a rate measured since then raises
// it: two in the 12 hours since the start is High.
#[test]
fn escalations_without_an_earlier_scan_are_graded_on_their_rate_since_the_start() {
    let mut s = escalations_on("orders", escalating(2.0, 0.0, 0.0), IndexContention::default());
    let f = &lock_escalations(&s)[0];
    assert_eq!(f.severity, Severity::High);
    assert!(f.is_actionable());
    // Counted for 12 hours: none of them is older than that (the fresh SQL Server reading said "they may be old" three
    // minutes after a restart).
    assert!(
        f.summary.starts_with(
            "SQL Server replaced the row and page locks on orders with one lock on the whole table 2 times since it started, on Oct 11 at 00:13 UTC, \
             12 hours before this scan, so none is older than that."
        ),
        "{}",
        f.summary
    );
    assert!(f.summary.contains("once a day or more on average over the counter history"), "{}", f.summary);
    assert_eq!(f.metrics.len(), 1);
    // Counted for days, they may be old.
    s.server.started_at = Some("2026-10-08T00:13:03Z".into());
    let f = &lock_escalations(&s)[0];
    assert_eq!(f.severity, Severity::Medium);
    assert!(f.is_actionable());
    assert!(
        f.summary.contains(
            "2 times since it started, on Oct 8 at 00:13 UTC. It keeps no time for each, and Culprit has no earlier scan to compare with, so they may be old"
        ),
        "{}",
        f.summary
    );
}

// The false flag: one escalation since a start six months ago was High on a first scan. Without an earlier scan
// it is history unless the count since the start makes a rate.
#[test]
fn one_escalation_in_six_months_is_low_history_on_a_first_scan() {
    let mut s = escalations_on("orders", escalating(1.0, 0.0, 0.0), IndexContention::default());
    s.server.started_at = Some("2026-04-11T12:00:00Z".into());
    let f = &lock_escalations(&s)[0];
    assert_eq!(f.severity, Severity::Low);
    assert_eq!(f.actionability, culprit_core::Actionability::Information);
    assert!(f.summary.contains("less than once a week on average since then: they may all be old"), "{}", f.summary);
    // A few a week since the start is Medium: likely to happen again.
    s.server.started_at = Some("2026-10-01T12:00:00Z".into());
    s.indexes.iter_mut().filter_map(|i| i.contention.as_mut()).for_each(|c| c.lock_escalations = 3.0);
    assert_eq!(lock_escalations(&s)[0].severity, Severity::Medium);
    assert!(lock_escalations(&s)[0].is_actionable());
    // Minutes of counting make no rate: the demo's server started 44 seconds before the scan.
    s.server.started_at = Some("2026-10-11T12:13:03Z".into());
    let f = &lock_escalations(&s)[0];
    assert_eq!(f.severity, Severity::Low);
    assert_eq!(f.actionability, culprit_core::Actionability::Information);
    assert!(f.summary.contains("started too recently to tell how often"), "{}", f.summary);
}

#[test]
fn escalations_since_the_previous_scan_are_high_and_counted() {
    let s = escalations_on("orders", escalating(3.0, 0.0, 0.0), escalating(2.0, 0.0, 0.0));
    let f = &lock_escalations(&with_earlier_scan(s, &[("orders", 4.0)], None))[0];
    assert_eq!(f.severity, Severity::High);
    assert_eq!(f.title, "Row locks on orders escalated to a table lock once since Oct 11, 06:00 UTC");
    assert!(
        f.summary.starts_with(
            "SQL Server replaced the row and page locks on orders with one lock on the whole table once since the previous scan, \
             on Oct 11 at 06:00 UTC. It counted 5 in all since SQL Server started (Oct 11)."
        ),
        "{}",
        f.summary
    );
    assert_eq!(f.impact.label, "1 new table lock");
    assert_eq!((f.metrics[0].label.as_str(), f.metrics[0].value), (ESCALATIONS_METRIC, 5.0));
    assert_eq!((f.metrics[1].label.as_str(), f.metrics[1].value), (ESCALATIONS_SINCE_METRIC, 1.0));
}

// The false flag: a rebuilt index (or a dropped partition) takes its own count with it, so the table's total falls with
// no restart. What remains may all be old: no "since the previous scan", only the rate since the start.
#[test]
fn a_count_that_fell_without_a_restart_is_not_new_escalations() {
    let s = escalations_on("orders", escalating(2.0, 0.0, 0.0), IndexContention::default());
    let s = with_earlier_scan(s, &[("orders", 5.0)], None);
    let f = &lock_escalations(&s)[0];
    assert!(!f.title.contains("since Oct 11, 06:00"), "{}", f.title);
    assert!(f.summary.contains("since it started"), "{}", f.summary);
}

// The case that looked recent: the count is old, and the writes listed are only the table's writes now.
#[test]
fn escalations_with_none_since_the_previous_scan_are_low_history() {
    let mut s = escalations_on("orders", escalating(2.0, 0.0, 0.0), IndexContention::default());
    s.options = Some(culprit_core::SnapshotOptions::default().with_window_minutes(6 * 60));
    let q = first_query(&mut s);
    q.text = "UPDATE TOP (1000) dbo.orders SET archived = 1 WHERE archived = 0".into();
    q.plan = None;
    q.rows = Some(q.executions * 900.0);
    let f = &lock_escalations(&with_earlier_scan(s, &[("orders", 2.0)], None))[0];
    assert_eq!(f.severity, Severity::Low);
    assert_eq!(
        f.title,
        "Row locks on orders escalated to a table lock 2 times since SQL Server started (Oct 11), none since Oct 11, 06:00 UTC"
    );
    assert!(
        f.summary.starts_with(
            "These escalations are history, not something happening now: SQL Server counts them from when it started, \
             on Oct 11 at 00:13 UTC, and the count on orders hasn't grown since the previous scan, on Oct 11 at 06:00 UTC."
        ),
        "{}",
        f.summary
    );
    assert!(f.summary.ends_with("none of them has escalated since the previous scan."), "{}", f.summary);
    assert!(!f.summary.contains("likeliest"), "{}", f.summary);
    assert_eq!(f.queries.len(), 1);
    assert_eq!(f.impact.label, "2 table locks, none new");
    assert_eq!(f.metrics[1].value, 0.0);
    assert!(!f.fix.as_ref().unwrap().summary.contains("since the previous scan"), "the fix doesn't repeat the history");
}

#[test]
fn all_timespan_windows_require_baseline_to_cover_the_window_before_silencing_escalations() {
    let mut base = escalations_on("orders", escalating(40.0, 0.0, 0.0), IndexContention::default());
    base.taken_at = "2026-10-31T12:00:00Z".into();
    base.server.started_at = Some("2026-10-01T00:00:00Z".into());

    for (window_minutes, partial_at, full_at) in [
        (60_u32, "2026-10-31T11:54:00Z", "2026-10-31T11:00:00Z"),
        (360, "2026-10-31T11:24:00Z", "2026-10-31T06:00:00Z"),
        (1440, "2026-10-31T09:36:00Z", "2026-10-30T12:00:00Z"),
        (7 * 1440, "2026-10-30T19:00:00Z", "2026-10-24T12:00:00Z"),
        (30 * 1440, "2026-10-28T12:00:00Z", "2026-10-01T12:00:00Z"),
    ] {
        let mut s = with_earlier_scan(base.clone(), &[("orders", 40.0)], None);
        s.options = Some(culprit_core::SnapshotOptions::default().with_window_minutes(window_minutes));

        // A baseline covering only a fraction of the selected window must never silence escalations into History.
        s.escalations_before.as_mut().unwrap().taken_at = partial_at.into();
        let f = &lock_escalations(&s)[0];
        assert_eq!(f.severity, Severity::High, "window {window_minutes}m should keep active escalations High");
        assert!(f.is_actionable(), "window {window_minutes}m");
        assert!(!f.title.contains(", none since "), "window {window_minutes}m: {}", f.title);

        // When the baseline covers the full selected window with zero delta, it is History.
        s.escalations_before.as_mut().unwrap().taken_at = full_at.into();
        let f = &lock_escalations(&s)[0];
        assert_eq!(f.severity, Severity::Low, "window {window_minutes}m with full baseline should be Low");
        assert_eq!(f.actionability, culprit_core::Actionability::History, "window {window_minutes}m");
        assert!(f.title.contains(", none since "), "window {window_minutes}m: {}", f.title);
    }
}

#[test]
fn after_a_restart_every_escalation_counted_is_new() {
    let mut s = escalations_on("orders", escalating(2.0, 0.0, 0.0), IndexContention::default());
    s.server.started_at = Some("2026-10-11T12:13:03Z".into());
    // The earlier scan counted more, before the server restarted at 12:13 and started the count again.
    let s = with_earlier_scan(s, &[("orders", 9.0)], Some("2026-10-01T00:00:00Z"));
    assert!(s.escalations_before.as_ref().unwrap().restarted);
    let f = &lock_escalations(&s)[0];
    assert_eq!(f.severity, Severity::High);
    assert_eq!(f.title, "Row locks on orders escalated to a table lock 2 times since Oct 11, 06:00 UTC");
    assert!(
        f.summary
            .contains("SQL Server restarted since then (on Oct 11 at 12:13 UTC), and these are all it counted since."),
        "{}",
        f.summary
    );
}

#[test]
fn the_fix_starts_an_event_session_that_keeps_the_statement() {
    let s = escalations_on("orders", escalating(2.0, 0.0, 0.0), IndexContention::default());
    let sql = lock_escalations(&s)[0].fix.clone().unwrap().sql.unwrap();
    assert!(sql.contains("CREATE EVENT SESSION culprit_lock_escalation ON SERVER"), "{sql}");
    assert!(sql.contains("WHERE sqlserver.database_name = N'culprit_demo')"), "{sql}");
    assert!(sql.contains("FROM sys.dm_xe_session_targets t"), "{sql}");
    // CX-12: every table's finding starts this one session, so its SQL creates it only where it isn't defined, starts
    // it only where it isn't running, and says how to stop it.
    assert!(
        sql.contains(
            "IF NOT EXISTS (SELECT 1 FROM sys.server_event_sessions WHERE name = N'culprit_lock_escalation')\n    \
             CREATE EVENT SESSION culprit_lock_escalation ON SERVER\n"
        ),
        "{sql}"
    );
    assert!(
        sql.contains(
            "IF NOT EXISTS (SELECT 1 FROM sys.dm_xe_sessions WHERE name = N'culprit_lock_escalation')\n    \
             ALTER EVENT SESSION culprit_lock_escalation ON SERVER STATE = START;"
        ),
        "{sql}"
    );
    assert!(
        sql.ends_with(
            "-- Once it has caught what you need, stop and remove it: DROP EVENT SESSION culprit_lock_escalation ON \
             SERVER;"
        ),
        "{sql}"
    );
    let other = escalations_on("customers", escalating(2.0, 0.0, 0.0), IndexContention::default());
    let again = lock_escalations(&other)[0].fix.clone().unwrap().sql.unwrap();
    let start = |sql: &str| sql[..sql.find("-- Then read").unwrap()].to_string();
    assert_eq!(start(&sql), start(&again), "one session, defined one way for every table");
    let mut azure = escalations_on("orders", escalating(2.0, 0.0, 0.0), IndexContention::default());
    azure.server.cloud = Some("Azure SQL Database".into());
    let sql = lock_escalations(&azure)[0].fix.clone().unwrap().sql.unwrap();
    assert!(sql.contains("CREATE EVENT SESSION culprit_lock_escalation ON DATABASE"), "{sql}");
    assert!(sql.contains("FROM sys.dm_xe_database_session_targets t"), "{sql}");
    assert!(!sql.contains("STARTUP_STATE") && !sql.contains("database_name ="), "{sql}");
}

#[test]
fn untrusted_constraints_group_per_table() {
    let mut s = snapshot_with(Engine::Mssql);
    let c = |name: &str, kind, disabled| ConstraintIssue {
        schema: "dbo".into(),
        table: "orders".into(),
        name: name.into(),
        kind,
        disabled,
        inheritance_child: false,
    };
    s.constraints = vec![c("FK_orders_customers", ConstraintKind::ForeignKey, false)];
    let f = untrusted_constraint(&s);
    assert_eq!(f[0].title, "SQL Server doesn't trust the foreign key FK_orders_customers on orders");
    assert_eq!(
        f[0].fix.as_ref().unwrap().sql.as_deref(),
        Some("ALTER TABLE [dbo].[orders] WITH CHECK CHECK CONSTRAINT [FK_orders_customers];")
    );
    s.constraints.push(c("CK_orders_total", ConstraintKind::Check, true));
    let f = untrusted_constraint(&s);
    assert_eq!(f.len(), 1);
    assert_eq!(f[0].severity, Severity::Medium);
    let mut pg = snapshot_with(Engine::Postgres);
    pg.constraints =
        vec![ConstraintIssue { schema: "public".into(), ..c("orders_total_check", ConstraintKind::Check, false) }];
    assert_eq!(
        untrusted_constraint(&pg)[0].fix.as_ref().unwrap().sql.as_deref(),
        Some("ALTER TABLE public.orders VALIDATE CONSTRAINT orders_total_check;")
    );
}

#[test]
fn histograms_for_guessed_filter_columns() {
    let mut s = snapshot_with(Engine::Mysql);
    let table = s.tables.iter().find(|t| t.rows.unwrap_or(0.0) >= 1_000.0).unwrap().clone();
    let q = s.workload.as_mut().unwrap().queries.iter_mut().find(|q| q.plan.is_some()).unwrap();
    q.text = format!("SELECT * FROM `{}` WHERE `flavor` = ?", table.name);
    let plan = q.plan.as_mut().unwrap();
    plan.root.object = Some(format!("`{}`.`{}`", table.schema, table.name));
    plan.root.predicate = Some(format!("(`{}`.`{}`.`flavor` = 'x')", table.schema, table.name));
    plan.root.props.insert("filtered".into(), "10.00".into());
    s.histograms = Some(vec![]);
    let all = missing_histogram(&s);
    let f: Vec<&Finding> = all.iter().filter(|f| f.subject.name.ends_with(&format!(".{}", table.name))).collect();
    assert_eq!(f.len(), 1, "{all:?}");
    assert_eq!(f[0].title, format!("MySQL guesses how many {} rows match flavor: no histogram", table.name));
    assert!(f[0].fix.as_ref().unwrap().sql.as_ref().unwrap().contains("UPDATE HISTOGRAM ON `flavor`"));
    s.histograms = Some(vec![ColumnHistogram {
        schema: table.schema.clone(),
        table: table.name.clone(),
        column: "flavor".into(),
        kind: Some("singleton".into()),
        buckets: Some(3.0),
    }]);
    assert!(!missing_histogram(&s).iter().any(|f| f.title.contains("flavor")));
    // MariaDB collects them with ANALYZE … PERSISTENT. Its guess is that the condition keeps every row: MySQL's 10%
    // isn't one there.
    s.histograms = Some(vec![]);
    s.server.product = "MariaDB 11.8.9".into();
    assert!(missing_histogram(&s).iter().all(|f| !f.title.contains("flavor")));
    let q = s.workload.as_mut().unwrap().queries.iter_mut().find(|q| q.plan.is_some()).unwrap();
    q.plan.as_mut().unwrap().root.props.insert("filtered".into(), "100".into());
    let maria = missing_histogram(&s);
    let f = maria.iter().find(|f| f.title.contains("flavor")).unwrap();
    assert!(f.title.starts_with("MariaDB guesses"));
    assert!(f.fix.as_ref().unwrap().sql.as_ref().unwrap().contains("PERSISTENT FOR COLUMNS (`flavor`) INDEXES ()"));
    // MySQL 5.7 has no histograms: no finding at all.
    s.histograms = None;
    assert!(missing_histogram(&s).is_empty());
}

#[test]
fn hot_updates_say_why() {
    let mut s = snapshot_with(Engine::Postgres);
    let t = &mut s.tables[0];
    t.updates = Some(100_000.0);
    t.storage = Some(TableStorage {
        hot_updates: Some(10_000.0),
        new_page_updates: Some(80_000.0),
        fillfactor: Some(100.0),
        bloat_bytes: None,
        ..Default::default()
    });
    let name = t.name.clone();
    let f = hot_updates(&s);
    assert_eq!(f[0].title, format!("Only 10% of updates to {name} are HOT: the rest rewrite every index"));
    assert!(f[0].fix.as_ref().unwrap().sql.as_ref().unwrap().contains("fillfactor = 85"));
    s.tables[0].storage.as_mut().unwrap().new_page_updates = Some(1_000.0);
    let f = hot_updates(&s);
    assert!(f[0].fix.as_ref().unwrap().sql.is_none());
    assert!(f[0].summary.contains("changed an indexed column"));
}

#[test]
fn bloat_needs_size_and_share() {
    let mut s = snapshot_with(Engine::Postgres);
    let t = &mut s.tables[0];
    t.data_bytes = Some(4.0 * GB);
    t.storage = Some(TableStorage { bloat_bytes: Some(2.4 * GB), ..Default::default() });
    let name = t.name.clone();
    assert_eq!(table_bloat(&s)[0].title, format!("{name} is about 60% empty space (≈ 2.4 GB)"));
    s.tables[0].storage.as_mut().unwrap().bloat_bytes = Some(50.0 * MB);
    assert!(table_bloat(&s).is_empty());
}

/// The demo's support tickets (its events on PostgreSQL) made into a table of 9.7K rows taking 2.9 GB, almost all
/// of it large values, with two large-value columns.
fn large_rows_snapshot(engine: Engine) -> Snapshot {
    let mut s = snapshot_with(engine);
    s.taken_at = "2026-10-13T09:00:00Z".into();
    let name = if engine == Engine::Postgres { "events" } else { "support_tickets" };
    let t = s.tables.iter_mut().find(|t| t.name == name).expect("the demo table");
    t.rows = Some(9_728.0);
    t.in_row_bytes = Some(31.0 * MB);
    t.lob_bytes = Some(2_900.0 * MB);
    t.overflow_bytes = Some(0.0);
    t.data_bytes = Some(2_931.0 * MB);
    let (schema, table) = (t.schema.clone(), t.name.clone());
    let (body, attachments) = match engine {
        Engine::Postgres => ("text", "bytea"),
        _ => ("nvarchar(max)", "varbinary(max)"),
    };
    for (ordinal, (name, data_type)) in
        [("id", "int"), ("body", body), ("attachments", attachments)].into_iter().enumerate()
    {
        s.columns.retain(|c| !(c.table == table && c.name == name));
        s.columns.push(culprit_core::ColumnInfo {
            schema: schema.clone(),
            table: table.clone(),
            name: name.into(),
            data_type: data_type.into(),
            nullable: true,
            default: None,
            generated: false,
            ordinal: 100 + ordinal as u32,
        });
    }
    s
}

fn large_rows_of(s: &Snapshot) -> Option<Finding> {
    let name = if s.server.engine == Engine::Postgres { "public.events" } else { "shop.support_tickets" };
    large_rows(s).into_iter().find(|f| f.id == format!("large-rows:{name}"))
}

/// A statement loading one whole support ticket by id, `runs` times over a 24-hour window, 280 pages a run.
fn load_ticket(s: &mut Snapshot, runs: f64) -> String {
    let w = s.workload.as_mut().unwrap();
    w.window_hours = 24.0;
    let q = w.queries.iter_mut().find(|q| !crate::support::is_monitoring_query(q)).unwrap();
    q.text = "SELECT * FROM shop.support_tickets WHERE id = @P1".into();
    q.plan = None;
    q.executions = runs;
    q.rows = Some(runs);
    q.logical_reads = Some(280.0 * runs);
    // Runs over the whole day, not the fixture's four hours, which in a day would be a burst.
    (q.first_seen, q.last_seen) = (None, None);
    q.id.clone()
}

#[test]
fn large_rows_stay_quiet_for_small_tables_small_rows_or_rows_kept_in_row() {
    let mut s = large_rows_snapshot(Engine::Mssql);
    assert!(large_rows_of(&s).is_some());
    // Small rows: a million of them, 400 bytes each.
    let t = s.tables.iter_mut().find(|t| t.name == "support_tickets").unwrap();
    t.rows = Some(1_000_000.0);
    assert!(large_rows_of(&s).is_none());
    // A small table: 110 MB.
    let t = s.tables.iter_mut().find(|t| t.name == "support_tickets").unwrap();
    (t.rows, t.in_row_bytes, t.lob_bytes) = (Some(1_000.0), Some(10.0 * MB), Some(100.0 * MB));
    assert!(large_rows(&s).is_empty());
    // Large rows kept in the rows' own pages: wide, but not large values.
    let t = s.tables.iter_mut().find(|t| t.name == "support_tickets").unwrap();
    (t.rows, t.in_row_bytes, t.lob_bytes) = (Some(10_000.0), Some(300.0 * MB), Some(100.0 * MB));
    assert!(large_rows(&s).is_empty());
    // An engine that doesn't split its pages says nothing.
    let t = s.tables.iter_mut().find(|t| t.name == "support_tickets").unwrap();
    (t.in_row_bytes, t.lob_bytes) = (None, None);
    assert!(large_rows(&s).is_empty());
}

#[test]
fn large_rows_name_the_table_its_columns_and_the_fix() {
    let s = large_rows_snapshot(Engine::Mssql);
    let f = large_rows_of(&s).expect("finding");
    assert_eq!(f.title, "support_tickets holds 2.9 GB in 9.7K rows: 309 KB a row, mostly large values");
    assert!(
        f.summary.contains(
            "almost all of that is large values kept apart from the rows (columns that can hold them: body and attachments)."
        ),
        "{}",
        f.summary
    );
    assert!(!f.summary.contains("previous scan"), "no growth line without an earlier scan: {}", f.summary);
    assert_eq!(f.severity, Severity::Low);
    assert_eq!(f.confidence, Confidence::High);
    assert!(f.queries.is_empty());
    let fix = f.fix.as_ref().unwrap();
    assert!(fix.sql.is_none());
    assert!(fix.summary.contains("Read only the columns a statement needs") && fix.summary.contains("COMPRESS()"));
    assert!(!f.verify.is_empty());
}

#[test]
fn large_rows_read_whole_many_times_a_day_raise_it() {
    let mut s = large_rows_snapshot(Engine::Mssql);
    let id = load_ticket(&mut s, 12_700.0);
    let f = large_rows_of(&s).expect("finding");
    assert_eq!(f.severity, Severity::Medium);
    assert_eq!(f.queries.iter().map(|q| q.id.as_str()).collect::<Vec<_>>(), vec![id.as_str()]);
    assert!(
        f.summary.contains(
            "The statement below reads them about 13K times a day, going through about 280 pages (2.2 MB) a run."
        ),
        "{}",
        f.summary
    );
    // An insert writing them is said as one.
    let q = s.workload.as_mut().unwrap().queries.iter_mut().find(|q| q.id == id).unwrap();
    q.text =
        "(@P1 nvarchar(4000),@P2 varbinary(max))INSERT INTO shop.support_tickets (body, attachments) VALUES (@P1, @P2)"
            .into();
    assert!(large_rows_of(&s).unwrap().summary.contains("The statement below writes them about 13K times a day"));
    // A seek that reads a few pages a row isn't a whole-row read.
    let q = s.workload.as_mut().unwrap().queries.iter_mut().find(|q| q.id == id).unwrap();
    q.logical_reads = Some(4.0 * 12_700.0);
    assert!(large_rows_of(&s).unwrap().queries.is_empty());
}

#[test]
fn large_rows_say_how_fast_they_grow_since_the_previous_scan() {
    let mut s = large_rows_snapshot(Engine::Mssql);
    load_ticket(&mut s, 12_700.0);
    let tables = BTreeMap::from([("shop.support_tickets".to_string(), 2_331.0 * MB)]);
    s.sizes_before =
        Some(culprit_core::SizeBaseline { taken_at: "2026-10-12T09:00:00Z".into(), tables, reference: None });
    let f = large_rows_of(&s).expect("finding");
    // Read whole thousands of times a day and growing by a fifth of its size a day.
    assert_eq!(f.severity, Severity::High);
    assert!(
        f.summary.contains("It grew by 600 MB since the scan on Oct 12 at 09:00 UTC, about 600 MB a day."),
        "{}",
        f.summary
    );
    assert!(f.metrics.iter().any(|m| m.label == "Growth a day"));

    // Six hours apart: the growth, without a daily rate drawn from a quarter of a day.
    s.sizes_before.as_mut().unwrap().taken_at = "2026-10-13T03:00:00Z".into();
    let f = large_rows_of(&s).unwrap();
    assert!(
        f.summary.contains("It grew by 600 MB in the 6 hours since the scan on Oct 13 at 03:00 UTC"),
        "{}",
        f.summary
    );
    assert!(!f.metrics.iter().any(|m| m.label == "Growth a day"));

    // Not grown.
    s.sizes_before.as_mut().unwrap().tables.insert("shop.support_tickets".into(), 2_931.0 * MB);
    let f = large_rows_of(&s).unwrap();
    assert!(f.summary.contains("It hasn't grown since the scan on"), "{}", f.summary);
    assert_eq!(f.severity, Severity::Medium);

    // The false flag: an earlier scan that measured only the in-row part (PostgreSQL's catalog sizes when the exact
    // ones timed out) read all the large values as growth. The baseline is taken the way the size now is.
    let mut earlier = s.clone();
    let t = earlier.tables.iter_mut().find(|t| t.name == "support_tickets").unwrap();
    t.data_bytes = t.in_row_bytes;
    let baseline = culprit_core::SizeBaseline::from_scan(&earlier).unwrap();
    let now = s.tables.iter().find(|t| t.name == "support_tickets").unwrap();
    assert_eq!(baseline.bytes("shop.support_tickets"), culprit_core::signals::rows_bytes(now));
}

#[test]
fn large_rows_on_postgres_count_toast() {
    let s = large_rows_snapshot(Engine::Postgres);
    let f = large_rows_of(&s).expect("finding");
    assert!(f.summary.contains("PostgreSQL has already compressed them"));
    assert!(f.fix.as_ref().unwrap().summary.contains("SET COMPRESSION lz4"));
    // The demo's own text and jsonb columns can hold them too.
    assert!(f.summary.contains("(columns that can hold them: kind, payload, body and attachments)"), "{}", f.summary);
    // MySQL doesn't report the split.
    let mut m = snapshot_with(Engine::Mysql);
    m.tables.iter_mut().for_each(|t| (t.rows, t.data_bytes) = (Some(9_728.0), Some(2_931.0 * MB)));
    assert!(large_rows(&m).is_empty());
}

#[test]
fn replicas_lagging_stopped_and_slots() {
    let r = |name: &str, role, state: &str, lag_seconds, lag_bytes| Replica {
        state: Some(state.into()),
        lag_seconds,
        lag_bytes,
        ..Replica::new(name, role)
    };
    let s = with_pressure(Engine::Postgres, |p| {
        p.replicas = vec![
            r("reporting", ReplicaRole::Replica, "streaming", Some(95.0), None),
            r("healthy", ReplicaRole::Replica, "streaming", Some(0.4), Some(1_000.0)),
            r("old_etl", ReplicaRole::Slot, "inactive", None, Some(14.0 * GB)),
        ];
    });
    let titles: Vec<String> = replica_lag(&s).into_iter().map(|f| f.title).collect();
    assert_eq!(
        titles,
        vec!["The replica reporting is 2 min behind", "The inactive replication slot old_etl keeps 14.0 GB of WAL"]
    );
    let mut stopped = r("from 10.0.0.5", ReplicaRole::ThisServer, "applying stopped (SQL thread)", None, None);
    stopped.error = Some("1062 duplicate key".into());
    let s = with_pressure(Engine::Mysql, |p| p.replicas = vec![stopped]);
    let f = replica_lag(&s);
    assert_eq!(f[0].title, "Replication to this server has stopped (applying stopped (SQL thread))");
    assert!(f[0].summary.contains("1062 duplicate key"));
}

#[test]
fn bloated_default_partition_is_flagged() {
    let mut s = snapshot_with(Engine::Postgres);
    let mut part = s.tables[0].clone();
    part.name = "events_default".into();
    part.partition_of = Some("public.events".into());
    part.rows = Some(250_000.0);
    part.data_bytes = Some(120.0 * MB);
    part.storage = Some(TableStorage { options: vec!["partition_bound=DEFAULT".into()], ..Default::default() });
    s.tables.push(part);
    let out = table_bloat(&s);
    let f = out.iter().find(|f| f.id.starts_with("table-bloat:default-partition:")).expect("default partition finding");
    assert!(f.title.contains("DEFAULT partition events_default holds 250K rows"), "{}", f.title);
    assert!(f.summary.contains("ACCESS EXCLUSIVE lock"), "{}", f.summary);
}

#[test]
fn expiring_range_partition_is_flagged_unless_default_exists() {
    let mut s = snapshot_with(Engine::Postgres);
    s.taken_at = "2026-10-30T12:00:00Z".into();
    let mut oct = s.tables[0].clone();
    oct.name = "events_2026_10".into();
    oct.partition_of = Some("public.events".into());
    oct.storage = Some(TableStorage {
        options: vec!["partition_bound=FOR VALUES FROM ('2026-10-01') TO ('2026-11-01')".into()],
        ..Default::default()
    });
    s.tables.push(oct);
    let out = table_bloat(&s);
    let f = out
        .iter()
        .find(|f| f.id == "table-bloat:expiring-partition:public.events")
        .expect("expiring range partition finding");
    assert_eq!(f.severity, Severity::High);
    assert!(f.title.contains("events's last partition (events_2026_10) ends on 2026-11-01"), "{}", f.title);

    // Adding a DEFAULT partition suppresses the expiring-partition warning.
    let mut def = s.tables[0].clone();
    def.name = "events_default".into();
    def.partition_of = Some("public.events".into());
    def.storage = Some(TableStorage { options: vec!["partition_bound=DEFAULT".into()], ..Default::default() });
    s.tables.push(def);
    assert!(!table_bloat(&s).iter().any(|f| f.id == "table-bloat:expiring-partition:public.events"));

    // MySQL and SQL Server parent-level VALUES LESS THAN range partitions are also checked via sequence_capacity.
    let mut mysql = snapshot_with(Engine::Mysql);
    mysql.taken_at = "2026-10-30T12:00:00Z".into();
    mysql.tables[0].storage = Some(TableStorage {
        options: vec!["partition_bound=VALUES LESS THAN ('2026-11-01'):p202610".into()],
        ..Default::default()
    });
    let qual = mysql.tables[0].qualified();
    let out = sequence_capacity::sequence_capacity(&mysql);
    let f = out
        .iter()
        .find(|f| f.id == format!("table-bloat:expiring-partition:{qual}"))
        .expect("mysql expiring partition");
    assert!(f.title.contains("(p202610) ends on 2026-11-01"), "{}", f.title);

    // Adding a MAXVALUE catch-all partition suppresses the warning on MySQL/MSSQL.
    mysql.tables[0].storage.as_mut().unwrap().options.push("partition_bound=VALUES LESS THAN (MAXVALUE):pmax".into());
    assert!(
        !sequence_capacity::sequence_capacity(&mysql)
            .iter()
            .any(|f| f.id == format!("table-bloat:expiring-partition:{qual}"))
    );
}

#[test]
fn large_rows_flags_json_column_on_large_postgres_tables() {
    let mut s = snapshot_with(Engine::Postgres);
    let schema = s.tables[0].schema.clone();
    let table = s.tables[0].name.clone();
    s.tables[0].rows = Some(250_000.0);
    s.columns.push(culprit_core::ColumnInfo {
        schema: schema.clone(),
        table: table.clone(),
        name: "raw_event".into(),
        data_type: "json".into(),
        nullable: true,
        default: None,
        generated: false,
        ordinal: 77,
    });
    let out = large_rows(&s);
    let f = out
        .iter()
        .find(|f| f.id == format!("large-rows:json-column:{schema}.{table}.raw_event"))
        .expect("json column finding");
    assert!(f.title.contains("uses legacy json instead of jsonb"), "{}", f.title);
    assert!(f.fix.as_ref().unwrap().sql.as_deref().unwrap().contains("TYPE jsonb USING raw_event::jsonb"));
}

#[test]
fn dead_rows_and_heavy_query_surface_scale_factor_and_triggers() {
    let mut s = snapshot_with(Engine::Postgres);
    s.tables[0].rows = Some(6_000_000.0);
    s.tables[0].updates = Some(400_000.0);
    s.tables[0].dead_rows = Some(150_000.0);
    s.tables[0].storage =
        Some(TableStorage { options: vec!["trigger=trg_audit_row:IUD".into()], ..Default::default() });
    let qual = s.tables[0].qualified();
    let table_name = s.tables[0].name.clone();
    let maint = crate::checks::maintenance::dead_rows(&s);
    let sf = maint
        .iter()
        .find(|f| f.id == format!("dead-rows:scale-factor:{qual}"))
        .expect("autovacuum scale factor finding");
    assert!(sf.title.contains("waits for") && sf.title.contains("dead rows before autovacuum starts"), "{}", sf.title);
    assert!(sf.fix.as_ref().unwrap().sql.as_deref().unwrap().contains("autovacuum_vacuum_scale_factor = 0.01"));

    // Heavy write query on a table with a matching trigger attributes the trigger in its summary.
    let q = first_query(&mut s);
    q.text = format!("UPDATE {table_name} SET status = 'archived' WHERE id = $1");
    q.total_time_ms = 500_000.0;
    q.executions = 10_000.0;
    let hq = crate::queries::heavy_query(&s);
    let write_finding = hq.iter().find(|f| f.summary.contains("fires a trigger")).expect("trigger attribution");
    assert!(write_finding.summary.contains("trg_audit_row"), "{}", write_finding.summary);

    // Explicitly disabled per-table autovacuum is flagged with RESET fix SQL.
    s.tables[0].storage.as_mut().unwrap().options.push("autovacuum_enabled=false".into());
    let maint = crate::checks::maintenance::dead_rows(&s);
    let disabled = maint
        .iter()
        .find(|f| f.id == format!("dead-rows:autovacuum-disabled:{qual}"))
        .expect("autovacuum disabled finding");
    assert!(disabled.title.contains("Autovacuum is disabled"), "{}", disabled.title);
    assert!(disabled.fix.as_ref().unwrap().sql.as_deref().unwrap().contains("RESET (autovacuum_enabled)"));
}

#[test]
fn checksum_failures_raise_high_database_attention_while_shared_connection_failures_stay_info() {
    use culprit_core::operations::{OperationalCounter, OperationalCounterKind};
    use culprit_core::{Actionability, FileScope};

    let mut s = snapshot_with(Engine::Postgres);
    s.operations.counters = vec![
        OperationalCounter {
            kind: OperationalCounterKind::ChecksumFailures,
            subject: "shop".into(),
            scope: FileScope::Database,
            count: 3.0,
            since: Some("2026-10-01T00:00:00Z".into()),
            last_event_at: Some("2026-10-21T10:00:00Z".into()),
            before: None,
        },
        OperationalCounter {
            kind: OperationalCounterKind::ConnectionFailures,
            subject: "Client connection handshakes (whole server)".into(),
            scope: FileScope::Server,
            count: 420.0,
            since: Some("2026-10-01T00:00:00Z".into()),
            last_event_at: None,
            before: None,
        },
    ];
    let out = operational_errors::operational_errors(&s);
    let checksum = out.iter().find(|f| f.id == "operational-errors:checksum-failures:shop").expect("checksum finding");
    assert_eq!(checksum.severity, Severity::High);
    assert_eq!(checksum.actionability, Actionability::NeedsAttention);
    assert!(checksum.summary.contains("immediate investigation"), "{}", checksum.summary);

    let conn = out
        .iter()
        .find(|f| f.id.starts_with("operational-errors:connection-failures:"))
        .expect("connection failures finding");
    assert_eq!(conn.severity, Severity::Low);
    assert_eq!(conn.actionability, Actionability::Information);
}

#[test]
fn sequence_capacity_flags_32bit_int_primary_keys_on_hundred_million_row_tables() {
    let mut s = snapshot_with(Engine::Postgres);
    let schema = s.tables[0].schema.clone();
    let table = s.tables[0].name.clone();
    let qual = s.tables[0].qualified();
    s.tables[0].rows = Some(600_000_000.0);
    let mut pk = s.indexes[0].clone();
    pk.schema = schema.clone();
    pk.table = table.clone();
    pk.name = format!("{table}_pkey");
    pk.columns = vec!["id".into()];
    pk.is_unique = true;
    pk.is_primary_key = true;
    s.indexes.push(pk);
    s.columns.retain(|c| !(c.schema == schema && c.table == table && c.name == "id"));
    s.columns.push(culprit_core::ColumnInfo {
        schema: schema.clone(),
        table: table.clone(),
        name: "id".into(),
        data_type: "integer".into(),
        nullable: false,
        default: None,
        generated: false,
        ordinal: 1,
    });
    let out = sequence_capacity::sequence_capacity(&s);
    let f = out.iter().find(|f| f.id == format!("sequence-capacity:int32-pk:{qual}.id")).expect("int32 pk finding");
    assert_eq!(f.severity, Severity::High);
    assert!(f.title.contains("32-bit integer primary key"), "{}", f.title);
}

#[test]
fn postgres_non_hot_updates_on_referenced_parent_escalate_fk_lock_contention() {
    let mut s = snapshot_with(Engine::Postgres);
    // `orders` in pg-demo is referenced by `order_items.order_id`.
    let orders = s.tables.iter_mut().find(|t| t.name == "orders").unwrap();
    orders.updates = Some(300_000.0);
    let st = orders.storage.get_or_insert_with(Default::default);
    st.hot_updates = Some(10_000.0);
    st.counter_hours = Some(24.0);

    let hot = hot_updates(&s);
    let f = hot.iter().find(|f| f.id == "hot-updates:public.orders").expect("orders hot-updates finding");
    assert_eq!(f.severity, Severity::Medium, "referencing FKs lower the non-HOT Medium threshold");
    assert!(f.summary.contains("FOR KEY SHARE"), "{}", f.summary);

    let fks = crate::indexes::unindexed_fk(&s);
    let fk_finding = fks.iter().find(|f| f.id == "unindexed-fk:public.order_items(order_id)").expect("unindexed fk");
    assert!(fk_finding.summary.contains("non-HOT updates"), "{}", fk_finding.summary);
}

#[test]
fn large_btree_index_on_append_only_postgres_time_series_table_recommends_brin() {
    let mut s = snapshot_with(Engine::Postgres);
    let mut events = s.tables[0].clone();
    events.schema = "public".into();
    events.name = "audit_events".into();
    events.rows = Some(12_000_000.0);
    events.data_bytes = Some(2.4 * GB);
    events.inserts = Some(12_000_000.0);
    events.updates = Some(0.0);
    events.deletes = Some(0.0);
    events.dead_rows = Some(0.0);
    s.tables.push(events);

    let mut idx = s.indexes[0].clone();
    idx.schema = "public".into();
    idx.table = "audit_events".into();
    idx.name = "ix_audit_events_created_at".into();
    idx.kind = "btree".into();
    idx.columns = vec!["created_at".into()];
    idx.is_unique = false;
    idx.is_primary_key = false;
    idx.filter = None;
    idx.include.clear();
    idx.reads = Some(5_000.0);
    idx.size_bytes = Some(420.0 * MB);
    s.indexes.push(idx);

    let out = table_bloat(&s);
    let f = out
        .iter()
        .find(|f| f.id == "table-bloat:brin-candidate:public.audit_events.ix_audit_events_created_at")
        .expect("brin candidate finding");
    assert!(f.title.contains("BRIN index would take < 1%"), "{}", f.title);
    assert!(f.fix.as_ref().unwrap().sql.as_deref().unwrap().contains("USING brin (created_at)"), "{:?}", f.fix);
}
