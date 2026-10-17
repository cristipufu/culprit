//! Which of SQL Server's conversion warnings are a type mismatch worth a finding.

use culprit_core::Snapshot;

fn snapshot() -> Snapshot {
    serde_json::from_str(include_str!("../../../fixtures/mssql-demo.json")).expect("fixture is a snapshot")
}

fn conversion_findings(snapshot: Snapshot) -> Vec<String> {
    culprit_checks::analyze(snapshot)
        .findings
        .into_iter()
        .map(|f| f.id)
        .filter(|id| id.starts_with("plan-warnings:") && id.contains("implicit-conversion"))
        .collect()
}

/// Query 7 of the demo compares `customers.email` (varchar) with an nvarchar parameter.
fn with_converts(converts: &str) -> Snapshot {
    let mut s = snapshot();
    let workload = s.workload.as_mut().expect("the demo has a workload");
    let plan = workload.queries.iter_mut().find(|q| q.id == "7").and_then(|q| q.plan.as_mut()).expect("query 7's plan");
    plan.props.insert("PlanAffectingConvert".into(), converts.into());
    s
}

#[test]
fn a_column_converted_to_match_a_parameter_is_a_mismatch() {
    assert_eq!(conversion_findings(snapshot()), vec!["plan-warnings:implicit-conversion:customers.email"]);
}

#[test]
fn a_plan_captured_before_the_column_changed_type_is_not_a_mismatch() {
    // The demo's customers.email is varchar(200) and query 7's plan converts it. Once the column is nvarchar(200), the
    // plan Query Store kept describes a table that's gone.
    let mut s = snapshot();
    let email = s.columns.iter_mut().find(|c| c.table == "customers" && c.name == "email").expect("email");
    email.data_type = "nvarchar(200)".into();
    assert_eq!(conversion_findings(s), Vec::<String>::new());
}

#[test]
fn conversions_the_query_asks_for_are_not_a_mismatch() {
    // `SELECT CONVERT(varchar(36), c.id), CONVERT(varchar(30), c.created_at, 126) …`: SQL Server warns about every
    // CONVERT in the select list, though nothing is compared with a value of another type.
    let explicit = "Cardinality Estimate: CONVERT(varchar(36),[c].[id],0)\n\
                    Cardinality Estimate: CONVERT(varchar(30),[c].[created_at],126)";
    assert_eq!(conversion_findings(with_converts(explicit)), Vec::<String>::new());
    // Converting the parameter instead of the column costs nothing either.
    assert_eq!(
        conversion_findings(with_converts("Cardinality Estimate: CONVERT_IMPLICIT(varchar(64),[@P1],0)")),
        Vec::<String>::new()
    );
}

#[test]
fn a_mismatch_sql_server_says_stops_a_seek_is_titled_as_a_seek_not_any_use() {
    // Query 7's plan still reads ix_customers_email, by scanning it: the conversion stops a seek, not every use.
    let report = culprit_checks::analyze(snapshot());
    let f = report
        .findings
        .iter()
        .find(|f| f.id == "plan-warnings:implicit-conversion:customers.email")
        .expect("the mismatch");
    assert!(f.title.ends_with("from seeking an index"), "{}", f.title);
    assert!(!f.title.contains("using an index"), "{}", f.title);
}

// SQL Server warned of no statistics on a column the plan seeks an index on: the index's statistics exist (they come
// with the index), so the finding says they're out of date, and what the catalog holds of them, not that there are none.
#[test]
fn statistics_an_index_has_are_said_out_of_date_with_what_the_catalog_holds() {
    fn seek_on_priority(n: &mut culprit_core::PlanNode) -> Option<&mut culprit_core::PlanNode> {
        if n.index.as_deref() == Some("ix_support_tickets_priority") {
            return Some(n);
        }
        n.children.iter_mut().find_map(seek_on_priority)
    }
    let mut s = snapshot();
    let workload = s.workload.as_mut().expect("the demo has a workload");
    let plan =
        workload.queries.iter_mut().find(|q| q.id == "10").and_then(|q| q.plan.as_mut()).expect("query 10's plan");
    let seek = seek_on_priority(&mut plan.root).expect("the seek on priority");
    seek.warnings.push(culprit_core::PlanWarning::MissingStatistics);
    seek.props.insert("ColumnsWithNoStatistics".into(), "priority".into());
    let stats = s.statistics.iter_mut().find(|st| st.name == "ix_support_tickets_priority").expect("its statistics");
    stats.last_updated = Some("2026-08-28T08:58:07Z".into());
    stats.rows = Some(1.0);
    stats.modifications = Some(95.0);

    let report = culprit_checks::analyze(s);
    let f = report.findings.iter().find(|f| f.id == "plan-warnings:10:missing-statistics").expect("the warning");
    assert!(f.title.ends_with("is planned on out-of-date statistics on priority"), "{}", f.title);
    assert!(
        f.summary.contains(
            "found nothing in them it could use. The statistics of ix_support_tickets_priority were last updated on Aug 28, when the table had 1 row, and have counted 95 changes since; the table held 300K rows when this plan was compiled, and holds 300K now."
        ),
        "{}",
        f.summary
    );
    assert!(!f.summary.contains("read like this"), "{}", f.summary);
    assert_eq!(f.fix.as_ref().and_then(|x| x.sql.as_deref()), Some("UPDATE STATISTICS [shop].[support_tickets];"));
}
