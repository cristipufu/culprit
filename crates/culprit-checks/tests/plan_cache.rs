//! The single-use plan finding's fix is something the reader can run where the server lives.

use culprit_checks::analyze;
use culprit_core::{PlanCacheUse, Snapshot};

fn snapshot(cloud: Option<&str>) -> Snapshot {
    let text = std::fs::read_to_string(format!("{}/../../fixtures/mssql-demo.json", env!("CARGO_MANIFEST_DIR")))
        .expect("fixture");
    let mut s: Snapshot = serde_json::from_str(&text).expect("snapshot");
    s.server.cloud = cloud.map(str::to_string);
    s.settings.retain(|x| x.name != "optimize for ad hoc workloads");
    let gb = 1024.0 * 1024.0 * 1024.0;
    s.plan_cache = Some(PlanCacheUse {
        single_use_plans: 40_000.0,
        single_use_bytes: 2.0 * gb,
        total_plans: 50_000.0,
        total_bytes: 3.0 * gb,
    });
    s
}

fn fix(cloud: Option<&str>) -> (String, Option<String>) {
    let report = analyze(snapshot(cloud));
    let f = report.findings.iter().find(|f| f.id == "query-family:plan-cache").expect("the plan-cache finding");
    let fix = f.fix.clone().expect("a fix");
    (fix.summary, fix.sql)
}

#[test]
fn the_ad_hoc_setting_is_changed_where_the_server_keeps_it() {
    let (_, sql) = fix(None);
    assert!(sql.as_deref().is_some_and(|s| s.contains("sp_configure 'optimize for ad hoc workloads'")), "{sql:?}");
    // Azure SQL Database keeps it per database.
    let (_, sql) = fix(Some("Azure SQL Database"));
    assert_eq!(sql.as_deref(), Some("ALTER DATABASE SCOPED CONFIGURATION SET OPTIMIZE_FOR_AD_HOC_WORKLOADS = ON;"));
    // RDS refuses sp_configure: its parameter group, and no SQL.
    let (summary, sql) = fix(Some("Amazon RDS"));
    assert_eq!(sql, None);
    assert!(summary.contains("DB parameter group"), "{summary}");
}

// The false flag: the plan cache counts each plan from its own compile. A plan cached 30 days ago (900 s in all) next
// to one compiled an hour ago (100 s) read as 90% of database time, so heavy-query named the old one. By the time each
// spends an hour, the recent one is the heavy one, and the old one is about 1%.
#[test]
fn a_plan_cached_for_a_month_claims_what_it_costs_an_hour() {
    let mut s = snapshot(None);
    s.plan_cache = None;
    let w = s.workload.as_mut().unwrap();
    w.queries.truncate(2);
    w.source = "Plan cache".into();
    w.kind = Some(culprit_core::WorkloadKind::PlanCache);
    w.windowed = false;
    w.timeline = None;
    w.window_end = Some("2026-10-11T12:00:00Z".into());
    w.window_start = Some("2026-09-11T12:00:00Z".into());
    w.window_hours = 720.0;
    let (old, recent) = (w.queries[0].id.clone(), w.queries[1].id.clone());
    for (q, (ms, first)) in
        w.queries.iter_mut().zip([(900_000.0, "2026-09-11T12:00:00Z"), (100_000.0, "2026-10-11T11:00:00Z")])
    {
        q.total_time_ms = ms;
        q.executions = 1_000.0;
        q.mean_time_ms = ms / 1_000.0;
        q.first_seen = Some(first.into());
        q.trend = None;
        q.plans.clear();
    }
    w.total_time_ms = 1_000_000.0;
    // What the collector records: each plan's time an hour of its own life, summed.
    w.hourly_time_ms = Some(900_000.0 / 720.0 + 100_000.0 / 1.0);
    let report = analyze(s);
    let share = |id: &str| {
        report
            .findings
            .iter()
            .filter(|f| f.queries.iter().any(|q| q.id == id))
            .filter_map(|f| f.impact.db_time_share)
            .fold(0.0, f64::max)
    };
    // The old plan claims what it costs an hour, about 1%: no heavy-query, and nothing above it.
    assert!(share(&old) < 0.02, "old plan graded at {}", share(&old));
    assert!(!report.findings.iter().any(|f| f.id == format!("heavy-query:{old}")));
    // The recent one keeps its raw share (10%), which can only understate it.
    assert!((share(&recent) - 0.1).abs() < 1e-9, "recent plan graded at {}", share(&recent));
}

// The PostgreSQL demo: a family of statements split by their value lists' lengths named no statement in its fix, and
// its brief said "No statement of the application is named here" beside a caveat quoting one. The fix says where the
// change is: the code that sends the statement it quotes.
#[test]
fn a_family_fix_points_at_the_statement_it_quotes() {
    let text = std::fs::read_to_string(format!("{}/../../fixtures/postgres-demo.json", env!("CARGO_MANIFEST_DIR")))
        .expect("fixture");
    let mut s: Snapshot = serde_json::from_str(&text).expect("snapshot");
    let sample = "SELECT count(*) FROM (VALUES ($1),($2)) v(x) JOIN orders o ON o.id = v.x";
    s.query_families = vec![culprit_core::QueryFamily {
        key: "f2f04171f37597d5793a740be1361d5c".into(),
        sample: sample.into(),
        variants: 6.0,
        executions: 6.0,
        total_time_ms: 1.0,
        cached_bytes: None,
        query_ids: vec!["1".into(), "2".into()],
    }];
    let report = analyze(s);
    let f = report.findings.iter().find(|f| f.check == "query-family").expect("the family");
    let fix = &f.fix.as_ref().unwrap().summary;
    assert!(
        fix.starts_with(
            "In the code that sends this statement (one copy is quoted below, as recorded), pass value lists"
        ),
        "{fix}"
    );
    assert!(f.caveats.iter().any(|c| c == &format!("One of them, as recorded:\n{sample}")), "{:?}", f.caveats);
}
