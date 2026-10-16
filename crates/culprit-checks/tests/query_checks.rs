//! The query checks say only what the statistics show: a plan change needs the times to back it, a spread one outlier
//! can't explain, a spill big enough to matter, a wait that reads can't explain, and fixes the server can run. Each test
//! plants one condition on a demo snapshot, with the case that must be flagged next to the ones that must not.

use culprit_checks::analyze;
use culprit_core::{
    Finding, ForcedPlan, PlanVariant, PlanWarning, QueryFamily, QueryStat, Setting, Severity, Snapshot, Spread,
    TuningAdvice, TuningRecommendation, WaitStat,
};

fn snapshot(name: &str) -> Snapshot {
    let text =
        std::fs::read_to_string(format!("{}/../../fixtures/{name}", env!("CARGO_MANIFEST_DIR"))).expect("fixture");
    serde_json::from_str(&text).expect("snapshot")
}

fn query<'a>(s: &'a mut Snapshot, id: &str) -> &'a mut QueryStat {
    s.workload.as_mut().expect("workload").queries.iter_mut().find(|q| q.id == id).expect("query")
}

fn find<'a>(findings: &'a [Finding], id: &str) -> Option<&'a Finding> {
    findings.iter().find(|f| f.id == id)
}

fn set(s: &mut Snapshot, name: &str, value: &str) {
    s.settings.retain(|x| !x.name.eq_ignore_ascii_case(name));
    s.settings.push(Setting { name: name.into(), value: value.into(), default_value: None, scope: "server".into() });
}

fn plan(id: &str, runs: f64, mean: f64, first: &str, last: &str) -> PlanVariant {
    serde_json::from_value(serde_json::json!({
        "id": id, "executions": runs, "total_time_ms": runs * mean, "mean_time_ms": mean,
        "first_seen": first, "last_seen": last, "is_forced": false
    }))
    .expect("plan variant")
}

/// The SQL Server demo's query 8 (orders by shipped_at) with these plans, its totals to match, and no timeline (so
/// `regression` stays out of it and `plan-regression` speaks for itself).
fn with_plans(plans: Vec<PlanVariant>) -> Snapshot {
    let mut s = snapshot("mssql-demo.json");
    let q = query(&mut s, "8");
    q.executions = plans.iter().map(|p| p.executions).sum();
    q.total_time_ms = plans.iter().map(|p| p.total_time_ms).sum();
    q.mean_time_ms = q.total_time_ms / q.executions;
    q.trend = None;
    q.plans = plans;
    let total: f64 = s.workload.as_ref().unwrap().queries.iter().map(|q| q.total_time_ms).sum();
    s.workload.as_mut().unwrap().total_time_ms = total;
    s
}

// ── plan-regression ──────────────────────────────────────────────────────────────────────────────

#[test]
fn a_plan_that_replaced_a_faster_one_is_a_plan_regression() {
    let s = with_plans(vec![
        plan("8", 200.0, 9.0, "2026-10-11T07:44:00Z", "2026-10-11T10:44:00Z"),
        plan("47", 100.0, 60.0, "2026-10-11T10:46:00Z", "2026-10-11T12:13:00Z"),
    ]);
    let report = analyze(s);
    let f = find(&report.findings, "plan-regression:8").expect("plan 47 replaced plan 8");
    assert!(f.title.contains("slower after its plan changed"), "{}", f.title);
    assert!(
        f.summary.contains("until Oct 11, 10:44 UTC") && f.summary.contains("since Oct 11, 10:46 UTC"),
        "{}",
        f.summary
    );
    assert!(f.fix.as_ref().unwrap().sql.as_deref().unwrap().contains("@plan_id = 8;"));
}

