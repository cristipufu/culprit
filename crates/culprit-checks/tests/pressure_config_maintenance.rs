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
    let verify = wide.verify.join(" ");
    assert!(verify.contains("dead-row count should fall below the table's autovacuum trigger"), "{verify}");
    assert!(
        verify.contains("last analyze time should be newer") && verify.contains("changes since analyze should fall"),
        "{verify}"
    );
    assert!(verify.contains("after representative writes"), "{verify}");

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
    // No plan the scan captured names the statistic: the next compile refreshes it, said as that, not as "no query
    // has used it" (a cached plan reused needn't compile).
    let next = "SQL Server refreshes a statistic this far behind when a query next compiles with it";
    assert!(f.caveats.iter().any(|c| c.contains(next)), "{:?}", f.caveats);
    assert!(!f.caveats.iter().any(|c| c.contains("no query has used it")), "{:?}", f.caveats);
    // With automatic updates off it stays stale for every query that reads the table.
    set(&mut s, "is_auto_update_stats_on", "0");
    let f = find(&analyze(s).findings, "stale-stats:shop.products").cloned().unwrap();
    assert!(!f.caveats.iter().any(|c| c.contains(next)));
}

// A staging SQL Server reading: "heavy queries in this workload read this table" for a table only tiny inserts and a monitoring
// query's text matched, next to the caveat that no query has compiled with its statistics since they fell behind.
#[test]
fn stale_statistics_name_heavy_reads_only_where_there_are_some() {
    let heavy = "Heavy queries in this workload read this table.";
    let mut s = snapshot("mssql-demo.json");
    s.statistics = vec![stat("products", 20_000.0, 15_000.0)];
    // AUTO_UPDATE_STATISTICS on: stale means nothing compiled with it, so nothing is said to read it.
    let f = find(&analyze(s.clone()).findings, "stale-stats:shop.products").cloned().unwrap();
    assert!(!f.summary.contains(heavy), "{}", f.summary);
    // Off: the demo's lookup of products by id, 2% of database time, reads it.
    set(&mut s, "is_auto_update_stats_on", "0");
    let f = find(&analyze(s.clone()).findings, "stale-stats:shop.products").cloned().unwrap();
    assert!(f.summary.contains(heavy), "{}", f.summary);
    assert_eq!(f.severity, Severity::Medium);
    // An insert into it, and a monitoring query whose text names it, read nothing worth the sentence.
    let w = s.workload.as_mut().unwrap();
    let insert = w.queries.iter_mut().find(|q| q.id == "6").unwrap();
    insert.text = "INSERT INTO [shop].[products] ([sku], [name]) VALUES (@P1, @P2)".into();
    insert.plan = None;
    let monitor = w.queries.iter_mut().find(|q| q.id == "5").unwrap();
    monitor.text = "SELECT s.name FROM sys.stats s WHERE OBJECT_NAME(s.object_id) LIKE '%products%'".into();
    monitor.plan = None;
    let f = find(&analyze(s).findings, "stale-stats:shop.products").cloned().unwrap();
    assert!(!f.summary.contains(heavy), "{}", f.summary);
    assert_eq!(f.severity, Severity::Low);
}

// The aptabase reading: pg_stat_statements' 5,000 entries and their evictions are the server's. A database holding 22
// of them isn't the one sending many distinct statements, and isn't graded on the server's evictions.
#[test]
fn pg_stat_statements_evictions_are_the_servers_when_this_database_has_few_entries() {
    let evicting = |entries: f64| {
        let mut s = snapshot("postgres-demo.json");
        set(&mut s, "pg_stat_statements.dealloc", "15000");
        set(&mut s, "pg_stat_statements.max", "5000");
        s.workload.as_mut().unwrap().query_count = entries;
        find(&analyze(s).findings, "config:pg_stat_statements.max").cloned().expect("the evictions")
    };
    let f = evicting(22.0);
    assert_eq!(f.severity, Severity::Low, "15K evictions in 5 hours, but not this database's");
    assert!(f.summary.contains("This database has only 22 of them"), "{}", f.summary);
    // 22 statements now against a count kept since the reset: whose they were is a likelihood, not a fact.
    assert!(f.summary.contains("likely come from other databases"), "{}", f.summary);
    assert!(!f.summary.contains("the app sends"), "{}", f.summary);
    let f = evicting(4_000.0);
    assert_eq!(f.severity, Severity::Medium);
    assert!(f.summary.contains("the app sends many distinct statements"), "{}", f.summary);
}

