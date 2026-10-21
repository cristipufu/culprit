use super::*;
use crate::waits::snapshot_with;
use culprit_core::{Pressure, Setting, Signal, SignalScope, SignalUnit, TableStorage, WaitStat};

fn set(s: &mut Snapshot, name: &str, value: &str) {
    s.settings.retain(|x| !x.name.eq_ignore_ascii_case(name));
    s.settings.push(Setting { name: name.into(), value: value.into(), default_value: None, scope: "server".into() });
}

fn found<'a>(findings: &'a [Finding], id: &str) -> Option<&'a Finding> {
    findings.iter().find(|f| f.id == id)
}

fn fixture(name: &str) -> Snapshot {
    let text =
        std::fs::read_to_string(format!("{}/../../fixtures/{name}", env!("CARGO_MANIFEST_DIR"))).expect("fixture");
    serde_json::from_str(&text).expect("snapshot")
}

// C5: the demo database has RCSI off and 15 ms of lock waits in 4.6 hours: advice for later, not Medium.
#[test]
fn rcsi_is_medium_only_with_reads_blocked() {
    let s = fixture("mssql-demo.json");
    let f = found(&mssql::check(&s), "config:read-committed-snapshot").cloned().expect("finding");
    assert_eq!(f.severity, Severity::Low);
    assert!(f.title.contains("reads would wait for writes"), "{}", f.title);

    // A read that waited minutes on locks is what RCSI removes.
    let mut s = fixture("mssql-demo.json");
    let w = s.workload.as_mut().unwrap();
    let read = w.queries.iter_mut().find(|q| q.text.starts_with("SELECT")).unwrap();
    read.waits.push(WaitStat { category: "Lock".into(), wait_type: None, wait_ms: 300_000.0, tasks: None });
    w.total_time_ms += 300_000.0;
    let f = found(&mssql::check(&s), "config:read-committed-snapshot").cloned().unwrap();
    assert_eq!(f.severity, Severity::Medium);
    assert!(f.summary.contains("reads waited 5 min on locks"), "{}", f.summary);
    // With snapshot isolation already allowed, versions are kept anyway.
    set(&mut s, "snapshot_isolation_state_desc", "ON");
    let f = found(&mssql::check(&s), "config:read-committed-snapshot").cloned().unwrap();
    assert!(f.fix.unwrap().cost.unwrap().contains("ALLOW_SNAPSHOT_ISOLATION is already on"));
}

// Demos MS-12: a read blocked a minute in 6 days is little time but 21% of a quiet database's: said by its share
// as well as its amount, with the read named and what row versioning would do for it, never "costs little".
#[test]
fn rcsi_low_is_worded_by_its_share_as_well_as_its_time() {
    let mut s = fixture("mssql-demo.json");
    let w = s.workload.as_mut().unwrap();
    w.window_hours = 146.66;
    let read = w.queries.iter_mut().find(|q| q.text.starts_with("SELECT")).unwrap();
    read.waits.push(WaitStat { category: "Lock".into(), wait_type: None, wait_ms: 71_692.0, tasks: None });
    (read.first_seen, read.last_seen, read.trend) = (None, None, None);
    w.total_time_ms = 335_668.0;
    let f = found(&mssql::check(&s), "config:read-committed-snapshot").cloned().unwrap();
    assert_eq!(f.severity, Severity::Low, "about 12 s a day");
    assert!(
        f.summary.contains("reads waited 1 min on locks held by writes (nearly all of it a read from"),
        "{}",
        f.summary
    );
    assert!(f.summary.contains("21% of database time, but only about 11.7 s a day"), "{}", f.summary);
    assert!(f.summary.contains("those reads would read the last committed version"), "{}", f.summary);
    assert!(!f.summary.contains("costs little"), "{}", f.summary);
    // No statement recorded any wait: nothing says the reads didn't wait.
    let mut s = fixture("mssql-demo.json");
    for q in &mut s.workload.as_mut().unwrap().queries {
        q.waits.clear();
    }
    let f = found(&mssql::check(&s), "config:read-committed-snapshot").cloned().unwrap();
    assert!(f.summary.contains("isn't known: the query history records no waits"), "{}", f.summary);
    assert_eq!(f.verify, vec!["Re-scan once the change has taken effect: this finding is gone.".to_string()]);
}