// The false flags: five quick runs in nine minutes as the "fast plan", and a slower plan that does the same work per
// run (its runs waited on blocking or load while it ran). Neither is a plan that made the query slower.
#[test]
fn a_regression_needs_a_real_baseline_and_more_work_per_run() {
    let s = with_plans(vec![
        plan("8", 5.0, 3.0, "2026-10-11T10:35:00Z", "2026-10-11T10:44:00Z"),
        plan("47", 100.0, 60.0, "2026-10-11T10:46:00Z", "2026-10-11T12:13:00Z"),
    ]);
    assert!(find(&analyze(s).findings, "plan-regression:8").is_none());
    let with_work = |cpu: f64, reads: f64| {
        let mut fast = plan("8", 200.0, 9.0, "2026-10-11T07:44:00Z", "2026-10-11T10:44:00Z");
        (fast.cpu_time_ms, fast.logical_reads) = (Some(200.0 * 4.0), Some(200.0 * 1_000.0));
        let mut slow = plan("47", 100.0, 60.0, "2026-10-11T10:46:00Z", "2026-10-11T12:13:00Z");
        (slow.cpu_time_ms, slow.logical_reads) = (Some(100.0 * cpu), Some(100.0 * reads));
        with_plans(vec![fast, slow])
    };
    assert!(find(&analyze(with_work(4.0, 1_000.0)).findings, "plan-regression:8").is_none());
    assert!(find(&analyze(with_work(4.0, 40_000.0)).findings, "plan-regression:8").is_some());
}

#[test]
fn a_fast_plan_that_came_later_is_not_a_regression() {
    // The slow plan ran from the start; a fast one appeared later and the slow one kept running after it.
    let s = with_plans(vec![
        plan("47", 100.0, 60.0, "2026-10-11T07:44:00Z", "2026-10-11T12:13:00Z"),
        plan("8", 200.0, 9.0, "2026-10-11T10:00:00Z", "2026-10-11T12:00:00Z"),
    ]);
    assert!(find(&analyze(s).findings, "plan-regression:8").is_none());
    // Latest to run is the slow one, but the fast one started after it.
    let s = with_plans(vec![
        plan("47", 100.0, 60.0, "2026-10-11T07:44:00Z", "2026-10-11T12:13:00Z"),
        plan("8", 200.0, 9.0, "2026-10-11T09:00:00Z", "2026-10-11T10:00:00Z"),
    ]);
    assert!(find(&analyze(s).findings, "plan-regression:8").is_none());
}

#[test]
fn plans_used_side_by_side_are_sensitivity_not_a_regression() {
    let s = with_plans(vec![
        plan("8", 900.0, 9.0, "2026-10-11T07:44:00Z", "2026-10-11T12:12:00Z"),
        plan("47", 100.0, 300.0, "2026-10-11T07:50:00Z", "2026-10-11T12:13:00Z"),
    ]);
    let report = analyze(s);
    assert!(find(&report.findings, "plan-regression:8").is_none(), "plans that overlap aren't a plan change");
    let f = find(&report.findings, "parameter-sensitive:8").expect("two plans side by side");
    assert!(f.title.contains("for most calls"), "{}", f.title);
}

#[test]
fn a_plan_change_on_a_sliver_of_the_workload_is_no_finding() {
    // 6 + 6 runs, a fraction of a percent of database time: 40× slower, and still nothing to act on.
    let s = with_plans(vec![
        plan("8", 6.0, 1.0, "2026-10-11T07:44:00Z", "2026-10-11T08:00:00Z"),
        plan("47", 6.0, 40.0, "2026-10-11T09:00:00Z", "2026-10-11T12:13:00Z"),
    ]);
    assert!(find(&analyze(s).findings, "plan-regression:8").is_none());
}

// ── one plan story per query ─────────────────────────────────────────────────────────────────────

fn recommend(query: &str, good: &str, bad: &str) -> TuningAdvice {
    TuningAdvice {
        recommendations: vec![TuningRecommendation {
            name: "PR_1".into(),
            kind: "FORCE_LAST_GOOD_PLAN".into(),
            state: Some("Active".into()),
            query_id: Some(query.into()),
            regressed_plan_id: Some(bad.into()),
            recommended_plan_id: Some(good.into()),
            estimated_gain: Some(0.7),
            ..Default::default()
        }],
        ..Default::default()
    }
}

