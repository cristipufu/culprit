use culprit_core::{Engine, Error, Plan, PlanKind, PlanNode, Result, signals::DocdbMetrics};
use serde_json::Value;

fn number(v: &Value, key: &str) -> Option<f64> {
    v.get(key)?.as_f64().filter(|n| n.is_finite() && *n >= 0.0)
}
fn sum(v: &Value, keys: &[&str]) -> Option<f64> {
    keys.iter().filter_map(|key| number(v, key)).reduce(|a, b| a + b).filter(|n| n.is_finite())
}
fn metrics(v: &Value, node: bool) -> Option<DocdbMetrics> {
    let m = if node {
        DocdbMetrics {
            read_requests: sum(v, &["Storage Table Read Requests", "Storage Index Read Requests"]),
            write_requests: sum(v, &["Storage Table Write Requests", "Storage Index Write Requests"]),
            read_operations: sum(v, &["Storage Table Read Ops", "Storage Index Read Ops"]),
            rows_scanned: sum(v, &["Storage Table Rows Scanned", "Storage Index Rows Scanned"]),
            read_ms: sum(v, &["Storage Table Read Execution Time", "Storage Index Read Execution Time"]),
            write_ms: sum(v, &["Storage Table Write Execution Time", "Storage Index Write Execution Time"]),
            ..Default::default()
        }
    } else {
        DocdbMetrics {
            read_requests: number(v, "Storage Read Requests"),
            write_requests: number(v, "Storage Write Requests"),
            read_operations: number(v, "Storage Read Ops"),
            rows_scanned: number(v, "Storage Rows Scanned"),
            read_ms: number(v, "Storage Read Execution Time"),
            catalog_ms: number(v, "Catalog Read Execution Time"),
            ..Default::default()
        }
    };
    (m != DocdbMetrics::default()).then_some(m)
}

fn enrich(node: &mut PlanNode, v: &Value) {
    node.facts_mut().estimated_rows_unavailable = number(v, "Plan Rows").is_none();
    node.facts_mut().docdb = metrics(v, true);
    // DIST node counters are per-loop averages. The normalized PlanNode row fields are totals across executions.
    if let Some(rows) = node.facts().docdb.as_ref().and_then(|m| m.rows_scanned) {
        node.rows_read = Some(rows * node.runs()).filter(|n| n.is_finite());
    }
    if let Some(filter) =
        ["Storage Filter", "Storage Index Filter"].iter().find_map(|k| v.get(k).and_then(Value::as_str))
    {
        node.predicate =
            Some(node.predicate.as_ref().map_or_else(|| filter.to_string(), |p| format!("({p}) AND ({filter})")));
        node.facts_mut().conditions.push(filter.into());
    }
    if let Some(guess) = culprit_postgres::plan_json::parameter_guess(node) {
        node.facts_mut().selectivity_guess = Some(guess.into());
        node.props.insert(culprit_core::GUESSED_SELECTIVITY.into(), guess.into());
    }
    // No heap row/page enrichment or PostgreSQL buffer measurements can describe DocDB reads.
    node.table_rows = None;
    let facts = node.facts_mut();
    facts.heap_fetches = None;
    facts.lossy_heap_blocks = None;
    facts.exact_heap_blocks = None;
    if let Some(runtime) = &mut node.runtime {
        runtime.logical_reads = None;
        runtime.physical_reads = None;
        runtime.spill_pages = None;
    }
    for (child, v) in node.children.iter_mut().zip(v.get("Plans").and_then(Value::as_array).into_iter().flatten()) {
        enrich(child, v);
    }
}

pub fn parse(raw: &str, kind: PlanKind, statement: Option<&str>) -> Result<Plan> {
    if raw.len() > culprit_core::limits::MAX_RESULT_BYTES {
        return Err(Error::invalid("The native YSQL plan exceeds the 8 MiB capture budget"));
    }
    let value: Value = serde_json::from_str(raw).map_err(|_| Error::invalid("YSQL returned malformed plan JSON"))?;
    let mut plan = from_value(&value, kind, statement)?;
    plan.raw = Some(raw.into());
    Ok(plan)
}

pub fn from_value(value: &Value, kind: PlanKind, statement: Option<&str>) -> Result<Plan> {
    let mut plan = culprit_postgres::plan_json::from_value(value, kind, statement)?;
    let top = value.as_array().and_then(|v| v.first()).unwrap_or(value);
    plan.engine = Engine::Yugabyte;
    plan.facts_mut().estimated_cost_unavailable = number(&top["Plan"], "Total Cost").is_none();
    plan.facts_mut().docdb = metrics(top, false);
    enrich(&mut plan.root, &top["Plan"]);
    if kind == PlanKind::Generic {
        culprit_postgres::plan_json::mark_generic(&mut plan);
    }
    Ok(plan)
}

pub(crate) fn native(plan: Plan) -> Result<Plan> {
    let raw = plan.raw.as_deref().ok_or_else(|| Error::invalid("YSQL returned no native plan text"))?;
    parse(raw, plan.kind, plan.statement.as_deref())
}