// C6: the threshold is moot where nothing goes parallel; RDS words it as its parameter group.
#[test]
fn cost_threshold_only_where_plans_can_go_parallel() {
    let mut s = fixture("mssql-demo.json");
    let f = found(&mssql::check(&s), "config:cost-threshold-for-parallelism").cloned().unwrap();
    assert!(f.fix.as_ref().unwrap().sql.as_deref().unwrap().contains("show advanced options"));
    set(&mut s, "MAXDOP", "1");
    assert!(found(&mssql::check(&s), "config:cost-threshold-for-parallelism").is_none());
    set(&mut s, "MAXDOP", "0");
    set(&mut s, "max degree of parallelism", "1");
    assert!(found(&mssql::check(&s), "config:cost-threshold-for-parallelism").is_none());
    set(&mut s, "max degree of parallelism", "0");
    s.server.cloud = Some("Amazon RDS".into());
    let fix = found(&mssql::check(&s), "config:cost-threshold-for-parallelism").cloned().unwrap().fix.unwrap();
    assert!(fix.sql.is_none() && fix.summary.contains("DB parameter group"), "{}", fix.summary);
}

// C20: Azure SQL reports version 12.0 whatever it runs; the level it offers comes from the service.
#[test]
fn azure_compatibility_level_is_not_read_from_the_version() {
    let mut s = fixture("mssql-demo.json");
    s.server.cloud = Some("Azure SQL Database".into());
    s.server.version = "12.0.2000.8".into();
    set(&mut s, "compatibility_level", "150");
    assert!(found(&mssql::check(&s), "config:compatibility-level").is_none(), "150 is one step behind 160");
    set(&mut s, "compatibility_level", "130");
    let f = found(&mssql::check(&s), "config:compatibility-level").cloned().unwrap();
    assert!(f.fix.unwrap().sql.unwrap().contains("COMPATIBILITY_LEVEL = 160"));
    assert!(f.summary.starts_with("Azure SQL Database supports level 160"), "{}", f.summary);
}

// C20: some products need automatic statistics creation off.
#[test]
fn auto_create_statistics_off_is_medium_with_the_vendor_caveat() {
    let mut s = fixture("mssql-demo.json");
    set(&mut s, "is_auto_create_stats_on", "0");
    let f = found(&mssql::check(&s), "config:auto_create_statistics").cloned().unwrap();
    assert_eq!(f.severity, Severity::Medium);
    assert!(f.caveats[0].contains("SharePoint"));
}

// C9: read-only by design (a secondary replica) isn't a size problem; the size limit gets more room, never less.
#[test]
fn query_store_read_only_says_why() {
    let mut s = fixture("mssql-demo.json");
    set(&mut s, "query_store_options.actual_state_desc", "READ_ONLY");
    set(&mut s, "query_store_options.readonly_reason", "8");
    assert!(found(&mssql::check(&s), "config:query-store").is_none());
    set(&mut s, "query_store_options.readonly_reason", "65536");
    set(&mut s, "query_store_options.max_storage_size_mb", "4096");
    let f = found(&mssql::check(&s), "config:query-store").cloned().unwrap();
    assert!(f.summary.contains("reached its size limit"), "{}", f.summary);
    assert!(f.fix.unwrap().sql.unwrap().contains("MAX_STORAGE_SIZE_MB = 8192"));
    set(&mut s, "query_store_options.readonly_reason", "131072");
    let f = found(&mssql::check(&s), "config:query-store").cloned().unwrap();
    assert!(f.fix.unwrap().sql.unwrap().contains("QUERY_CAPTURE_MODE = AUTO"));
}

