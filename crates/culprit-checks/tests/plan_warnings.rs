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
