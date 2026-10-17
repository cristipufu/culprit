//! The query checks say only what the statistics show: a plan change needs the times to back it, a spread one outlier
//! can't explain, a spill big enough to matter, a wait that reads can't explain, and fixes the server can run. Each test
//! plants one condition on a demo snapshot, with the case that must be flagged next to the ones that must not.

use culprit_checks::analyze;
use culprit_core::{
    Finding, ForcedPlan, NodeKind, Plan, PlanVariant, PlanWarning, QueryFamily, QueryStat, Setting, Severity, Snapshot,
    Spread, TuningAdvice, TuningRecommendation, Unit, WaitStat,
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
    // Forty times the reads a run is a regression: the scan of orders the new plan reads for want of an index on
    // shipped_at came with it, so the scan's finding carries it (`scan_carries_its_regression`).
    let report = analyze(with_work(4.0, 40_000.0));
    let carried = report.findings.iter().any(|f| {
        f.id == "plan-regression:8"
            || (f.id.ends_with("shop.orders(shipped_at)") && f.metrics.iter().any(|m| m.label == "Fast plan"))
    });
    assert!(carried, "{:?}", report.findings.iter().map(|f| &f.id).collect::<Vec<_>>());
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

/// [`with_plans`] over six days of Query Store rather than the demo's four and a half hours.
fn six_days_with_plans(plans: Vec<PlanVariant>) -> Snapshot {
    let mut s = with_plans(plans);
    let w = s.workload.as_mut().unwrap();
    (w.window_start, w.window_hours) = (Some("2026-10-05T12:13:47Z".into()), 144.0);
    s
}

// The fresh SQL Server demo: a statement that ran its slow plan for three minutes and stopped got "Extra time per day:
// 665 ms", a rate it never ran at, and "Force the fast plan" for a plan that read a table no longer in the catalog.
#[test]
fn a_plan_regression_says_a_day_only_of_a_day_of_slow_runs_and_forces_nothing_onto_a_table_that_is_gone() {
    let burst = || {
        six_days_with_plans(vec![
            plan("8", 450.0, 0.5, "2026-10-11T07:44:00Z", "2026-10-11T07:47:00Z"),
            plan("47", 300.0, 60.0, "2026-10-11T07:48:00Z", "2026-10-11T07:51:00Z"),
        ])
    };
    let report = analyze(burst());
    let f = find(&report.findings, "plan-regression:8").expect("plan 47 replaced plan 8");
    assert!(!f.metrics.iter().any(|m| m.label == "Extra time per day"), "{:?}", f.metrics);
    assert!(
        f.metrics.iter().any(|m| m.label == "Extra time" && (m.value - 300.0 * 59.5).abs() < 1.0),
        "{:?}",
        f.metrics
    );
    assert_eq!(f.impact.time_ms_per_day, None);
    // Two days of the slow plan, still running: its extra time a day over them.
    let s = six_days_with_plans(vec![
        plan("8", 450.0, 0.5, "2026-10-08T07:44:00Z", "2026-10-09T07:47:00Z"),
        plan("47", 300.0, 60.0, "2026-10-09T12:13:47Z", "2026-10-11T12:13:00Z"),
    ]);
    let f = find(&analyze(s).findings, "plan-regression:8").cloned().expect("plan 47 replaced plan 8");
    let a_day = f.metrics.iter().find(|m| m.label == "Extra time per day").expect("a day of slow runs").value;
    assert!((a_day - 300.0 * 59.5 / 2.0).abs() < 1.0, "{a_day}");

    // The table the statement read is gone: nothing to force.
    let mut s = burst();
    s.tables.retain(|t| t.name != "orders");
    let f = find(&analyze(s).findings, "plan-regression:8").cloned().expect("plan 47 replaced plan 8");
    let fix = f.fix.unwrap();
    assert!(fix.sql.is_none(), "{:?}", fix.sql);
    assert!(
        fix.summary.starts_with("shop.orders is no longer in the catalog") && !fix.summary.contains("Force"),
        "{}",
        fix.summary
    );
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

/// A PostgreSQL plan of an INSERT of the values it is given: a ModifyTable over a Result, no table or index read.
fn insert_values_plan() -> Plan {
    serde_json::from_value(serde_json::json!({
        "engine": "postgres", "kind": "generic", "total_cost": 0.01, "warnings": [], "index_hints": [], "props": {},
        "root": {
            "id": 0, "op": "ModifyTable", "kind": "modify", "object": "public.app_salts", "detail": "Insert",
            "est_rows": 0.0, "est_executions": 1.0, "subtree_cost": 0.01, "self_cost": 0.0,
            "warnings": [], "props": {"Conflict Resolution": "NOTHING"},
            "children": [{
                "id": 1, "op": "Result", "kind": "compute", "est_rows": 1.0, "est_executions": 1.0,
                "subtree_cost": 0.01, "self_cost": 0.01, "warnings": [], "props": {}, "children": []
            }]
        }
    }))
    .expect("plan")
}

#[test]
fn an_insert_of_its_values_whose_runs_swing_waited() {
    // `INSERT … VALUES … ON CONFLICT DO NOTHING` reads no table: its values can't make a run do more, so no parameter
    // sensitivity, generic plan or statistics target explains its slow runs.
    let (mean, sd, max, n) = spread_of(&[(2.0, 9_900.0), (5000.0, 100.0)]);
    let mut s = unstable(mean, sd, max, n);
    let q = query(&mut s, "3432126458605143571");
    q.text = "INSERT INTO app_salts (app_id, date, salt) VALUES ($1, $2, $3) ON CONFLICT DO NOTHING".into();
    q.plan = Some(insert_values_plan());
    let report = analyze(s);
    let f = find(&report.findings, "unstable-query:3432126458605143571").expect("an unstable insert");
    assert!(f.summary.contains("Its plan reads no table"), "{}", f.summary);
    assert!(f.summary.contains("inserting the same key"), "{}", f.summary);
    assert!(!f.summary.contains("parameter"), "{}", f.summary);
    let fix = f.fix.as_ref().unwrap();
    assert!(fix.sql.is_none(), "{:?}", fix.sql);
    assert!(!fix.summary.contains("plan_cache_mode") && fix.summary.contains("wait"), "{}", fix.summary);
}

#[test]
fn an_unstable_querys_share_is_its_slow_runs_time() {
    // The share and the time a day beside the finding (its row's bar, the dashboard, the digest, an agent's
    // db_time_pct) are what its label says: the slow runs', not the statement's 25% that the label calls "<1%".
    let (mean, sd, max, n) = spread_of(&[(2.0, 9_900.0), (5000.0, 100.0)]);
    let mut s = unstable(mean, sd, max, n);
    s.workload.as_mut().unwrap().window_hours = 48.0;
    let report = analyze(s);
    let f = find(&report.findings, "unstable-query:3432126458605143571").expect("an unstable query");
    let share = f.impact.db_time_share.expect("the slow runs' share");
    assert_eq!(f.impact.label, format!("{} of DB time in slow runs", culprit_core::format::percent(share)));
    let extra = f.metrics.iter().find(|m| m.label.starts_with("Slow runs' extra time")).unwrap().value;
    let w = report.snapshot.workload.as_ref().unwrap();
    let q = w.query("3432126458605143571").unwrap();
    assert!((share - w.share_of(extra, q)).abs() < 1e-12, "{share}");
    assert!(share < w.share_of(q.total_time_ms, q), "below the statement's own share");
    let a_day = f.impact.time_ms_per_day.expect("48 hours of data");
    assert!((a_day - extra / 2.0).abs() < 1e-6, "{a_day} vs {extra} over two days");
}

// The aptabase reading: an insert taking a quarter of database time lost its heavy-query finding to the unstable-query
// one, whose slow runs were under 1% of it, so nothing said what the statement costs every day. A finding explains a
// heavy query away only when it accounts for its time.
#[test]
fn slow_runs_that_are_a_part_of_a_heavy_insert_leave_its_heavy_query() {
    let (mean, sd, max, n) = spread_of(&[(20.0, 19_940.0), (5000.0, 20.0)]);
    let mut s = unstable(mean, sd, max, n);
    let q = query(&mut s, "3432126458605143571");
    q.text = "INSERT INTO app_salts (app_id, date, salt) VALUES ($1, $2, $3) ON CONFLICT DO NOTHING".into();
    q.plan = Some(insert_values_plan());
    let report = analyze(s);
    let unstable = find(&report.findings, "unstable-query:3432126458605143571").expect("an unstable insert");
    let heavy = find(&report.findings, "heavy-query:3432126458605143571").expect("its everyday cost, still said");
    assert!(
        unstable.impact.db_time_share.unwrap() < heavy.impact.db_time_share.unwrap() * 0.5,
        "{:?} vs {:?}",
        unstable.impact,
        heavy.impact
    );
    // A finding that accounts for a statement's time still stands for it: the demo's sort that spills.
    let report = analyze(snapshot("postgres-demo.json"));
    assert!(find(&report.findings, "temp-spill:9065391607317686058").is_some());
    assert!(find(&report.findings, "heavy-query:9065391607317686058").is_none());
}

/// [`insert_values_plan`] writing `object`.
fn insert_plan_into(object: &str) -> Plan {
    let mut plan = insert_values_plan();
    plan.root.object = Some(object.into());
    plan
}

// The aptabase reading: "its plan reads no table, so different values don't make a run do more work" for an insert
// whose foreign key checks every new row against apps, a read and a row lock its plan doesn't show; "the log at
// commit" for PostgreSQL, whose statement time ends before the commit; a guess said as fact ("the slow runs were
// waiting, for…"); "Culprit can't tell which values were slow" right after saying values don't matter; and "another
// transaction inserting the same key" whatever the table's unique keys, even one the database generates.
#[test]
fn an_unstable_insert_says_what_its_foreign_keys_lock() {
    let (mean, sd, max, n) = spread_of(&[(2.0, 9_900.0), (5000.0, 100.0)]);
    let mut s = unstable(mean, sd, max, n);
    let q = query(&mut s, "3432126458605143571");
    q.text = "INSERT INTO order_items (order_id, product_id, quantity) VALUES ($1, $2, $3)".into();
    q.plan = Some(insert_plan_into("public.order_items"));
    let report = analyze(s.clone());
    let f = find(&report.findings, "unstable-query:3432126458605143571").expect("an unstable insert");
    assert!(
        f.summary.contains(
            "Its plan reads no table (it writes the values it is given), but its foreign keys on product_id and order_id check each row it inserts against products and orders: the referenced row is looked up and share-locked (FOR KEY SHARE)."
        ),
        "{}",
        f.summary
    );
    // Its only unique key is the id the sequence gives it: no other transaction inserts the same one.
    assert!(
        f.summary.contains("The slow runs may have been waiting for a transaction that locks (SELECT … FOR UPDATE), deletes or changes the key of the referenced row of products and orders, for a lock on the table"),
        "{}",
        f.summary
    );
    for wrong in ["different values don't", "at commit", "were waiting, for", "same key"] {
        assert!(!f.summary.contains(wrong), "{wrong}: {}", f.summary);
    }
    let fix = f.fix.as_ref().unwrap();
    assert!(fix.summary.starts_with("Find what the slow runs wait for while they happen"), "{}", fix.summary);
    assert!(
        fix.summary.contains(
            "A transaction that locks (SELECT … FOR UPDATE), deletes or changes the key of the referenced row of products and orders holds this one until it commits or rolls back"
        ),
        "{}",
        fix.summary
    );
    assert!(!fix.summary.contains("same key"), "{}", fix.summary);
    let caveat = f.caveats.iter().find(|c| c.contains("could be from any time")).unwrap();
    assert!(!caveat.contains("which values") && caveat.ends_with("how many calls were slow."), "{caveat}");

    // A unique key on the columns it sets: another transaction inserting the same order and product holds it.
    let pkey = s.indexes.iter().find(|i| i.name == "order_items_pkey").cloned().expect("demo index");
    s.indexes.push(culprit_core::IndexStat {
        name: "order_items_order_product_key".into(),
        columns: vec!["order_id".into(), "product_id".into()],
        is_primary_key: false,
        ..pkey
    });
    let report = analyze(s);
    let f = find(&report.findings, "unstable-query:3432126458605143571").expect("an unstable insert");
    assert!(
        f.summary.contains(
            "referenced row of products and orders, for another transaction inserting the same key, for a lock"
        ),
        "{}",
        f.summary
    );
    assert!(
        f.fix.as_ref().unwrap().summary.contains(
            "Another transaction inserting the same key holds this one until it commits or rolls back, and so does one that locks (SELECT … FOR UPDATE), deletes or changes the key of the referenced row of products and orders"
        ),
        "{}",
        f.fix.as_ref().unwrap().summary
    );
}

// The aptabase-us reading: a stopped insert was told to "find what the slow runs wait for while they happen", and
// storage was a cause though its data-file reads and writes took 86 ms against 19 s of slow runs.
#[test]
fn a_stopped_unstable_insert_points_at_what_runs_now() {
    let (mean, sd, max, n) = spread_of(&[(2.0, 9_900.0), (5000.0, 100.0)]);
    let mut s = unstable(mean, sd, max, n);
    let q = query(&mut s, "3432126458605143571");
    q.text = "INSERT INTO events (customer_id, kind) VALUES ($1, $2)".into();
    q.plan = Some(insert_plan_into("public.events"));
    // Quiet since the morning before the data ends: a day of silence (four quiet hours are too soon to say it stopped).
    q.first_seen = Some("2026-10-10T06:00:00Z".into());
    q.last_seen = Some("2026-10-10T08:00:00Z".into());
    q.waits = vec![WaitStat {
        category: "IO".into(),
        wait_type: Some("data file reads and writes".into()),
        wait_ms: 86.0,
        tasks: None,
    }];
    let next = query(&mut s, "2885972271926804069");
    next.text = "INSERT INTO events (customer_id, kind, payload) VALUES ($1, $2, $3)".into();
    next.plan = None;
    let report = analyze(s.clone());
    let f = find(&report.findings, "unstable-query:3432126458605143571").expect("an unstable insert");
    assert!(f.inactive, "it stopped");
    let fix = &f.fix.as_ref().unwrap().summary;
    assert!(
        fix.starts_with("This statement no longer runs. Query 2885972271926804069, an insert into events too, still does: if it replaced this one, its runs can wait the same way"),
        "{fix}"
    );
    assert!(!fix.contains("Find what the slow runs wait for") && !fix.contains("on storage"), "{fix}");
    assert!(f.summary.contains("Its data-file reads and writes took 86 ms in all"), "{}", f.summary);
    assert!(f.summary.contains("storage reads weren't it"), "{}", f.summary);
    assert!(f.summary.contains(", or on writing WAL"), "{}", f.summary);
    // Nothing else writes the table: it no longer runs, and that's what it says.
    query(&mut s, "2885972271926804069").text =
        "SELECT kind, count(*) FROM events WHERE customer_id = $1 GROUP BY kind".into();
    let report = analyze(s);
    let fix = report.findings.iter().find(|f| f.check == "unstable-query").unwrap().fix.clone().unwrap().summary;
    assert!(fix.starts_with("This statement no longer runs, so there is nothing to watch now"), "{fix}");
}

// SQL Server's Query Store times an autocommit statement's commit with it: its log flush stays among the waits.
#[test]
fn an_unstable_insert_on_sql_server_can_wait_on_its_commit() {
    let mut s = snapshot("mssql-demo.json");
    let (mean, sd, max, n) = spread_of(&[(2.0, 9_900.0), (5000.0, 100.0)]);
    let q = query(&mut s, "6");
    (q.executions, q.mean_time_ms, q.total_time_ms, q.stddev_time_ms, q.max_time_ms) =
        (n, mean, mean * n, Some(sd), Some(max));
    q.text = "INSERT INTO [shop].[products] ([name]) VALUES (@P1)".into();
    q.plan = Some(
        serde_json::from_value(serde_json::json!({
            "engine": "mssql", "kind": "estimated", "total_cost": 0.01, "warnings": [], "index_hints": [], "props": {},
            "root": {
                "id": 0, "op": "Clustered Index Insert", "kind": "modify", "object": "[shop].[products]",
                "est_rows": 1.0, "est_executions": 1.0, "subtree_cost": 0.01, "self_cost": 0.01,
                "warnings": [], "props": {},
                "children": [{
                    "id": 1, "op": "Constant Scan", "kind": "compute", "est_rows": 1.0, "est_executions": 1.0,
                    "subtree_cost": 0.0, "self_cost": 0.0, "warnings": [], "props": {}, "children": []
                }]
            }
        }))
        .expect("plan"),
    );
    let total: f64 = s.workload.as_ref().unwrap().queries.iter().map(|q| q.total_time_ms).sum();
    s.workload.as_mut().unwrap().total_time_ms = total;
    let report = analyze(s);
    let f = find(&report.findings, "unstable-query:6").expect("an unstable insert");
    assert!(f.summary.contains("the log flush at commit"), "{}", f.summary);
    assert!(!f.summary.contains("FOR KEY SHARE"), "{}", f.summary);
}

// ── stopped running ──────────────────────────────────────────────────────────────────────────────

// The aptabase-us reading: pg_stat_statements keeps no last run, so Culprit dates one by its own scans, and a
// statement that hasn't grown since the first of them last ran by then at the latest. "Has not run since" that scan
// said it stopped then, and "a deploy most likely changed it" sent the reader to that day's deploy.
#[test]
fn a_statement_quiet_since_the_first_scan_may_have_stopped_earlier() {
    let mut s = snapshot("postgres-demo.json");
    s.workload.as_mut().unwrap().timeline = Some(
        serde_json::from_value(serde_json::json!({
            "source": "scans",
            "starts": ["2026-10-11T08:00:00Z", "2026-10-11T09:00:00Z", "2026-10-11T10:00:00Z", "2026-10-11T11:00:00Z"],
            "end": "2026-10-11T11:54:21Z",
            "time_ms": [1000.0, 1000.0, 1000.0, 1000.0],
            "executions": [100.0, 100.0, 100.0, 100.0]
        }))
        .expect("timeline"),
    );
    let q = query(&mut s, ORDERS_SORT);
    q.trend = None;
    q.last_seen = Some("2026-10-11T08:00:00Z".into());
    // Four hours of Culprit's scans without a run: too soon to say it stopped.
    let report = analyze(s.clone());
    let f = find(&report.findings, &format!("temp-spill:{ORDERS_SORT}")).expect("the spill");
    assert!(!f.inactive);
    assert_eq!(
        f.caveats[0],
        "This query hasn't run in the 3 h 54 min since Culprit's first scan (Oct 11, 08:00 UTC): too short a time to say it stopped."
    );
    // A day and more of them.
    let timeline = s.workload.as_mut().unwrap().timeline.as_mut().unwrap();
    timeline.starts = ["2026-10-10T08:00:00Z", "2026-10-10T16:00:00Z", "2026-10-11T00:00:00Z", "2026-10-11T08:00:00Z"]
        .map(String::from)
        .to_vec();
    query(&mut s, ORDERS_SORT).last_seen = Some("2026-10-10T08:00:00Z".into());
    let report = analyze(s.clone());
    let f = find(&report.findings, &format!("temp-spill:{ORDERS_SORT}")).expect("the spill");
    assert!(f.inactive);
    assert!(
        f.caveats[0].starts_with("This query hasn't run in the ")
            && f.caveats[0].contains(" since Culprit's first scan (Oct 10, 08:00 UTC); it may have stopped earlier."),
        "{}",
        f.caveats[0]
    );
    assert!(!f.caveats[0].contains("most likely"), "{}", f.caveats[0]);
    // Seen growing at a later scan: it ran until about then, and a deploy most likely changed it.
    query(&mut s, ORDERS_SORT).last_seen = Some("2026-10-10T09:00:00Z".into());
    let report = analyze(s);
    let f = find(&report.findings, &format!("temp-spill:{ORDERS_SORT}")).expect("the spill");
    assert!(f.caveats[0].starts_with("This query has not run since 2026-10-10 09:00"), "{}", f.caveats[0]);
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

#[test]
fn a_spill_points_at_the_step_that_spills() {
    // PostgreSQL: the plan's one sort (under a Gather Merge).
    let report = analyze(snapshot("postgres-demo.json"));
    let f = find(&report.findings, &format!("temp-spill:{ORDERS_SORT}")).unwrap();
    assert_eq!(f.queries[0].plan_node, Some(1), "{:?}", f.queries);
    // MySQL: the grouping step's internal temporary table, not the filesort above it.
    let report = analyze(snapshot("mysql-demo.json"));
    let f = find(&report.findings, &format!("temp-spill:{GROUP_BY}")).unwrap();
    assert_eq!(f.queries[0].plan_node, Some(2), "{:?}", f.queries);
}

#[test]
fn an_exchange_is_never_a_plans_costliest_step() {
    // The demo's Gather Merge is costed above the scan for passing 667K rows between processes: not work on the data.
    let s = snapshot("postgres-demo.json");
    let plan = s.workload.as_ref().unwrap().query(ORDERS_SORT).and_then(|q| q.plan.as_ref()).expect("plan");
    assert_eq!(plan.root.op, "Gather Merge");
    assert!(plan.root.self_cost > plan.nodes().iter().filter(|n| n.id != 0).map(|n| n.self_cost).fold(0.0, f64::max));
    assert_eq!(plan.hottest().map(|n| n.op.as_str()), Some("Seq Scan"));
}

const GROUP_BY: &str = "04cd7be9c366fadc";

#[test]
fn mysql_temp_table_advice_raises_only_what_limits_and_never_lowers() {
    // MySQL 8.4 running internal temporary tables on TempTable: max_heap_table_size doesn't limit them.
    let mut s = snapshot("mysql-demo.json");
    set(&mut s, "internal_tmp_mem_storage_engine", "TempTable");
    let report = analyze(s);
    let f = find(&report.findings, &format!("temp-spill:{GROUP_BY}")).unwrap();
    let sql = f.fix.as_ref().unwrap().sql.clone().unwrap();
    assert_eq!(sql, "SET PERSIST tmp_table_size = 67108864;");
    assert!(f.title.ends_with("builds an on-disk temporary table about once per run"), "{}", f.title);
    // Which engine it uses unread (the demo's saved scan): MEMORY's cap is both variables, so both are raised.
    let f = find(&analyze(snapshot("mysql-demo.json")).findings, &format!("temp-spill:{GROUP_BY}")).cloned().unwrap();
    assert_eq!(
        f.fix.unwrap().sql.as_deref(),
        Some("SET PERSIST tmp_table_size = 67108864;\nSET PERSIST max_heap_table_size = 67108864;")
    );

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

// The fresh SQL Server demo: 750 runs in three minutes, then none, read as "about 123 runs a day"; the fresh PostgreSQL
// demo's three runs in six days as "about 0.5 times a day".
#[test]
fn a_heavy_query_says_when_a_burst_ran_and_counts_a_few_runs() {
    let mut s = point_lookup(1.0, 0.0066);
    let w = s.workload.as_mut().unwrap();
    (w.window_start, w.window_hours) = (Some("2026-10-05T12:13:47Z".into()), 144.0);
    let q = query(&mut s, "7");
    (q.first_seen, q.last_seen) = (Some("2026-10-11T07:44:33Z".into()), Some("2026-10-11T07:47:52Z".into()));
    let f = find(&analyze(s).findings, "heavy-query:7").cloned().expect("heavy");
    assert!(
        f.summary.starts_with("It ran 3.0K times at 1 ms on average, on Oct 11, 07:44–07:47 UTC: "),
        "{}",
        f.summary
    );
    assert!(!f.summary.contains("a day"), "{}", f.summary);
    assert!(!f.metrics.iter().any(|m| m.label == "Runs per day"), "{:?}", f.metrics);
    assert!(f.metrics.iter().any(|m| m.label == "Runs" && m.value == 3_000.0), "{:?}", f.metrics);

    let mut s = snapshot("postgres-demo.json");
    let w = s.workload.as_mut().unwrap();
    (w.window_start, w.window_hours) = (Some("2026-10-05T11:54:21Z".into()), 144.0);
    let q = query(&mut s, "4911431806993420100");
    (q.executions, q.total_time_ms) = (3.0, 3.0 * q.mean_time_ms);
    let f = find(&analyze(s).findings, "heavy-query:4911431806993420100").cloned().expect("heavy");
    assert!(f.summary.starts_with("It ran 3 times at 19.0 s on average: "), "{}", f.summary);
}

/// The PostgreSQL demo as if pg_stat_statements had counted for 40 days, with 36 hours of Culprit's own scans in three
/// steps of 10 s of database time each, in which statement `id` ran `runs` times in `time_ms` a step.
fn with_scans(id: &str, runs: f64, time_ms: f64) -> Snapshot {
    let mut s = snapshot("postgres-demo.json");
    let w = s.workload.as_mut().unwrap();
    (w.window_start, w.window_hours) = (Some("2026-09-01T11:54:21Z".into()), 40.0 * 24.0);
    w.timeline = Some(
        serde_json::from_value(serde_json::json!({
            "source": "scans",
            "starts": ["2026-10-10T00:00:00Z", "2026-10-10T12:00:00Z", "2026-10-11T00:00:00Z"],
            "end": "2026-10-11T12:00:00Z",
            "time_ms": [10000.0, 10000.0, 10000.0],
            "executions": [5000.0, 5000.0, 5000.0]
        }))
        .expect("timeline"),
    );
    query(&mut s, id).trend =
        Some(culprit_core::QueryTrend { time_ms: vec![Some(time_ms); 3], executions: vec![Some(runs); 3] });
    s
}

// The aptabase readings: "about 388 times a day" and 31% averaged over three and a half years, where Culprit's own scans
// measured 1,670 a day and 43%; and "re-scan and compare this query's share", which a week moves by a hair and which a
// rewritten statement (a new id) freezes.
#[test]
fn a_heavy_query_on_running_totals_says_what_culprits_scans_measured() {
    // Forty runs in 40 days, 15 of them in the 36 hours of scans: 10 a day, most of the time there.
    let mut s = with_scans("4911431806993420100", 5.0, 9_500.0);
    let q = query(&mut s, "4911431806993420100");
    (q.executions, q.total_time_ms, q.logical_reads) = (40.0, 40.0 * q.mean_time_ms, Some(40.0 * 15.0));
    let total: f64 = s.workload.as_ref().unwrap().queries.iter().map(|q| q.total_time_ms).sum();
    s.workload.as_mut().unwrap().total_time_ms = total;
    let report = analyze(s);
    let f = find(&report.findings, "heavy-query:4911431806993420100").expect("heavy");
    // The scans' figures lead, the totals since the reset beside them where they differ twofold.
    assert!(
        f.summary.starts_with(
            "It ran about 10 times a day at 1.9 s on average over 36 hours of Culprit's own scans: 95% of all database time in them ("
        ) && f.summary.contains("about 1 run a day and 19.0 s on average over the 40 days since statistics were reset)."),
        "{}",
        f.summary
    );
    assert!(f.title.contains("takes 95% of database time"), "{}", f.title);
    assert!(f.metrics.iter().any(|m| m.label == "Runs per day over Culprit's scans"), "{:?}", f.metrics);
    let metric = |label: &str| f.metrics.iter().find(|m| m.label == label).map(|m| m.value);
    assert_eq!(metric("Average duration over Culprit's scans"), Some(1_900.0));
    assert!(metric("Average duration over the 40 days since statistics were reset").is_some(), "{:?}", f.metrics);
    let verify = &f.verify[0];
    assert!(
        verify.starts_with("pg_stat_statements keeps running totals, so a later scan's runs, share and averages for this query still hold every run from before the change.")
            && verify.contains("Scan when the change ships and again a day or more later, then compare the two in What changed, with Since at the ship time (`culprit changes <connection> --since <ship time>`): there ")
            && verify.ends_with("A rewritten statement is a new entry with an id of its own, and this one then shows as stopped running."),
        "{verify}"
    );
}

// The aptabase-us reading: the insert that replaced a stopped one took 12% of database time over Culprit's scans, but
// its entry was days old, so its share since 2023 was 0.13% and it had no finding while the stopped one had two.
#[test]
fn a_recent_entry_is_judged_by_culprits_scans() {
    // 6K of its 8.2K runs came in the scans, 1 s of every 10 s step.
    let report = analyze(with_scans("3432126458605143571", 2_000.0, 1_000.0));
    let f = find(&report.findings, "heavy-query:3432126458605143571").expect("heavy now");
    assert!(f.title.ends_with("takes 10% of database time"), "{}", f.title);
    assert!(
        f.summary.starts_with(
            "Its entry in pg_stat_statements is recent: 6.0K of its 8.2K runs came during 36 hours of Culprit's own scans, where it ran about 4.0K times a day at 0.50 ms on average and took 10% of database time."
        ),
        "{}",
        f.summary
    );
    assert!((f.impact.db_time_share.unwrap() - 0.1).abs() < 1e-9, "{:?}", f.impact);
    // Under a day of scans, its share of the totals decides, and it isn't heavy.
    let mut s = with_scans("3432126458605143571", 2_000.0, 1_000.0);
    let w = s.workload.as_mut().unwrap();
    let t = w.timeline.as_mut().unwrap();
    t.starts = vec!["2026-10-11T00:00:00Z".into(), "2026-10-11T04:00:00Z".into(), "2026-10-11T08:00:00Z".into()];
    assert!(find(&analyze(s).findings, "heavy-query:3432126458605143571").is_none());
    // An old entry whose runs mostly came before the scans is read by them too, in both directions: 10% over them,
    // against well under 1% of the totals since the reset, said beside them.
    let report = analyze(with_scans("3432126458605143571", 100.0, 1_000.0));
    let f = find(&report.findings, "heavy-query:3432126458605143571").expect("heavy over the scans");
    assert!(f.title.ends_with("takes 10% of database time"), "{}", f.title);
    assert!(
        f.summary
            .contains("over 36 hours of Culprit's own scans: 10% of all database time in them (<1% of database time"),
        "{}",
        f.summary
    );
    assert!(!f.summary.contains("is recent"), "{}", f.summary);
}

// Theme A's open ends, the aptabase-eu reading: chatty's "Runs per day: 43K" beside a title over Culprit's scans and a
// summary that named no period; an unstable query's average over the scans beside its slow runs' excess since 2023.
// Each figure says what it is over: the scans' runs, share and average so, the totals' beside them where they differ
// twofold, and what only the totals keep (rows a run, the slow runs' excess) over those.
#[test]
fn figures_over_the_scans_and_over_the_totals_each_say_what_they_are_over() {
    let totals = "over the 40 days since statistics were reset";
    // 15K of a 12-hour step's 50K runs (30%, 30K a day over the scans); 4M since the reset (100K a day, 5% of 80M).
    let mut s = with_scans("3432126458605143571", 15_000.0, 7_500.0);
    let w = s.workload.as_mut().unwrap();
    let t = w.timeline.as_mut().unwrap();
    (t.executions, t.time_ms) = (vec![50_000.0; 3], vec![30_000.0; 3]);
    w.total_executions = 80_000_000.0;
    let q = query(&mut s, "3432126458605143571");
    (q.executions, q.total_time_ms, q.rows) = (4_000_000.0, 4_000_000.0 * q.mean_time_ms, Some(4_000_000.0));
    let total: f64 = s.workload.as_ref().unwrap().queries.iter().map(|q| q.total_time_ms).sum();
    s.workload.as_mut().unwrap().total_time_ms = total;
    let report = analyze(s);
    let f = find(&report.findings, "chatty-query:3432126458605143571").expect("chatty over the scans");
    let metric = |label: &str| f.metrics.iter().find(|m| m.label == label).map(|m| m.value.round());
    assert_eq!(metric("Runs per day over Culprit's scans"), Some(30_000.0), "{:?}", f.metrics);
    assert_eq!(metric(&format!("Runs per day {totals}")), Some(100_000.0), "{:?}", f.metrics);
    let share = |label: &str| f.metrics.iter().find(|m| m.label == label).map(|m| (m.value * 100.0).round());
    assert_eq!(share("Share of executions over Culprit's scans"), Some(30.0), "{:?}", f.metrics);
    assert_eq!(share(&format!("Share of executions {totals}")), Some(5.0), "{:?}", f.metrics);
    assert!(metric("Average duration over Culprit's scans").is_some(), "{:?}", f.metrics);
    assert!(
        f.summary.starts_with("Its runs are 30% of all statement runs over 36 hours of Culprit's scans, and each run")
            && f.summary.contains(&format!(" (counted {totals})")),
        "{}",
        f.summary
    );

    // An unstable read: 1% of its 10K runs take 5 s, at the same average over the scans as over the totals.
    let (mean, sd, max, n) = spread_of(&[(2.0, 9_900.0), (5000.0, 100.0)]);
    let mut s = with_scans("3432126458605143571", 100.0, 100.0 * mean);
    let q = query(&mut s, "3432126458605143571");
    (q.executions, q.mean_time_ms, q.total_time_ms) = (n, mean, mean * n);
    (q.stddev_time_ms, q.max_time_ms) = (Some(sd), Some(max));
    let total: f64 = s.workload.as_ref().unwrap().queries.iter().map(|q| q.total_time_ms).sum();
    s.workload.as_mut().unwrap().total_time_ms = total;
    let report = analyze(s);
    let f = find(&report.findings, "unstable-query:3432126458605143571").expect("unstable over the scans");
    assert!(f.summary.contains(&format!("more than the average, together {totals}.")), "{}", f.summary);
    assert!(f.metrics.iter().any(|m| m.label == "Average over Culprit's scans"), "{:?}", f.metrics);
    let extra = format!("Slow runs' extra time {totals} (at least)");
    assert!(f.metrics.iter().any(|m| m.label == extra), "{:?}", f.metrics);
}

// Theme D's tense rule on an idle heavy query (the fresh PostgreSQL reading: "took 99% of database time, most likely
// waiting for locks" for an update that stopped): its share is until it stopped, whatever it spent it on.
#[test]
fn a_stopped_heavy_query_that_waited_took_its_share_until_it_stopped() {
    let mut s = snapshot("postgres-demo.json");
    let w = s.workload.as_mut().unwrap();
    (w.window_start, w.window_hours) = (Some("2026-10-05T11:54:21Z".into()), 144.0);
    let q = query(&mut s, "4911431806993420100");
    (q.executions, q.total_time_ms, q.logical_reads) = (50.0, 50.0 * q.mean_time_ms, Some(50.0 * 15.0));
    (q.first_seen, q.last_seen) = (Some("2026-10-06T00:00:00Z".into()), Some("2026-10-08T00:00:00Z".into()));
    let total: f64 = s.workload.as_ref().unwrap().queries.iter().map(|q| q.total_time_ms).sum();
    s.workload.as_mut().unwrap().total_time_ms = total;
    let f = find(&analyze(s).findings, "heavy-query:4911431806993420100").cloned().expect("heavy");
    assert!(f.inactive, "{}", f.title);
    assert!(
        f.title.starts_with("An update of products took ")
            && f.title.ends_with(" of database time until it stopped, most likely waiting for locks"),
        "{}",
        f.title
    );
}

/// The heavy-query check alone, before the refine stage takes away a heavy query another finding explains.
fn heavy_queries(s: &Snapshot) -> Vec<culprit_core::Finding> {
    let check = culprit_checks::all().into_iter().find(|c| c.info().id == "heavy-query").expect("heavy-query");
    check.run(s)
}

// The aptabase-us reading: the six heavy places went by the totals since 2023, so an insert that stopped running before
// the first scan (30% of those totals) took one, and a read at 10.7% over Culprit's scans (5.2% of the totals) lost
// its finding. Over a day of scans the six are those that run, ranked by the scans; history follows them.
#[test]
fn a_stopped_statement_never_takes_a_heavy_place_of_one_that_runs() {
    let mut s = with_scans("9065391607317686058", 10.0, 2_000.0);
    let w = s.workload.as_mut().unwrap();
    let t = w.timeline.as_mut().unwrap();
    t.time_ms = vec![20_000.0; 3];
    // Seven statements that run, at 10%, 9.5% … 7% of the scans' time.
    let running = [
        "9065391607317686058",
        "5308963031944094559",
        "-3722457391793582113",
        "2004611025554862962",
        "2885972271926804069",
        "3432126458605143571",
        "8465053948014496855",
    ];
    for (i, id) in running.iter().enumerate() {
        let q = query(&mut s, id);
        q.executions = q.executions.max(30.0);
        q.trend = Some(culprit_core::QueryTrend {
            time_ms: vec![Some(2_000.0 - 100.0 * i as f64); 3],
            executions: vec![Some(10.0); 3],
        });
    }
    // An insert that stopped two and a half days ago: most of the totals since the reset, none of the scans.
    let stopped = query(&mut s, "4911431806993420100");
    (stopped.executions, stopped.total_time_ms, stopped.mean_time_ms) = (100_000.0, 10_000_000.0, 100.0);
    (stopped.first_seen, stopped.last_seen) = (None, Some("2026-10-09T00:00:00Z".into()));
    stopped.trend = Some(culprit_core::QueryTrend { time_ms: vec![Some(0.0); 3], executions: vec![Some(0.0); 3] });
    let total: f64 = s.workload.as_ref().unwrap().queries.iter().map(|q| q.total_time_ms).sum();
    s.workload.as_mut().unwrap().total_time_ms = total;

    let heavy = heavy_queries(&s);
    let ids: Vec<&str> = heavy.iter().map(|f| f.id.trim_start_matches("heavy-query:")).collect();
    // The six that run, by their share over the scans, then the one that stopped: the seventh has no place.
    assert_eq!(
        ids,
        [&running[..6], &["4911431806993420100"]].concat(),
        "{:?}",
        heavy.iter().map(|f| &f.title).collect::<Vec<_>>()
    );
    assert!(heavy[0].title.ends_with("takes 10% of database time"), "{}", heavy[0].title);
    // In the report the stopped one is history, ranked after every statement that runs.
    let report = analyze(s);
    let f = find(&report.findings, "heavy-query:4911431806993420100").expect("history");
    // What it took until it stopped: its share of the totals, the scans having measured none of it.
    assert!(f.inactive && f.title.contains("took 99% of database time"), "{}", f.title);
    assert!(find(&report.findings, "heavy-query:8465053948014496855").is_none());
}

#[test]
fn an_aggregate_is_not_titled_by_rows_read_per_row_returned() {
    // `SELECT COUNT(*) FROM orders WHERE status = ? AND created_at >= ?` returns one row whatever it counts.
    let report = analyze(snapshot("mysql-demo.json"));
    let f = find(&report.findings, "heavy-query:c7b5ac9bb4bcecfe").unwrap();
    assert!(!f.title.contains("for every row it returns"), "{}", f.title);
}

// ── the costliest step of a cheap plan ───────────────────────────────────────────────────────────

/// Query 7 as a lookup on a unique index, run thousands of times at a millisecond and returning its row each time (not
/// so often that it's a chatty query): a seek and a key lookup of `looked_up` rows each in a table of 247K rows (SQL
/// Server's TableCardinality), at `cost` in all.
fn point_lookup(looked_up: f64, cost: f64) -> Snapshot {
    let mut s = snapshot("mssql-demo.json");
    let q = query(&mut s, "7");
    (q.executions, q.mean_time_ms, q.total_time_ms, q.rows) = (3_000.0, 1.0, 3_000.0, Some(3_000.0));
    q.logical_reads = Some(6.2 * 3_000.0);
    let plan = q.plan.as_mut().expect("plan");
    // The demo's query compares a converted column; this one is the plain seek it stands in for.
    plan.warnings.clear();
    plan.props.remove("PlanAffectingConvert");
    plan.total_cost = cost;
    plan.root.subtree_cost = cost;
    plan.root.self_cost = 0.0;
    let (seek, lookup) = plan.root.children.split_at_mut(1);
    let (seek, lookup) = (&mut seek[0], &mut lookup[0]);
    (seek.op, seek.kind, seek.predicate) = ("Index Seek".into(), NodeKind::Seek, None);
    (seek.est_rows, seek.rows_read, seek.table_rows) = (looked_up, Some(looked_up), Some(247_435.0));
    (seek.self_cost, seek.subtree_cost) = (cost * 0.4, cost * 0.4);
    (lookup.op, lookup.kind, lookup.object) = ("Key Lookup".into(), NodeKind::Lookup, Some("shop.customers".into()));
    (lookup.est_rows, lookup.est_executions, lookup.rows_read) = (1.0, looked_up, None);
    lookup.table_rows = Some(247_435.0);
    (lookup.self_cost, lookup.subtree_cost) = (cost * 0.6, cost * 0.6);
    for n in [seek, lookup] {
        n.warnings.clear();
    }
    s
}

#[test]
fn a_point_lookups_costliest_step_is_not_where_its_time_goes() {
    // Half of a plan costing 0.0066 is a lookup of one row: not "a key lookup that reads 247K rows", and no covering
    // index to add. The time is in how often it runs.
    let report = analyze(point_lookup(1.0, 0.0066));
    let f = find(&report.findings, "heavy-query:7").expect("still the heaviest query");
    assert!(!f.summary.contains("247"), "{}", f.summary);
    assert!(!f.summary.contains("key lookup"), "{}", f.summary);
    assert!(f.summary.contains("Its plan is cheap"), "{}", f.summary);
    let fix = &f.fix.as_ref().unwrap().summary;
    assert!(!fix.contains("covering index"), "{fix}");
    assert!(fix.contains("how often it runs"), "{fix}");
    assert_eq!(f.queries[0].plan_node, None, "no step to point at");
    let brief = culprit_checks::agent_brief(&report, f);
    assert!(!brief.contains("most expensive step"), "{brief}");
}

#[test]
fn a_plan_costed_cheap_whose_runs_are_slow_is_not_called_cheap() {
    // Stale statistics: costed as a point lookup, measured at over a second a run. The estimate is what's wrong.
    let mut s = point_lookup(1.0, 0.0066);
    let q = query(&mut s, "7");
    (q.executions, q.mean_time_ms, q.total_time_ms) = (1_000.0, 1_200.0, 1_200_000.0);
    let report = analyze(s);
    let f = find(&report.findings, "heavy-query:7").expect("heavy");
    assert!(!f.summary.contains("Its plan is cheap"), "{}", f.summary);
    assert!(!f.fix.as_ref().unwrap().summary.contains("already cheap"));
}

#[test]
fn many_key_lookups_in_a_plan_that_does_real_work_are_still_covered() {
    // 20K lookups in a plan costing 40: covering them is the fix, said once, by the plan's key-lookups finding that
    // explains the query's time. The rows it reads are the plan's, never the table's size.
    let report = analyze(point_lookup(20_000.0, 40.0));
    let f = report.findings.iter().find(|f| f.queries.iter().any(|q| q.id == "7")).expect("query 7");
    assert_eq!(f.id, "plan-insights:key-lookups:shop.customers");
    assert!(f.fix.as_ref().unwrap().summary.contains("as included columns"), "{:?}", f.fix);
    assert!(!f.summary.contains("247"), "{}", f.summary);
}

#[test]
fn a_plan_is_called_cheap_only_where_its_runs_measure_cheap() {
    // Each costed as a point lookup and run at a millisecond: what the runs read and return decides, and the summary
    // and the fix say it on the same condition.
    let cheap = |s: Snapshot| {
        let f = find(&analyze(s).findings, "heavy-query:7").cloned().expect("heavy");
        let fix = f.fix.as_ref().unwrap().summary.clone();
        assert_eq!(f.summary.contains("Its plan is cheap"), fix.contains("already cheap"), "{} | {fix}", f.summary);
        f.summary.contains("Its plan is cheap")
    };
    assert!(cheap(point_lookup(1.0, 0.0066)));
    // 750 pages a run where the plan expected to read one row (stale statistics).
    let mut s = point_lookup(1.0, 0.0066);
    let q = query(&mut s, "7");
    (q.logical_reads, q.rows) = (Some(750.0 * 3_000.0), Some(0.0));
    assert!(!cheap(s));
    // 201 rows a run where the plan expected one (a generic plan's guess).
    let mut s = point_lookup(1.0, 0.0066);
    let q = query(&mut s, "7");
    (q.logical_reads, q.rows) = (Some(67.0 * 3_000.0), Some(201.0 * 3_000.0));
    assert!(!cheap(s));
    // 6 ms a run is under the 10 ms the summary asks of a cheap run: the fix says so too.
    let mut s = point_lookup(1.0, 0.0066);
    let q = query(&mut s, "7");
    (q.mean_time_ms, q.total_time_ms) = (6.0, 18_000.0);
    assert!(cheap(s));
}

#[test]
fn a_read_that_returns_nothing_is_titled_by_its_reads_per_run() {
    // Per row its reads would be counted against a row it never returns; per run they're 750 pages for nothing.
    let mut s = point_lookup(1.0, 0.0066);
    let q = query(&mut s, "7");
    (q.logical_reads, q.rows) = (Some(750.0 * 3_000.0), Some(0.0));
    let f = find(&analyze(s).findings, "heavy-query:7").cloned().expect("heavy");
    assert!(f.title.ends_with("reads 750 pages a run to return nothing"), "{}", f.title);
    // A lookup that finds nothing reads a few pages: no title about its reads.
    let mut s = point_lookup(1.0, 0.0066);
    query(&mut s, "7").rows = Some(0.0);
    let f = find(&analyze(s).findings, "heavy-query:7").cloned().expect("heavy");
    assert!(!f.title.contains("pages"), "{}", f.title);
}

#[test]
fn a_cheap_write_is_told_to_write_more_per_statement_not_to_cache() {
    let mut s = point_lookup(1.0, 0.0066);
    query(&mut s, "7").text = "INSERT INTO [shop].[customers] ([name], [email]) VALUES (@P1, @P2)".into();
    let f = find(&analyze(s).findings, "heavy-query:7").cloned().expect("heavy");
    // A write's plan costs next to nothing whatever its runs cost: not "cheap", the write itself.
    assert!(!f.summary.contains("Its plan is cheap"), "{}", f.summary);
    assert!(f.summary.contains("what a run costs is the write itself") && f.summary.contains("Each run inserts 1 row"));
    let fix = f.fix.unwrap().summary;
    // SQL Server's words for many rows at once; its only unique key is the id it doesn't set, so one transaction
    // makes no other insert wait; and no index to drop where none goes unread.
    assert!(
        fix.contains(
            "Insert several rows per statement where the application has them together (a multi-row VALUES, a table-valued parameter, or a bulk insert), in one transaction."
        ),
        "{fix}"
    );
    assert!(!fix.contains("cache") && !fix.contains("skip") && !fix.contains("index"), "{fix}");
}

/// The PostgreSQL demo's lookup of customers turned into `text`, an insert of the values it is given into products
/// (2,000 runs at 5 ms, 15 pages each, `rows` rows changed in all, 1.7 KB of WAL a run), counted for 40 days.
fn heavy_insert(text: &str, rows: f64) -> Snapshot {
    let mut s = snapshot("postgres-demo.json");
    s.pressure.get_or_insert_with(Default::default).uptime_hours = Some(24.0 * 40.0);
    let q = query(&mut s, "3432126458605143571");
    q.text = text.into();
    q.plan = Some(insert_plan_into("public.products"));
    (q.executions, q.mean_time_ms, q.total_time_ms) = (2_000.0, 5.0, 10_000.0);
    (q.rows, q.logical_reads) = (Some(rows), Some(15.0 * 2_000.0));
    q.costs = Some(culprit_core::QueryCosts {
        log_bytes: Some(2_000.0 * 1_700.0),
        log_share: Some(0.99),
        log_full_pages: Some(1_350.0),
        ..Default::default()
    });
    let total: f64 = s.workload.as_ref().unwrap().queries.iter().map(|q| q.total_time_ms).sum();
    s.workload.as_mut().unwrap().total_time_ms = total;
    s
}

// The aptabase readings: "skip the statement where the application already knows the row exists", verified by "its
// time and its reads per run should drop". Skipping cuts runs, and the runs left are the ones that write, which read
// more each: a developer would read that as the fix failing. A fix that cuts runs expects fewer runs and less time.
#[test]
fn a_fix_that_cuts_runs_expects_fewer_runs_never_fewer_reads() {
    // pg_stat_statements: the difference between two scans, in What changed, which carries runs and time, said the
    // CLI's way too.
    let report =
        analyze(heavy_insert("INSERT INTO products (sku, name) VALUES ($1, $2) ON CONFLICT DO NOTHING", 500.0));
    let f = find(&report.findings, "heavy-query:3432126458605143571").expect("a heavy insert");
    assert!(f.fix.as_ref().unwrap().summary.contains("skip the statement"), "{:?}", f.fix);
    let verify = f.verify.join(" ");
    assert!(
        verify.contains(
            "Scan when the change ships and again a day or more later, then compare the two in What changed, with Since at the ship time (`culprit changes <connection> --since <ship time>`): there it should run fewer times and take less time in all, while a run's own time can stay as it is, or rise."
        ),
        "{verify}"
    );
    assert!(!verify.contains("reads"), "{verify}");
    // Query Store: the next scan's window shows it.
    let report = analyze(point_lookup(1.0, 0.0066));
    let f = find(&report.findings, "heavy-query:7").expect("the heaviest query");
    assert_eq!(
        f.verify,
        [
            "After the change ships, re-scan: this query should run fewer times and take a smaller share of database time, while what one run reads and takes can stay as it is, or rise."
        ]
    );
}

// A statement that stopped has nothing to verify: its own step ("there its time and its reads per run should drop …
// and this one then shows as stopped running") promised a change no scan shows, for a statement already stopped.
#[test]
fn a_finding_whose_statements_stopped_has_nothing_to_check() {
    let mut s = heavy_insert("INSERT INTO products (sku, name) VALUES ($1, $2) ON CONFLICT DO NOTHING", 500.0);
    // Three days of statistics, and none of its runs in the last 28 hours.
    s.workload.as_mut().unwrap().window_hours = 72.0;
    let q = query(&mut s, "3432126458605143571");
    (q.first_seen, q.last_seen, q.trend) = (None, Some("2026-10-10T08:00:00Z".into()), None);
    let report = analyze(s);
    let f = find(&report.findings, "heavy-query:3432126458605143571").expect("a heavy insert");
    assert!(f.inactive, "{:?}", f.caveats);
    assert_eq!(
        f.verify,
        ["Nothing to check unless it runs again: a later scan that sees it run lists this finding as current again."]
    );
    // Said once: the brief's codebase section doesn't say it again, nor that the totals keep the earlier runs.
    let brief = culprit_checks::agent_brief(&report, f);
    assert_eq!(brief.matches("Nothing to check").count(), 1, "{brief}");
    assert!(!brief.contains("keeps running totals") && !brief.contains("the way *How to verify* says"), "{brief}");
}

// The aptabase readings: `INSERT … ON CONFLICT DO NOTHING` took a quarter of database time at 0.56 ms a run, with 3 of
// 4 runs inserting nothing and 99% of the WAL. "Its plan is cheap, so the share comes from how often it runs", and a
// fix to batch "inside one transaction" (unstable-query on the same insert says keep those short), with a table-valued
// parameter (SQL Server's) and indexes to drop where none goes unread.
#[test]
fn a_write_whose_runs_mostly_change_nothing_is_told_to_skip_them() {
    let text = "INSERT INTO products (sku, name) VALUES ($1, $2) ON CONFLICT DO NOTHING";
    let report = analyze(heavy_insert(text, 500.0));
    let f = find(&report.findings, "heavy-query:3432126458605143571").expect("a heavy insert");
    assert!(!f.summary.contains("cheap") && !f.summary.contains("how often it runs"), "{}", f.summary);
    assert!(
        f.summary.contains(
            "Each run inserts 0.25 rows on average (75% of its runs insert nothing) and writes 1.7 KB of WAL, 99% of all the WAL the statements wrote: 6.6 KB and 2.7 full-page images"
        ),
        "{}",
        f.summary
    );
    let fix = &f.fix.as_ref().unwrap().summary;
    assert!(
        fix.contains(
            "75% of its runs insert nothing: skip the statement where the application already knows the row exists"
        ),
        "{fix}"
    );
    for wrong in ["table-valued", "transaction", "Insert several rows"] {
        assert!(!fix.contains(wrong), "{wrong}: {fix}");
    }

    // A row a run: more rows per statement, in PostgreSQL's words. products_sku_key is unique on the sku it sets, so
    // another transaction inserting the same sku waits for this one: not "in one transaction". products_sku_idx has
    // had no reads in 40 days: the one index worth dropping.
    let f = analyze(heavy_insert("INSERT INTO products (sku, name) VALUES ($1, $2)", 2_000.0))
        .findings
        .into_iter()
        .find(|f| f.id == "heavy-query:3432126458605143571")
        .expect("a heavy insert");
    let fix = f.fix.unwrap().summary;
    assert!(
        fix.contains(
            "Insert several rows per statement where the application has them together (a multi-row VALUES, arrays unnested into rows, or COPY)."
        ),
        "{fix}"
    );
    assert!(!fix.contains("transaction") && !fix.contains("table-valued"), "{fix}");
    assert!(fix.contains("products_sku_idx has had no reads in the 40 days"), "{fix}");
    assert!(f.summary.contains("Each run inserts 1 row on average and writes 1.7 KB of WAL"), "{}", f.summary);
}

// MS-10: the PostgreSQL demo's fresh reading told an UPDATE that ran 8 times in 6 days and a FOR UPDATE read that ran 3
// times to "find what holds the locks while it waits (Live shows the blocking chain)", while unstable-query already
// says runs that rare are logged next time. One wording for every check that names a lock wait.
#[test]
fn a_lock_wait_too_rare_to_catch_live_is_logged_next_time() {
    let fix = |runs: f64| {
        let mut s = snapshot("postgres-demo.json");
        let q = query(&mut s, "4911431806993420100");
        (q.executions, q.total_time_ms, q.mean_time_ms) = (runs, runs * 19_000.0, 19_000.0);
        let total: f64 = s.workload.as_ref().unwrap().queries.iter().map(|q| q.total_time_ms).sum();
        s.workload.as_mut().unwrap().total_time_ms = total;
        find(&analyze(s).findings, "heavy-query:4911431806993420100")
            .cloned()
            .expect("an update waiting for locks")
            .fix
            .unwrap()
    };
    let rare = fix(3.0);
    assert!(
        rare.summary.starts_with(
            "It ran 3 times in 5 hours, too seldom to count on catching it while it waits. Have the next ones logged instead. Set log_lock_waits to on"
        ),
        "{}",
        rare.summary
    );
    assert!(rare.summary.contains("logs each lock wait longer than deadlock_timeout"), "{}", rare.summary);
    assert!(!rare.summary.contains("Live shows"), "{}", rare.summary);
    assert!(
        rare.sql.as_deref().is_some_and(|sql| sql.contains("ALTER SYSTEM SET log_lock_waits = 'on'")),
        "{:?}",
        rare.sql
    );
    // Twenty runs an hour: there are waits to catch while they happen.
    let often = fix(100.0);
    assert!(
        often.summary.starts_with("Find what holds the locks while it waits (Live shows the blocking chain)."),
        "{}",
        often.summary
    );
    assert_eq!(often.sql, None);
}

// US-4: the aptabase-us reading said "Each run inserts 1 row on average" and "Insert several rows per statement" for an
// `INSERT … ON CONFLICT … DO UPDATE SET salt = app_salts.salt RETURNING salt`: a get-or-create whose rows count the
// rows it rewrote, each a new row version and its WAL. Its levers are not sending it when the row is known, and
// DO NOTHING with a read on a miss; an upsert with new values updates only rows that differ.
#[test]
fn an_upsert_is_said_to_insert_or_update_and_a_get_or_create_is_fixed_as_one() {
    let get_or_create = "INSERT INTO products (sku, name) VALUES ($1, $2) ON CONFLICT (sku) DO UPDATE SET name = products.name RETURNING name";
    let f = find(&analyze(heavy_insert(get_or_create, 2_000.0)).findings, "heavy-query:3432126458605143571")
        .cloned()
        .expect("a heavy upsert");
    assert!(
        f.summary.contains("Each run inserts or updates 1 row on average (a run that finds the row there rewrites it"),
        "{}",
        f.summary
    );
    let fix = f.fix.unwrap().summary;
    assert!(fix.contains("It is a get-or-create: its DO UPDATE sets a column to the value it has"), "{fix}");
    assert!(
        fix.contains(
            "insert with ON CONFLICT DO NOTHING RETURNING and read the row with a SELECT only when that returns nothing"
        ),
        "{fix}"
    );
    assert!(!fix.contains("several rows per statement"), "{fix}");
    let upsert =
        "INSERT INTO products (sku, name) VALUES ($1, $2) ON CONFLICT (sku) DO UPDATE SET name = EXCLUDED.name";
    let f = find(&analyze(heavy_insert(upsert, 2_000.0)).findings, "heavy-query:3432126458605143571")
        .cloned()
        .expect("a heavy upsert");
    assert!(f.summary.contains("Each run inserts or updates 1 row on average and writes"), "{}", f.summary);
    let fix = f.fix.unwrap().summary;
    assert!(fix.contains("give its DO UPDATE a WHERE that updates only a row whose values differ"), "{fix}");
}

// The aptabase-us reading: "An insert into app_salts takes 30% of database time" for an insert silent since Culprit's
// first scan.
#[test]
fn a_statement_that_stopped_is_titled_in_the_past() {
    let mut s = heavy_insert("INSERT INTO products (sku, name) VALUES ($1, $2) ON CONFLICT DO NOTHING", 500.0);
    // Quiet for the last four hours of the window: too soon to say it stopped, so it is told as it runs.
    query(&mut s, "3432126458605143571").last_seen = Some("2026-10-11T08:00:00Z".into());
    let f = find(&analyze(s.clone()).findings, "heavy-query:3432126458605143571").cloned().expect("a heavy insert");
    assert!(!f.inactive);
    assert!(f.title.starts_with("An insert into products takes "), "{}", f.title);
    // Quiet since the morning before: a day of silence. Its title, summary and fix are in the past.
    let q = query(&mut s, "3432126458605143571");
    q.first_seen = Some("2026-10-10T06:00:00Z".into());
    q.last_seen = Some("2026-10-10T08:00:00Z".into());
    let report = analyze(s);
    let f = find(&report.findings, "heavy-query:3432126458605143571").expect("a heavy insert");
    assert!(f.inactive);
    assert!(
        f.title.starts_with("An insert into products took ") && f.title.ends_with(" until it stopped"),
        "{}",
        f.title
    );
    assert!(
        f.summary.contains("Its plan had nothing to make faster: what a run cost was the write itself"),
        "{}",
        f.summary
    );
    assert!(f.summary.contains(" and wrote 1.7 KB of WAL"), "{}", f.summary);
}

#[test]
fn a_wait_that_takes_most_of_a_statements_time_is_said() {
    let mut s = point_lookup(1.0, 0.0066);
    let q = query(&mut s, "7");
    q.waits = vec![WaitStat {
        category: "IO".into(),
        wait_type: Some("Buffer IO".into()),
        wait_ms: q.total_time_ms * 0.85,
        tasks: None,
    }];
    let f = find(&analyze(s).findings, "heavy-query:7").cloned().expect("heavy");
    assert!(
        f.summary.contains(
            "85% of its time is spent waiting for data to come from storage because it wasn't in memory (Query Store: Buffer IO)."
        ),
        "{}",
        f.summary
    );
    // Under half of its time, the wait isn't what its runs are about.
    let mut s = point_lookup(1.0, 0.0066);
    let q = query(&mut s, "7");
    q.waits = vec![WaitStat {
        category: "IO".into(),
        wait_type: Some("Buffer IO".into()),
        wait_ms: q.total_time_ms * 0.3,
        tasks: None,
    }];
    let f = find(&analyze(s).findings, "heavy-query:7").cloned().expect("heavy");
    assert!(!f.summary.contains("Buffer IO"), "{}", f.summary);
}

// The fresh PostgreSQL demo: `SELECT … FOR UPDATE` ran 3 times at 38 s, reading 9 pages a run, and was told to "check
// whether it reads more rows or columns than it needs" while the UPDATE beside it was read as waiting for a lock.
#[test]
fn a_read_that_locks_its_rows_and_waited_is_waiting_for_locks() {
    let mut s = snapshot("postgres-demo.json");
    let q = query(&mut s, "4911431806993420100");
    q.text = "SELECT id FROM orders WHERE id = $1 FOR UPDATE".into();
    (q.executions, q.mean_time_ms, q.total_time_ms) = (3.0, 38_412.0, 115_236.0);
    (q.rows, q.logical_reads, q.plan) = (Some(3.0), Some(27.0), None);
    let total: f64 = s.workload.as_ref().unwrap().queries.iter().map(|q| q.total_time_ms).sum();
    s.workload.as_mut().unwrap().total_time_ms = total;
    let report = analyze(s);
    let f = find(&report.findings, "heavy-query:4911431806993420100").expect("the locking read");
    assert!(f.title.ends_with("most likely waiting for locks"), "{}", f.title);
    assert!(
        f.summary.contains(
            "most likely for the lock its FOR UPDATE takes on the rows it finds, which another transaction held"
        ),
        "{}",
        f.summary
    );
    let fix = &f.fix.as_ref().unwrap().summary;
    assert!(!fix.contains("reads more rows") && fix.contains("won't shorten the wait"), "{fix}");
    assert!(f.metrics.iter().any(|m| m.label == "Rows returned per run"), "{:?}", f.metrics);
}

// A staging SQL Server reading: "with a unique index on those columns, an insert that handles the duplicate-key error… needs
// no read first" for an idempotency check whose plan seeks that very unique index: it reads as one to create.
#[test]
fn a_check_for_a_row_names_the_unique_index_its_plan_seeks() {
    let mut s = point_lookup(1.0, 0.0066);
    let q = query(&mut s, "7");
    q.rows = Some(1.0);
    q.waits = vec![WaitStat {
        category: "IO".into(),
        wait_type: Some("Buffer IO".into()),
        wait_ms: q.total_time_ms * 0.86,
        tasks: None,
    }];
    s.indexes.iter_mut().find(|i| i.name == "ix_customers_email").unwrap().is_unique = true;
    let fix = find(&analyze(s).findings, "heavy-query:7").cloned().expect("heavy").fix.unwrap().summary;
    assert!(
        fix.contains("this plan seeks ix_customers_email on shop.customers, a unique index that exists already, so an insert that handles its duplicate-key error (or an upsert) needs no read first"),
        "{fix}"
    );
    assert!(!fix.contains("with a unique index on those columns"), "{fix}");
}

/// Query 7 as a seek of ix_customers_email costed for one row whose key lookups check `segment` and keep none: 750
/// pages a run (about 250 lookups into a clustered index three levels deep), and no row returned.
fn lookups_for_nothing() -> Snapshot {
    let mut s = point_lookup(1.0, 0.0066);
    let q = query(&mut s, "7");
    q.text = "SELECT [id], [name] FROM [shop].[customers] WHERE [email] = @P1 AND [segment] = @P2".into();
    (q.logical_reads, q.rows) = (Some(750.0 * 3_000.0), Some(0.0));
    let plan = q.plan.as_mut().expect("plan");
    plan.root.children[0].seek_predicate = Some("[email] = [@P1]".into());
    plan.root.children[1].predicate = Some("[culprit_demo].[shop].[customers].[segment]=[@P2]".into());
    s
}

#[test]
fn lookups_that_find_nothing_are_the_plans_estimate_not_the_applications_code() {
    let f = find(&analyze(lookups_for_nothing()).findings, "heavy-query:7").cloned().expect("heavy");
    assert!(f.title.ends_with("reads 750 pages a run to return nothing"), "{}", f.title);
    assert!(
        f.summary.contains(
            "Its plan expects 1 row from ix_customers_email (on email), then looks up each row it finds in the table to check segment: its runs read 750 pages each, about 250 lookups at 3 pages each, and return nothing."
        ),
        "{}",
        f.summary
    );
    assert!(f.summary.ends_with("The plan was costed for far fewer rows than its runs find."), "{}", f.summary);
    assert_eq!(f.queries[0].plan_node, Some(4), "the lookup that checks segment");
    // No index leads with segment: one that does, and the statistics the plan was costed with.
    let fix = f.fix.clone().unwrap();
    assert!(!fix.summary.contains("Find this statement in the application"), "{}", fix.summary);
    assert!(fix.summary.contains("The plan was costed for 1 row from ix_customers_email, but its runs look up about 250 rows each and keep none"), "{}", fix.summary);
    assert!(fix.summary.contains("an index that leads with segment finds the rows directly"), "{}", fix.summary);
    let sql = fix.sql.unwrap();
    assert!(
        sql.contains("CREATE NONCLUSTERED INDEX [IX_customers_segment]")
            && sql.ends_with("UPDATE STATISTICS [shop].[customers];"),
        "{sql}"
    );
    assert_eq!(f.verify, vec!["After the change, re-scan: its pages read per run should drop from 750 to a handful."]);

    // An index already leads with segment: the statistics are the fix, and the index is named, the one that holds
    // every row before a filtered one.
    let mut s = lookups_for_nothing();
    let email = s.indexes.iter().find(|i| i.name == "ix_customers_email").cloned().unwrap();
    let mut filtered = email.clone();
    (filtered.name, filtered.columns, filtered.filter) =
        ("ix_customers_segment_active".into(), vec!["segment".into()], Some("([is_active]=(1))".into()));
    let mut segment = email;
    (segment.name, segment.columns) = ("ix_customers_segment".into(), vec!["segment".into()]);
    s.indexes.extend([filtered, segment]);
    let fix = find(&analyze(s).findings, "heavy-query:7").cloned().expect("heavy").fix.unwrap();
    assert!(
        fix.summary.contains("find the rows by segment instead: ix_customers_segment already leads with it."),
        "{}",
        fix.summary
    );
    assert_eq!(fix.sql.as_deref(), Some("UPDATE STATISTICS [shop].[customers];"));
}

// A staging SQL Server reading: heavy-query:36649 told to update statistics the catalog shows updated with a full scan two
// days after the plan was compiled, the runs since still reading 750 pages, and with no way on if they keep doing so.
#[test]
fn statistics_updated_since_the_plan_was_compiled_are_said_with_a_way_on() {
    let mut s = lookups_for_nothing();
    let email = s.indexes.iter().find(|i| i.name == "ix_customers_email").cloned().unwrap();
    let mut segment = email;
    (segment.name, segment.columns) = ("ix_customers_segment".into(), vec!["segment".into()]);
    s.indexes.push(segment);
    // The plan was compiled with the statistics of Oct 1; the catalog has them updated from every row on Oct 5, and
    // every run measured came after.
    query(&mut s, "7").plan.as_mut().unwrap().raw = Some(
        r#"<ShowPlanXML><StatisticsInfo LastUpdate="2026-10-01T10:00:00.00" ModificationCount="12" SamplingPercent="100" Statistics="[ix_customers_email]" Table="[customers]" Schema="[shop]" Database="[culprit_demo]" /></ShowPlanXML>"#
            .into(),
    );
    let stat = s.statistics.iter_mut().find(|st| st.name == "ix_customers_email").unwrap();
    (stat.last_updated, stat.rows, stat.rows_sampled) = (Some("2026-10-05T10:00:00Z".into()), Some(2e5), Some(2e5));
    let fix = find(&analyze(s.clone()).findings, "heavy-query:7").cloned().expect("heavy").fix.unwrap();
    assert!(
        fix.summary.contains(
            "The statistics on ix_customers_email were last updated on Oct 5 from every row, after this plan was compiled with those of Oct 1, and the runs measured since still look up as many rows: fresh statistics may not change the estimate."
        ),
        "{}",
        fix.summary
    );
    // CX-2: a bare FORCESEEK allows the seek the plan already does; the hint names the index and the columns it seeks
    // on, and goes in the statement where the application sends it, which the fix's SQL says for the brief.
    assert!(
        fix.summary.ends_with(
            "change the statement where the application sends it to read shop.customers with a table hint that seeks ix_customers_segment on segment: `WITH (FORCESEEK ([ix_customers_segment] ([segment])))`."
        ),
        "{}",
        fix.summary
    );
    assert_eq!(
        fix.sql.as_deref(),
        Some(
            "UPDATE STATISTICS [shop].[customers];\n\n-- If a re-scan still shows about 750 pages a run, change the statement in the application:\n--    read [shop].[customers] WITH (FORCESEEK ([ix_customers_segment] ([segment])))"
        )
    );
    // The brief allows the code change the fallback is: the upkeep is not the whole fix.
    let report = analyze(s.clone());
    let finding = find(&report.findings, "heavy-query:7").cloned().unwrap();
    let brief = culprit_checks::agent_brief(&report, &finding);
    assert!(!brief.contains("The fix is upkeep of the database, not a change to the application's code"), "{brief}");
    assert!(brief.contains("The fix's other option is a change in the application's code"), "{brief}");
    // Statistics as old as the plan's: the update is the fix, as before.
    s.statistics.iter_mut().find(|st| st.name == "ix_customers_email").unwrap().last_updated =
        Some("2026-10-01T10:00:00Z".into());
    let fix = find(&analyze(s).findings, "heavy-query:7").cloned().expect("heavy").fix.unwrap();
    assert!(!fix.summary.contains("last updated") && !fix.summary.contains("table hint"), "{}", fix.summary);
}

#[test]
fn a_cheap_read_waiting_for_storage_is_told_its_pages_dont_stay_in_memory_not_to_cache() {
    // An idempotency check: 1 row in 3K runs of a few pages each, 86% of the time waiting for storage.
    let mut s = point_lookup(1.0, 0.0066);
    let q = query(&mut s, "7");
    q.rows = Some(1.0);
    q.waits = vec![WaitStat {
        category: "IO".into(),
        wait_type: Some("Buffer IO".into()),
        wait_ms: q.total_time_ms * 0.86,
        tasks: None,
    }];
    let f = find(&analyze(s).findings, "heavy-query:7").cloned().expect("heavy");
    assert!(!f.summary.contains("the share comes from how often it runs"), "{}", f.summary);
    assert!(f.summary.contains("Its plan is cheap: each run reads about 6 pages"), "{}", f.summary);
    // What the change shortens is the wait, not the six pages a run reads.
    assert_eq!(
        f.verify,
        vec![
            "After the change ships, re-scan: this query's share of database time should drop, with less of its time spent waiting for storage (86% now)."
        ]
    );
    let fix = f.fix.unwrap().summary;
    assert!(fix.contains("they don't stay in memory between runs") && fix.contains("more memory"), "{fix}");
    assert!(!fix.contains("cache the result") && !fix.contains("result cached"), "{fix}");
    assert!(fix.contains("a duplicate or idempotency check"), "{fix}");
    // One row in 3K runs reads as what it is, not as "0.0" rows a run.
    let rows = f.metrics.iter().find(|m| m.label.starts_with("Rows returned")).expect("rows");
    assert_eq!((rows.label.as_str(), rows.text.as_deref()), ("Rows returned", Some("1 row in 3.0K runs")));
}

#[test]
fn a_step_beside_one_the_plan_puts_no_cost_on_is_given_no_share() {
    // MySQL's filesort carries no cost: beside it, the scan reads as 100% of the plan's. Said plainly (MY-8), not as
    // "the step its plan costs highest… so no share is given" for a share the summary never mentioned.
    let report = analyze(snapshot("mysql-demo.json"));
    let f = find(&report.findings, "heavy-query:19bd4ff468839ea3").expect("the promo-code read");
    assert!(!f.summary.contains("of its estimated cost") && !f.summary.contains("share"), "{}", f.summary);
    assert!(
        f.summary.ends_with(
            "Its only step with a cost is a table scan of shop.orders that examines 500K rows a run (MySQL puts no cost on the sort)."
        ),
        "{}",
        f.summary
    );
}

#[test]
fn no_fix_tells_its_reader_to_copy_the_brief_it_is_in() {
    for name in ["mssql-demo.json", "postgres-demo.json", "mysql-demo.json"] {
        for f in analyze(snapshot(name)).findings {
            let fix = f.fix.map(|x| x.summary).unwrap_or_default();
            assert!(!fix.contains("copy the brief"), "{}: {fix}", f.id);
        }
    }
}

#[test]
fn mysqls_rows_examined_are_rows_and_measured_where_the_plan_reads_one_table() {
    // `SELECT COUNT(*) FROM orders WHERE status = ? AND created_at >= ?`: the plan estimates 29K rows read, the runs
    // examined 11K each.
    let report = analyze(snapshot("mysql-demo.json"));
    let f = find(&report.findings, "heavy-query:c7b5ac9bb4bcecfe").unwrap();
    assert!(f.summary.contains("an index range scan of shop.orders that examines 11K rows a run"), "{}", f.summary);
    assert!(!f.summary.contains("29K"), "{}", f.summary);
    let examined = f.metrics.iter().find(|m| m.label == "Rows examined per run").expect("rows examined");
    assert_eq!(examined.unit, Unit::Rows);
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
