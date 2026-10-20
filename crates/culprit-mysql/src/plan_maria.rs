//! MariaDB's `EXPLAIN FORMAT=JSON`, `ANALYZE FORMAT=JSON` and `SHOW ANALYZE FORMAT=JSON FOR <id>` into Culprit's plan
//! model.
//!
//! MariaDB's JSON is shaped differently from MySQL's: estimates are per loop (`rows`, `loops`, `filtered`), costs are
//! plain numbers (MariaDB 11.0+; none before), sorts and temporary tables wrap their input (`filesort`,
//! `temporary_table`, `read_sorted_file`), join buffers sit beside the table they join (`block-nl-join`), and ANALYZE
//! adds measured `r_*` members to every operation: loops, rows per loop, the share kept by the condition, time and
//! pages. A running statement's `SHOW ANALYZE` has the same members, counted so far.

use culprit_core::{Engine, NodeKind, NodeRuntime, Plan, PlanKind, PlanNode, PlanWarning, Properties, Result};
use serde_json::{Map, Value};

use crate::plan_json::{Context, access_op, flag, new_node, number_field, scalar_text, strings, text, unquote};

/// Members only MariaDB writes; MySQL's version 1 JSON always has `cost_info` instead.
const MARIADB_KEYS: [&str; 9] = [
    "r_loops",
    "loops",
    "read_sorted_file",
    "filesort",
    "temporary_table",
    "query_optimization",
    "block-nl-join",
    "expression_cache",
    "subqueries",
];

/// Whether a parsed EXPLAIN JSON document is MariaDB's.
pub fn is_mariadb(value: &Value) -> bool {
    fn walk(value: &Value, found: &mut (bool, bool)) {
        match value {
            Value::Object(map) => {
                for (key, inner) in map {
                    if key == "cost_info" || key == "rows_examined_per_scan" {
                        found.1 = true;
                    }
                    // A table with a plain `rows` estimate is MariaDB's; MySQL writes rows_examined_per_scan.
                    if MARIADB_KEYS.contains(&key.as_str()) || (key == "rows" && map.contains_key("access_type")) {
                        found.0 = true;
                    }
                    walk(inner, found);
                }
            }
            Value::Array(items) => items.iter().for_each(|i| walk(i, found)),
            _ => {}
        }
    }
    let mut found = (false, false);
    walk(value, &mut found);
    found.0 && !found.1
}

pub fn parse(value: &Value, json: &str, kind: PlanKind, statement: Option<&str>, schema: Option<&str>) -> Result<Plan> {
    let block = value
        .get("query_block")
        .and_then(Value::as_object)
        .ok_or_else(|| culprit_core::Error::internal("MariaDB plan has no query_block"))?;
    let measured = has_key(value, "r_rows") || has_key(value, "r_total_time_ms");
    // EXPLAIN never measures; a plan with measurements is an actual one whatever the caller expected.
    let kind = if measured && kind == PlanKind::Estimated { PlanKind::Actual } else { kind };
    let cx = Context::new(statement, schema);
    let costed = has_key(block_value(value), "cost");
    let mut next_id = 0;
    let mut root = query_block(block, &cx, &mut next_id, costed);
    fill_times(&mut root);
    if measured {
        root.flag_misestimates();
    }

    let mut props = Properties::new();
    props.insert("flavor".into(), "MariaDB".into());
    if !costed {
        props.insert(
            "cost_basis".into(),
            "Rows examined: this MariaDB version reports no optimizer costs, so the costs shown are row counts".into(),
        );
    }
    let optimization = value.get("query_optimization").and_then(|o| number_field(o.as_object()?, "r_total_time_ms"));
    let in_progress = value.as_object().and_then(|m| number_field(m, "r_query_time_in_progress_ms"));
    let elapsed = in_progress.or_else(|| root.act_time_ms.map(|ms| ms + optimization.unwrap_or(0.0)));
    if let Some(ms) = elapsed {
        props.insert("ElapsedMs".into(), ms.to_string());
    }
    if let Some(ms) = optimization {
        props.insert("optimization_ms".into(), ms.to_string());
    }
    let mut plan = Plan {
        facts: Some(culprit_core::PlanFacts {
            elapsed_ms: elapsed.and_then(culprit_core::plan_facts::nonnegative),
            flavor: Some("MariaDB".into()),
            cost_basis: (!costed).then(|| {
                "Rows examined: this MariaDB version reports no optimizer costs, so the costs shown are row counts"
                    .into()
            }),
            ..Default::default()
        }),
        view: None,
        engine: Engine::Mysql,
        kind,
        statement: statement.map(str::to_string),
        total_cost: root.subtree_cost,
        root,
        warnings: Vec::new(),
        index_hints: Vec::new(),
        props,
        raw: Some(json.to_string()),
        insights: None,
    };
    plan.compute_self_costs();
    Ok(plan)
}