// The aptabase reading: random_page_cost 4 on RDS. The ratio is to seq_page_cost, not a fixed "four times"; the change
// is the platform's (`setting_change`), and what the planner does with it is a possibility nothing in the scan showed.
#[test]
fn random_page_cost_says_its_ratio_and_changes_where_the_server_runs() {
    let mut s = snapshot("postgres-demo.json");
    s.server.cloud = Some("Amazon RDS".into());
    set(&mut s, "random_page_cost", "4");
    set(&mut s, "seq_page_cost", "1");
    let f = find(&analyze(s.clone()).findings, "config:random_page_cost").cloned().expect("random_page_cost");
    assert!(
        f.summary.contains(
            "random_page_cost is 4, which tells the planner that a random read costs 4 times a sequential one"
        ),
        "{}",
        f.summary
    );
    assert!(f.summary.contains("may choose a sequential scan") && !f.summary.contains("more often than it should"));
    let fix = f.fix.unwrap();
    // A setting any owner sets per database: this database first, the parameter group for every database.
    assert_eq!(fix.sql.as_deref(), Some("ALTER DATABASE demo SET random_page_cost = 1.1;"));
    assert!(fix.summary.contains("DB parameter group"), "{}", fix.summary);
    set(&mut s, "random_page_cost", "8");
    set(&mut s, "seq_page_cost", "2");
    let f = find(&analyze(s.clone()).findings, "config:random_page_cost").cloned().expect("a ratio of 4");
    assert!(
        f.summary.contains(
            "random_page_cost is 8 and seq_page_cost 2, which tells the planner that a random read costs 4 times"
        ),
        "{}",
        f.summary
    );
    // Raised together, the ratio the planner weighs is 2.
    set(&mut s, "random_page_cost", "4");
    assert!(find(&analyze(s).findings, "config:random_page_cost").is_none());
}

// The aptabase readings: `dealloc` counts rounds, each evicting 5% of the limit (at least 10), so "15K evictions" was
// about 3.8M entries. The count runs from the statistics reset and outlives restarts and a higher limit: the finding
// says since when, and its verify step is the count no longer growing, not the finding going away.
#[test]
fn pg_stat_statements_evictions_are_rounds_counted_since_the_reset() {
    let mut s = snapshot("postgres-demo.json");
    set(&mut s, "pg_stat_statements.dealloc", "15397");
    set(&mut s, "pg_stat_statements.max", "5000");
    s.workload.as_mut().unwrap().query_count = 22.0;
    s.workload.as_mut().unwrap().window_start = Some("2023-04-17T10:00:21Z".into());
    let f = find(&analyze(s.clone()).findings, "config:pg_stat_statements.max").cloned().expect("the evictions");
    assert_eq!(f.title, "pg_stat_statements ran out of room 15K times, evicting about 3.8M entries");
    assert!(f.summary.contains("It keeps at most 5000 distinct statements for the whole server"), "{}", f.summary);
    assert!(f.summary.contains("Since its statistics were reset on Apr 17, 2023"), "{}", f.summary);
    assert!(f.summary.contains("the least-used 5% of them, 250 entries at a limit of 5000"), "{}", f.summary);
    let metric = |label: &str| f.metrics.iter().find(|m| m.label == label).map(|m| m.value);
    assert_eq!(metric("Eviction rounds"), Some(15_397.0));
    assert_eq!(metric("Entries evicted (estimate)"), Some(15_397.0 * 250.0));
    assert!(metric("Evictions").is_none(), "a round isn't one eviction");
    assert!(f.verify.iter().any(|v| v.contains("the eviction rounds (15397 now) should not grow")), "{:?}", f.verify);
    assert!(f.caveats.iter().any(|c| c.contains("neither a restart nor a higher limit clears it")), "{:?}", f.caveats);
    // A small limit still evicts at least 10 a round; without the limit, only that floor is known.
    set(&mut s, "pg_stat_statements.max", "100");
    let f = find(&analyze(s.clone()).findings, "config:pg_stat_statements.max").cloned().unwrap();
    assert!(f.title.ends_with("evicting about 154K entries"), "{}", f.title);
    s.settings.retain(|x| x.name != "pg_stat_statements.max");
    let f = find(&analyze(s).findings, "config:pg_stat_statements.max").cloned().unwrap();
    assert!(f.title.ends_with("evicting at least 154K entries"), "{}", f.title);
    assert!(!f.summary.contains("likely come from") && !f.summary.contains("the app sends"), "{}", f.summary);
    assert_eq!(f.severity, Severity::Low, "whose statements fill it is unknown without the limit");
}

