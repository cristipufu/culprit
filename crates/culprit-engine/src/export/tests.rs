use super::*;

#[test]
fn file_names_are_short_safe_and_lowercase() {
    assert_eq!(
        file_stem("A type mismatch on customers.email stops a read"),
        "a-type-mismatch-on-customers-email-stops-a-read"
    );
    assert_eq!(file_stem("../../etc/passwd"), "etc-passwd");
    assert_eq!(file_stem("   "), "culprit");
    assert!(file_stem(&"x".repeat(500)).len() <= 61);
}

#[test]
fn native_exports_keep_the_engine_format() {
    let mut plan: Plan = serde_json::from_value(serde_json::json!({
        "engine": "postgres", "kind": "estimated", "statement": null, "total_cost": 1.0, "warnings": [],
        "index_hints": [], "props": {}, "raw": "[{\"Plan\": {}}]",
        "root": {"id": 0, "op": "Seq Scan", "kind": "scan", "detail": null, "object": "orders", "index": null,
            "seek_predicate": null, "predicate": "(status = 'open'::text)", "est_rows": 1.0, "est_executions": 1.0,
            "act_rows": null, "act_executions": null, "rows_read": null, "table_rows": null, "subtree_cost": 1.0,
            "self_cost": 1.0, "act_time_ms": null, "warnings": [], "props": {}, "children": []}
    }))
    .unwrap();
    assert_eq!(native_extension(&plan), "json");
    plan.raw = Some("Seq Scan on orders  (cost=0.00..1.00 rows=1 width=4)".into());
    assert_eq!(native_extension(&plan), "txt");
    plan.raw = Some("<ShowPlanXML/>".into());
    assert_eq!(native_extension(&plan), "sqlplan");
    plan.raw = None;
    assert!(plan_native(&plan, &ExportOptions::default()).is_none());
}

#[test]
fn redaction_is_on_unless_asked() {
    assert!(ExportOptions::default().redact);
    let parsed: ExportOptions = serde_json::from_str("{}").unwrap();
    assert!(parsed.redact && parsed.notes.is_empty());
}
