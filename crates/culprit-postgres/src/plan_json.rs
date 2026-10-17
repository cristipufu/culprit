use culprit_core::{Engine, Error, NodeKind, NodeRuntime, Plan, PlanKind, PlanNode, PlanWarning, Properties, Result};
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
        .map_err(|e| Error::internal(format!("PostgreSQL returned a plan Culprit cannot read: {e}")))?;
    let mut plan = from_value(&value, kind, statement)?;
    plan.raw = Some(json.to_string());
    Ok(plan)
}

/// A plan a scan saved, read again from its JSON with this parser, so a scan saved by an older Culprit reads the way
/// one taken today would. What the scan added from the catalog (each scanned table's size, and the marks that follow
/// from it) is worked out again from the sizes the saved plan carries; nothing is looked up. `entry` is the
/// pg_stat_statements entry (its queryid) the plan was made for, whose generic plan says what it can't know; `None`
/// for a plan of another source's statement (an application's log). `None` when the plan kept no JSON, or this parser
/// can't read it.
pub fn reparse(saved: &Plan, entry: Option<&str>) -> Option<Plan> {
    let raw = saved.raw.as_deref()?;
    let mut plan = parse(raw, saved.kind, saved.statement.as_deref()).ok()?;
    crate::tables::refill(&mut plan, saved);
    if let Some(entry) = entry.filter(|_| saved.kind == PlanKind::Generic) {
        crate::collect::mark_generic(&mut plan);
        crate::collect::mark_planned_text(&mut plan, entry);
    }
    Some(plan)
}

/// The same, from JSON already parsed (an imported plan, or text EXPLAIN rebuilt as JSON). `raw` is left empty.
pub fn from_value(value: &Value, kind: PlanKind, statement: Option<&str>) -> Result<Plan> {
    let top = match value {
        Value::Array(items) => items.first(),
        Value::Object(_) => Some(value),
        _ => None,
    }
    .and_then(Value::as_object)
    .ok_or_else(|| Error::internal("PostgreSQL returned an empty plan"))?;
    let root = top
        .get("Plan")
        .and_then(Value::as_object)
        .ok_or_else(|| Error::internal("PostgreSQL plan has no root node"))?;

    let mut next_id = 0;
    let (mut root, _) = node(root, &mut next_id, 1.0, None);
    root.flag_misestimates();
    let mut props = Properties::new();
    for (key, value) in top {
        if key == "Plan" {
            continue;
        }
        if let Some(text) = prop_text(value) {
            props.insert(key.clone(), text);
        }
    }
    // The same names every engine uses for the statement's own timing.
    if let Some(ms) = number(top, "Execution Time") {
        props.insert("ElapsedMs".into(), ms.to_string());
    }
    if let Some(ms) = number(top, "Planning Time") {
        props.insert("PlanningMs".into(), ms.to_string());
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
        insights: None,
    };
    plan.compute_self_costs();
    Ok(plan)
}

/// Buffer counts (with BUFFERS), temp-file spills and per-worker rows. Postgres reports buffers including the operator's
/// inputs; Culprit keeps each operator's own share so they add up across the plan.
fn runtime(map: &Map<String, Value>) -> Option<NodeRuntime> {
    let blocks = |m: &Map<String, Value>, keys: &[&str]| -> Option<f64> {
        let values: Vec<f64> = keys.iter().filter_map(|k| number(m, k)).collect();
        (!values.is_empty()).then(|| values.iter().sum())
    };
    const LOGICAL: &[&str] = &["Shared Hit Blocks", "Shared Read Blocks", "Local Hit Blocks", "Local Read Blocks"];
    const PHYSICAL: &[&str] = &["Shared Read Blocks", "Local Read Blocks"];
    const TEMP: &[&str] = &["Temp Written Blocks"];
    let children: Vec<&Map<String, Value>> = map
        .get("Plans")
        .and_then(Value::as_array)
        .map(|p| p.iter().filter_map(Value::as_object).collect())
        .unwrap_or_default();
    let own = |keys: &[&str]| {
        blocks(map, keys).map(|total| (total - children.iter().filter_map(|c| blocks(c, keys)).sum::<f64>()).max(0.0))
    };
    let thread_rows: Vec<f64> = map
        .get("Workers")
        .and_then(Value::as_array)
        .map(|workers| {
            workers
                .iter()
                .filter_map(Value::as_object)
                .filter_map(|w| number(w, "Actual Rows").map(|r| r * number(w, "Actual Loops").unwrap_or(1.0).max(1.0)))
                .collect()
        })
        .unwrap_or_default();
    let runtime = NodeRuntime {
        cpu_ms: None,
        logical_reads: own(LOGICAL),
        physical_reads: own(PHYSICAL),
        thread_rows: if thread_rows.len() >= 2 { thread_rows } else { Vec::new() },
        spill_pages: own(TEMP).filter(|p| *p > 0.0),
    };
    (!runtime.is_empty()).then_some(runtime)
}