#[test]
fn sql_servers_recommendation_carries_the_regression_of_its_query() {
    // The demo's query 8 got 3.4× slower when plan 47 replaced plan 8 (`regression`); SQL Server recommends plan 8.
    let mut s = snapshot("mssql-demo.json");
    s.tuning = Some(recommend("8", "8", "47"));
    let report = analyze(s);
    let about_8: Vec<&str> = report
        .findings
        .iter()
        .filter(|f| f.queries.iter().any(|q| q.id == "8"))
        .filter(|f| matches!(f.check.as_str(), "regression" | "plan-regression" | "tuning-advice"))
        .map(|f| f.id.as_str())
        .collect();
    assert_eq!(about_8, vec!["tuning-advice:force:8"]);
    let f = find(&report.findings, "tuning-advice:force:8").unwrap();
    assert!(f.title.contains("(estimated 70% less CPU per run)"), "{}", f.title);
    assert!(f.summary.contains("Query Store's timeline: until Oct 11, 10:45 UTC it averaged"), "{}", f.summary);
    assert!(f.metrics.iter().any(|m| m.label == "Plan change"), "{:?}", f.metrics);
    assert!(f.fix.as_ref().unwrap().sql.as_deref().unwrap().contains("@plan_id = 8"));
    let alone = analyze(snapshot("mssql-demo.json"));
    let regression = alone
        .findings
        .iter()
        .find(|r| r.queries.iter().any(|q| q.id == "8") && matches!(r.check.as_str(), "regression" | "plan-regression"))
        .expect("the regression on its own");
    assert_eq!(f.severity, regression.severity, "the regression's severity carries over");
}

#[test]
fn sql_servers_recommendation_is_a_caveat_on_plans_used_side_by_side() {
    let mut s = with_plans(vec![
        plan("8", 900.0, 9.0, "2026-10-11T07:44:00Z", "2026-10-11T12:12:00Z"),
        plan("47", 100.0, 300.0, "2026-10-11T07:50:00Z", "2026-10-11T12:13:00Z"),
    ]);
    s.tuning = Some(recommend("8", "47", "8"));
    let report = analyze(s);
    assert!(find(&report.findings, "tuning-advice:force:8").is_none());
    let f = find(&report.findings, "parameter-sensitive:8").unwrap();
    let caveat = f.caveats.iter().find(|c| c.contains("recommends forcing plan 47")).expect("the recommendation");
    assert!(caveat.contains("most calls run fast with plan 8"), "{caveat}");
}

// ── unstable-query ───────────────────────────────────────────────────────────────────────────────

/// Postgres demo's customer lookup (8.2K runs, 0.02 ms) with this spread.
fn unstable(mean: f64, stddev: f64, max: f64, runs: f64) -> Snapshot {
    let mut s = snapshot("postgres-demo.json");
    let q = query(&mut s, "3432126458605143571");
    q.executions = runs;
    q.mean_time_ms = mean;
    q.total_time_ms = mean * runs;
    q.stddev_time_ms = Some(stddev);
    q.max_time_ms = Some(max);
    let total: f64 = s.workload.as_ref().unwrap().queries.iter().map(|q| q.total_time_ms).sum();
    s.workload.as_mut().unwrap().total_time_ms = total;
    s
}

fn spread_of(xs: &[(f64, f64)]) -> (f64, f64, f64, f64) {
    let n: f64 = xs.iter().map(|(_, k)| k).sum();
    let mean = xs.iter().map(|(x, k)| x * k).sum::<f64>() / n;
    let var = xs.iter().map(|(x, k)| (x - mean).powi(2) * k).sum::<f64>() / n;
    let max = xs.iter().map(|(x, _)| *x).fold(0.0, f64::max);
    (mean, var.sqrt(), max, n)
}

#[test]
fn one_slow_run_among_thousands_is_not_an_unstable_query() {
    // 10,000 runs at 2 ms and one at 5 s: a lock wait during a migration, not a query that "sometimes" takes 5 s.
    let (mean, sd, max, n) = spread_of(&[(2.0, 10_000.0), (5000.0, 1.0)]);
    let ids: Vec<String> = analyze(unstable(mean, sd, max, n)).findings.into_iter().map(|f| f.id).collect();
    assert!(!ids.iter().any(|id| id.starts_with("unstable-query:")), "{ids:?}");
}

#[test]
fn many_slow_runs_are_an_unstable_query_rated_by_their_time() {
    // 1% of the runs take 5 s.
    let (mean, sd, max, n) = spread_of(&[(2.0, 9_900.0), (5000.0, 100.0)]);
    let report = analyze(unstable(mean, sd, max, n));
    let f = find(&report.findings, "unstable-query:3432126458605143571").expect("an unstable query");
    assert!(f.title.contains("but some runs take far longer (up to 5.0 s)"), "{}", f.title);
    assert!(
        f.summary.starts_with("Same statement, very different run times"),
        "PostgreSQL keeps no plan identity: {}",
        f.summary
    );
    assert!(f.caveats.iter().any(|c| c.contains("could be from any time")), "{:?}", f.caveats);
    let extra = f.metrics.iter().find(|m| m.label.starts_with("Slow runs' extra time")).unwrap().value;
    assert!((extra - 100.0 * (5000.0 - mean)).abs() / extra < 0.1, "{extra}");
}