// C7: managed services refuse ALTER SYSTEM and SET GLOBAL; the advice names their control instead.
#[test]
fn settings_on_managed_services_name_the_platform_control() {
    let mut pg = snapshot_with(Engine::Postgres);
    set(&mut pg, "track_io_timing", "off");
    let fix = found(&postgres::check(&pg), "config:track_io_timing").cloned().unwrap().fix.unwrap();
    assert!(fix.sql.unwrap().starts_with("ALTER SYSTEM SET track_io_timing"));
    pg.server.cloud = Some("Amazon RDS".into());
    let fix = found(&postgres::check(&pg), "config:track_io_timing").cloned().unwrap().fix.unwrap();
    assert!(fix.sql.is_none() && fix.summary.contains("DB parameter group"), "{}", fix.summary);

    let mut my = snapshot_with(Engine::Mysql);
    set(&mut my, "slow_query_log", "OFF");
    set(&mut my, "log_output", "FILE");
    let fix = found(&mysql::check(&my), "config:slow_query_log").cloned().unwrap().fix.unwrap();
    assert!(fix.sql.as_deref().unwrap().contains("slow_query_log = 'ON'"), "{:?}", fix.sql);
    my.server.cloud = Some("Google Cloud SQL".into());
    let fix = found(&mysql::check(&my), "config:slow_query_log").cloned().unwrap().fix.unwrap();
    assert!(fix.sql.is_none());
    assert!(
        fix.summary.contains("Set slow_query_log to ON, long_query_time to 1 and log_output to FILE,TABLE in the instance's database flags"),
        "{}",
        fix.summary
    );
    // MY-10: MySQL's lock time is the same either way; MariaDB's, written to a table, is the whole logged run.
    assert_eq!(fix.cost, None);
    my.server.cloud = None;
    my.server.product = "MariaDB".into();
    let fix = found(&mysql::check(&my), "config:slow_query_log").cloned().unwrap().fix.unwrap();
    let cost = fix.cost.unwrap_or_default();
    assert!(
        cost.starts_with("Written to a table (TABLE in log_output), MariaDB counts the whole run of each statement it logs as that statement's lock time"),
        "{cost}"
    );
    assert!(cost.contains("Leaving TABLE out keeps lock time real"), "{cost}");
}

fn buffer_pool_snapshot(hit: f64, reads: f64) -> Snapshot {
    let mut s = snapshot_with(Engine::Mysql);
    for t in &mut s.tables {
        t.total_bytes = Some(40.0 * 1024.0 * 1024.0 * 1024.0);
    }
    set(&mut s, "innodb_buffer_pool_size", &(8u64 * 1024 * 1024 * 1024).to_string());
    s.pressure = Some(Pressure {
        uptime_hours: Some(72.0),
        signals: vec![
            Signal::new("buffer-pool-hit", "hit", hit, SignalUnit::Percent, SignalScope::SinceStartup),
            Signal::new("buffer-pool-reads", "reads", reads, SignalUnit::PerSecond, SignalScope::SinceStartup),
        ],
        ..Pressure::default()
    });
    s
}

// C8: a large, mostly cold schema served from memory 99.9% of the time needs no bigger pool; when one is needed,
// no size is proposed without knowing the server's memory.
#[test]
fn buffer_pool_is_graded_on_misses_not_data_size() {
    assert!(mysql::buffer_pool(&buffer_pool_snapshot(99.9, 3.0)).is_none());
    let f = mysql::buffer_pool(&buffer_pool_snapshot(90.0, 400.0)).unwrap();
    assert_eq!(f.severity, Severity::Low);
    assert!(f.fix.as_ref().unwrap().sql.is_none());
    assert_eq!(mysql::buffer_pool(&buffer_pool_snapshot(97.0, 400.0)).unwrap().severity, Severity::Low);
    // MyISAM tables (a primary key that isn't clustered) have no pages in the buffer pool.
    let mut s = buffer_pool_snapshot(90.0, 400.0);
    for i in &mut s.indexes {
        i.is_clustered = false;
    }
    for t in &mut s.tables {
        t.has_primary_key = true;
    }
    assert!(mysql::buffer_pool(&s).is_none());
}

// C9: with size-based cleanup on AUTO, Query Store trims itself at 90% and never fills.
#[test]
fn a_query_store_that_cleans_itself_is_never_too_full() {
    let mut s = snapshot_with(Engine::Mssql);
    set(&mut s, "query_store_options.current_storage_size_mb", "950");
    set(&mut s, "query_store_options.max_storage_size_mb", "1000");
    set(&mut s, "query_store_options.size_based_cleanup_mode_desc", "AUTO");
    assert!(found(&mssql::check(&s), "config:query-store-size").is_none());
    set(&mut s, "query_store_options.size_based_cleanup_mode_desc", "OFF");
    assert!(found(&mssql::check(&s), "config:query-store-size").is_some());
}

