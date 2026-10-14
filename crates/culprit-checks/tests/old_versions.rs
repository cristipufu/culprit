//! Older servers get advice they can run: MySQL 5.7 has no invisible indexes (8.0) and MariaDB before 10.6 can't
//! ignore one, so both get the definition-saving drop instead.

use culprit_checks::analyze;
use culprit_core::{Report, Snapshot};

fn snapshot(name: &str) -> Snapshot {
    let path = format!("{}/../../fixtures/{name}.report.json", env!("CARGO_MANIFEST_DIR"));
    let report: Report = serde_json::from_str(&std::fs::read_to_string(path).expect("fixture")).expect("report");
    counted_for_ten_days(report.snapshot)
}

/// The demo's counters are minutes old; an index is only called unused after a week of them.
fn counted_for_ten_days(mut s: Snapshot) -> Snapshot {
    s.server.started_at = Some("2026-10-01T12:00:00Z".into());
    if let Some(p) = s.pressure.as_mut() {
        p.uptime_hours = Some(240.0);
    }
    s
}

fn unused_index_fix(s: Snapshot) -> (String, String) {
    let report = analyze(s);
    let f = report.findings.iter().find(|f| f.id.starts_with("unused-index:")).expect("unused index finding");
    let fix = f.fix.as_ref().expect("fix");
    (fix.summary.clone(), fix.sql.clone().unwrap_or_default())
}

#[test]
fn mysql_5_7_drops_instead_of_hiding() {
    let mut s = snapshot("mysql-demo");
    s.server.version = "5.7.44".into();
    s.server.product = "MySQL 5.7.44".into();
    let (summary, sql) = unused_index_fix(s);
    assert!(sql.contains("DROP INDEX") && !sql.contains("INVISIBLE"), "{sql}");
    assert!(summary.contains("MySQL 5.7 can't hide an index"), "{summary}");

    let (summary, sql) = unused_index_fix(snapshot("mysql-demo"));
    assert!(sql.contains("INVISIBLE"), "MySQL 8 hides first: {sql}");
    assert!(summary.starts_with("Make it invisible first"), "{summary}");
}

#[test]
fn mariadb_before_10_6_drops_instead_of_ignoring() {
    let mut s = snapshot("mariadb-demo");
    s.server.version = "10.5.29-MariaDB-ubu2004".into();
    s.server.product = "MariaDB 10.5.29".into();
    let (summary, sql) = unused_index_fix(s);
    assert!(sql.contains("DROP INDEX") && !sql.contains("IGNORED"), "{sql}");
    assert!(summary.contains("ignored indexes arrived in 10.6"), "{summary}");
}