// The count outlives restarts and a higher limit, so after the fix the lifetime count could never clear: the previous
// scan's count tells the rounds that go on now, and a day without one closes the finding.
#[test]
fn pg_stat_statements_evictions_are_graded_on_the_rounds_since_the_previous_scan() {
    let mut s = snapshot("postgres-demo.json");
    set(&mut s, "pg_stat_statements.dealloc", "15397");
    set(&mut s, "pg_stat_statements.max", "5000");
    s.workload.as_mut().unwrap().query_count = 4_000.0;
    s.workload.as_mut().unwrap().window_start = Some("2023-04-17T10:00:21Z".into());
    let mut before = s.clone();
    let id = "config:pg_stat_statements.max";
    let with_before = |before: &Snapshot, s: &Snapshot, dealloc: &str| {
        let mut before = before.clone();
        set(&mut before, "pg_stat_statements.dealloc", dealloc);
        let mut s = s.clone();
        s.evictions_before = culprit_core::CountBaseline::from_setting(&before, &s, "pg_stat_statements.dealloc");
        analyze(s)
    };

    // No round in the two days since the previous scan: the table has had room, so no finding.
    before.taken_at = "2026-10-09T11:54:21Z".into();
    assert!(find(&with_before(&before, &s, "15397").findings, id).is_none());

    // 20 rounds since then: graded on those, 2.5K entries a day, not on the lifetime's.
    let r = with_before(&before, &s, "15377");
    let f = find(&r.findings, id).expect("rounds since the previous scan");
    assert_eq!(
        f.title,
        "pg_stat_statements ran out of room 20 times since Culprit's scan on Oct 9 at 11:54 UTC, evicting about 5.0K entries"
    );
    // The rate is the scans' (over a day or more of them), with the one since the reset beside it where the two differ
    // twofold, and not said a second time with the lifetime count.
    assert!(
        f.summary.contains(
            "20 of those rounds came since Culprit's scan on Oct 9 at 11:54 UTC: about 5.0K entries, about 2.5K a day (about "
        ),
        "{}",
        f.summary
    );
    assert!(f.summary.contains("a day since its statistics were reset)."), "{}", f.summary);
    assert!(
        f.summary.contains("it has run out of room 15K times, and each time it evicts the least-used 5% of them, 250 entries at a limit of 5000: about 3.8M entries in all."),
        "the lifetime count stays in the summary, without its rate: {}",
        f.summary
    );
    assert_eq!(f.severity, Severity::Medium);
    assert_eq!(
        f.metrics
            .iter()
            .find(|m| m.label == "Eviction rounds since Culprit's scan on Oct 9 at 11:54 UTC")
            .map(|m| m.value),
        Some(20.0)
    );
    // The room the change adds fills at the scans' rate.
    assert!(
        f.verify[0].contains("at about 2.5K new entries a day (the rate Culprit's scans measured it evicting them)"),
        "{:?}",
        f.verify
    );
    assert!(f.verify[0].contains("with none between them this finding is gone"), "{:?}", f.verify);

    // Two hours without a round says little: the finding stays, on the lifetime count.
    before.taken_at = "2026-10-11T09:54:21Z".into();
    let r = with_before(&before, &s, "15397");
    assert!(
        find(&r.findings, id)
            .expect("too soon to close")
            .title
            .starts_with("pg_stat_statements ran out of room 15K times")
    );

    // Statistics reset since the previous scan: the count is all new, read as it is.
    before.taken_at = "2026-10-09T11:54:21Z".into();
    before.workload.as_mut().unwrap().window_start = Some("2022-01-01T00:00:00Z".into());
    let r = with_before(&before, &s, "15397");
    assert!(
        find(&r.findings, id).expect("after a reset").title.starts_with("pg_stat_statements ran out of room 15K times")
    );
}