// ── temp-spill ───────────────────────────────────────────────────────────────────────────────────

const ORDERS_SORT: &str = "9065391607317686058";

#[test]
fn a_spill_too_small_to_matter_is_not_a_finding() {
    // 16 KB per run on a 332 ms statement: writing it takes a fraction of a millisecond.
    let mut s = snapshot("postgres-demo.json");
    let q = query(&mut s, ORDERS_SORT);
    q.temp_writes = Some(2.0 * q.executions);
    let report = analyze(s);
    assert!(find(&report.findings, &format!("temp-spill:{ORDERS_SORT}")).is_none());
}

#[test]
fn work_mem_advice_is_for_the_applications_role_and_never_lower() {
    let report = analyze(snapshot("postgres-demo.json"));
    let f = find(&report.findings, &format!("temp-spill:{ORDERS_SORT}")).unwrap();
    assert!(!f.summary.contains("on every run"), "{}", f.summary);
    let sql = f.fix.as_ref().unwrap().sql.clone().unwrap();
    assert!(sql.contains("ALTER ROLE <app_role> SET work_mem = '64MB';") && !sql.contains("current_user"), "{sql}");

    // Already 64MB: more than that, never the same or less.
    let mut s = snapshot("postgres-demo.json");
    set(&mut s, "work_mem", "64MB");
    let sql =
        find(&analyze(s).findings, &format!("temp-spill:{ORDERS_SORT}")).unwrap().fix.clone().unwrap().sql.unwrap();
    assert!(sql.contains("work_mem = '128MB'"), "{sql}");
    // Already 1GB: no more memory, read less.
    let mut s = snapshot("postgres-demo.json");
    set(&mut s, "work_mem", "1GB");
    let fix = find(&analyze(s).findings, &format!("temp-spill:{ORDERS_SORT}")).unwrap().fix.clone().unwrap();
    assert!(fix.sql.is_none() && fix.summary.starts_with("work_mem is already 1GB"), "{}", fix.summary);
}

const GROUP_BY: &str = "04cd7be9c366fadc";

#[test]
fn mysql_temp_table_advice_raises_only_what_limits_and_never_lowers() {
    // MySQL 8.4 runs internal temporary tables on TempTable: max_heap_table_size doesn't limit them.
    let report = analyze(snapshot("mysql-demo.json"));
    let f = find(&report.findings, &format!("temp-spill:{GROUP_BY}")).unwrap();
    let sql = f.fix.as_ref().unwrap().sql.clone().unwrap();
    assert_eq!(sql, "SET PERSIST tmp_table_size = 67108864;");
    assert!(f.title.ends_with("builds an on-disk temporary table about once per run"), "{}", f.title);

    // MariaDB (MEMORY engine): the smaller of the two limits wins; the larger one is left alone.
    let mut s = snapshot("mysql-demo.json");
    s.server.product = "MariaDB 10.11.8".into();
    s.server.version = "10.11.8-MariaDB".into();
    set(&mut s, "tmp_table_size", &(256.0 * 1024.0 * 1024.0).to_string());
    set(&mut s, "max_heap_table_size", &(16.0 * 1024.0 * 1024.0).to_string());
    let sql = find(&analyze(s).findings, &format!("temp-spill:{GROUP_BY}")).unwrap().fix.clone().unwrap().sql.unwrap();
    assert_eq!(sql, "SET GLOBAL max_heap_table_size = 67108864;", "tmp_table_size is already 256 MB");
}

#[test]
fn a_text_column_sends_a_mariadb_temporary_table_to_disk_whatever_the_limit() {
    let mut s = snapshot("mysql-demo.json");
    s.server.product = "MariaDB 10.11.8".into();
    s.server.version = "10.11.8-MariaDB".into();
    let mut note = s.columns.iter().find(|c| c.table == "orders").cloned().expect("a column of orders");
    note.name = "channel".into();
    note.data_type = "text".into();
    s.columns.retain(|c| !(c.table == "orders" && c.name.eq_ignore_ascii_case("channel")));
    s.columns.push(note);
    let f = find(&analyze(s).findings, &format!("temp-spill:{GROUP_BY}")).cloned().unwrap();
    assert!(
        f.summary.contains("whatever its size") && f.summary.contains("Raising the memory limit won't change that"),
        "{}",
        f.summary
    );
    assert!(f.fix.unwrap().sql.is_none());
}

