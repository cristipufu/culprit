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
    let mut s = snapshot();
    assert_eq!(s.server.name(), "MariaDB");
    // The demo's counters are an hour old; an index is only called unused after a week of them.
    s.server.started_at = Some("2026-10-01T12:00:00Z".into());
    if let Some(p) = s.pressure.as_mut() {
        p.uptime_hours = Some(240.0);
    }
    // MariaDB estimates that a filter on an unindexed column keeps every row. The demo's statement ends in `LIMIT ?`,
    // so the rows it returns say nothing about how many the filter kept: no evidence, no index.
    let scan = "filtered-scan:shop.orders(promo_code)";
    assert!(!analyze(s.clone()).findings.iter().any(|f| f.id == scan));
    // The same statement returning every row it keeps: rows examined against rows returned say the filter keeps few,
    // so the scan is found.
    let q = s.workload.as_mut().unwrap().queries.iter_mut().find(|q| q.text.contains("promo_code")).unwrap();
    q.text = q.text.split(" ORDER BY").next().unwrap().to_string();
    let report = analyze(s);
    assert!(report.findings.iter().any(|f| f.id == scan));
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
    // One statement per setting, worded by setting_change (a managed MariaDB gets its parameter group instead).
    assert!(
        sql.contains(
            "SET GLOBAL userstat = 'ON';\nSET GLOBAL slow_query_log = 'ON';\nSET GLOBAL log_output = 'FILE,TABLE';\nSET GLOBAL long_query_time = 0.1;"
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