/// PostgreSQL's share of a partial plan's rows each process is expected to handle: the workers planned, plus what the
/// leader contributes while it also gathers (`get_parallel_divisor`: 30% less per worker, none from four workers on).
fn parallel_divisor(workers: f64) -> f64 {
    let leader = 1.0 - 0.3 * workers;
    workers + if leader > 0.0 { leader } else { 0.0 }
}

/// `parallelism`: how many processes share the loops of this node (the workers a Gather launched plus the leader), so
/// loops that ran side by side don't add up to more time than the query took. `divisor`: below a Gather, the
/// [`parallel_divisor`] of its planned workers. Returns the node and whether it is partial: one process's share of a
/// parallel scan (a parallel-aware node, or one fed by a partial outer input), whose `Plan Rows` PostgreSQL gives per
/// process. An estimated plan's partial nodes are scaled to all processes, like every other row count Culprit shows;
/// an analyzed plan's runs (`Actual Loops`, one per process) do that already.
fn node(map: &Map<String, Value>, next_id: &mut u32, parallelism: f64, divisor: Option<f64>) -> (PlanNode, bool) {
    let id = *next_id;
    *next_id += 1;
    let op = text(map, "Node Type").unwrap_or_else(|| "Unknown".to_string());
    let loops = number(map, "Actual Loops");
    let analyzed = loops.is_some();
    let loops_or_one = loops.unwrap_or(1.0).max(1.0);
    // Rows per loop are averaged and rounded by PostgreSQL; the total is a whole number of rows.
    let act_rows = number(map, "Actual Rows").map(|r| (r * loops_or_one).round());
    let removed = number(map, "Rows Removed by Filter").map(|r| r * loops_or_one);

    let object = text(map, "Relation Name").map(|relation| match text(map, "Schema") {
        Some(schema) => format!("{schema}.{relation}"),
        None => relation,
    });
    let seek_predicate = text(map, "Index Cond").or_else(|| text(map, "Recheck Cond"));
    let predicate = ["Filter", "Join Filter", "Hash Cond", "Merge Cond"].iter().find_map(|key| text(map, key));

    // Buffer counters include the node's children, so only temp blocks this node wrote itself count as its spill.
    let child_temp: f64 = map
        .get("Plans")
        .and_then(Value::as_array)
        .map(|plans| plans.iter().filter_map(|p| p.get("Temp Written Blocks").and_then(Value::as_f64)).sum())
        .unwrap_or(0.0);
    let own_temp = number(map, "Temp Written Blocks").map(|b| b - child_temp).unwrap_or(0.0);
    let mut warnings = Vec::new();
    if text(map, "Sort Space Type").as_deref() == Some("Disk")
        || number(map, "Hash Batches").is_some_and(|b| b > 1.0)
        || number(map, "Original Hash Batches").is_some_and(|b| b > 1.0)
        || number(map, "HashAgg Batches").is_some_and(|b| b > 1.0)
        || own_temp > 0.0 && matches!(op.as_str(), "Sort" | "Hash" | "Incremental Sort" | "Aggregate")
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

    let gather = matches!(op.as_str(), "Gather" | "Gather Merge");
    // Below a Gather, its workers and the leader each run the subtree at the same time.
    let below = if gather {
        1.0 + number(map, "Workers Launched").or(number(map, "Workers Planned")).unwrap_or(0.0)
    } else {
        parallelism
    };
    let children_divisor = if gather { number(map, "Workers Planned").map(parallel_divisor) } else { divisor };
    let mut children = Vec::new();
    let mut outer_partial = None;
    for child in map.get("Plans").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_object) {
        let (built, partial) = node(child, next_id, below, children_divisor);
        // The outer input decides: a join's first input, an aggregate's only one; a SubPlan or InitPlan doesn't.
        let side = child.get("Parent Relationship").and_then(Value::as_str);
        if outer_partial.is_none() && !matches!(side, Some("SubPlan" | "InitPlan")) {
            outer_partial = Some(partial);
        }
        children.push(built);
    }
    // A Gather's output is whole again.
    let partial = !gather
        && divisor.is_some()
        && (map.get("Parallel Aware").and_then(Value::as_bool) == Some(true) || outer_partial == Some(true));
    let plan_rows = number(map, "Plan Rows").unwrap_or(0.0);
    let est_rows = match divisor {
        Some(d) if partial && !analyzed => plan_rows * d,
        _ => plan_rows,
    };

    if partial && !analyzed {
        props.insert("Plan Rows per process".into(), plan_rows.to_string());
    }
    let total_cost = number(map, "Total Cost").unwrap_or(0.0);
    if op == "Nested Loop" {
        repeat_inner_side(&mut children, total_cost);
    }
    let result = PlanNode {
        id,
        kind: kind_of(&op),
        detail: detail(map),
        op,
        object,
        index: text(map, "Index Name"),
        seek_predicate,
        predicate,
        est_rows,
        est_executions: if analyzed { loops_or_one } else { 1.0 },
        act_rows,
        act_executions: loops,
        rows_read: act_rows.map(|r| r + removed.unwrap_or(0.0)).filter(|_| removed.is_some()),
        table_rows: None,
        subtree_cost: total_cost,
        self_cost: 0.0,
        act_time_ms: number(map, "Actual Total Time").map(|t| t * (loops_or_one / parallelism).max(1.0)),
        runtime: runtime(map),
        warnings,
        props,
        children,
    };
    (result, partial)
}