// ── heavy-query ──────────────────────────────────────────────────────────────────────────────────

#[test]
fn a_sleep_and_a_blocked_update_are_waiting_not_reading() {
    let report = analyze(snapshot("postgres-demo.json"));
    let sleep = find(&report.findings, "heavy-query:-8886085649470066503").unwrap();
    assert!(sleep.title.ends_with("of database time, sleeping"), "{}", sleep.title);
    assert!(!sleep.fix.as_ref().unwrap().summary.contains("reads more rows"));
    let update = find(&report.findings, "heavy-query:4911431806993420100").unwrap();
    assert!(update.title.ends_with("most likely waiting for locks"), "{}", update.title);
    assert!(
        update.summary.contains("It read 15 pages per run, which takes milliseconds, yet ran 19.0 s"),
        "{}",
        update.summary
    );
    assert!(update.metrics.iter().any(|m| m.label == "Rows changed per run"));
}

#[test]
fn query_store_waits_name_what_a_statement_waited_for() {
    // Lock and network waits have findings of their own (lock-time, network-wait), which then explain the statement;
    // latch waits are this one's to name.
    let mut s = snapshot("mssql-demo.json");
    let q = query(&mut s, "10");
    q.mean_time_ms = 900.0;
    q.total_time_ms = 900.0 * q.executions;
    q.cpu_time_ms = Some(q.total_time_ms * 0.02);
    q.waits = vec![WaitStat {
        category: "Latch".into(),
        wait_type: Some("Buffer Latch".into()),
        wait_ms: q.total_time_ms * 0.9,
        tasks: None,
    }];
    let total: f64 = s.workload.as_ref().unwrap().queries.iter().map(|q| q.total_time_ms).sum();
    s.workload.as_mut().unwrap().total_time_ms = total;
    let report = analyze(s);
    let named: Vec<String> = report
        .findings
        .iter()
        .filter(|f| f.queries.iter().any(|q| q.id == "10"))
        .map(|f| format!("{} | {}", f.id, f.title))
        .collect();
    let f = find(&report.findings, "heavy-query:10").unwrap_or_else(|| panic!("{named:#?}"));
    assert!(f.title.ends_with("most of it waiting for pages other sessions are using"), "{}", f.title);
    assert!(f.summary.contains("Only 2% of its time is CPU, and Query Store records"), "{}", f.summary);
    assert!(f.summary.contains("of Buffer Latch waits"), "{}", f.summary);
}

#[test]
fn an_aggregate_is_not_titled_by_rows_read_per_row_returned() {
    // `SELECT COUNT(*) FROM orders WHERE status = ? AND created_at >= ?` returns one row whatever it counts.
    let report = analyze(snapshot("mysql-demo.json"));
    let f = find(&report.findings, "heavy-query:c7b5ac9bb4bcecfe").unwrap();
    assert!(!f.title.contains("for every row it returns"), "{}", f.title);
}

// ── the slow query log ───────────────────────────────────────────────────────────────────────────

fn slow_log(mut s: Snapshot) -> Snapshot {
    let w = s.workload.as_mut().unwrap();
    w.source = "slow query log".into();
    w.windowed = true;
    set(&mut s, "long_query_time", "0.100000");
    s
}

#[test]
fn shares_from_the_slow_log_are_of_logged_slow_runs() {
    let report = analyze(slow_log(snapshot("mysql-demo.json")));
    let f = find(&report.findings, "heavy-query:c7b5ac9bb4bcecfe").unwrap();
    assert!(f.title.ends_with("of logged slow-run time"), "{}", f.title);
    assert!(f.impact.label.ends_with("of logged slow-run time"), "{}", f.impact.label);
    // A log that keeps every statement covers all of the time.
    let mut s = slow_log(snapshot("mysql-demo.json"));
    set(&mut s, "long_query_time", "0.000000");
    let f = find(&analyze(s).findings, "heavy-query:c7b5ac9bb4bcecfe").cloned().unwrap();
    assert!(f.title.ends_with("of database time"), "{}", f.title);
}

