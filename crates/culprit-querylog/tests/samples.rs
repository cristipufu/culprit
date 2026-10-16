//! Whole logs as frameworks write them, read the way a scan reads them: the format told from the lines, repeats of one
//! statement shape added up.

use culprit_core::{Engine, Workload};

fn read(name: &str) -> (&'static str, Workload) {
    let text = std::fs::read_to_string(format!("{}/tests/data/{name}", env!("CARGO_MANIFEST_DIR"))).expect("sample");
    let (format, aggregator) = culprit_querylog::read(text.lines(), Engine::Sqlite).expect("a format Culprit reads");
    (format, aggregator.workload(format, 50))
}

#[test]
fn a_rails_log_adds_up_the_listing_query() {
    let (format, w) = read("rails.log");
    assert_eq!(format, "Rails");
    // The same listing for two categories is one shape; another filter combination is another; the cache hit isn't a run.
    let top = &w.queries[0];
    assert_eq!(top.executions, 2.0);
    assert!((top.total_time_ms - 24.3).abs() < 1e-9, "{}", top.total_time_ms);
    assert!(top.text.contains("\"products\".\"category_id\" = ?") && top.text.ends_with("LIMIT ?"), "{}", top.text);
    assert!(w.queries.iter().any(|q| q.text.contains("\"products\".\"color\" = ?") && q.executions == 1.0));
    assert_eq!(w.total_executions, 7.0);
    // Three requests a second apart: a two-second window, dated in UTC.
    assert_eq!(w.window_start.as_deref(), Some("2026-10-15T12:00:01Z"));
    assert!((w.window_hours * 3600.0 - 2.0).abs() < 1e-6, "{}", w.window_hours);
}

#[test]
fn a_django_log_puts_the_values_back_into_one_shape() {
    let (format, w) = read("django.log");
    assert_eq!(format, "Django");
    let listing = w.queries.iter().find(|q| q.text.contains("shop_product")).expect("the listing");
    assert_eq!(listing.executions, 2.0);
    assert!(listing.text.contains("\"category_id\" = ?"), "{}", listing.text);
    // One local timestamp, no zone: no window to claim, and nothing written as UTC.
    assert!(w.window_unknown());
    assert_eq!(w.window_start, None);
    assert_eq!(w.window_phrase(), "the period the Django log covers");
}

#[test]
fn json_lines() {
    let (format, w) = read("statements.jsonl");
    assert_eq!(format, "JSON lines");
    assert_eq!(w.queries[0].executions, 2.0);
    assert!((w.queries[0].total_time_ms - 17.6).abs() < 1e-9);
}

#[test]
fn a_log_with_no_statement_is_no_format() {
    let lines = ["GET /health 200 1ms", "worker started"];
    assert!(culprit_querylog::read(lines.iter().copied(), Engine::Sqlite).is_none());
}

#[test]
fn another_database_s_statements_leave_the_totals_too() {
    let text = std::fs::read_to_string(format!("{}/tests/data/rails.log", env!("CARGO_MANIFEST_DIR"))).expect("sample");
    let (_, mut aggregator) = culprit_querylog::read(text.lines(), Engine::Sqlite).expect("Rails");
    let before = aggregator.entries();
    let left_out = aggregator.retain(|text| !text.contains("\"color\""));
    assert_eq!(left_out, 1);
    assert_eq!(aggregator.entries(), before - 1);
    let w = aggregator.workload("Rails", 50);
    assert!(w.queries.iter().all(|q| !q.text.contains("\"color\"")));
    assert_eq!(w.total_executions, (before - 1) as f64);
}
