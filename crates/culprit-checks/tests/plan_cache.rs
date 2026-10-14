//! The single-use plan finding's fix is something the reader can run where the server lives.

use culprit_checks::analyze;
use culprit_core::{PlanCacheUse, Snapshot};

fn snapshot(cloud: Option<&str>) -> Snapshot {
    let text = std::fs::read_to_string(format!("{}/../../fixtures/mssql-demo.json", env!("CARGO_MANIFEST_DIR")))
        .expect("fixture");
    let mut s: Snapshot = serde_json::from_str(&text).expect("snapshot");
    s.server.cloud = cloud.map(str::to_string);
    s.settings.retain(|x| x.name != "optimize for ad hoc workloads");
    let gb = 1024.0 * 1024.0 * 1024.0;
    s.plan_cache = Some(PlanCacheUse {
        single_use_plans: 40_000.0,
        single_use_bytes: 2.0 * gb,
        total_plans: 50_000.0,
        total_bytes: 3.0 * gb,
    });
    s
}

fn fix(cloud: Option<&str>) -> (String, Option<String>) {
    let report = analyze(snapshot(cloud));
    let f = report.findings.iter().find(|f| f.id == "query-family:plan-cache").expect("the plan-cache finding");
    let fix = f.fix.clone().expect("a fix");
    (fix.summary, fix.sql)
}

#[test]
fn the_ad_hoc_setting_is_changed_where_the_server_keeps_it() {
    let (_, sql) = fix(None);
    assert!(sql.as_deref().is_some_and(|s| s.contains("sp_configure 'optimize for ad hoc workloads'")), "{sql:?}");
    // Azure SQL Database keeps it per database.
    let (_, sql) = fix(Some("Azure SQL Database"));
    assert_eq!(sql.as_deref(), Some("ALTER DATABASE SCOPED CONFIGURATION SET OPTIMIZE_FOR_AD_HOC_WORKLOADS = ON;"));
    // RDS refuses sp_configure: its parameter group, and no SQL.
    let (summary, sql) = fix(Some("Amazon RDS"));
    assert_eq!(sql, None);
    assert!(summary.contains("DB parameter group"), "{summary}");
}
