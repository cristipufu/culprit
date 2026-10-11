use culprit_core::{Engine, Error, NodeKind, Plan, PlanKind, PlanNode, PlanWarning, Properties, Result};
use serde_json::{Map, Value};

/// Fields mapped onto `PlanNode` directly, or too bulky to keep as properties.
const CONSUMED: &[&str] = &[
    "Plans",
    "Node Type",
    "Relation Name",
    "Schema",
    "Index Name",
    "Plan Rows",
    "Actual Rows",
    "Actual Loops",
    "Actual Total Time",
    "Total Cost",
    "Output",
];

/// Parse `EXPLAIN (FORMAT JSON)` output into Culprit's plan model.
pub fn parse(json: &str, kind: PlanKind, statement: Option<&str>) -> Result<Plan> {
    let value: Value = serde_json::from_str(json)
        .map_err(|e| Error::internal(format!("Postgres returned a plan Culprit cannot read: {e}")))?;
    let mut plan = from_value(&value, kind, statement)?;
    plan.raw = Some(json.to_string());
    Ok(plan)
}

/// The same, from JSON already parsed (an imported plan, or text EXPLAIN rebuilt as JSON). `raw` is left empty.
pub fn from_value(value: &Value, kind: PlanKind, statement: Option<&str>) -> Result<Plan> {
    let top = match value {
        Value::Array(items) => items.first(),
        Value::Object(_) => Some(value),
        _ => None,
    }
    .and_then(Value::as_object)
    .ok_or_else(|| Error::internal("Postgres returned an empty plan"))?;
    let root =
        top.get("Plan").and_then(Value::as_object).ok_or_else(|| Error::internal("Postgres plan has no root node"))?;

    let mut next_id = 0;
    let root = node(root, &mut next_id);
    let mut props = Properties::new();
    for (key, value) in top {
        if key == "Plan" {
            continue;
        }
        if let Some(text) = prop_text(value) {
            props.insert(key.clone(), text);
        }
    }
    let mut plan = Plan {
        engine: Engine::Postgres,
        kind,
        statement: statement.map(str::to_string),
        total_cost: root.subtree_cost,
        root,
        warnings: Vec::new(),
        index_hints: Vec::new(),
        props,
        raw: None,
    };
    plan.compute_self_costs();
    Ok(plan)
}

fn node(map: &Map<String, Value>, next_id: &mut u32) -> PlanNode {
    let id = *next_id;
    *next_id += 1;
    let op = text(map, "Node Type").unwrap_or_else(|| "Unknown".to_string());
    let loops = number(map, "Actual Loops");
    let analyzed = loops.is_some();
    let loops_or_one = loops.unwrap_or(1.0).max(1.0);
    let act_rows = number(map, "Actual Rows").map(|r| r * loops_or_one);
    let removed = number(map, "Rows Removed by Filter").map(|r| r * loops_or_one);

    let object = text(map, "Relation Name").map(|relation| match text(map, "Schema") {
        Some(schema) => format!("{schema}.{relation}"),
        None => relation,
    });
    let seek_predicate = text(map, "Index Cond").or_else(|| text(map, "Recheck Cond"));
    let predicate = ["Filter", "Join Filter", "Hash Cond", "Merge Cond"].iter().find_map(|key| text(map, key));

    let mut warnings = Vec::new();
    if text(map, "Sort Space Type").as_deref() == Some("Disk")
        || number(map, "Hash Batches").is_some_and(|b| b > 1.0)
        || number(map, "Original Hash Batches").is_some_and(|b| b > 1.0)
        || number(map, "Temp Written Blocks").is_some_and(|b| b > 0.0)
            && matches!(op.as_str(), "Sort" | "Hash" | "Incremental Sort" | "Aggregate")
    {
        warnings.push(PlanWarning::Spill);
    }
    if let (Some(returned), Some(removed)) = (act_rows, removed)
        && removed >= 1_000.0
        && removed >= 10.0 * returned.max(1.0)
    {
        warnings.push(PlanWarning::ResidualPredicate);
    }

    let mut props = Properties::new();
    for (key, value) in map {
        if CONSUMED.contains(&key.as_str()) {
            continue;
        }
        if let Some(text) = prop_text(value) {
            props.insert(key.clone(), text);
        }
    }

    let children = map
        .get("Plans")
        .and_then(Value::as_array)
        .map(|plans| plans.iter().filter_map(Value::as_object).map(|child| node(child, next_id)).collect())
        .unwrap_or_default();

    let mut result = PlanNode {
        id,
        kind: kind_of(&op),
        detail: detail(map),
        op,
        object,
        index: text(map, "Index Name"),
        seek_predicate,
        predicate,
        est_rows: number(map, "Plan Rows").unwrap_or(0.0),
        est_executions: if analyzed { loops_or_one } else { 1.0 },
        act_rows,
        act_executions: loops,
        rows_read: act_rows.map(|r| r + removed.unwrap_or(0.0)).filter(|_| removed.is_some()),
        table_rows: None,
        subtree_cost: number(map, "Total Cost").unwrap_or(0.0),
        self_cost: 0.0,
        act_time_ms: number(map, "Actual Total Time").map(|t| t * loops_or_one),
        warnings,
        props,
        children,
    };
    if let Some(factor) = result.misestimate()
        && factor.abs() >= 10.0
        && result.est_rows_total().max(result.act_rows.unwrap_or(0.0)) >= 100.0
    {
        result.warnings.push(PlanWarning::Misestimate);
    }
    result
}