fn block_value(value: &Value) -> &Value {
    value.get("query_block").unwrap_or(value)
}

fn has_key(value: &Value, wanted: &str) -> bool {
    match value {
        Value::Object(map) => map.iter().any(|(k, v)| k == wanted || has_key(v, wanted)),
        Value::Array(items) => items.iter().any(|v| has_key(v, wanted)),
        _ => false,
    }
}

fn query_block(block: &Map<String, Value>, cx: &Context, id: &mut u32, costed: bool) -> PlanNode {
    let mut root = new_node(id, "Query block", NodeKind::Other);
    for key in ["select_id", "operation"] {
        if let Some(v) = block.get(key) {
            root.props.insert(key.into(), scalar_text(v));
        }
    }
    if let Some(having) = text(block, "having_condition") {
        root.detail = Some(format!("having {}", unquote(&having)));
    }
    root.children = operations(block, cx, id, costed);
    for sub in block.get("subqueries").and_then(Value::as_array).into_iter().flatten() {
        if let Some(child) = subquery(sub, cx, id, costed) {
            root.children.push(child);
        }
    }
    if let Some(union) = block.get("union_result").and_then(Value::as_object) {
        root.children.push(union_result(union, cx, id, costed));
    }
    let children: f64 = root.children.iter().map(|c| c.subtree_cost).sum();
    root.subtree_cost = number_field(block, "cost").filter(|_| costed).unwrap_or(0.0).max(children);
    // MariaDB reports no rows for a query block itself. Like MySQL's reader, estimate from the last step's output;
    // never borrow a child's actual rows (rows a scan examined aren't rows the query returned: COUNT(*) returns one).
    root.est_rows =
        number_field(block, "rows").unwrap_or_else(|| root.children.last().map(|c| c.est_rows).unwrap_or(0.0));
    measured(block, &mut root);
    root.act_time_ms = number_field(block, "r_total_time_ms");
    if root.act_rows.is_none() {
        root.act_rows = number_field(block, "r_rows");
    }
    root
}

/// The operations directly inside `map`. MariaDB nests one wrapper (sort, temporary table, window, distinct) around
/// the next level, ending in a table or a nested loop of tables.
fn operations(map: &Map<String, Value>, cx: &Context, id: &mut u32, costed: bool) -> Vec<PlanNode> {
    let mut out = Vec::new();
    for (key, value) in map {
        let Some(inner) = value.as_object() else {
            if key == "nested_loop" {
                out.extend(nested_loop(value, cx, id, costed));
            } else if key == "duplicates_removal" {
                let mut node = new_node(id, "Distinct", NodeKind::Aggregate);
                node.children = value
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_object)
                    .flat_map(|m| operations(m, cx, id, costed))
                    .collect();
                finish_wrapper(&mut node);
                out.push(node);
            }
            continue;
        };
        match key.as_str() {
            "table" => out.push(table_node(inner, None, cx, id, costed)),
            "read_sorted_file" => out.extend(operations(inner, cx, id, costed)),
            "filesort" => out.push(wrapper("Sort", NodeKind::Sort, inner, cx, id, costed)),
            "temporary_table" => out.push(wrapper("Temporary table", NodeKind::Other, inner, cx, id, costed)),
            "duplicates_removal" => out.push(wrapper("Distinct", NodeKind::Aggregate, inner, cx, id, costed)),
            "window_functions_computation" => out.push(wrapper("Window", NodeKind::Compute, inner, cx, id, costed)),
            _ => {}
        }
    }
    out
}

