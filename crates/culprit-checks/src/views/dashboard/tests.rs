#[test]
fn trend_spans_add_up_to_the_window() {
    let t = Trend { recent_hours: 10.25 / 4.0, baseline_hours: 10.25 * 3.0 / 4.0, ratio: 2.0, recent_ms: 0.0 };
    assert_eq!(trend_spans(&t), ("the last 3 hours".to_string(), "7 hours".to_string()));
    let day = Trend { recent_hours: 24.0, baseline_hours: 48.0, ratio: 2.0, recent_ms: 0.0 };
    assert_eq!(trend_spans(&day), ("the last day".to_string(), "2 days".to_string()));
}

use super::*;

#[test]
fn a_bucket_holding_a_lower_bound_is_left_out_of_the_comparison() {
    // Eight hours at a steady second an hour, except a bucket that holds only a lower bound (a running total fell
    // without a reset). Counted as 50 ms it would make the earlier hours look quieter and today look busier.
    let mut w = crate::waits::snapshot_with(culprit_core::Engine::Postgres).workload.expect("workload");
    let at = |h: usize| format!("2026-10-12T{h:02}:00:00Z");
    w.window_start = Some(at(0));
    let mut time_ms = vec![1_000.0; 8];
    time_ms[2] = 50.0;
    w.timeline = Some(culprit_core::Timeline {
        source: Default::default(),
        starts: (0..8).map(at).collect(),
        end: at(8),
        time_ms,
        executions: vec![0.0; 8],
        unknown: Some((0..8).map(|i| i == 2).collect()),
    });
    let t = trend(&w).expect("a trend");
    assert!((t.ratio - 1.0).abs() < 1e-9, "{}", t.ratio);
}

#[test]
fn fix_statements_come_out_of_sentences() {
    assert_eq!(
        fix_statement("Run CREATE EXTENSION pg_stat_statements; in this database. No restart is needed.").as_deref(),
        Some("CREATE EXTENSION pg_stat_statements;")
    );
    assert_eq!(
        fix_statement("ALTER DATABASE CURRENT SET QUERY_STORE = ON;").as_deref(),
        Some("ALTER DATABASE CURRENT SET QUERY_STORE = ON;")
    );
    assert_eq!(fix_statement("Ask an administrator."), None);
    assert_eq!(fix_statement("Set performance_schema = ON in the server configuration and restart."), None);
}

#[test]
fn reasons_read_as_sentence_ends() {
    assert_eq!(sentence("Query Store is OFF on this database."), "Query Store is OFF on this database.");
    assert_eq!(sentence("The login can't read it"), "the login can't read it.");
    assert_eq!(sentence("SQL Server 2016 or newer"), "SQL Server 2016 or newer.");
}

#[test]
fn the_first_of_equally_severe_findings_leads() {
    use culprit_core::{Category, SubjectKind};
    let mut report = crate::analyze(crate::waits::snapshot_with(Engine::Mssql));
    let make = |key: &str| {
        let mut f = crate::support::finding(
            "failing-query",
            key,
            Category::Queries,
            crate::support::subject(SubjectKind::Query, key),
        );
        f.severity = Severity::High;
        f.title = format!("{key} fails");
        f
    };
    report.findings.retain(|f| f.check != "failing-query");
    report.findings.extend([make("first"), make("second")]);
    assert_eq!(worst_finding(&report, "failing-query").map(|f| f.title.as_str()), Some("first fails"));
}

#[test]
fn labels_keep_acronyms() {
    assert_eq!(lower_label("CPU"), "CPU");
    assert_eq!(lower_label("Disk reads"), "disk reads");
}