// The aptabase readings: after raising pg_stat_statements.max as advised, the count still holds the rounds from before
// it, which evicted 5% of the old limit. Counted at today's limit the entries would quadruple and the grade rise after
// the fix: a limit above its default counts them at the default's share, a floor, isn't graded on them, and says to
// compare two scans before raising it again.
#[test]
fn pg_stat_statements_evictions_after_the_limit_was_raised_are_a_floor() {
    let evicting = |default: &str| {
        let mut s = snapshot("postgres-demo.json");
        set(&mut s, "pg_stat_statements.dealloc", "15397");
        s.settings.retain(|x| x.name != "pg_stat_statements.max");
        s.settings.push(Setting {
            name: "pg_stat_statements.max".into(),
            value: "20000".into(),
            default_value: Some(default.into()),
            scope: "server".into(),
        });
        // This database's statements fill it: 6,000 of 20,000.
        s.workload.as_mut().unwrap().query_count = 6_000.0;
        find(&analyze(s).findings, "config:pg_stat_statements.max").cloned().expect("the evictions")
    };
    // At its default the limit was never raised: today's 5% is the estimate, and the grade stands.
    let f = evicting("20000");
    assert_eq!(f.title, "pg_stat_statements ran out of room 15K times, evicting about 15.4M entries");
    assert_eq!(f.severity, Severity::Medium);
    assert!(!f.caveats.iter().any(|c| c.contains("estimated at today's limit")), "{:?}", f.caveats);
    // Raised from 5000: the default's 250 a round is the floor.
    let f = evicting("5000");
    assert_eq!(f.title, "pg_stat_statements ran out of room 15K times, evicting at least 3.8M entries");
    assert_eq!(f.severity, Severity::Low, "the rounds may all be from before the raise");
    assert!(
        f.summary
            .contains("the least-used 5% of them: 1.0K entries at today's limit of 20000, 250 at its default of 5000"),
        "{}",
        f.summary
    );
    let metric = |label: &str| f.metrics.iter().find(|m| m.label == label).map(|m| m.value);
    assert_eq!(metric("Entries evicted (at least)"), Some(15_397.0 * 250.0));
    let fix = f.fix.clone().unwrap();
    assert!(fix.summary.starts_with("The limit is already above its default of 5000"), "{}", fix.summary);
    assert!(fix.summary.contains("compare the eviction rounds between two scans first"), "{}", fix.summary);
    assert!(f.caveats.iter().any(|c| c.contains("counted at the default's 250 a round, a floor")), "{:?}", f.caveats);
}