fn nested_loop(value: &Value, cx: &Context, id: &mut u32, costed: bool) -> Vec<PlanNode> {
    let mut children = Vec::new();
    for item in value.as_array().into_iter().flatten().filter_map(Value::as_object) {
        // A join buffer sits beside the table it joins: the table is read once per buffer fill, not once per row.
        let buffer = item.get("block-nl-join").and_then(Value::as_object);
        match (item.get("table").and_then(Value::as_object), buffer) {
            (Some(table), buffer) => children.push(table_node(table, buffer, cx, id, costed)),
            (None, Some(buffer)) => {
                if let Some(table) = buffer.get("table").and_then(Value::as_object) {
                    children.push(table_node(table, Some(buffer), cx, id, costed));
                }
            }
            (None, None) => children.extend(operations(item, cx, id, costed)),
        }
    }
    // Before MariaDB 11 there is no `loops`: each table runs once per row the tables before it produce, except a table
    // behind a join buffer, which is read once per buffer fill (the plan doesn't say how many fills; count one).
    let mut prefix: f64 = 1.0;
    let mut joined_rows = None;
    for child in children.iter_mut() {
        if let Some(rows) = child.props.get("rows").and_then(|r| r.parse::<f64>().ok())
            && !child.props.contains_key("loops")
        {
            let buffered = child.props.get("join_buffer_join_type").is_some_and(|j| !j.starts_with("BKA"));
            let per_row = prefix.max(1.0);
            child.est_executions = if buffered { 1.0 } else { per_row };
            // Rows read over every run, unless ANALYZE counted them.
            if child.act_rows.is_none() {
                child.rows_read = Some(rows * child.est_executions);
            }
            if !costed {
                let inner: f64 = child.children.iter().map(|c| c.subtree_cost).sum();
                child.subtree_cost = rows * child.est_executions + inner;
            }
            // What the join has produced after this table, buffered or not.
            prefix = per_row * child.est_rows_total() / child.est_executions.max(1.0);
            joined_rows = Some(prefix);
        } else {
            prefix = child.est_rows_total();
            joined_rows = None;
        }
    }
    // One table needs no join around it.
    if children.len() <= 1 {
        return children;
    }
    let mut join = new_node(id, "Nested loop", NodeKind::Join);
    join.children = children;
    finish_wrapper(&mut join);
    join.est_rows = joined_rows.unwrap_or_else(|| join.children.last().map(|c| c.est_rows_total()).unwrap_or(0.0));
    join.act_rows = join.children.last().and_then(|c| c.act_rows);
    vec![join]
}

fn wrapper(op: &str, kind: NodeKind, map: &Map<String, Value>, cx: &Context, id: &mut u32, costed: bool) -> PlanNode {
    let mut node = new_node(id, op, kind);
    if let Some(key) = text(map, "sort_key") {
        node.detail = Some(unquote(&key));
    }
    node.children = operations(map, cx, id, costed);
    finish_wrapper(&mut node);
    measured(map, &mut node);
    node.act_time_ms = number_field(map, "r_total_time_ms");
    if let Some(rows) = number_field(map, "r_output_rows") {
        node.act_rows = Some(rows);
    } else if node.act_executions.is_some() || node.children.iter().any(|c| c.act_rows.is_some()) {
        node.act_rows = node.children.first().and_then(|c| c.act_rows);
    }
    if let Some(limit) = number_field(map, "r_limit") {
        node.est_rows = node.est_rows.min(limit.max(1.0));
    }
    // A sort that needed merge passes didn't fit sort_buffer_size and went through temporary files.
    node.facts_mut().sort_merge_passes = number_field(map, "r_sort_passes").and_then(culprit_core::plan_facts::count);
    if number_field(map, "r_sort_passes").is_some_and(|p| p > 0.0) {
        node.warnings.push(PlanWarning::Spill);
    }
    crate::plan_json::copy_node_facts(map, &mut node);
    node
}

fn finish_wrapper(node: &mut PlanNode) {
    node.subtree_cost = node.children.iter().map(|c| c.subtree_cost).sum();
    node.est_rows = node.children.first().map(|c| c.est_rows_total()).unwrap_or(0.0);
}

fn union_result(map: &Map<String, Value>, cx: &Context, id: &mut u32, costed: bool) -> PlanNode {
    let mut node = new_node(id, "Union", NodeKind::Other);
    node.children = map
        .get("query_specifications")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|s| s.get("query_block").and_then(Value::as_object))
        .map(|block| query_block(block, cx, id, costed))
        .collect();
    node.subtree_cost = node.children.iter().map(|c| c.subtree_cost).sum();
    node.est_rows = node.children.iter().map(|c| c.est_rows).sum();
    measured(map, &mut node);
    node
}

/// A subquery; a cache in front of it (`expression_cache`, `subquery_cache`) means it depends on the outer row.
fn subquery(sub: &Value, cx: &Context, id: &mut u32, costed: bool) -> Option<PlanNode> {
    let (cache, block) = match sub.get("query_block") {
        Some(block) => (None, block),
        None => {
            let cache = sub.get("expression_cache").or_else(|| sub.get("subquery_cache"))?;
            (Some(cache), cache.get("query_block")?)
        }
    };
    let mut node = query_block(block.as_object()?, cx, id, costed);
    node.op = if cache.is_some() { "Dependent subquery" } else { "Subquery" }.into();
    if let Some(cache) = cache.and_then(Value::as_object) {
        node.facts_mut().dependent = Some(true);
        node.facts_mut().expression_cache = cache.contains_key("state");
        node.props.insert("dependent".into(), "true".into());
        for key in ["state", "r_loops", "r_hit_ratio"] {
            if let Some(v) = cache.get(key) {
                node.props.insert(format!("cache_{key}"), scalar_text(v));
            }
        }
    }
    Some(node)
}

