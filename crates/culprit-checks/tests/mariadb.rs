//! MariaDB rides on the MySQL engine: the same checks, in MariaDB's words and syntax, on the MariaDB demo scan.

use culprit_checks::{analyze, index_advice};
use culprit_core::{Capability, Report, Setting, Severity, Snapshot};

fn snapshot() -> Snapshot {
    let path = format!("{}/../../fixtures/mariadb-demo.report.json", env!("CARGO_MANIFEST_DIR"));
    let report: Report = serde_json::from_str(&std::fs::read_to_string(path).expect("fixture")).expect("report");
    report.snapshot
}

fn set(s: &mut Snapshot, name: &str, value: &str) {
    s.settings.retain(|x| x.name != name);
    s.settings.push(Setting { name: name.into(), value: value.into(), default_value: None, scope: "server".into() });
}

#[test]
fn findings_use_mariadb_words_and_syntax() {
    let s = snapshot();
    assert_eq!(s.server.name(), "MariaDB");
    let report = analyze(s);
    // MariaDB estimates that a filter on an unindexed column keeps every row; rows examined against rows returned say
    // otherwise, so the scan is still found.
    assert!(report.findings.iter().any(|f| f.id == "filtered-scan:shop.orders(promo_code)"));
    let unused = report.findings.iter().find(|f| f.id.starts_with("unused-index:")).expect("unused index");
    let sql = unused.fix.as_ref().and_then(|f| f.sql.as_deref()).unwrap_or_default();
    assert!(sql.contains("IGNORED") && !sql.contains("INVISIBLE"), "{sql}");
    let advice = index_advice(&report);
    assert!(advice.notes.iter().any(|n| n.starts_with("MariaDB doesn't record missing-index requests")));
}

#[test]
fn without_performance_schema_the_slow_log_is_offered_first() {
    let mut s = snapshot();
    set(&mut s, "performance_schema", "OFF");
    set(&mut s, "slow_query_log", "OFF");
    set(&mut s, "log_output", "FILE");
    set(&mut s, "userstat", "OFF");
    set(&mut s, "long_query_time", "10.000000");
    s.server.capabilities.retain(|c| *c != Capability::PerformanceSchema);
    s.workload = None;
    let report = analyze(s);
    let f = report.findings.iter().find(|f| f.id == "config:performance_schema").expect("finding");
    assert_eq!(f.severity, Severity::High);
    assert!(f.summary.starts_with("Without it MariaDB keeps"), "{}", f.summary);
    let sql = f.fix.as_ref().and_then(|x| x.sql.as_deref()).unwrap_or_default();
    assert!(
        sql.contains(
            "SET GLOBAL userstat = ON, GLOBAL slow_query_log = ON, GLOBAL log_output = 'FILE,TABLE', GLOBAL long_query_time = 0.1;"
        ),
        "{sql}"
    );
    assert!(
        !report.findings.iter().any(|f| f.id == "config:slow_query_log"),
        "the performance_schema finding covers it"
    );

    // Once the slow log feeds the workload, performance_schema is a gap, not a blind spot.
    let mut logged = snapshot();
    set(&mut logged, "performance_schema", "OFF");
    logged.server.capabilities.retain(|c| *c != Capability::PerformanceSchema);
    if let Some(w) = logged.workload.as_mut() {
        w.source = "slow query log".into();
        w.windowed = true;
    }
    let report = analyze(logged);
    let f = report.findings.iter().find(|f| f.id == "config:performance_schema").expect("finding");
    assert_eq!(f.severity, Severity::Medium);
    assert!(f.summary.contains("Culprit reads the slow query log"), "{}", f.summary);
}