/// A Nested Loop runs its inner side once for each row its outer side returns, and PostgreSQL gives that side's `Plan
/// Rows` and `Total Cost` for one run. Count the runs on the inner side's operators (an analyzed plan has them already,
/// as loops) and its cost over all of them, so the loop's own cost is the joining, not its lookups run again and again,
/// and a lookup run ten times isn't one run at a tenth of its cost. The cost is the planner's: over the runs it planned
/// (per process below a Gather, like the costs themselves), and never more than the loop's total leaves after its outer
/// side (a semi or unique join stops at the first match and is costed for less).
fn repeat_inner_side(children: &mut [PlanNode], loop_cost: f64) {
    // Its two inputs, outer first; its other children are InitPlans and SubPlans (text EXPLAIN names only those).
    let mut inputs = children
        .iter_mut()
        .filter(|c| c.props.get("Parent Relationship").is_none_or(|r| matches!(r.as_str(), "Outer" | "Inner")));
    let (Some(outer), Some(inner)) = (inputs.next(), inputs.next()) else { return };
    let runs = outer.est_rows.max(1.0);
    let planned =
        outer.props.get("Plan Rows per process").and_then(|r| r.parse::<f64>().ok()).unwrap_or(outer.est_rows);
    let left = loop_cost - outer.subtree_cost;
    let cost_runs = if inner.subtree_cost > 0.0 { planned.min(left / inner.subtree_cost).max(1.0) } else { 1.0 };
    repeat(inner, runs, cost_runs);
}