#[test]
fn a_slow_log_tail_says_it_is_of_logged_runs_and_needs_several() {
    let mut s = slow_log(snapshot("mysql-demo.json"));
    let q = query(&mut s, "c7b5ac9bb4bcecfe");
    q.executions = 80.0;
    q.spread = Some(Spread {
        p50_ms: Some(150.0),
        p99_ms: Some(4000.0),
        histogram: vec![[100.0, 200.0, 70.0], [200.0, 400.0, 7.0], [2000.0, 5000.0, 3.0]],
        ..Default::default()
    });
    let report = analyze(s.clone());
    let f = find(&report.findings, "parameter-sensitive:c7b5ac9bb4bcecfe").expect("three slow logged runs");
    assert!(f.title.starts_with("Logged runs of a read from orders usually take 150 ms"), "{}", f.title);
    assert!(f.summary.contains("the slow query log kept (only those over long_query_time, 100 ms"), "{}", f.summary);
    assert!(!f.summary.contains("99 in 100"), "80 runs have no 1-in-100: {}", f.summary);
    // Two slow runs are not a tail.
    query(&mut s, "c7b5ac9bb4bcecfe").spread.as_mut().unwrap().histogram[2][2] = 2.0;
    assert!(find(&analyze(s).findings, "parameter-sensitive:c7b5ac9bb4bcecfe").is_none());
}

// ── plan-warnings: no join predicate ─────────────────────────────────────────────────────────────

fn without_join_predicate(text: &str) -> Finding {
    let mut s = snapshot("mssql-demo.json");
    let q = query(&mut s, "8");
    q.text = text.into();
    q.plan.as_mut().unwrap().root.warnings.push(PlanWarning::NoJoinPredicate);
    analyze(s).findings.into_iter().find(|f| f.id == "plan-warnings:8:no-join-predicate").expect("the warning")
}

#[test]
fn a_join_predicate_sql_server_dropped_is_not_a_missing_on() {
    let f = without_join_predicate(
        "SELECT o.id, c.name FROM shop.orders o JOIN shop.customers c ON c.id = o.customer_id WHERE o.customer_id = @P1 AND c.id = @P1",
    );
    assert_eq!(f.severity, Severity::Low);
    assert!(f.summary.contains("though the statement does join them"), "{}", f.summary);
    let f = without_join_predicate(
        "SELECT o.id, c.name FROM shop.orders o, shop.customers c WHERE o.customer_id = c.id AND c.id = @P1",
    );
    assert_eq!(f.severity, Severity::Low);
    let f = without_join_predicate("SELECT o.id, c.name FROM shop.orders o, shop.customers c WHERE c.id = @P1");
    assert!(f.title.ends_with("joins tables without a join condition"), "{}", f.title);
}

// ── query-family ─────────────────────────────────────────────────────────────────────────────────

fn family(sample: &str, ids: &[&str]) -> QueryFamily {
    QueryFamily {
        key: "0x8A1F".into(),
        sample: sample.into(),
        variants: 300.0,
        executions: 9000.0,
        total_time_ms: 5000.0,
        cached_bytes: None,
        query_ids: ids.iter().map(|s| s.to_string()).collect(),
    }
}

#[test]
fn copies_that_differ_in_parameter_declarations_need_sizes_not_parameterization() {
    let mut s = snapshot("mssql-demo.json");
    // Two of the members, same statement, string parameter sized from each value.
    let mut twin = query(&mut s, "7").clone();
    query(&mut s, "7").parameters = Some("@P1 nvarchar(7)".into());
    twin.id = "7001".into();
    twin.parameters = Some("@P1 nvarchar(12)".into());
    s.workload.as_mut().unwrap().queries.push(twin);
    s.query_families = vec![family(
        "(@P1 nvarchar(7))SELECT [id], [name], [segment] FROM [shop].[customers] WHERE [email] = @P1",
        &["7", "7001"],
    )];
    let f = analyze(s).findings.into_iter().find(|f| f.check == "query-family").expect("a family");
    assert!(f.title.contains("differ only in how their parameters are declared"), "{}", f.title);
    assert!(f.title.contains("a read from customers"), "{}", f.title);
    assert!(f.summary.contains("`@P1 nvarchar(7)` and `@P1 nvarchar(12)`"), "{}", f.summary);
    let fix = f.fix.unwrap();
    assert!(fix.sql.is_none() && fix.summary.contains("fixed size"), "{}", fix.summary);
}

