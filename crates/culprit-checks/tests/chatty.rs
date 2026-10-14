//! Frequent tiny statements: a poll that finds nothing, a write repeated row by row, a read inside a loop. MySQL counts
//! the rows a write changed apart from rows returned, and returns none for a write, so a busy INSERT must not read as a
//! poll.

use culprit_checks::analyze;
use culprit_core::Snapshot;

fn snapshot() -> Snapshot {
    let text = std::fs::read_to_string(format!("{}/../../fixtures/mysql-demo.json", env!("CARGO_MANIFEST_DIR")))
        .expect("fixture");
    let mut s: Snapshot = serde_json::from_str(&text).expect("snapshot");
    // Two hours of statement history: enough for a rate.
    let workload = s.workload.as_mut().expect("workload");
    workload.window_hours = 2.0;
    s
}

/// The demo's first statement, made a single-row INSERT run 30K times in the window.
fn with_insert(rows_sent: f64, rows_affected: Option<f64>) -> Snapshot {
    let mut s = snapshot();
    let workload = s.workload.as_mut().expect("workload");
    let q = &mut workload.queries[0];
    q.text = "INSERT INTO `shop`.`events` (`kind`, `at`) VALUES (?, NOW())".into();
    q.executions = 30_000.0;
    q.total_time_ms = 30_000.0 * 0.4;
    q.mean_time_ms = 0.4;
    q.rows = Some(rows_sent);
    q.rows_affected = rows_affected;
    workload.total_executions = workload.queries.iter().map(|q| q.executions).sum();
    s
}

fn chatty_title(s: Snapshot) -> Option<String> {
    analyze(s).findings.into_iter().find(|f| f.check == "chatty-query").map(|f| f.title)
}

#[test]
fn a_busy_insert_is_a_row_by_row_write_not_a_poll() {
    let title = chatty_title(with_insert(0.0, Some(30_000.0))).expect("a chatty finding");
    assert!(title.contains("one row each time"), "{title}");
    assert!(!title.contains("find nothing"), "{title}");
}

#[test]
fn without_rows_affected_a_write_is_judged_by_what_the_engine_counted() {
    // Engines that count a write's rows as `rows` (SQL Server, PostgreSQL) leave rows_affected empty.
    let title = chatty_title(with_insert(30_000.0, None)).expect("a chatty finding");
    assert!(title.contains("one row each time"), "{title}");
}