fn kind_of(op: &str) -> NodeKind {
    match op {
        "Seq Scan"
        | "Sample Scan"
        | "Tid Scan"
        | "Tid Range Scan"
        | "Function Scan"
        | "Table Function Scan"
        | "Values Scan"
        | "CTE Scan"
        | "Named Tuplestore Scan"
        | "WorkTable Scan"
        | "Subquery Scan"
        | "Foreign Scan"
        | "Custom Scan" => NodeKind::Scan,
        "Index Scan" | "Index Only Scan" | "Bitmap Index Scan" => NodeKind::Seek,
        "Bitmap Heap Scan" => NodeKind::Lookup,
        "Nested Loop" | "Hash Join" | "Merge Join" => NodeKind::Join,
        "Sort" | "Incremental Sort" => NodeKind::Sort,
        "Aggregate" | "Group" | "WindowAgg" | "Unique" | "SetOp" => NodeKind::Aggregate,
        "Materialize" | "Memoize" => NodeKind::Spool,
        "Gather" | "Gather Merge" => NodeKind::Parallelism,
        "ModifyTable" => NodeKind::Modify,
        "Result" | "ProjectSet" => NodeKind::Compute,
        _ => NodeKind::Other,
    }
}

fn detail(map: &Map<String, Value>) -> Option<String> {
    let mut parts = Vec::new();
    if let Some(operation) = text(map, "Operation") {
        parts.push(operation);
    }
    if let Some(join) = text(map, "Join Type") {
        parts.push(format!("{join} join"));
    }
    if let Some(strategy) = text(map, "Strategy").filter(|s| s != "Plain") {
        parts.push(strategy);
    }
    if map.get("Parallel Aware").and_then(Value::as_bool) == Some(true) {
        parts.push("Parallel".to_string());
    }
    if text(map, "Scan Direction").as_deref() == Some("Backward") {
        parts.push("Backward".to_string());
    }
    for key in ["Sort Key", "Group Key", "Presorted Key"] {
        if let Some(keys) = text(map, key) {
            parts.push(format!("{}: {keys}", key.to_lowercase()));
        }
    }
    (!parts.is_empty()).then(|| parts.join(" · "))
}

fn text(map: &Map<String, Value>, key: &str) -> Option<String> {
    prop_text(map.get(key)?).filter(|s| !s.is_empty())
}

fn number(map: &Map<String, Value>, key: &str) -> Option<f64> {
    map.get(key)?.as_f64()
}

