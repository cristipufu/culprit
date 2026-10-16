//! SQLite with its application's query log (`fixtures/sqlite-demo.json`: `dev/sqlite` scanned with the Rails log that
//! `dev/sqlite/app-log.py` writes). The index SQLite builds for itself on every run becomes index advice, weighed by
//! the statements that need it; nothing the plan doesn't have (costs, a hottest step) is invented.

use culprit_checks::{analyze, index_advice};
use culprit_core::{CandidateSource, Severity, Snapshot};

fn demo() -> Snapshot {
    let text = std::fs::read_to_string(format!("{}/../../fixtures/sqlite-demo.json", env!("CARGO_MANIFEST_DIR")))
        .expect("fixture");
    serde_json::from_str(&text).expect("snapshot")
}

#[test]
fn the_log_covers_its_own_period() {
    let w = demo().workload.expect("the Rails log's statements");
    assert_eq!(w.log.as_deref(), Some("Rails"));
    assert_eq!(w.window_phrase(), "the 90 minutes the Rails log covers");
    // BEGIN and COMMIT count in the totals but aren't ranked as statements to tune.
    assert!(w.queries.iter().all(|q| !q.text.starts_with("BEGIN") && !q.text.starts_with("COMMIT")));
}

#[test]
fn an_index_sqlite_builds_on_every_run_is_advice() {
    let report = analyze(demo());
    let advice = index_advice(&report);
    let c = advice
        .candidates
        .iter()
        .find(|c| c.table == "orders" && c.keys == ["customer_id"])
        .expect("orders(customer_id): SQLite builds it for the region report");
    assert_eq!(c.source, CandidateSource::EngineBuilt);
    assert!(c.sql.as_deref().is_some_and(|s| s.contains("CREATE INDEX")), "{:?}", c.sql);
    // The listing whose plan reads every order to filter by customer needs the same index (it's also the foreign key):
    // its statement and share are the candidate's too, so it outweighs the report alone. The listing SQLite searches by
    // status instead isn't: its plan doesn't show that the key's index would serve it.
    assert_eq!(c.queries.len(), 2, "{:?}", c.queries);
    assert!(c.queries.iter().all(|q| !q.text.contains("\"status\" = ?")), "{:?}", c.queries);
    let report_share = report
        .snapshot
        .workload
        .as_ref()
        .and_then(|w| w.queries.iter().find(|q| q.text.contains("LEFT OUTER JOIN")))
        .map(|q| q.total_time_ms / report.snapshot.workload.as_ref().unwrap().total_time_ms)
        .expect("the report statement");
    assert!(c.db_time_share.is_some_and(|s| s > report_share), "{:?} vs {report_share}", c.db_time_share);
    // One cause, one finding: the foreign key's, which takes the evidence and points at the step that builds the index.
    assert_eq!(c.finding_id.as_deref(), Some("unindexed-fk:main.orders(customer_id)"));
    assert!(!report.findings.iter().any(|f| f.id.starts_with("plan-insights:automatic-index")));
    let fk =
        report.findings.iter().find(|f| f.id == "unindexed-fk:main.orders(customer_id)").expect("the key's finding");
    assert!(fk.summary.contains("builds this index for itself"), "{}", fk.summary);
    assert!(fk.queries.iter().any(|r| r.plan_node.is_some()), "{:?}", fk.queries);
    // It lists only the statements it claims. The listing SQLite searches by status isn't one, so that statement keeps a
    // finding of its own, and the headline adds the two.
    assert_eq!(fk.queries.len(), 2, "{:?}", fk.queries);
    let status = report
        .findings
        .iter()
        .find(|f| f.check == "heavy-query" && f.queries.iter().any(|r| !fk.queries.iter().any(|x| x.id == r.id)))
        .expect("the status listing's own finding");
    let together = fk.impact.db_time_share.unwrap_or(0.0) + status.impact.db_time_share.unwrap_or(0.0);
    assert_eq!(report.summary.top_share.map(|s| (s * 100.0).round()), Some((together * 100.0).round()));
}

#[test]
fn without_a_foreign_key_the_engine_built_index_is_its_own_finding() {
    let mut s = demo();
    s.foreign_keys.retain(|fk| fk.table != "orders");
    let report = analyze(s);
    // Weighed by the statement, and never above Medium: the plan doesn't say how much of each run the throwaway index
    // costs, nor what share of the plan it is.
    let f = report.findings.iter().find(|f| f.id == "plan-insights:automatic-index:orders").expect("the finding");
    assert!(f.severity <= Severity::Medium);
    assert!(!f.metrics.iter().any(|m| m.label == "Share of the plan's work"), "{:?}", f.metrics);
    let advice = index_advice(&report);
    let c = advice.candidates.iter().find(|c| c.table == "orders" && c.keys == ["customer_id"]).expect("advice");
    assert_eq!(c.source, CandidateSource::EngineBuilt);
    assert_eq!(c.finding_id.as_deref(), Some("plan-insights:automatic-index:orders"));
}

#[test]
fn an_index_that_exists_is_never_advised() {
    let mut s = demo();
    let mut existing = s.indexes.iter().find(|i| i.table == "orders").cloned().expect("an index on orders");
    existing.name = "ix_orders_customer".into();
    existing.columns = vec!["customer_id".into()];
    s.indexes.push(existing);
    let advice = index_advice(&analyze(s));
    assert!(!advice.candidates.iter().any(|c| c.table == "orders" && c.keys == ["customer_id"]));
}

#[test]
fn a_plan_without_costs_has_no_hottest_step() {
    let s = demo();
    let plans: Vec<_> = s.workload.iter().flat_map(|w| &w.queries).filter_map(|q| q.plan.as_ref()).collect();
    assert!(!plans.is_empty());
    assert!(plans.iter().all(|p| !p.weighed() && p.hottest().is_none()));
}

#[test]
fn a_statement_that_only_names_the_key_adds_no_weight() {
    let mut s = demo();
    // Without captured plans, nothing shows the listings read every order: the text naming customer_id isn't evidence,
    // so the key's finding falls back to what its rows say, and claims no share of database time.
    for q in s.workload.as_mut().expect("workload").queries.iter_mut() {
        q.plan = None;
    }
    let report = analyze(s);
    let fk =
        report.findings.iter().find(|f| f.id == "unindexed-fk:main.orders(customer_id)").expect("the key's finding");
    assert_eq!(fk.severity, Severity::Low, "{}", fk.summary);
    assert_eq!(fk.impact.db_time_share, None);
    assert!(!fk.summary.contains("of database time"), "{}", fk.summary);
}
