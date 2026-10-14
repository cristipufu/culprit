//! False flags in the pressure, configuration and maintenance checks, through the whole pipeline: what each check may
//! claim from the data a scan has, and the refine rules that keep one finding per cause.

use culprit_checks::analyze;
use culprit_core::{
    CleanupHolder, Finding, OpenTransaction, PlanCacheUse, Pressure, Report, Setting, Severity, Signal, SignalScope,
    SignalUnit, Snapshot, StatisticsInfo, TableStorage, WaitStat,
};

fn snapshot(name: &str) -> Snapshot {
    let text =
        std::fs::read_to_string(format!("{}/../../fixtures/{name}", env!("CARGO_MANIFEST_DIR"))).expect("fixture");
    serde_json::from_str(&text).expect("snapshot")
}

fn report_snapshot(name: &str) -> Snapshot {
    let text = std::fs::read_to_string(format!("{}/../../fixtures/{name}.report.json", env!("CARGO_MANIFEST_DIR")))
        .expect("fixture");
    serde_json::from_str::<Report>(&text).expect("report").snapshot
}

fn find<'a>(findings: &'a [Finding], id: &str) -> Option<&'a Finding> {
    findings.iter().find(|f| f.id == id)
}

fn set(s: &mut Snapshot, name: &str, value: &str) {
    s.settings.retain(|x| !x.name.eq_ignore_ascii_case(name));
    s.settings.push(Setting { name: name.into(), value: value.into(), default_value: None, scope: "server".into() });
}

// C12: a table at its autovacuum trigger is between two runs, not behind; the demo's archive.wide (2.8 times its live
// rows dead) is, and its stale statistics are the same backlog: one finding, with ANALYZE in it.
#[test]
fn dead_rows_wait_for_the_tables_own_trigger_and_absorb_stale_statistics() {
    let s = report_snapshot("pg-demo");
    let report = analyze(s.clone());
    let wide = find(&report.findings, "dead-rows:archive.wide").expect("archive.wide is behind");
    assert!(wide.summary.contains("Autovacuum should visit it at about"), "{}", wide.summary);
    assert!(wide.summary.contains("Its statistics are behind too"), "{}", wide.summary);
    assert!(find(&report.findings, "stale-stats:archive.wide").is_none(), "absorbed by dead-rows");
    assert!(wide.fix.as_ref().unwrap().sql.as_deref().unwrap().contains("VACUUM (ANALYZE)"));

    // 25% of a busy table dead is just past the default trigger (50 + 20%): autovacuum's rhythm, not a backlog.
    let mut s = report_snapshot("pg-demo");
    let t = s.tables.iter_mut().find(|t| t.name == "orders").unwrap();
    t.dead_rows = Some(t.rows.unwrap() * 0.25);
    assert!(find(&analyze(s.clone()).findings, "dead-rows:public.orders").is_none());
    // With its own lower trigger, the same dead rows are well past it.
    let t = s.tables.iter_mut().find(|t| t.name == "orders").unwrap();
    t.storage = Some(TableStorage {
        options: vec!["autovacuum_vacuum_scale_factor=0.05".into()],
        ..t.storage.clone().unwrap_or_default()
    });
    assert!(find(&analyze(s).findings, "dead-rows:public.orders").is_some());
}

fn stat(table: &str, rows: f64, modifications: f64) -> StatisticsInfo {
    StatisticsInfo {
        schema: "shop".into(),
        table: table.into(),
        name: format!("st_{table}"),
        last_updated: Some("2026-10-01T00:00:00Z".into()),
        rows: Some(rows),
        rows_sampled: Some(rows),
        modifications: Some(modifications),
        auto_created: true,
    }
}

// C13: SQL Server refreshes a statistic itself at MIN(500 + 20%, √(1000 n)); one still behind with AUTO_UPDATE on is one
// no query has compiled with since, which is Low.
#[test]
fn sql_server_stale_statistics_use_its_own_threshold() {
    let mut s = snapshot("mssql-demo.json");
    // 12K rows: SQL Server waits for 2,900 changes; 2,600 (21%) is fresh by its measure.
    s.statistics = vec![stat("products", 12_000.0, 2_600.0)];
    assert!(find(&analyze(s.clone()).findings, "stale-stats:shop.products").is_none());

    s.statistics = vec![stat("products", 20_000.0, 15_000.0)];
    let f = find(&analyze(s.clone()).findings, "stale-stats:shop.products").cloned().expect("past the threshold");
    assert_eq!(f.severity, Severity::Low);
    assert!(f.caveats.iter().any(|c| c.contains("no query has used it since")), "{:?}", f.caveats);
    // With automatic updates off it stays stale for every query that reads the table.
    set(&mut s, "is_auto_update_stats_on", "0");
    let f = find(&analyze(s).findings, "stale-stats:shop.products").cloned().unwrap();
    assert!(!f.caveats.iter().any(|c| c.contains("no query has used it since")));
}

