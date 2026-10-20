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
    // its statement and share are the candidate's too, so it outweighs the report alone. So does the listing SQLite
    // searches through the index on status (MY-7): it reads every order of that status and checks customer_id on each.
    assert_eq!(c.queries.len(), 3, "{:?}", c.queries);
    assert!(c.queries.iter().any(|q| q.text.contains("\"status\" = ?")), "{:?}", c.queries);
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
    // It lists the statements it claims, the status listing among them: one cause, one finding, so that statement's
    // heavy-query finding goes into this one, and the headline counts the key's share once.
    assert_eq!(fk.queries.len(), 3, "{:?}", fk.queries);
    assert!(
        !report.findings.iter().any(|f| f.id == "heavy-query:3c81e035bd6f73fa"),
        "{:?}",
        report.findings.iter().map(|f| &f.id).collect::<Vec<_>>()
    );
    let share = fk.impact.db_time_share.unwrap_or(0.0);
    assert!(share > 0.95, "{share}");
    // MY-9d: its grade rests on these reads, not on deletes alone: the verify shows the index serving them, in the
    // words SQLite's plan uses.
    assert_eq!(
        fk.verify.first().map(String::as_str),
        Some(
            "Explain the queries listed again: each reads main.orders through ix_orders_customer_id (`SEARCH … USING \
             INDEX ix_orders_customer_id`) instead of reading all of it."
        ),
        "{:?}",
        fk.verify
    );
    assert_eq!(report.summary.top_share.map(|s| (s * 100.0).round()), Some((share * 100.0).round()));
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

// Row counts appear with or without statistics: what a scan after ANALYZE shows is sqlite_stat1.
#[test]
fn the_statistics_finding_is_verified_by_sqlite_stat1() {
    let report = analyze(demo());
    let f = report.findings.iter().find(|f| f.id == "config:sqlite-statistics").expect("no statistics");
    assert!(f.verify.iter().any(|v| v.contains("sqlite_stat1")), "{:?}", f.verify);
    assert!(!f.verify.iter().any(|v| v.contains("row count")), "{:?}", f.verify);
}

#[test]
fn definition_based_cleanup_does_not_claim_usage_counters() {
    let mut s = demo();
    s.workload = None;
    let mut narrow = s.indexes.iter().find(|i| !i.is_primary_key && !i.is_unique).unwrap().clone();
    narrow.name = "by_customer".into();
    narrow.columns = vec!["customer_id".into()];
    narrow.filter = None;
    let mut wide = narrow.clone();
    wide.name = "by_customer_date".into();
    wide.columns.push("created_at".into());
    s.indexes = vec![narrow, wide];
    let advice = index_advice(&analyze(s));
    assert_eq!(advice.cleanups.len(), 1);
    assert_eq!(advice.cleanups[0].index, "by_customer");
    assert!(advice.counters.is_empty());
    assert!(advice.notes.iter().any(|n| n.contains("runtime impact of overlaps are unknown")));
    assert!(advice.notes.iter().any(|n| n.contains("use the recorded definitions")));
    assert!(!advice.notes.iter().any(|n| n.contains("overlapping indexes can't be judged")));
}

#[test]
fn virtual_table_sizes_do_not_turn_a_small_files_statistics_into_a_warning() {
    let mut s = demo();
    for t in &mut s.tables {
        t.rows = Some(100.0);
    }
    let mut virtual_table = s.tables[0].clone();
    virtual_table.name = "search".into();
    virtual_table.kind = Some("virtual".into());
    virtual_table.rows = None;
    s.tables.push(virtual_table);
    assert!(!analyze(s).findings.iter().any(|f| f.id == "config:sqlite-statistics"));
}