/// Run `node` and what it reads `runs` times, costed for `cost_runs`. A Materialize or a Memoize replays the rows its
/// input returned: it runs each time, its input once (or once per new key, which the plan doesn't say), and replaying
/// costs little. An InitPlan (a CTE among them) runs once for the whole statement.
fn repeat(node: &mut PlanNode, runs: f64, cost_runs: f64) {
    if node.act_executions.is_none() {
        node.est_executions *= runs;
    }
    if matches!(node.op.as_str(), "Materialize" | "Memoize") {
        return;
    }
    node.subtree_cost *= cost_runs;
    for child in node.children.iter_mut() {
        if child.props.get("Parent Relationship").is_none_or(|r| !matches!(r.as_str(), "InitPlan" | "CTE")) {
            repeat(child, runs, cost_runs);
        }
    }
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

    #[test]
    fn parallel_workers_share_their_loops() {
        let json = r#"[{"Plan": {"Node Type": "Gather", "Workers Planned": 2, "Workers Launched": 2,
            "Actual Total Time": 60.0, "Actual Rows": 14, "Actual Loops": 1, "Plan Rows": 15, "Total Cost": 100,
            "Plans": [{"Node Type": "Seq Scan", "Parallel Aware": true, "Relation Name": "orders",
                "Actual Total Time": 56.0, "Actual Rows": 4.67, "Actual Loops": 3, "Plan Rows": 6, "Total Cost": 90}]},
            "Execution Time": 60.5}]"#;
        let plan = parse(json, PlanKind::Actual, None).unwrap();
        let scan = &plan.root.children[0];
        // Three loops side by side: 56 ms of wall time, not 168.
        assert_eq!(scan.act_time_ms, Some(56.0));
        assert_eq!(scan.act_rows, Some(14.0));
        assert_eq!(plan.root.act_time_ms, Some(60.0));
    }

    #[test]
    fn estimated_parallel_rows_are_scaled_to_every_process() {
        // Plan Rows below a Gather are one process's share: divided by 2 workers plus 0.4 of a leader.
        let json = r#"[{"Plan": {"Node Type": "Gather", "Workers Planned": 2, "Plan Rows": 10000, "Total Cost": 47000,
            "Plans": [{"Node Type": "Nested Loop", "Parent Relationship": "Outer", "Join Type": "Inner", "Plan Rows": 4167, "Total Cost": 46000,
              "Plans": [
                {"Node Type": "Seq Scan", "Parent Relationship": "Outer", "Parallel Aware": true, "Relation Name": "orders",
                 "Plan Rows": 4167, "Total Cost": 45000, "Filter": "(status = 'x')"},
                {"Node Type": "Index Scan", "Parent Relationship": "Inner", "Parallel Aware": false, "Relation Name": "customers",
                 "Index Name": "customers_pkey", "Plan Rows": 1, "Total Cost": 0.4, "Index Cond": "(id = orders.customer_id)"}]}]}}]"#;
        let plan = parse(json, PlanKind::Estimated, None).unwrap();
        assert_eq!(plan.root.est_rows, 10_000.0, "a Gather's rows are already whole");
        let join = &plan.root.children[0];
        let scan = &join.children[0];
        assert!((scan.est_rows - 4_167.0 * 2.4).abs() < 1e-6, "{}", scan.est_rows);
        assert!((join.est_rows - 4_167.0 * 2.4).abs() < 1e-6, "fed by the partial scan: {}", join.est_rows);
        assert_eq!(scan.props.get("Plan Rows per process").map(String::as_str), Some("4167"));
        // The inner side of the loop runs per outer row in each process: its rows are per run, not shared out.
        assert_eq!(join.children[1].est_rows, 1.0);

        // Measured, each process's loop counts: rows per loop times loops is the total already.
        let analyzed = r#"[{"Plan": {"Node Type": "Gather", "Workers Planned": 2, "Workers Launched": 2, "Plan Rows": 10000,
            "Actual Rows": 9000, "Actual Loops": 1, "Total Cost": 47000,
            "Plans": [{"Node Type": "Seq Scan", "Parallel Aware": true, "Relation Name": "orders", "Plan Rows": 4167,
              "Actual Rows": 3000, "Actual Loops": 3, "Total Cost": 45000}]}}]"#;
        let plan = parse(analyzed, PlanKind::Actual, None).unwrap();
        assert_eq!(plan.root.children[0].est_rows, 4_167.0);
        assert!(plan.nodes().iter().all(|n| !n.warnings.contains(&PlanWarning::Misestimate)));
    }

    #[test]
    fn early_stops_and_operators_that_never_ran_are_not_misestimates() {
        let json = r#"[{"Plan": {"Node Type": "Limit", "Plan Rows": 20, "Actual Rows": 20, "Actual Loops": 1, "Total Cost": 15,
            "Plans": [{"Node Type": "Nested Loop", "Join Type": "Inner", "Plan Rows": 399599, "Actual Rows": 20, "Actual Loops": 1,
              "Total Cost": 292890, "Plans": [
                {"Node Type": "Index Scan", "Relation Name": "orders", "Plan Rows": 399599, "Actual Rows": 20, "Actual Loops": 1, "Total Cost": 141997},
                {"Node Type": "Index Scan", "Relation Name": "customers", "Plan Rows": 1, "Actual Rows": 0, "Actual Loops": 0, "Total Cost": 0.46}
              ]}]}}]"#;
        let plan = parse(json, PlanKind::Actual, None).unwrap();
        assert!(plan.nodes().iter().all(|n| !n.warnings.contains(&PlanWarning::Misestimate)));
    }

    #[test]
    fn buffers_are_each_operators_own_share() {
        let json = r#"[{"Plan": {"Node Type": "Hash Join", "Total Cost": 10, "Plan Rows": 5, "Actual Rows": 5, "Actual Loops": 1,
            "Actual Total Time": 30.0, "Shared Hit Blocks": 120, "Shared Read Blocks": 30, "Temp Written Blocks": 0,
            "Plans": [
              {"Node Type": "Seq Scan", "Relation Name": "orders", "Total Cost": 8, "Plan Rows": 100, "Actual Rows": 100,
               "Actual Loops": 1, "Actual Total Time": 25.0, "Shared Hit Blocks": 100, "Shared Read Blocks": 30,
               "Workers": [{"Worker Number": 0, "Actual Rows": 70, "Actual Loops": 1}, {"Worker Number": 1, "Actual Rows": 10, "Actual Loops": 1}]}
            ]}, "Planning Time": 0.4, "Execution Time": 31.5}]"#;
        let plan = parse(json, PlanKind::Actual, None).unwrap();
        let join = plan.root.runtime.as_ref().unwrap();
        assert_eq!((join.logical_reads, join.physical_reads), (Some(20.0), Some(0.0)));
        let scan = plan.root.children[0].runtime.as_ref().unwrap();
        assert_eq!((scan.logical_reads, scan.physical_reads), (Some(130.0), Some(30.0)));
        assert_eq!(scan.thread_rows, vec![70.0, 10.0]);
        assert_eq!(plan.props.get("ElapsedMs").map(String::as_str), Some("31.5"));
        let summary = culprit_core::actual_summary(&plan).unwrap();
        assert_eq!((summary.elapsed_ms, summary.logical_reads), (Some(31.5), Some(150.0)));
    }

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
        // Self costs from cumulative ones: the loop's own is what's left after its scan and its 10 lookups at 5 each.
        assert!((plan.root.self_cost - 0.5).abs() < 1e-9, "{}", plan.root.self_cost);
        assert!((seek.self_cost - 50.0).abs() < 1e-9, "{}", seek.self_cost);
        assert_eq!(plan.hottest().unwrap().id, 1);
        assert!(plan.root.act_rows.is_none());
    }

    #[test]
    fn a_nested_loops_inner_side_runs_once_per_outer_row() {
        // A generic plan from a real scan: 10 apps, each looking up its owner by primary key. PostgreSQL costs the
        // lookup for one run (7.90) and the loop for all ten: 149.54 = 70.54 + 10 × 7.90. The loop isn't the costliest
        // step at 47% of the plan, and the lookup isn't one run at 5%.
        let json = r#"[{"Plan": {"Node Type": "Nested Loop", "Join Type": "Inner", "Inner Unique": true,
            "Total Cost": 149.54, "Plan Rows": 10, "Plans": [
              {"Node Type": "Bitmap Heap Scan", "Parent Relationship": "Outer", "Relation Name": "apps", "Schema": "public",
               "Total Cost": 70.54, "Plan Rows": 10, "Recheck Cond": "((a.id)::text = ANY ($1))", "Plans": [
                 {"Node Type": "Bitmap Index Scan", "Parent Relationship": "Outer", "Index Name": "PK_apps",
                  "Total Cost": 38.90, "Plan Rows": 10, "Index Cond": "((a.id)::text = ANY ($1))"}]},
              {"Node Type": "Index Scan", "Parent Relationship": "Inner", "Index Name": "PK_users", "Relation Name": "users",
               "Schema": "public", "Total Cost": 7.90, "Plan Rows": 1, "Index Cond": "((id)::text = (a.owner_id)::text)"}]}}]"#;
        let plan = parse(json, PlanKind::Generic, None).unwrap();
        let lookup = &plan.root.children[1];
        assert_eq!((lookup.est_executions, lookup.est_rows, lookup.est_rows_total()), (10.0, 1.0, 10.0));
        assert!((lookup.self_cost - 79.0).abs() < 1e-9, "{}", lookup.self_cost);
        assert!(plan.root.self_cost < 1e-9, "the loop's own cost: {}", plan.root.self_cost);
        assert_eq!(plan.hottest().map(|n| n.id), Some(lookup.id));
        assert!((plan.cost_share(lookup) - 79.0 / 149.54).abs() < 1e-9);
        // The outer side runs once.
        assert_eq!(plan.root.children[0].est_executions, 1.0);

        // A Materialize replays its input's rows: it runs per outer row, its input once, and its cost isn't repeated.
        // A semi join stops at the first match: the inner side is costed for no more than the loop leaves for it.
        let json = r#"[{"Plan": {"Node Type": "Nested Loop", "Join Type": "Semi", "Total Cost": 400.0, "Plan Rows": 100,
            "Plans": [
              {"Node Type": "Seq Scan", "Parent Relationship": "Outer", "Relation Name": "orders", "Total Cost": 100.0,
               "Plan Rows": 100},
              {"Node Type": "Nested Loop", "Parent Relationship": "Inner", "Total Cost": 20.0, "Plan Rows": 1, "Plans": [
                {"Node Type": "Index Scan", "Parent Relationship": "Outer", "Relation Name": "items", "Total Cost": 8.0,
                 "Plan Rows": 2},
                {"Node Type": "Materialize", "Parent Relationship": "Inner", "Total Cost": 4.0, "Plan Rows": 5, "Plans": [
                  {"Node Type": "Seq Scan", "Parent Relationship": "Outer", "Relation Name": "flags", "Total Cost": 3.0,
                   "Plan Rows": 5}]}]}]}}]"#;
        let plan = parse(json, PlanKind::Estimated, None).unwrap();
        let inner = &plan.root.children[1];
        let (items, materialize) = (&inner.children[0], &inner.children[1]);
        let flags = &materialize.children[0];
        assert_eq!(inner.est_executions, 100.0);
        assert_eq!(items.est_executions, 100.0);
        assert_eq!(materialize.est_executions, 200.0, "twice per run of the inner loop, 100 runs");
        assert_eq!(flags.est_executions, 1.0, "read once, replayed after");
        // 100 runs at 20 would be 2,000; the semi join is costed at 300 for its inner side.
        assert!((inner.subtree_cost - 300.0).abs() < 1e-9, "{}", inner.subtree_cost);
        assert!(plan.root.self_cost.abs() < 1e-9);
        assert!(plan.nodes().iter().all(|n| n.self_cost >= 0.0));

        // An analyzed plan counted its loops: they stay as measured, and the cost is still the planner's for its runs.
        let json = r#"[{"Plan": {"Node Type": "Nested Loop", "Total Cost": 90.0, "Plan Rows": 10, "Actual Rows": 4,
            "Actual Loops": 1, "Plans": [
              {"Node Type": "Seq Scan", "Parent Relationship": "Outer", "Relation Name": "a", "Total Cost": 10.0,
               "Plan Rows": 10, "Actual Rows": 4, "Actual Loops": 1},
              {"Node Type": "Index Scan", "Parent Relationship": "Inner", "Relation Name": "b", "Total Cost": 8.0,
               "Plan Rows": 1, "Actual Rows": 1, "Actual Loops": 4}]}}]"#;
        let plan = parse(json, PlanKind::Actual, None).unwrap();
        let lookup = &plan.root.children[1];
        assert_eq!((lookup.est_executions, lookup.act_executions), (4.0, Some(4.0)));
        assert!((lookup.subtree_cost - 80.0).abs() < 1e-9);
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