// C17: autovacuum freezes on its own from autovacuum_freeze_max_age; an age within twice that is freezing at work.
#[test]
fn xid_age_is_judged_against_autovacuum_freeze_max_age() {
    let mut s = report_snapshot("pg-demo");
    set(&mut s, "datfrozenxid_age", "700000000");
    set(&mut s, "autovacuum_freeze_max_age", "400000000");
    assert!(analyze(s.clone()).findings.iter().all(|f| f.check != "xid-age"), "under 2 × 400M");
    set(&mut s, "autovacuum_freeze_max_age", "200000000");
    let report = analyze(s);
    let f = report.findings.iter().find(|f| f.check == "xid-age").expect("past 2 × 200M");
    assert!(f.summary.contains("autovacuum_freeze_max_age (200.0M), and this is 3.5 times that"), "{}", f.summary);
}

fn with_pressure(mut s: Snapshot, signals: Vec<Signal>) -> Snapshot {
    s.pressure = Some(Pressure { uptime_hours: Some(72.0), signals, ..Pressure::default() });
    s
}

fn signal(key: &str, value: f64) -> Signal {
    Signal::new(key, key, value, SignalUnit::Percent, SignalScope::SinceStartup)
}

// C3: compiles and single-use plans are one cause (literal values); the plan-cache finding keeps the compile load.
#[test]
fn compile_pressure_and_the_plan_cache_are_one_finding() {
    let mut s = with_pressure(
        snapshot("mssql-demo.json"),
        vec![
            signal("compiles-per-batch", 45.0).graded("under 10% is healthy", 10.0, 25.0, true),
            Signal::new("batch-requests", "b", 200.0, SignalUnit::PerSecond, SignalScope::SinceStartup),
        ],
    );
    s.plan_cache = Some(PlanCacheUse {
        total_plans: 40_000.0,
        total_bytes: 2.0 * 1024.0 * 1024.0 * 1024.0,
        single_use_plans: 30_000.0,
        single_use_bytes: 1.2 * 1024.0 * 1024.0 * 1024.0,
    });
    let report = analyze(s);
    assert!(report.findings.iter().all(|f| f.check != "compile-pressure"));
    let cache = find(&report.findings, "query-family:plan-cache").expect("plan cache");
    assert!(
        cache.summary.contains("The CPU side of the same cause: SQL Server compiles a new plan for 45%"),
        "{}",
        cache.summary
    );
    assert!(cache.metrics.iter().any(|m| m.label == "Compiles per 100 batches"));
    assert_eq!(cache.severity, Severity::Medium);
}

// C2: an abandoned slot that keeps WAL and holds VACUUM back is one finding, the slot's.
#[test]
fn a_slot_holding_vacuum_back_joins_its_replica_finding() {
    let mut s = report_snapshot("pg-demo");
    let slot = s.pressure.as_ref().unwrap().replicas.iter().find(|r| r.name == "analytics_slot").cloned();
    assert!(slot.is_some(), "the demo keeps an inactive slot");
    s.pressure.as_mut().unwrap().oldest_transaction = Some(OpenTransaction {
        session: "analytics_slot".into(),
        state: Some("inactive slot".into()),
        holder: Some(CleanupHolder::ReplicationSlot),
        xid_age: Some(5_000_000.0),
        ..OpenTransaction::default()
    });
    let report = analyze(s);
    assert!(report.findings.iter().all(|f| f.check != "held-cleanup"));
    let lag = find(&report.findings, "replica-lag:analytics_slot").expect("replica finding");
    assert!(lag.summary.contains("It also holds VACUUM back by 5.0M transactions"), "{}", lag.summary);
}

// C10: waiting on the application explains a heavy query only when it is most of its time; an over-sized grant slows
// other queries, not its own, so the heavy-query finding stays.
#[test]
fn a_heavy_query_stays_unless_another_finding_explains_most_of_it() {
    let base = snapshot("mssql-demo.json");
    let heavy = analyze(base.clone());
    let Some(h) = heavy.findings.iter().find(|f| f.check == "heavy-query") else {
        return; // the demo has no heavy query to keep
    };
    let id = h.queries[0].id.clone();
    let with_network = |share: f64| {
        let mut s = base.clone();
        let q = s.workload.as_mut().unwrap().queries.iter_mut().find(|q| q.id == id).unwrap();
        q.waits = vec![WaitStat {
            category: "Network".into(),
            wait_type: None,
            wait_ms: q.total_time_ms * share,
            tasks: None,
        }];
        analyze(s)
    };
    let r = with_network(0.3);
    assert!(find(&r.findings, &format!("heavy-query:{id}")).is_some(), "30% waiting on the app: the rest is its own");
    let r = with_network(0.8);
    assert!(find(&r.findings, &format!("heavy-query:{id}")).is_none(), "80%: the network finding explains it");
    let net = find(&r.findings, &format!("network-wait:{id}")).expect("the network finding");
    assert!(net.metrics.iter().any(|m| m.label == "Waiting on the app" && m.value >= 0.5));
}