pub(crate) fn redacted_constants(plan: &mut Plan) {
    fn mark(node: &mut PlanNode) {
        if node.predicate.is_some() || node.seek_predicate.is_some() {
            let reason =
                "This retained YSQL plan redacts constants; the original values cannot establish filter selectivity.";
            node.facts_mut().selectivity_guess = Some(reason.into());
            node.props.insert(culprit_core::GUESSED_SELECTIVITY.into(), reason.into());
        }
        for child in &mut node.children {
            mark(child);
        }
    }
    plan.facts_mut().constants_redacted = true;
    mark(&mut plan.root);
}

pub fn reparse(saved: &Plan) -> Option<Plan> {
    let mut plan = parse(saved.raw.as_deref()?, saved.kind, saved.statement.as_deref()).ok()?;
    if saved.facts().constants_redacted {
        redacted_constants(&mut plan);
    }
    Some(plan)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retained_qpm_plans_keep_redaction_and_missing_estimates_when_reparsed() {
        let mut plan = parse(include_str!("../tests/data/qpm.json"), PlanKind::Estimated, None).unwrap();
        redacted_constants(&mut plan);
        assert!(plan.facts().constants_redacted && plan.facts().estimated_cost_unavailable);
        assert!(plan.nodes().iter().all(|n| n.facts().estimated_rows_unavailable));
        assert!(plan.root.facts().selectivity_guess.is_some());
        assert!(plan.root.kept_share().is_none());
        assert!(plan.root.act_rows.is_none() && plan.root.act_time_ms.is_none());
        let replay = reparse(&plan).unwrap();
        assert!(replay.facts().constants_redacted);
        assert!(replay.root.facts().selectivity_guess.is_some());
        assert_eq!(replay.kind, PlanKind::Estimated);
    }

    #[test]
    fn native_storage_filters_and_counters_survive_replay_without_heap_measurements() {
        let raw = include_str!("../tests/data/scan.json");
        let plan =
            parse(raw, PlanKind::Actual, Some("SELECT * FROM public.orders WHERE customer_id = 42 LIMIT 10")).unwrap();
        assert_eq!(plan.engine, Engine::Yugabyte);
        assert_eq!(plan.facts().docdb.as_ref().unwrap().rows_scanned, Some(19_492.0));
        let scan = &plan.root.children[0];
        assert_eq!(scan.rows_read_total(), Some(19_492.0));
        assert!(scan.predicate.as_deref().unwrap().contains("customer_id = 42"));
        assert!(scan.facts().conditions.iter().any(|s| s.contains("customer_id = 42")));
        assert!(plan.nodes().iter().all(|n| n.table_rows.is_none() && n.facts().heap_fetches.is_none()));
        let replay = reparse(&plan).unwrap();
        assert_eq!(replay.facts().docdb, plan.facts().docdb);
        assert_eq!(replay.root.children[0].rows_read_total(), Some(19_492.0));
    }

    #[test]
    fn repeated_docdb_reads_are_normalized_once_and_keep_native_per_loop_metrics() {
        let plan = parse(include_str!("../tests/data/loops.json"), PlanKind::Actual, None).unwrap();
        let lookup = &plan.root.children[1];
        assert_eq!(lookup.runs(), 20.0);
        assert_eq!(lookup.act_rows, Some(20.0));
        assert_eq!(lookup.rows_read_total(), Some(20.0));
        let facts = lookup.facts();
        let native = facts.docdb.as_ref().unwrap();
        assert_eq!(native.rows_scanned, Some(1.0));
        assert_eq!(native.read_requests, Some(1.0));
        assert_eq!(plan.facts().docdb.as_ref().unwrap().read_requests, Some(21.0));
    }

    #[test]
    fn invalid_native_values_are_unknown_and_estimates_never_invent_storage_counts() {
        let raw = r#"[{"Plan":{"Node Type":"Seq Scan","Plan Rows":4,"Storage Table Read Requests":-1,"Storage Table Read Ops":"NaN"}}]"#;
        let plan = parse(raw, PlanKind::Estimated, None).unwrap();
        assert!(plan.facts().docdb.is_none());
        assert!(plan.root.facts().docdb.is_none());
        assert_eq!(plan.root.rows_read_total(), None);
    }

    #[test]
    fn generic_ysql_ranges_and_arrays_keep_the_shared_parameter_evidence_on_replay() {
        for predicate in ["created_at >= $1", "id = ANY($1)"] {
            let raw = serde_json::json!([{"Plan":{"Node Type":"Seq Scan","Schema":"public","Relation Name":"orders",
                "Plan Rows":6667,"Total Cost":149,"Filter":predicate}}])
            .to_string();
            let mut plan = parse(&raw, PlanKind::Generic, None).unwrap();
            plan.root.table_rows = Some(20_000.0);
            assert!(plan.has_guess());
            assert_eq!(plan.root.kept_share(), None);
            assert_eq!(plan.guess(&plan.root), Some(culprit_core::Guess::Rows));
            assert!(reparse(&plan).unwrap().has_guess());
            assert!(!plan.root.facts().selectivity_guess.as_deref().unwrap().contains("may well use an index"));
        }
    }
}