#[test]
fn copies_with_the_same_text_are_no_literal_family() {
    // SQL Server: the same text and declaration under two query ids (another module, other SET options).
    let mut s = snapshot("mssql-demo.json");
    let mut twin = query(&mut s, "7").clone();
    twin.id = "7001".into();
    s.workload.as_mut().unwrap().queries.push(twin);
    s.query_families = vec![family("SELECT [id] FROM [shop].[customers] WHERE [email] = @P1", &["7", "7001"])];
    assert!(!analyze(s).findings.iter().any(|f| f.check == "query-family"));

    // PostgreSQL: one entry per role that ran the same statement, which has no value list to differ in.
    let mut pg = snapshot("postgres-demo.json");
    pg.query_families = vec![family("SELECT name, email FROM customers WHERE id = $1", &[])];
    assert!(!analyze(pg).findings.iter().any(|f| f.check == "query-family"));
}

// ── tuning-advice: a forced plan that scans ──────────────────────────────────────────────────────

/// Automatic tuning forced plan 9 for the demo's query 9 on Oct 3; the plan scans order_items for an order id, and an
/// index leading with order_id exists. The scan keeps `kept` of the table's 1.5M rows.
fn forced_scan(kept: f64, include: &[&str], created_at_latest: Option<&str>) -> Option<Finding> {
    let mut s = snapshot("mssql-demo.json");
    s.tuning = Some(TuningAdvice {
        recommendations: vec![TuningRecommendation {
            name: "PR_9".into(),
            kind: "FORCE_LAST_GOOD_PLAN".into(),
            state: Some("Verifying".into()),
            query_id: Some("9".into()),
            recommended_plan_id: Some("9".into()),
            applied_at: Some("2026-10-03T08:00:00Z".into()),
            ..Default::default()
        }],
        forced_plans: vec![ForcedPlan {
            query_id: "9".into(),
            plan_id: "9".into(),
            forcing: Some("AUTO".into()),
            ..Default::default()
        }],
        ..Default::default()
    });
    let mut index = s.indexes.iter().find(|i| i.name == "ix_order_items_product_id").unwrap().clone();
    index.name = "ix_order_items_order_id".into();
    index.columns = vec!["order_id".into()];
    index.include = include.iter().map(|c| c.to_string()).collect();
    index.created_at_latest = created_at_latest.map(str::to_string);
    s.indexes.push(index);
    let scan = &mut query(&mut s, "9").plan.as_mut().unwrap().root.children[0];
    scan.est_rows = 1_500_000.0 * kept;
    analyze(s).findings.into_iter().find(|f| f.id == "tuning-advice:forced-scan:9")
}

#[test]
fn a_forced_scan_is_flagged_only_where_a_seek_would_read_less() {
    // 1.8 rows of 1.5M: a seek and a lookup per row beats the scan, covering index or not.
    let f = forced_scan(1.8 / 1_500_000.0, &[], None).expect("a selective filter");
    assert!(f.summary.contains("doesn't hold product_id, quantity and unit_price"), "{}", f.summary);
    assert!(!f.summary.contains("forced before it existed"), "nothing dates the index: {}", f.summary);
    // Keeping 70% of the table, the scan is the right plan.
    assert!(forced_scan(0.7, &["product_id", "quantity", "unit_price"], None).is_none());
    // Keeping 5%: only an index that holds every column it returns beats the scan.
    assert!(forced_scan(0.05, &[], None).is_none());
    let f = forced_scan(0.05, &["product_id", "quantity", "unit_price"], None).expect("a covering index");
    assert!(!f.summary.contains("doesn't hold"), "{}", f.summary);
}

#[test]
fn a_forced_plan_is_called_older_than_its_index_only_when_the_index_is_dated() {
    let f = forced_scan(1e-6, &[], Some("2026-10-09T12:00:00Z")).unwrap();
    assert!(f.summary.contains("so this plan was most likely forced before it existed"), "{}", f.summary);
    // Dated before the forcing: the index was already there.
    let f = forced_scan(1e-6, &[], Some("2026-10-01T12:00:00Z")).unwrap();
    assert!(!f.summary.contains("forced before it existed"), "{}", f.summary);
}