// The aptabase readings: config findings had no verify step. Each says what a re-scan shows once the change is in.
#[test]
fn every_config_finding_says_how_to_verify_it() {
    let mut pg = snapshot("postgres-demo.json");
    pg.server.cloud = Some("Amazon RDS".into());
    set(&mut pg, "random_page_cost", "4");
    set(&mut pg, "seq_page_cost", "1");
    set(&mut pg, "pg_stat_statements.dealloc", "15000");
    set(&mut pg, "pg_stat_statements.max", "5000");
    let report = analyze(pg);
    let rpc = find(&report.findings, "config:random_page_cost").expect("random_page_cost");
    assert!(rpc.verify.iter().any(|v| v.contains("random_page_cost reads 1.1")), "{:?}", rpc.verify);
    assert!(find(&report.findings, "config:pg_stat_statements.max").is_some_and(|f| !f.verify.is_empty()));
    let mut all = report.findings;
    for name in ["mssql-demo.json", "mysql-demo.json"] {
        all.extend(analyze(snapshot(name)).findings);
    }
    let config: Vec<&Finding> = all.iter().filter(|f| f.check == "config").collect();
    assert!(config.len() >= 4, "{:?}", config.iter().map(|f| &f.id).collect::<Vec<_>>());
    for f in config {
        assert!(!f.verify.is_empty(), "{} has no way to verify it", f.id);
    }
}

