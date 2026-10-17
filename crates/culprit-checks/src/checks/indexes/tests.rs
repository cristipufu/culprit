use super::*;

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
