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

#[test]
fn orphaned_invalid_postgres_index_is_reported_and_redundant_invalid_index_is_duplicate() {
    let mut s: Snapshot = serde_json::from_str(include_str!("../../../../../fixtures/postgres-demo.json")).unwrap();
    s.pressure.get_or_insert_with(Default::default).uptime_hours = Some(10.0 * 24.0);
    for i in &mut s.indexes {
        i.counters_since = None;
    }
    let mut invalid = s.indexes.iter().find(|i| i.table == "orders" && !i.is_primary_key).unwrap().clone();
    invalid.name = "orders_abandoned_ccnew".into();
    invalid.columns = vec!["shipping_cents".into()];
    invalid.is_disabled = true;
    invalid.maintained = Some(true);
    invalid.size_bytes = Some(8.0 * 1024.0 * 1024.0);
    s.indexes.push(invalid);
    let out = unused_index(&s);
    let f = out
        .iter()
        .find(|f| f.id == "unused-index:invalid:public.orders.orders_abandoned_ccnew")
        .expect("orphaned invalid index");
    assert!(f.summary.contains("indisvalid = false"), "{}", f.summary);
    assert!(f.fix.as_ref().unwrap().sql.as_deref().unwrap().contains("DROP INDEX CONCURRENTLY"));

    // When a valid index already covers the same keys, duplicate-index flags the leftover invalid index immediately.
    let valid_col =
        s.indexes.iter().find(|i| i.table == "orders" && !i.is_primary_key && !i.is_disabled).unwrap().columns.clone();
    s.indexes.last_mut().unwrap().columns = valid_col;
    let dup = duplicate_index(&s);
    assert!(
        dup.iter().any(|f| f.id == "duplicate-index:public.orders.orders_abandoned_ccnew"),
        "{:?}",
        dup.iter().map(|f| &f.id).collect::<Vec<_>>()
    );
}