// Demos 9e: "They crowd the store" with 7 statements overstated it. A handful of statements costs little now; the
// crowding is what capture mode ALL does once one-off statements arrive.
#[test]
fn query_store_capture_is_worded_by_how_many_statements_it_has() {
    let s = snapshot("mssql-demo.json");
    let f = find(&analyze(s.clone()).findings, "config:query-store-capture").cloned().expect("capture mode ALL");
    assert_eq!(f.severity, Severity::Low);
    assert!(f.summary.contains("With 7 statements in the last 5 hours it costs little now"), "{}", f.summary);
    assert!(!f.summary.contains("They crowd the store"), "{}", f.summary);
    let mut busy = s;
    busy.workload.as_mut().unwrap().query_count = 12_000.0;
    let f = find(&analyze(busy).findings, "config:query-store-capture").cloned().unwrap();
    assert_eq!(f.severity, Severity::Medium);
    assert!(f.summary.contains("(12K statements in the last 5 hours). They crowd the store"), "{}", f.summary);
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

fn set_with_default(s: &mut Snapshot, name: &str, value: &str, default: &str) {
    s.settings.retain(|x| !x.name.eq_ignore_ascii_case(name));
    s.settings.push(Setting {
        name: name.into(),
        value: value.into(),
        default_value: Some(default.into()),
        scope: "server".into(),
    });
}

// Aptabase EU P4: after raising pg_stat_statements.max from 5,000 to 20,000, the 15,000 new slots fill first, at the
// ~3,000 entries a day the table evicted while full, and nothing is evicted for about 5 days whatever the statements
// do. One quiet day can't close the finding; the verify step says how long to wait, and why.
#[test]
fn pg_stat_statements_evictions_close_only_once_the_raised_room_has_had_time_to_fill() {
    let mut s = snapshot("postgres-demo.json");
    set(&mut s, "pg_stat_statements.dealloc", "15397");
    set_with_default(&mut s, "pg_stat_statements.max", "5000", "5000");
    let w = s.workload.as_mut().unwrap();
    w.query_count = 22.0;
    w.window_start = Some("2023-04-17T10:00:21Z".into());
    w.window_hours = 1_273.08 * 24.0;
    let id = "config:pg_stat_statements.max";

    // Before the change: 15,397 rounds of 250 over 1,273 days, about 3,000 entries a day; 15,000 entries of new room.
    let f = find(&analyze(s.clone()).findings, id).cloned().expect("the evictions");
    let verify = &f.verify[0];
    assert!(verify.starts_with("Re-scan once the change has taken effect, then 5 days later"), "{verify}");
    assert!(verify.contains("the eviction rounds (15397 now) should not grow"), "{verify}");
    assert!(verify.contains("at most 15K entries") && verify.contains("about 3.0K new entries a day"), "{verify}");
    assert!(verify.contains("a shorter quiet span proves nothing"), "{verify}");
    // EU P6: an entry is under half a kilobyte of shared memory; its text is in a file.
    let cost = f.fix.as_ref().and_then(|x| x.cost.clone()).unwrap();
    assert!(cost.contains("Under half a kilobyte") && cost.contains("9.8 MB for 20000 entries"), "{cost}");
    assert!(cost.contains("kept in a file on disk") && !cost.contains("2 KB"), "{cost}");

    // After the change: the limit reads 20,000 against its default of 5,000.
    set_with_default(&mut s, "pg_stat_statements.max", "20000", "5000");
    // The previous scan `days_quiet` days earlier counted the same rounds.
    let after = |s: &Snapshot, days_quiet: i64| {
        let mut before = s.clone();
        before.taken_at = chrono_like(&s.taken_at, -days_quiet);
        let mut s = s.clone();
        s.evictions_before = culprit_core::CountBaseline::from_setting(&before, &s, "pg_stat_statements.dealloc");
        analyze(s)
    };
    assert!(find(&after(&s, 2).findings, id).is_some(), "two quiet days: the new room is still filling");
    assert!(find(&after(&s, 4).findings, id).is_some(), "four quiet days: still under the ~5 the room takes");
    assert!(find(&after(&s, 6).findings, id).is_none(), "six quiet days: past the room's fill, the table has room");

    // A table that churns fast fills new room within a day: a day is enough, as before.
    s.workload.as_mut().unwrap().window_hours = 24.0;
    let f = find(&after(&s, 0).findings, id).cloned().expect("no previous scan");
    assert!(f.verify[0].starts_with("Re-scan once the change has taken effect, then a day later"), "{:?}", f.verify);
    assert!(find(&after(&s, 1).findings, id).is_none(), "a quiet day closes it when the room fills in hours");
}

/// `taken_at` moved by whole days, for a previous scan's timestamp (`2026-10-11T11:54:21Z` style).
fn chrono_like(taken_at: &str, days: i64) -> String {
    let epoch = culprit_core::format::epoch_seconds(taken_at).expect("a timestamp") as i64 + days * 86_400;
    let (day, secs) = (epoch.div_euclid(86_400), epoch.rem_euclid(86_400));
    // Civil date from days since 1970 (Howard Hinnant's algorithm).
    let z = day + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", secs / 3_600, secs % 3_600 / 60, secs % 60)
}

// MySQL demo MY-10: the slow query log's settings are dynamic, so its verify step names no restart; performance_schema
// is read only at startup, and its verify step says so.
#[test]
fn a_restart_is_named_only_where_the_setting_needs_one() {
    let s = snapshot("mysql-demo.json");
    let f = find(&analyze(s.clone()).findings, "config:slow_query_log").cloned().expect("the slow log is off");
    assert_eq!(f.verify, vec!["Re-scan once the change has taken effect: this finding is gone.".to_string()]);
    assert!(f.fix.as_ref().unwrap().summary.contains("long_query_time applies to new connections"), "{:?}", f.fix);
    let mut off = s;
    set(&mut off, "performance_schema", "OFF");
    let f = find(&analyze(off).findings, "config:performance_schema").cloned().expect("performance_schema off");
    assert!(f.verify[0].starts_with("Re-scan after the server has restarted with the change"), "{:?}", f.verify);
}

#[test]
fn mysql_and_mariadb_temp_limits_reach_existing_connections_only_when_the_app_sets_them() {
    let spill = |s: Snapshot| analyze(s).findings.into_iter().find(|f| f.check == "temp-spill").unwrap();
    for s in [snapshot("mysql-demo.json"), report_snapshot("mariadb-demo")] {
        let f = spill(s);
        let fix = f.fix.as_ref().unwrap();
        assert!(fix.summary.contains("Global values apply to new connections"), "{}", fix.summary);
        assert!(
            fix.summary.contains("reopen the application's existing connections")
                && fix.summary.contains("set these session variables on them"),
            "{}",
            fix.summary
        );
        assert!(f.verify[0].starts_with("Once the application's connections use the new limits"), "{:?}", f.verify);
    }
    // Before MySQL 8.0.28, the TempTable engine has only its server-wide RAM cap: no new session is needed for that.
    let mut shared = snapshot("mysql-demo.json");
    shared.server.version = "8.0.27".into();
    set(&mut shared, "internal_tmp_mem_storage_engine", "TempTable");
    set(&mut shared, "temptable_max_ram", "1073741824");
    let f = spill(shared);
    assert!(!f.fix.as_ref().unwrap().summary.contains("new connections"), "{:?}", f.fix);
    assert!(f.verify[0].starts_with("Once the change is live"), "{:?}", f.verify);
}

// Aptabase EU: scans by older versions counted the server's WAL write and sync time as 99.98% of statement time. A
// report read today puts it back with the rest, as a scan now does.
#[test]
fn old_postgres_scans_keep_the_servers_wal_time_out_of_statement_time() {
    let mut s = snapshot("postgres-demo.json");
    let wait = |category: &str, wait_type: &str, ms: f64| WaitStat {
        category: category.into(),
        wait_type: Some(wait_type.into()),
        wait_ms: ms,
        tasks: None,
    };
    s.waits = vec![
        wait("Log", "WAL write and sync (the whole server's, over the same window)", 4_131_617.0),
        wait("IO", "data file reads and writes", 750.0),
        wait("Other", "CPU, locks and untimed waits", 1_000.0),
    ];
    let waits = analyze(s.clone()).snapshot.waits;
    assert!(waits.iter().all(|w| w.category != "Log"), "{waits:?}");
    assert_eq!(waits[0].category, "Other");
    assert_eq!(waits[0].wait_ms, 4_132_617.0);
    assert_eq!(waits[1].wait_ms, 750.0);
    // Without disk time to set apart, the rest is all there is, which says nothing: no breakdown, as a scan now reads it.
    s.waits = vec![wait("Log", "WAL write and sync (the whole server's, over the same window)", 4_131_617.0)];
    assert!(analyze(s).snapshot.waits.is_empty());
}

// MySQL demo MY-2: the demo server keeps internal temporary tables in the MEMORY engine, where the cap is the smaller
// of tmp_table_size and max_heap_table_size: raising tmp_table_size alone changes nothing. A scan that didn't read the
// engine raises both and states no temptable_max_ram value; only TempTable's own limit is said with one, as read.
#[test]
fn mysql_temp_spill_raises_what_caps_the_engine_the_server_uses() {
    let spill = |s: Snapshot| {
        analyze(s).findings.into_iter().find(|f| f.check == "temp-spill").expect("the demo's on-disk temporary tables")
    };
    let base = snapshot("mysql-demo.json");
    // The engine wasn't read (the saved demo): both caps, and no 1 GB said as the server's.
    let f = spill(base.clone());
    let sql = f.fix.as_ref().and_then(|x| x.sql.clone()).unwrap();
    assert!(sql.contains("tmp_table_size = 67108864") && sql.contains("max_heap_table_size = 67108864"), "{sql}");
    assert!(!f.summary.contains("GB"), "{}", f.summary);
    assert!(f.summary.contains("1.0 MB, the smaller of tmp_table_size and max_heap_table_size"), "{}", f.summary);
    // MEMORY, as the demo runs: both caps, the smaller one wins.
    let mut memory = base.clone();
    set(&mut memory, "internal_tmp_mem_storage_engine", "MEMORY");
    let f = spill(memory);
    let sql = f.fix.as_ref().and_then(|x| x.sql.clone()).unwrap();
    assert!(sql.contains("max_heap_table_size = 67108864"), "{sql}");
    assert!(!f.summary.contains("temptable_max_ram"), "{}", f.summary);
    // TempTable: tmp_table_size per table on 8.4, and temptable_max_ram as the server has it.
    let mut temptable = base.clone();
    set(&mut temptable, "internal_tmp_mem_storage_engine", "TempTable");
    set(&mut temptable, "temptable_max_ram", "2147483648");
    let f = spill(temptable.clone());
    let sql = f.fix.as_ref().and_then(|x| x.sql.clone()).unwrap();
    assert!(sql.contains("tmp_table_size = 67108864") && !sql.contains("max_heap_table_size"), "{sql}");
    assert!(f.summary.contains("temptable_max_ram, 2.0 GB, for all such tables together"), "{}", f.summary);
    // TempTable whose shared limit wasn't read: named, with no value.
    temptable.settings.retain(|x| x.name != "temptable_max_ram");
    let f = spill(temptable);
    assert!(f.summary.contains("and temptable_max_ram, for all such tables together"), "{}", f.summary);
    assert!(!f.summary.contains("GB"), "{}", f.summary);
}

// SQL Server/PostgreSQL demos P2-5c: temp-spill had no verify step. pg_stat_statements and performance_schema keep
// totals since their last reset: the step names the numbers to compare and what shows the spill gone.
#[test]
fn temp_spill_says_what_a_later_scan_shows() {
    let f = analyze(report_snapshot("pg-demo"))
        .findings
        .into_iter()
        .find(|f| f.check == "temp-spill")
        .expect("the demo's spilling sort");
    // What to look at is the spill's total between two scans, not whether this finding (which reads the totals) is
    // still listed.
    let verify = &f.verify[0];
    assert!(verify.starts_with("Once the application's connections use the new setting"), "{verify}");
    assert!(
        verify.ends_with(", its temp files should stop growing while its runs go on: pg_stat_statements keeps running totals, so compare its temp files and runs in a scan taken once the change is live and one a day or more later (4.2 GB over 214 runs in this scan)."),
        "{verify}"
    );
    assert!(!verify.contains("finding can stay"), "{verify}");
    let f = analyze(snapshot("mysql-demo.json")).findings.into_iter().find(|f| f.check == "temp-spill").unwrap();
    assert!(
        f.verify[0].contains("compare its on-disk temporary tables and runs in a scan taken once the change is live and one a day or more later (20 in 20 runs in this scan)"),
        "{:?}",
        f.verify
    );
}

// MariaDB demo: with the slow log writing to its table, MariaDB counts a logged statement's whole run as its lock time
// (375c…: 49,963.3 ms of lock in 49,963.9 ms). That number isn't kept for real, so the report holds none, and no check
// reads the statement as waiting on locks.
#[test]
fn mariadb_lock_time_that_is_the_whole_run_is_unknown() {
    let s = report_snapshot("mariadb-demo");
    let report = analyze(s);
    let w = report.snapshot.workload.as_ref().unwrap();
    let q = w.queries.iter().find(|q| q.id.starts_with("375c")).expect("the GROUP BY over a million rows");
    assert_eq!(q.spread.as_ref().and_then(|s| s.lock_ms), None);
    assert!(q.waits.iter().all(|w| w.category != "Lock"), "{:?}", q.waits);
    assert!(
        report.snapshot.notes.iter().any(|n| n.message.contains("whole run of a statement the slow query log writes")),
        "{:?}",
        report.snapshot.notes
    );
    for f in report.findings.iter().filter(|f| f.queries.iter().any(|r| r.id == q.id)) {
        assert!(
            !f.summary.contains("waiting on locks") && !f.title.contains("waiting on locks"),
            "{}: {}",
            f.id,
            f.title
        );
    }
    // Read again (a saved report is prepared already): nothing changes and the note isn't repeated.
    let again = analyze(report.snapshot.clone());
    let notes = |r: &Report| r.snapshot.notes.iter().filter(|n| n.message.contains("slow query log writes")).count();
    assert_eq!(notes(&again), 1);
}