fn prop_text(value: &Value) -> Option<String> {
    match value {
        Value::Null => None,
        Value::String(s) => Some(s.clone()),
        Value::Bool(b) => Some(b.to_string()),
        Value::Number(n) => Some(n.to_string()),
        Value::Array(items) if items.iter().all(|v| !v.is_object() && !v.is_array()) => {
            Some(items.iter().filter_map(prop_text).collect::<Vec<_>>().join(", "))
        }
        other => Some(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GENERIC: &str = r#"[{"Plan": {
        "Node Type": "Nested Loop", "Join Type": "Inner", "Startup Cost": 0.43, "Total Cost": 1250.5,
        "Plan Rows": 10, "Plan Width": 64,
        "Plans": [
          {"Node Type": "Seq Scan", "Parent Relationship": "Outer", "Relation Name": "orders", "Schema": "public",
           "Alias": "o", "Startup Cost": 0.0, "Total Cost": 1200.0, "Plan Rows": 10, "Plan Width": 32,
           "Filter": "(o.customer_id = $1)", "Output": ["o.id", "o.total"]},
          {"Node Type": "Index Scan", "Parent Relationship": "Inner", "Scan Direction": "Forward",
           "Index Name": "customers_pkey", "Relation Name": "customers", "Schema": "public", "Alias": "c",
           "Startup Cost": 0.43, "Total Cost": 5.0, "Plan Rows": 1, "Plan Width": 32, "Index Cond": "(c.id = $1)"}
        ]}}]"#;

    const ANALYZED: &str = r#"[{"Plan": {
        "Node Type": "Sort", "Total Cost": 500.0, "Plan Rows": 100, "Actual Rows": 90000, "Actual Loops": 1,
        "Actual Total Time": 812.5, "Sort Key": ["created_at DESC"], "Sort Method": "external merge",
        "Sort Space Used": 20480, "Sort Space Type": "Disk",
        "Plans": [
          {"Node Type": "Seq Scan", "Relation Name": "events", "Total Cost": 400.0, "Plan Rows": 100,
           "Actual Rows": 90000, "Actual Loops": 1, "Actual Total Time": 300.0,
           "Filter": "(kind = 'click'::text)", "Rows Removed by Filter": 1910000, "Shared Hit Blocks": 12, "Shared Read Blocks": 4500}
        ]}, "Planning Time": 0.2, "Execution Time": 815.1}]"#;

    #[test]
    fn generic_plan_maps_nodes() {
        let plan = parse(GENERIC, PlanKind::Generic, Some("select 1")).unwrap();
        assert_eq!(plan.kind, PlanKind::Generic);
        assert_eq!(plan.root.kind, NodeKind::Join);
        assert_eq!(plan.root.detail.as_deref(), Some("Inner join"));
        let scan = &plan.root.children[0];
        assert_eq!(scan.id, 1);
        assert_eq!(scan.kind, NodeKind::Scan);
        assert_eq!(scan.object.as_deref(), Some("public.orders"));
        assert_eq!(scan.predicate.as_deref(), Some("(o.customer_id = $1)"));
        assert!(!scan.props.contains_key("Output"));
        let seek = &plan.root.children[1];
        assert_eq!(seek.kind, NodeKind::Seek);
        assert_eq!(seek.index.as_deref(), Some("customers_pkey"));
        assert_eq!(seek.seek_predicate.as_deref(), Some("(c.id = $1)"));
        assert!((plan.root.self_cost - 45.5).abs() < 1e-9);
        assert_eq!(plan.hottest().id, 1);
        assert!(plan.root.act_rows.is_none());
    }

    #[test]
    fn analyzed_plan_flags_spills_misestimates_and_filters() {
        let plan = parse(ANALYZED, PlanKind::Actual, None).unwrap();
        assert!(plan.root.warnings.contains(&PlanWarning::Spill));
        assert!(plan.root.warnings.contains(&PlanWarning::Misestimate));
        assert_eq!(plan.root.detail.as_deref(), Some("sort key: created_at DESC"));
        let scan = &plan.root.children[0];
        assert_eq!(scan.act_rows, Some(90000.0));
        assert_eq!(scan.rows_read, Some(2_000_000.0));
        assert!(scan.warnings.contains(&PlanWarning::ResidualPredicate));
        assert_eq!(scan.props.get("Shared Read Blocks").map(String::as_str), Some("4500"));
        assert_eq!(plan.props.get("Execution Time").map(String::as_str), Some("815.1"));
        assert_eq!(plan.root.act_time_ms, Some(812.5));
    }

    #[test]
    fn loops_multiply_actuals() {
        let json = r#"[{"Plan": {"Node Type": "Index Scan", "Index Name": "i", "Relation Name": "t", "Total Cost": 8.0,
            "Plan Rows": 1, "Actual Rows": 2, "Actual Loops": 500, "Actual Total Time": 0.01}}]"#;
        let plan = parse(json, PlanKind::Actual, None).unwrap();
        assert_eq!(plan.root.act_rows, Some(1000.0));
        assert_eq!(plan.root.est_executions, 500.0);
        assert_eq!(plan.root.act_executions, Some(500.0));
        assert!((plan.root.act_time_ms.unwrap() - 5.0).abs() < 1e-9);
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse("not json", PlanKind::Estimated, None).is_err());
        assert!(parse("[]", PlanKind::Estimated, None).is_err());
    }
}