fn table_node(
    table: &Map<String, Value>,
    buffer: Option<&Map<String, Value>>,
    cx: &Context,
    id: &mut u32,
    costed: bool,
) -> PlanNode {
    let raw_access = text(table, "access_type").unwrap_or_default().to_ascii_lowercase();
    // BNLH joins (hash joins) prefix the access type: hash_ALL, hash_range, hash_ref.
    let (access, hashed) = match raw_access.strip_prefix("hash_") {
        Some(rest) => (rest.to_string(), true),
        None => (raw_access.clone(), false),
    };
    let (op, kind) = access_op(&access, table);
    let mut node = new_node(id, op, kind);
    let name = text(table, "table_name").unwrap_or_default();
    node.object = cx.object(&name);
    let mut detail = Vec::new();
    if name.is_empty()
        && raw_access.is_empty()
        && let Some(message) = text(table, "message")
    {
        // "No tables used": the step is the message.
        node.op = message;
    } else if let Some(message) = text(table, "message") {
        detail.push(message);
    } else if node.object.is_none() && !name.is_empty() {
        detail.push(name.clone());
    }
    node.index = text(table, "key");
    if flag(table, "using_index") {
        detail.push("covering index".into());
    }
    if table.contains_key("index_condition") {
        detail.push("index condition pushdown".into());
    }
    if let Some(filter) = table.get("rowid_filter").and_then(Value::as_object)
        && let Some(key) = filter.get("range").and_then(|r| r.get("key")).and_then(Value::as_str)
    {
        detail.push(format!("rowid filter on {key}"));
    }
    if let Some(buffer) = buffer {
        let join = text(buffer, "join_type").unwrap_or_else(|| "BNL".into());
        detail.push(if hashed || join.contains('H') {
            "hash join (join buffer)".into()
        } else {
            format!("join buffer ({join})")
        });
    }
    if !detail.is_empty() {
        node.detail = Some(detail.join(", "));
    }
    let key_parts = strings(table, "used_key_parts");
    let refs = strings(table, "ref");
    node.seek_predicate = match (key_parts.is_empty(), refs.is_empty(), kind) {
        (false, false, _) => Some(format!("{} = {}", key_parts.join(", "), refs.join(", "))),
        (false, true, NodeKind::Seek) => Some(format!("range on {}", key_parts.join(", "))),
        _ => None,
    };
    let condition = text(table, "attached_condition")
        .or_else(|| buffer.and_then(|b| text(b, "attached_condition")))
        .or_else(|| text(table, "index_condition"));
    node.predicate = condition.map(|c| unquote(&c));

    // Estimates are per loop: rows read by one scan, and the percentage of them the condition keeps. Rows read count
    // every scan: one per loop (Batched Key Access looks each buffered row up), but a table behind a block nested loop
    // or hash join buffer is read once per buffer fill (the plan doesn't say how many fills; count one).
    let loops = number_field(table, "loops").unwrap_or(1.0).max(1.0);
    let read = number_field(table, "rows");
    let reported_filtered = number_field(table, "filtered");
    node.facts_mut().estimated_filtered_percent = reported_filtered.and_then(culprit_core::plan_facts::percentage);
    let filtered = reported_filtered.unwrap_or(100.0);
    let read_once = buffer.is_some_and(|b| !text(b, "join_type").is_some_and(|j| j.starts_with("BKA")));
    node.rows_read = read.map(|r| if read_once { r } else { r * loops });
    node.est_executions = loops;
    node.est_rows = read.map(|r| r * filtered / 100.0).unwrap_or(0.0);
    // A fixed guess for a condition without statistics says nothing about how much the filter keeps.
    let conditions = node.predicate.as_deref().map_or(1, culprit_core::insights::condition_count);
    if kind == NodeKind::Scan
        && read.is_some_and(|r| r >= 1_000.0)
        && filtered < 10.0
        && !culprit_core::insights::mysql_guessed_filtered(filtered, conditions)
    {
        node.warnings.push(PlanWarning::ResidualPredicate);
    }

    let mut children = Vec::new();
    if let Some(block) = table.get("materialized").and_then(|m| m.get("query_block")).and_then(Value::as_object) {
        let mut child = query_block(block, cx, id, costed);
        child.op = "Materialized subquery".into();
        children.push(child);
    }
    // Without optimizer costs (before MariaDB 11.0), rows examined across all loops stand in for cost.
    let own = if costed { number_field(table, "cost").unwrap_or(0.0) } else { read.unwrap_or(0.0) * loops };
    node.subtree_cost = own + children.iter().map(|c| c.subtree_cost).sum::<f64>();
    node.children = children;

    // Measured: loops, rows read per loop, the share the condition kept, time spent in the table and on conditions.
    if let Some(r_loops) = number_field(table, "r_loops") {
        node.act_executions = Some(r_loops);
        let per_loop = number_field(table, "r_rows");
        let kept = number_field(table, "r_filtered").or_else(|| buffer.and_then(|b| number_field(b, "r_filtered")));
        if let Some(per_loop) = per_loop {
            let total = per_loop * r_loops;
            node.rows_read = Some(total);
            node.act_rows = Some(total * kept.unwrap_or(100.0) / 100.0);
            if kind == NodeKind::Scan
                && total >= 1_000.0
                && kept.is_some_and(|k| k < 10.0)
                && !node.warnings.contains(&PlanWarning::ResidualPredicate)
            {
                node.warnings.push(PlanWarning::ResidualPredicate);
            }
        } else if r_loops == 0.0 {
            // Never ran: nothing read, whatever was estimated.
            node.act_rows = Some(0.0);
            node.rows_read = Some(0.0);
        }
        let own_ms = ["r_table_time_ms", "r_other_time_ms"].iter().filter_map(|k| number_field(table, k)).sum::<f64>();
        let timed = table.contains_key("r_table_time_ms") || table.contains_key("r_other_time_ms");
        if timed {
            node.act_time_ms = Some(own_ms + node.children.iter().filter_map(|c| c.act_time_ms).sum::<f64>());
        }
        if let Some(stats) = table.get("r_engine_stats").and_then(Value::as_object) {
            let runtime = NodeRuntime {
                logical_reads: number_field(stats, "pages_accessed"),
                physical_reads: number_field(stats, "pages_read_count"),
                ..Default::default()
            };
            if !runtime.is_empty() {
                node.runtime = Some(runtime);
            }
        }
    }
    crate::plan_json::copy_node_facts(table, &mut node);
    if let Some(buffer) = buffer {
        node.facts_mut().join_buffer_join_type = text(buffer, "join_type");
        for key in ["join_type", "buffer_type", "buffer_size", "r_loops", "r_filtered"] {
            if let Some(v) = buffer.get(key) {
                node.props.insert(format!("join_buffer_{key}"), scalar_text(v));
            }
        }
    }
    node
}