// C6: MAXDOP 0 is Microsoft's own advice up to 8 processors; above that the cap fits one NUMA node.
#[test]
fn maxdop_zero_matters_only_on_a_bigger_server() {
    let mut s = snapshot_with(Engine::Mssql);
    set(&mut s, "max degree of parallelism", "0");
    set(&mut s, "MAXDOP", "0");
    s.pressure = None;
    assert!(found(&mssql::check(&s), "config:maxdop").is_none(), "unknown processor count: nothing to judge");
    s.pressure = Some(Pressure { cpus: Some(4.0), numa_nodes: Some(1.0), ..Pressure::default() });
    assert!(found(&mssql::check(&s), "config:maxdop").is_none(), "4 processors: MAXDOP 0 is fine");
    s.pressure = Some(Pressure { cpus: Some(24.0), numa_nodes: Some(2.0), ..Pressure::default() });
    let f = found(&mssql::check(&s), "config:maxdop").cloned().unwrap();
    assert!(f.title.contains("all 24 processors"), "{}", f.title);
    assert!(f.fix.unwrap().sql.unwrap().contains("MAXDOP = 8"));
    s.pressure = Some(Pressure { cpus: Some(12.0), numa_nodes: Some(2.0), ..Pressure::default() });
    assert!(
        found(&mssql::check(&s), "config:maxdop").cloned().unwrap().fix.unwrap().sql.unwrap().contains("MAXDOP = 6")
    );
}

// C16: only big tables that churn and follow the server's factor; the title says the factor in use.
#[test]
fn autovacuum_scale_skips_append_only_tables_and_their_own_settings() {
    let mut s = snapshot_with(Engine::Postgres);
    set(&mut s, "autovacuum_vacuum_scale_factor", "0.3");
    for t in &mut s.tables {
        t.rows = Some(1_000.0);
    }
    let t = &mut s.tables[0];
    t.rows = Some(20_000_000.0);
    t.updates = Some(0.0);
    t.deletes = Some(0.0);
    assert!(found(&postgres::check(&s), "config:autovacuum-scale").is_none(), "append-only: no dead rows to wait for");
    s.tables[0].updates = Some(2_000_000.0);
    let f = found(&postgres::check(&s), "config:autovacuum-scale").cloned().unwrap();
    assert!(f.title.starts_with("Autovacuum waits for 30% of"), "{}", f.title);
    s.tables[0].storage =
        Some(TableStorage { options: vec!["autovacuum_vacuum_scale_factor=0.01".into()], ..Default::default() });
    assert!(found(&postgres::check(&s), "config:autovacuum-scale").is_none(), "it has its own factor");
}

// C7: ALTER SYSTEM is refused on managed PostgreSQL; never propose a smaller pg_stat_statements.max.
#[test]
fn postgres_settings_are_worded_where_they_run() {
    let mut s = snapshot_with(Engine::Postgres);
    set(&mut s, "autovacuum", "off");
    set(&mut s, "pg_stat_statements.dealloc", "40");
    set(&mut s, "pg_stat_statements.max", "50000");
    let out = postgres::check(&s);
    assert!(
        found(&out, "config:autovacuum")
            .unwrap()
            .fix
            .as_ref()
            .unwrap()
            .sql
            .as_deref()
            .unwrap()
            .starts_with("ALTER SYSTEM SET autovacuum")
    );
    let max = found(&out, "config:pg_stat_statements.max").unwrap().fix.clone().unwrap();
    assert!(max.sql.unwrap().contains("pg_stat_statements.max = 100000"));
    s.server.cloud = Some("Azure Database for PostgreSQL".into());
    let fix = found(&postgres::check(&s), "config:autovacuum").unwrap().fix.clone().unwrap();
    assert!(fix.sql.is_none() && fix.summary.contains("server parameters"), "{}", fix.summary);
}

// C7: with performance_schema off on RDS, the fix is the parameter group, not SET GLOBAL and my.cnf.
#[test]
fn mysql_restart_settings_on_a_managed_server() {
    let mut s = snapshot_with(Engine::Mysql);
    set(&mut s, "performance_schema", "OFF");
    set(&mut s, "slow_query_log", "OFF");
    let fix = found(&mysql::check(&s), "config:performance_schema").unwrap().fix.clone().unwrap();
    assert!(fix.sql.as_deref().unwrap().contains("[mysqld]\nperformance_schema = ON"));
    s.server.cloud = Some("Amazon RDS".into());
    let fix = found(&mysql::check(&s), "config:performance_schema").unwrap().fix.clone().unwrap();
    assert!(fix.sql.is_none(), "{:?}", fix.sql);
    assert!(fix.summary.contains("in the instance's DB parameter group"), "{}", fix.summary);
    assert!(fix.summary.contains("after a reboot"));
}
