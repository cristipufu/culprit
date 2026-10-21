use super::*;

#[test]
fn hiding_filter_literals_preserves_equality_and_statement_coverage() {
    let mut s: Snapshot = serde_json::from_str(include_str!("../../../../../fixtures/mssql-demo.json")).unwrap();
    let mut template = s.indexes[0].clone();
    template.schema = "shop".into();
    template.table = "orders".into();
    template.columns = vec!["tenant".into()];
    template.is_primary_key = false;
    template.is_unique = false;
    template.is_clustered = false;
    template.is_disabled = false;
    s.indexes.clear();
    let filters = ["tenant = 'private-alpha'", "tenant = 'private-beta'", "tenant = 'private-alpha'", "is_deleted = 0"];
    for (i, filter) in filters.iter().enumerate() {
        let mut index = template.clone();
        index.name = format!("ix_{i}");
        index.filter = Some((*filter).into());
        s.indexes.push(index);
    }
    let w = s.workload.as_mut().unwrap();
    let query = w.queries[0].clone();
    w.queries = ["tenant = 'private-alpha'", "tenant = 'private-beta'", "is_deleted = 0", "is_deleted = @p"]
        .into_iter()
        .enumerate()
        .map(|(i, condition)| {
            let mut q = query.clone();
            q.id = i.to_string();
            q.text = format!("SELECT id FROM shop.orders WHERE {condition}");
            q
        })
        .collect();
    let shared = culprit_core::redact::redact_snapshot(&s);
    assert_eq!(
        shared.indexes[0].filter, shared.indexes[1].filter,
        "textual placeholders alone cannot prove equivalence"
    );
    assert!(!shared.indexes[0].same_filter(&shared.indexes[1]));
    assert!(shared.indexes[0].same_filter(&shared.indexes[2]));
    for (original, hidden) in s.indexes.iter().zip(&shared.indexes) {
        for (query, hidden_query) in s.queries().iter().zip(shared.queries()) {
            assert_eq!(fits_index_filter(original, &[query]), fits_index_filter(hidden, &[hidden_query]));
        }
    }
    assert_eq!(redundant_indexes(&s).len(), redundant_indexes(&shared).len());
    let output = serde_json::to_string(&shared).unwrap();
    assert!(!output.contains("private-alpha") && !output.contains("private-beta"));
}

#[test]
fn filters_made_of_literal_equalities() {
    let pairs =
        |f: &str| filter_equalities(f).map(|v| v.into_iter().map(|(c, l)| format!("{c}={l}")).collect::<Vec<_>>());
    assert_eq!(pairs("([is_deleted]=(0))"), Some(vec!["is_deleted=0".to_string()]));
    assert_eq!(
        pairs("([status]='open' AND [kind]=(2))"),
        Some(vec!["status='open'".to_string(), "kind=2".to_string()])
    );
    assert_eq!(pairs("([customer_id] IS NOT NULL)"), None);
    assert_eq!(pairs("([is_deleted]=(0) AND [due_at]>=getdate())"), None);
    assert_eq!(pairs("([status]<>'closed')"), None);
}

#[test]
fn a_parameter_compared_on_either_side() {
    assert!(compares_to_parameter(
        "select id from [t] where (([a] = @p0) and ([is_deleted] = @par1))",
        "t",
        "is_deleted"
    ));
    assert!(compares_to_parameter("SELECT id FROM t WHERE @deleted = t.is_deleted", "t", "is_deleted"));
    assert!(!compares_to_parameter("SELECT id FROM t WHERE is_deleted = 0 AND a = @p", "t", "is_deleted"));
    assert!(!compares_to_parameter("SELECT id FROM t WHERE is_deleted >= @p", "t", "is_deleted"));
    assert!(!compares_to_parameter("SELECT id FROM t WHERE was_is_deleted = @p", "t", "is_deleted"));
    assert!(!compares_to_parameter("SELECT id FROM t WHERE @p <= is_deleted", "t", "is_deleted"));
}

#[test]
fn a_literal_compared_on_either_side() {
    let fits = |text: &str, literal: &str| compared_to(text, "t", "status", |v| same_literal(v, literal));
    assert!(fits("SELECT id FROM t WHERE [t].[status] = N'open' AND a = @p", "'open'"));
    assert!(fits("SELECT id FROM t WHERE 'open' = status", "N'open'"));
    assert!(fits("SELECT id FROM t WHERE (status = (2))", "2"));
    assert!(fits("SELECT id FROM t WHERE status=2.0", "(2)"));
    assert!(!fits("SELECT id FROM t WHERE status = 'closed'", "'open'"));
    assert!(!fits("SELECT id FROM t WHERE status = @p", "'open'"));
    assert!(!fits("SELECT id FROM t WHERE status >= 2", "2"));
    assert!(!fits("SELECT id FROM t WHERE 2 <= status", "2"));
    assert!(!fits("SELECT id FROM t WHERE a = 2", "2"));
}

// A staging scan's dispatch_intents: a join compares the other table's is_deleted with a value, not this one's. A column
// qualified by this table, or by the alias the statement gives it, is this table's.
#[test]
fn a_column_qualified_by_another_table_is_that_tables() {
    let deleted = |text: &str| compared_to(text, "dispatch_intents", "is_deleted", |v| same_literal(v, "0"));
    assert!(!deleted(
        "select [i].[id] from [app].[call_sessions] left join [app].[dispatch_intents] on [app].[dispatch_intents].[call_session_id] = [app].[call_sessions].[id] where [app].[call_sessions].[is_deleted] = 0"
    ));
    assert!(deleted("select id from [app].[dispatch_intents] where [app].[dispatch_intents].[is_deleted] = 0"));
    assert!(deleted(
        "select i.id from app.call_sessions s join app.dispatch_intents i on i.call_session_id = s.id where i.is_deleted = 0"
    ));
    assert!(!deleted(
        "select i.id from app.call_sessions s join app.dispatch_intents i on i.call_session_id = s.id where s.is_deleted = 0"
    ));
    assert!(deleted("select id from dispatch_intents where is_deleted = 0"));
}