/// Loops measured by ANALYZE on a wrapper or block.
fn measured(map: &Map<String, Value>, node: &mut PlanNode) {
    if let Some(loops) = number_field(map, "r_loops") {
        node.act_executions = Some(loops);
    }
}

/// Wrappers MariaDB doesn't time (nested loops, temporary tables) take the time of their inputs, so every operator
/// in an actual plan has an inclusive time.
fn fill_times(node: &mut PlanNode) {
    node.children.iter_mut().for_each(fill_times);
    if node.act_time_ms.is_none() && !node.children.is_empty() && node.children.iter().all(|c| c.act_time_ms.is_some())
    {
        node.act_time_ms = Some(node.children.iter().filter_map(|c| c.act_time_ms).sum());
    }
}

/// Per-operator progress of a running statement from `SHOW ANALYZE`: tables that have started report rows so far
/// against the estimate for all their loops.
pub fn progress(plan: &Plan) -> Vec<culprit_core::NodeProgress> {
    use culprit_core::{NodeProgress, NodeState};
    plan.nodes()
        .into_iter()
        .filter(|n| n.props.contains_key("rows"))
        .filter_map(|n| {
            let loops = n.act_executions?;
            let state = if loops == 0.0 { NodeState::Pending } else { NodeState::Running };
            Some(NodeProgress {
                node_id: n.id,
                state,
                rows: n.rows_read.filter(|_| loops > 0.0).unwrap_or(0.0),
                est_rows: n.props.get("rows").and_then(|r| r.parse::<f64>().ok()).unwrap_or(0.0) * n.est_executions,
                elapsed_ms: n.act_time_ms,
                cpu_ms: None,
                logical_reads: n.runtime.as_ref().and_then(|r| r.logical_reads),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_query_block_keeps_its_own_rows() {
        // ANALYZE of `SELECT COUNT(*) FROM orders WHERE …`: the scan examined 125K rows; the query returned one row,
        // which MariaDB doesn't report for the block. The root must not claim the scan's rows.
        let json = r#"{"query_block": {"select_id": 1, "r_loops": 1, "r_total_time_ms": 52.1,
            "table": {"table_name": "orders", "access_type": "ALL", "r_loops": 1, "rows": 226000, "r_rows": 125000,
                      "r_total_time_ms": 50.2, "filtered": 100, "r_filtered": 40, "attached_condition": "orders.status = 'x'"}}}"#;
        let plan =
            crate::plan_json::parse(json, PlanKind::Actual, Some("SELECT COUNT(*) FROM orders"), Some("shop")).unwrap();
        assert_eq!(plan.root.act_rows, None, "{:?}", plan.root);
        assert_eq!(plan.root.est_rows, 226000.0);
    }

    const ANALYZE_SORT: &str = r#"{
      "query_optimization": {"r_total_time_ms": 0.13},
      "query_block": {
        "select_id": 1, "r_loops": 1, "r_total_time_ms": 34.26,
        "nested_loop": [{
          "read_sorted_file": {
            "r_rows": 0,
            "filesort": {
              "sort_key": "o.total desc", "r_loops": 1, "r_total_time_ms": 34.2, "r_limit": 5,
              "r_used_priority_queue": true, "r_output_rows": 0, "r_sort_mode": "sort_key,addon_fields",
              "table": {
                "table_name": "o", "access_type": "ALL", "r_loops": 1, "rows": 498315, "r_rows": 500000,
                "r_table_time_ms": 27.8, "r_other_time_ms": 6.3, "r_engine_stats": {"pages_accessed": 1723},
                "filtered": 100, "r_filtered": 0, "attached_condition": "o.promo_code = 'SPRING1'"
              }
            }
          }
        }]
      }
    }"#;

    const JOIN_11: &str = r#"{
      "query_block": {
        "select_id": 1, "cost": 0.0087,
        "nested_loop": [
          {"table": {"table_name": "oi", "access_type": "ref", "possible_keys": ["fk_items_order"], "key": "fk_items_order",
                     "key_length": "4", "used_key_parts": ["order_id"], "ref": ["const"], "loops": 1, "rows": 2,
                     "cost": 0.0053, "filtered": 100}},
          {"table": {"table_name": "p", "access_type": "eq_ref", "key": "PRIMARY", "used_key_parts": ["id"],
                     "ref": ["shop.oi.product_id"], "loops": 2, "rows": 1, "cost": 0.0034, "filtered": 100}},
          {"block-nl-join": {"table": {"table_name": "c", "access_type": "ALL", "loops": 2, "rows": 20000, "cost": 4.1,
                     "filtered": 100}, "buffer_type": "flat", "buffer_size": "256Kb", "join_type": "BNL",
                     "attached_condition": "c.id = oi.order_id"}}
        ],
        "subqueries": [{"expression_cache": {"state": "uninitialized",
          "query_block": {"select_id": 2, "cost": 0.0019, "nested_loop": [{"table": {"table_name": "o", "access_type": "ref",
            "key": "fk_orders_customer", "used_key_parts": ["customer_id"], "ref": ["shop.c.id"], "loops": 1, "rows": 2,
            "cost": 0.0019, "filtered": 100, "using_index": true}}]}}}]
      }
    }"#;

    #[test]
    fn mariadb_json_is_told_apart_from_mysql() {
        assert!(is_mariadb(&serde_json::from_str(ANALYZE_SORT).unwrap()));
        assert!(is_mariadb(&serde_json::from_str(JOIN_11).unwrap()));
        let mysql = r#"{"query_block": {"select_id": 1, "cost_info": {"query_cost": "1.0"},
            "table": {"table_name": "t", "access_type": "ALL", "rows_examined_per_scan": 10}}}"#;
        assert!(!is_mariadb(&serde_json::from_str(mysql).unwrap()));
    }

    #[test]
    fn analyze_measures_every_operator() {
        let sql = "SELECT o.id FROM orders o WHERE o.promo_code = 'SPRING1' ORDER BY o.total DESC LIMIT 5";
        let plan = crate::plan_json::parse(ANALYZE_SORT, PlanKind::Estimated, Some(sql), Some("shop")).unwrap();
        assert_eq!(plan.kind, PlanKind::Actual, "r_* members make it an actual plan");
        assert_eq!(plan.props.get("flavor").map(String::as_str), Some("MariaDB"));
        let sort = &plan.root.children[0];
        assert_eq!((sort.op.as_str(), sort.kind), ("Sort", NodeKind::Sort));
        assert_eq!(sort.detail.as_deref(), Some("o.total desc"));
        assert_eq!(sort.act_time_ms, Some(34.2));
        let scan = &sort.children[0];
        assert_eq!((scan.kind, scan.object.as_deref()), (NodeKind::Scan, Some("shop.orders")));
        assert_eq!(scan.rows_read, Some(500_000.0));
        assert_eq!(scan.act_rows, Some(0.0));
        assert!((scan.act_time_ms.unwrap() - 34.1).abs() < 1e-9);
        assert_eq!(scan.runtime.as_ref().and_then(|r| r.logical_reads), Some(1723.0));
        assert!(scan.warnings.contains(&PlanWarning::ResidualPredicate), "500K rows read to keep none");
        let elapsed: f64 = plan.props["ElapsedMs"].parse().unwrap();
        assert!((elapsed - 34.39).abs() < 1e-9, "execution plus optimization");
        let own = culprit_core::actual::self_times(&plan).expect("timed");
        assert!(own[&scan.id] > own[&sort.id], "the scan is where the time went");
    }

    #[test]
    fn joins_buffers_and_dependent_subqueries() {
        let sql = "SELECT oi.quantity, p.name FROM order_items oi JOIN products p ON p.id = oi.product_id, customers c";
        let plan = crate::plan_json::parse(JOIN_11, PlanKind::Estimated, Some(sql), Some("shop")).unwrap();
        assert_eq!(plan.kind, PlanKind::Estimated);
        let join = &plan.root.children[0];
        assert_eq!(join.kind, NodeKind::Join);
        assert_eq!(join.children.len(), 3);
        let lookup = &join.children[1];
        assert_eq!((lookup.op.as_str(), lookup.object.as_deref()), ("Unique index lookup", Some("shop.products")));
        assert_eq!(lookup.est_executions, 2.0);
        assert_eq!(lookup.seek_predicate.as_deref(), Some("id = shop.oi.product_id"));
        let buffered = &join.children[2];
        assert_eq!(buffered.detail.as_deref(), Some("join buffer (BNL)"));
        assert_eq!(buffered.predicate.as_deref(), Some("c.id = oi.order_id"));
        assert_eq!(plan.hottest().unwrap().id, buffered.id, "the buffered scan carries the cost");
        let sub = &plan.root.children[1];
        assert_eq!(sub.op, "Dependent subquery");
        assert_eq!(sub.children[0].detail.as_deref(), Some("covering index"));
    }

    #[test]
    fn running_statements_report_rows_so_far() {
        let json = r#"{"r_query_time_in_progress_ms": 2209, "query_block": {"select_id": 1, "cost": 3.5, "r_loops": 1,
          "filesort": {"sort_key": "o.customer_id", "r_loops": null, "temporary_table": {"nested_loop": [
            {"table": {"table_name": "o", "access_type": "range", "key": "PRIMARY", "used_key_parts": ["id"], "loops": 1,
                       "r_loops": 1, "rows": 1499, "r_rows": 367, "cost": 0.31, "filtered": 100, "r_filtered": 100}},
            {"table": {"table_name": "c", "access_type": "eq_ref", "key": "PRIMARY", "used_key_parts": ["id"],
                       "ref": ["shop.o.customer_id"], "loops": 1499, "r_loops": 367, "rows": 1, "r_rows": 1, "cost": 2.2,
                       "filtered": 100, "r_filtered": 100}}]}}}}"#;
        let plan = crate::plan_json::parse(json, PlanKind::Actual, None, Some("shop")).unwrap();
        assert_eq!(plan.props.get("ElapsedMs").map(String::as_str), Some("2209"));
        let nodes = progress(&plan);
        assert_eq!(nodes.len(), 2);
        assert_eq!((nodes[0].rows, nodes[0].est_rows), (367.0, 1499.0));
        assert_eq!((nodes[1].rows, nodes[1].est_rows), (367.0, 1499.0));
    }

    #[test]
    fn misestimates_compare_rows_per_loop_with_the_loops_that_ran() {
        // The lookup expected one row a loop and got one; only the outer table returned far more rows than expected, so
        // it ran 50K loops where 10 were planned. That is the outer table's misestimate, not the lookup's.
        let json = r#"{"query_block": {"select_id": 1, "r_loops": 1, "r_total_time_ms": 300, "nested_loop": [
            {"table": {"table_name": "o", "access_type": "ALL", "loops": 1, "r_loops": 1, "rows": 10, "r_rows": 50000,
                       "filtered": 100, "r_filtered": 100}},
            {"table": {"table_name": "c", "access_type": "eq_ref", "key": "PRIMARY", "used_key_parts": ["id"],
                       "ref": ["shop.o.customer_id"], "loops": 10, "r_loops": 50000, "rows": 1, "r_rows": 1,
                       "filtered": 100, "r_filtered": 100}}]}}"#;
        let plan = crate::plan_json::parse(json, PlanKind::Actual, None, Some("shop")).unwrap();
        let join = &plan.root.children[0];
        assert!(join.children[0].warnings.contains(&PlanWarning::Misestimate));
        assert!(!join.children[1].warnings.contains(&PlanWarning::Misestimate));
    }

    #[test]
    fn plans_without_costs_use_rows() {
        let json = r#"{"query_block": {"select_id": 1, "filesort": {"sort_key": "sum(orders.total) desc",
          "temporary_table": {"nested_loop": [{"table": {"table_name": "orders", "access_type": "ALL", "rows": 498315,
          "filtered": 100}}]}}}}"#;
        let plan = crate::plan_json::parse(json, PlanKind::Estimated, None, Some("shop")).unwrap();
        assert!(plan.props.contains_key("cost_basis"));
        assert_eq!(plan.total_cost, 498_315.0);
        let temp = &plan.root.children[0].children[0];
        assert_eq!(temp.op, "Temporary table");
        assert_eq!(temp.children[0].object.as_deref(), Some("shop.orders"));
    }

    /// MariaDB 10.5's EXPLAIN for a three-table join, as the server wrote it: one `table` member per table, repeated,
    /// with a join buffer between them.
    const JOIN_10_5: &str = r#"{
  "query_block": {
    "select_id": 1,
    "table": {"table_name": "o", "access_type": "range", "possible_keys": ["PRIMARY"], "key": "PRIMARY",
              "key_length": "4", "used_key_parts": ["id"], "rows": 4, "filtered": 100, "attached_condition": "o.`id` < 5"},
    "block-nl-join": {
      "table": {"table_name": "p", "access_type": "ALL", "rows": 19658, "filtered": 100},
      "buffer_type": "flat", "buffer_size": "1Kb", "join_type": "BNL", "attached_condition": "p.category = o.`status`"
    },
    "table": {"table_name": "c", "access_type": "ref", "possible_keys": ["ix_customers_region"],
              "key": "ix_customers_region", "key_length": "66", "used_key_parts": ["region"], "ref": ["shop.o.channel"],
              "rows": 24870, "filtered": 100}
  }
}"#;

    #[test]
    fn mariadb_10_joins_keep_every_table() {
        let sql = "SELECT o.id FROM orders o JOIN customers c ON c.region = o.channel JOIN products p ON p.category = o.status";
        let plan = crate::plan_json::parse(JOIN_10_5, PlanKind::Estimated, Some(sql), Some("shop")).unwrap();
        let join = &plan.root.children[0];
        assert_eq!(join.op, "Nested loop");
        let tables: Vec<&str> = join.children.iter().filter_map(|c| c.object.as_deref()).collect();
        assert_eq!(tables, ["shop.orders", "shop.products", "shop.customers"]);
        // The buffered table is read once per buffer fill, not once per order; the lookup after it runs once per row
        // the join has produced so far.
        assert_eq!(join.children[1].est_executions, 1.0);
        assert_eq!(join.children[2].est_executions, 4.0 * 19_658.0);
        assert_eq!(join.est_rows, 4.0 * 19_658.0 * 24_870.0);
    }

    #[test]
    fn rows_read_count_every_loop_and_a_buffered_table_once() {
        let join_of = |json: &str| {
            let plan = crate::plan_json::parse(json, PlanKind::Estimated, None, Some("shop")).unwrap();
            plan.root.children[0].children.clone()
        };
        // MariaDB 11: two product lookups of one row; the customers read once into the join buffer.
        let tables = join_of(JOIN_11);
        assert_eq!((tables[1].rows_read, tables[2].rows_read), (Some(2.0), Some(20_000.0)));
        // Before 11: the lookup runs once per row joined so far, the buffered table once.
        let tables = join_of(JOIN_10_5);
        assert_eq!(tables[1].rows_read, Some(19_658.0));
        assert_eq!(tables[2].rows_read, Some(24_870.0 * 4.0 * 19_658.0));

        // ANALYZE: the outer table found nothing, so the scan of customers never ran and read nothing.
        let json = r#"{"query_block": {"select_id": 1, "r_loops": 1, "r_total_time_ms": 3, "nested_loop": [
            {"table": {"table_name": "o", "access_type": "ALL", "loops": 1, "r_loops": 1, "rows": 10, "r_rows": 0,
                       "filtered": 100, "r_filtered": 100}},
            {"table": {"table_name": "c", "access_type": "ALL", "loops": 10, "r_loops": 0, "rows": 50000,
                       "r_rows": null, "filtered": 100, "r_filtered": null}}]}}"#;
        let never = &join_of(json)[1];
        assert_eq!((never.act_rows, never.rows_read), (Some(0.0), Some(0.0)));
    }
}