#[test]
fn foreign_key_column_type_mismatch_is_flagged_without_false_positives_on_aliases_or_lengths() {
    let mut s: Snapshot = serde_json::from_str(include_str!("../../../../../fixtures/postgres-demo.json")).unwrap();
    // Standard demo fixture has matching types (and aliases like int4 vs integer or varchar(50) vs text must not fire).
    assert!(!unindexed_fk(&s).iter().any(|f| f.id.contains("type-mismatch")));

    let fk = s.foreign_keys[0].clone();
    let child_col =
        s.columns.iter_mut().find(|c| c.schema == fk.schema && c.table == fk.table && c.name == fk.columns[0]).unwrap();
    child_col.data_type = "integer".into();
    let parent_col = s
        .columns
        .iter_mut()
        .find(|c| c.schema == fk.ref_schema && c.table == fk.ref_table && c.name == fk.ref_columns[0])
        .unwrap();
    parent_col.data_type = "int4".into();
    assert!(!unindexed_fk(&s).iter().any(|f| f.id.contains("type-mismatch")), "int4 and integer are the same type");

    let parent_col = s
        .columns
        .iter_mut()
        .find(|c| c.schema == fk.ref_schema && c.table == fk.ref_table && c.name == fk.ref_columns[0])
        .unwrap();
    parent_col.data_type = "bigint".into();
    let findings = unindexed_fk(&s);
    let mismatch = findings.iter().find(|f| f.id.starts_with("unindexed-fk:type-mismatch:")).expect("int vs bigint");
    assert!(mismatch.title.contains("(integer)") && mismatch.title.contains("(bigint)"), "{}", mismatch.title);

    // ON DELETE CASCADE on an unindexed FK elevates severity and calls out cascade amplification.
    s.foreign_keys[0].delete_rule = Some("CASCADE".into());
    let findings = unindexed_fk(&s);
    let cascade_fk = findings
        .iter()
        .find(|f| f.id.starts_with("unindexed-fk:") && !f.id.contains("type-mismatch:") && !f.id.contains("uuid-text:"))
        .expect("unindexed fk finding");
    assert!(cascade_fk.summary.contains("ON DELETE CASCADE"), "{}", cascade_fk.summary);

    // UUID stored as varchar(36) on an indexed column of a large table is flagged.
    let tbl_schema = s.tables[0].schema.clone();
    let tbl_name = s.tables[0].name.clone();
    s.tables[0].rows = Some(250_000.0);
    s.columns.push(culprit_core::ColumnInfo {
        schema: tbl_schema.clone(),
        table: tbl_name.clone(),
        name: "external_uuid".into(),
        data_type: "varchar(36)".into(),
        nullable: false,
        default: None,
        generated: false,
        ordinal: 88,
    });
    let mut ix = s.indexes[0].clone();
    ix.schema = tbl_schema.clone();
    ix.table = tbl_name.clone();
    ix.name = "idx_external_uuid".into();
    ix.columns = vec!["external_uuid".into()];
    ix.is_disabled = false;
    s.indexes.push(ix);
    let findings = unindexed_fk(&s);
    let uuid_f = findings
        .iter()
        .find(|f| f.id == format!("unindexed-fk:uuid-text:{tbl_schema}.{tbl_name}(external_uuid)"))
        .expect("uuid text finding");
    assert!(uuid_f.title.contains("stores UUIDs as varchar(36)"), "{}", uuid_f.title);

    // Collation/charset mismatch on string foreign keys is also caught by fk_type_mismatches.
    let child_col =
        s.columns.iter_mut().find(|c| c.schema == fk.schema && c.table == fk.table && c.name == fk.columns[0]).unwrap();
    child_col.data_type = "varchar(36) COLLATE utf8mb4_unicode_ci".into();
    let parent_col = s
        .columns
        .iter_mut()
        .find(|c| c.schema == fk.ref_schema && c.table == fk.ref_table && c.name == fk.ref_columns[0])
        .unwrap();
    parent_col.data_type = "varchar(36)".into();
    let findings = unindexed_fk(&s);
    let coll_mismatch =
        findings.iter().find(|f| f.id.starts_with("unindexed-fk:type-mismatch:")).expect("collation mismatch");
    assert!(coll_mismatch.title.contains("COLLATE utf8mb4_unicode_ci"), "{}", coll_mismatch.title);

    // A broad Index Seek with a selective residual filter on an unindexed second column produces a filtered-scan finding.
    let mut seek_snap: Snapshot =
        serde_json::from_str(include_str!("../../../../../fixtures/postgres-demo.json")).unwrap();
    let target_schema = seek_snap.tables[0].schema.clone();
    let target_table = seek_snap.tables[0].name.clone();
    let mut tenant_ix = seek_snap.indexes[0].clone();
    tenant_ix.schema = target_schema.clone();
    tenant_ix.table = target_table.clone();
    tenant_ix.name = "ix_tenant".into();
    tenant_ix.columns = vec!["tenant_id".into()];
    tenant_ix.is_disabled = false;
    seek_snap.indexes.retain(|i| !(i.schema == target_schema && i.table == target_table));
    seek_snap.indexes.push(tenant_ix);
    if let Some(w) = seek_snap.workload.as_mut()
        && let Some(q) = w.queries.iter_mut().find(|q| q.plan.is_some())
    {
        q.total_time_ms = w.total_time_ms * 0.5;
        q.mean_time_ms = 250.0;
        q.executions = 500.0;
        if let Some(plan) = q.plan.as_mut() {
            plan.root.kind = NodeKind::Seek;
            plan.root.op = "Index Scan".into();
            plan.root.object = Some(format!("{target_schema}.{target_table}"));
            plan.root.index = Some("ix_tenant".into());
            plan.root.seek_predicate = Some("(tenant_id = $1)".into());
            plan.root.predicate = Some("(status = 'open')".into());
            plan.root.rows_read = Some(50_000.0);
            plan.root.est_rows = 50.0;
            plan.root.act_rows = Some(50.0);
            plan.root.est_executions = 1.0;
            plan.root.act_executions = Some(1.0);
            plan.root.children.clear();
        }
    }
    let scan_findings = filtered_scan(&seek_snap);
    assert!(
        scan_findings.iter().any(|f| f.id == format!("filtered-scan:{target_schema}.{target_table}(tenant_id,status)")),
        "{scan_findings:#?}"
    );
}
