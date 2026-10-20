//! `EXPLAIN FORMAT=JSON` (version 1) into Culprit's plan model. MariaDB's JSON, which differs in shape, goes to
//! [`crate::plan_maria`].

use std::collections::HashMap;

use culprit_core::{Engine, Error, NodeKind, Plan, PlanKind, PlanNode, PlanWarning, Properties, Result};
use serde_json::{Map, Value};

/// Operations that wrap other operations, in the order MySQL nests them.
const OPERATIONS: [&str; 7] = [
    "ordering_operation",
    "grouping_operation",
    "duplicates_removal",
    "windowing",
    "buffer_result",
    "nested_loop",
    "table",
];

/// Parse EXPLAIN JSON. `schema` is the session's default database, used for unqualified tables; `statement` lets table
/// aliases resolve back to table names.
pub fn parse(json: &str, kind: PlanKind, statement: Option<&str>, schema: Option<&str>) -> Result<Plan> {
    // MariaDB before 11 repeats `table` members for a join; read them all (MySQL's JSON is unaffected).
    let value: Value = crate::json_members::parse(json)
        .map_err(|e| Error::internal(format!("The server returned a plan Culprit cannot read: {e}")))?;
    if crate::plan_maria::is_mariadb(&value) {
        return crate::plan_maria::parse(&value, json, kind, statement, schema);
    }
    let block = value
        .get("query_block")
        .and_then(Value::as_object)
        .ok_or_else(|| Error::internal("MySQL plan has no query_block"))?;
    let context = Context::new(statement, schema);
    let mut next_id = 0;
    let root = query_block(block, &context, &mut next_id);

    let mut props = Properties::new();
    if let Some(cost) = block.get("cost_info").and_then(|c| c.get("query_cost")) {
        props.insert("query_cost".into(), scalar_text(cost));
    }
    if let Some(message) = block.get("message").and_then(Value::as_str) {
        props.insert("message".into(), message.into());
    }
    let mut plan = Plan {
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

/// Plan props a scan adds to the plan it reads: the server that made it, where its sample statement came from, and the
/// row count of the sample's LIMIT.
const SCAN_PROPS: &[&str] = &["server_version", "flavor", "Sample", culprit_core::insights::LIMIT_ROWS];

/// A plan a scan saved, read again from its text with this parser (EXPLAIN JSON, or the tree of EXPLAIN ANALYZE), so a
/// scan saved by an older Culprit reads the way one taken today would. The scan kept the plan's JSON with the sample's
/// values replaced, and the plan read from it is redacted the same way; what the scan said of the plan stays. `None`
/// when the plan kept no text, or this parser can't read it.
pub fn reparse(saved: &Plan, schema: Option<&str>) -> Option<Plan> {
    let raw = saved.raw.as_deref()?;
    let text = raw.trim_start();
    let plan = if text.starts_with('{') || text.starts_with('[') {
        parse(raw, saved.kind, saved.statement.as_deref(), schema)
    } else if text.starts_with("->") {
        crate::plan_tree::parse(raw, saved.statement.as_deref(), schema)
    } else {
        return None;
    }
    .ok()?;
    let mut clean = culprit_core::redact::redact_plan(&plan);
    clean.raw = saved.raw.clone();
    for key in SCAN_PROPS {
        if let Some(value) = saved.props.get(*key) {
            clean.props.insert(key.to_string(), value.clone());
        }
    }
    Some(clean)
}

pub(crate) struct Context {
    pub aliases: HashMap<String, (Option<String>, String)>,
    pub schema: Option<String>,
}

impl Context {
    pub fn new(statement: Option<&str>, schema: Option<&str>) -> Context {
        Context { aliases: statement.map(aliases).unwrap_or_default(), schema: schema.map(str::to_string) }
    }

    /// `o` → `shop.orders`, `orders` → `shop.orders`; internal tables (`<derived2>`, `<temporary>`) have no object.
    pub fn object(&self, table_name: &str) -> Option<String> {
        // `<derived2>`, `<subquery3>` are steps, not tables; no name ("No tables used") is no table at all.
        if table_name.is_empty() || table_name.starts_with('<') {
            return None;
        }
        let (schema, table) = match self.aliases.get(&table_name.to_ascii_lowercase()) {
            Some((schema, table)) => (schema.clone().or_else(|| self.schema.clone()), table.clone()),
            None => (self.schema.clone(), table_name.to_string()),
        };
        Some(match schema {
            Some(schema) => format!("{schema}.{table}"),
            None => table,
        })
    }
}

pub(crate) fn new_node(id: &mut u32, op: &str, kind: NodeKind) -> PlanNode {
    let node = PlanNode {
        id: *id,
        op: op.to_string(),
        kind,
        detail: None,
        object: None,
        index: None,
        seek_predicate: None,
        predicate: None,
        est_rows: 0.0,
        est_executions: 1.0,
        act_rows: None,
        act_executions: None,
        rows_read: None,
        table_rows: None,
        subtree_cost: 0.0,
        self_cost: 0.0,
        act_time_ms: None,
        runtime: None,
        warnings: Vec::new(),
        props: Properties::new(),
        children: Vec::new(),
    };
    *id += 1;
    node
}

fn query_block(block: &Map<String, Value>, cx: &Context, id: &mut u32) -> PlanNode {
    let mut root = new_node(id, "Query block", NodeKind::Other);
    if let Some(select_id) = block.get("select_id") {
        root.props.insert("select_id".into(), scalar_text(select_id));
    }
    if let Some(message) = block.get("message").and_then(Value::as_str) {
        root.detail = Some(message.to_string());
    }
    root.children = operations(block, cx, id);
    if let Some(union) = block.get("union_result").and_then(Value::as_object) {
        root.children.push(union_result(union, cx, id));
    }
    let children: f64 = root.children.iter().map(|c| c.subtree_cost).sum();
    let query_cost = block.get("cost_info").and_then(|c| number(c, "query_cost")).unwrap_or(0.0);
    root.subtree_cost = query_cost.max(children);
    root.est_rows = root.children.last().map(|c| c.est_rows).unwrap_or(0.0);
    // Scalar subqueries in the SELECT list; DEPENDENT and not cacheable ones re-run for every row.
    for sub in block.get("select_list_subqueries").and_then(Value::as_array).into_iter().flatten() {
        if let Some(inner) = sub.get("query_block").and_then(Value::as_object) {
            let mut child = query_block(inner, cx, id);
            for flag in ["dependent", "cacheable"] {
                if let Some(v) = sub.get(flag) {
                    child.props.insert(flag.into(), scalar_text(v));
                }
            }
            child.op = subquery_label(sub).into();
            root.subtree_cost = root.subtree_cost.max(children + child.subtree_cost);
            root.children.push(child);
        }
    }
    root
}

/// What a subquery block is called in the diagram: whether it re-runs per row is the thing to know about it.
fn subquery_label(sub: &Value) -> &'static str {
    if sub.get("dependent").and_then(Value::as_bool) == Some(true) { "Dependent subquery" } else { "Subquery" }
}

/// The operations directly inside `map`: at most one of the wrappers, which in turn hold the next level.
fn operations(map: &Map<String, Value>, cx: &Context, id: &mut u32) -> Vec<PlanNode> {
    let mut out = Vec::new();
    for key in OPERATIONS {
        let Some(value) = map.get(key) else { continue };
        match key {
            "table" => {
                if let Some(table) = value.as_object() {
                    out.push(table_node(table, cx, id));
                }
            }
            "nested_loop" => out.push(nested_loop(value, cx, id)),
            _ => {
                if let Some(inner) = value.as_object() {
                    out.push(wrapper(key, inner, cx, id));
                }
            }
        }
    }
    out
}

fn wrapper(key: &str, map: &Map<String, Value>, cx: &Context, id: &mut u32) -> PlanNode {
    let filesort = flag(map, "using_filesort");
    let temporary = flag(map, "using_temporary_table");
    let (op, kind) = match key {
        "ordering_operation" if filesort => ("Sort", NodeKind::Sort),
        "ordering_operation" => ("Ordered read", NodeKind::Other),
        "grouping_operation" => ("Group", NodeKind::Aggregate),
        "duplicates_removal" => ("Distinct", NodeKind::Aggregate),
        "windowing" => ("Window", NodeKind::Compute),
        _ => ("Buffer result", NodeKind::Spool),
    };
    let mut node = new_node(id, op, kind);
    let mut detail = Vec::new();
    if filesort {
        detail.push("using filesort");
    }
    if temporary {
        // An internal temporary table, in memory unless it outgrows tmp_table_size; the workload's on-disk temp table
        // count says whether it actually spilled, so this is a detail rather than a spill warning.
        detail.push("using temporary table");
    }
    if !detail.is_empty() {
        node.detail = Some(detail.join(", "));
    }
    node.children = operations(map, cx, id);
    let sort_cost = map.get("cost_info").and_then(|c| number(c, "sort_cost")).unwrap_or(0.0);
    node.subtree_cost = sort_cost + node.children.iter().map(|c| c.subtree_cost).sum::<f64>();
    node.est_rows = node.children.last().map(|c| c.est_rows).unwrap_or(0.0);
    copy_scalars(map, &mut node.props);
    node
}

fn nested_loop(value: &Value, cx: &Context, id: &mut u32) -> PlanNode {
    let mut node = new_node(id, "Nested loop", NodeKind::Join);
    node.children = value
        .as_array()
        .map(|items| items.iter().filter_map(Value::as_object).flat_map(|item| operations(item, cx, id)).collect())
        .unwrap_or_default();
    // A table's rows examined are per scan of it, and its rows produced count the join up to it. Joined row by row (or
    // a batch of keys at a time, Batched Key Access), it is scanned once per row the tables before it produce; behind a
    // block nested loop or hash join buffer, once per buffer fill (the plan doesn't say how many fills; count one).
    let mut prefix = 1.0;
    for child in node.children.iter_mut() {
        let read_once = child.props.get("using_join_buffer").is_some_and(|b| !b.contains("Batched Key Access"));
        if prefix > 1.0 && !read_once && child.props.contains_key("rows_produced_per_join") {
            child.est_executions = prefix;
            child.est_rows /= prefix;
            child.rows_read = child.rows_read.map(|per_scan| per_scan * prefix);
        }
        prefix = child.est_rows_total();
    }
    node.subtree_cost = node.children.iter().map(|c| c.subtree_cost).sum();
    node.est_rows = node.children.last().map(|c| c.est_rows_total()).unwrap_or(0.0);
    node
}

fn union_result(map: &Map<String, Value>, cx: &Context, id: &mut u32) -> PlanNode {
    let mut node = new_node(id, "Union", NodeKind::Other);
    if flag(map, "using_temporary_table") {
        node.detail = Some("using temporary table".into());
    }
    node.children = map
        .get("query_specifications")
        .and_then(Value::as_array)
        .map(|specs| {
            specs
                .iter()
                .filter_map(|s| s.get("query_block").and_then(Value::as_object))
                .map(|block| query_block(block, cx, id))
                .collect()
        })
        .unwrap_or_default();
    node.subtree_cost = node.children.iter().map(|c| c.subtree_cost).sum();
    node
}

fn table_node(table: &Map<String, Value>, cx: &Context, id: &mut u32) -> PlanNode {
    let access = text(table, "access_type").unwrap_or_default().to_ascii_lowercase();
    let (op, kind) = access_op(&access, table);
    let mut node = new_node(id, op, kind);
    let name = text(table, "table_name").unwrap_or_default();
    node.object = cx.object(&name);
    if node.object.is_none() && !name.is_empty() {
        node.detail = Some(name.clone());
    }
    // A message where a table would be ("No tables used", "Impossible WHERE"): the step is the message.
    if name.is_empty()
        && access.is_empty()
        && let Some(message) = text(table, "message")
    {
        node.op = message;
    }
    node.index = text(table, "key");
    if flag(table, "using_index") {
        node.detail = Some(match node.detail.take() {
            Some(d) => format!("{d}, covering index"),
            None => "covering index".into(),
        });
    }
    let key_parts = strings(table, "used_key_parts");
    let refs = strings(table, "ref");
    node.seek_predicate = match (key_parts.is_empty(), refs.is_empty(), kind) {
        (false, false, _) => Some(format!("{} = {}", key_parts.join(", "), refs.join(", "))),
        (false, true, NodeKind::Seek) => Some(format!("range on {}", key_parts.join(", "))),
        _ => None,
    };
    node.predicate = text(table, "attached_condition").map(|c| unquote(&c));

    // Per scan, and the join's rows so far: `nested_loop` counts a joined table's scans.
    let examined = number_field(table, "rows_examined_per_scan");
    let produced = number_field(table, "rows_produced_per_join");
    node.rows_read = examined;
    node.est_rows = produced.or(examined).unwrap_or(0.0);
    let filtered = number_field(table, "filtered");
    // MySQL's fixed guesses for conditions it has no statistics on say nothing about how much the filter keeps.
    let conditions = node.predicate.as_deref().map_or(1, culprit_core::insights::condition_count);
    if kind == NodeKind::Scan
        && let (Some(examined), Some(filtered)) = (examined, filtered)
        && examined >= 1_000.0
        && filtered < 10.0
        && !culprit_core::insights::mysql_guessed_filtered(filtered, conditions)
    {
        node.warnings.push(PlanWarning::ResidualPredicate);
    }

    let own = table
        .get("cost_info")
        .map(|c| number(c, "read_cost").unwrap_or(0.0) + number(c, "eval_cost").unwrap_or(0.0))
        .unwrap_or(0.0);
    let mut children = Vec::new();
    if let Some(block) =
        table.get("materialized_from_subquery").and_then(|m| m.get("query_block")).and_then(Value::as_object)
    {
        children.push(query_block(block, cx, id));
    }
    for sub in table.get("attached_subqueries").and_then(Value::as_array).into_iter().flatten() {
        if let Some(block) = sub.get("query_block").and_then(Value::as_object) {
            let mut child = query_block(block, cx, id);
            // DEPENDENT and not cacheable: the subquery re-runs for every row of this table.
            for flag in ["dependent", "cacheable", "using_temporary_table"] {
                if let Some(v) = sub.get(flag) {
                    child.props.insert(flag.into(), scalar_text(v));
                }
            }
            child.op = subquery_label(sub).into();
            children.push(child);
        }
    }
    node.subtree_cost = own + children.iter().map(|c| c.subtree_cost).sum::<f64>();
    node.children = children;
    copy_scalars(table, &mut node.props);
    if let Some(cost) = table.get("cost_info").and_then(Value::as_object) {
        for (k, v) in cost {
            node.props.insert(k.clone(), scalar_text(v));
        }
    }
    node
}

pub(crate) fn access_op(access: &str, table: &Map<String, Value>) -> (&'static str, NodeKind) {
    match access {
        "all" => ("Table scan", NodeKind::Scan),
        "index" => ("Index scan", NodeKind::Scan),
        "range" => ("Index range scan", NodeKind::Seek),
        "ref" | "ref_or_null" => ("Index lookup", NodeKind::Seek),
        "eq_ref" => ("Unique index lookup", NodeKind::Seek),
        "const" | "system" => ("Constant row", NodeKind::Seek),
        "fulltext" => ("Full-text index lookup", NodeKind::Seek),
        "index_merge" => ("Index merge", NodeKind::Seek),
        "unique_subquery" | "index_subquery" => ("Subquery index lookup", NodeKind::Seek),
        _ if table.get("message").is_some() => ("Table", NodeKind::Other),
        _ => ("Table access", NodeKind::Other),
    }
}

/// MySQL quotes every identifier in conditions; `\`shop\`.\`orders\`.\`status\`` reads better (and parses) without them.
pub fn unquote(condition: &str) -> String {
    condition.replace('`', "")
}

pub(crate) fn copy_scalars(map: &Map<String, Value>, props: &mut Properties) {
    for (key, value) in map {
        if matches!(key.as_str(), "table_name" | "access_type" | "key" | "attached_condition" | "cost_info") {
            continue;
        }
        let text = match value {
            Value::String(s) => s.clone(),
            Value::Number(n) => n.to_string(),
            Value::Bool(b) => b.to_string(),
            Value::Array(items) if items.iter().all(Value::is_string) => {
                items.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(", ")
            }
            _ => continue,
        };
        props.insert(key.clone(), text);
    }
}

pub(crate) fn text(map: &Map<String, Value>, key: &str) -> Option<String> {
    map.get(key).and_then(Value::as_str).map(str::to_string)
}

pub(crate) fn flag(map: &Map<String, Value>, key: &str) -> bool {
    map.get(key).and_then(Value::as_bool).unwrap_or(false)
}

pub(crate) fn strings(map: &Map<String, Value>, key: &str) -> Vec<String> {
    map.get(key)
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(Value::as_str).map(unquote).collect())
        .unwrap_or_default()
}

/// MySQL prints most numbers as strings ("1234.50").
fn number(value: &Value, key: &str) -> Option<f64> {
    value.get(key).and_then(|v| v.as_f64().or_else(|| v.as_str().and_then(|s| s.parse().ok())))
}

pub(crate) fn number_field(map: &Map<String, Value>, key: &str) -> Option<f64> {
    map.get(key).and_then(|v| v.as_f64().or_else(|| v.as_str().and_then(|s| s.parse().ok())))
}

pub(crate) fn scalar_text(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

const NOT_ALIASES: [&str; 22] = [
    "where",
    "join",
    "inner",
    "left",
    "right",
    "cross",
    "straight_join",
    "natural",
    "on",
    "using",
    "group",
    "order",
    "limit",
    "having",
    "union",
    "for",
    "lock",
    "window",
    "set",
    "outer",
    "force",
    "use",
];

/// Table aliases from FROM/JOIN/UPDATE/INTO clauses: `orders o` → `o` → (None, "orders").
pub fn aliases(statement: &str) -> HashMap<String, (Option<String>, String)> {
    let tokens = tokenize(statement);
    let mut out = HashMap::new();
    let mut i = 0;
    while i < tokens.len() {
        let token = tokens[i].to_ascii_lowercase();
        let starts_table = matches!(token.as_str(), "from" | "join" | "update" | "into" | "straight_join")
            || (token == "," && in_from_list(&tokens, i));
        if !starts_table || i + 1 >= tokens.len() {
            i += 1;
            continue;
        }
        let name = tokens[i + 1].trim_matches('`').to_string();
        if name == "(" || name.is_empty() {
            i += 1;
            continue;
        }
        let (schema, table) = match name.split_once('.') {
            Some((s, t)) => (Some(s.trim_matches('`').to_string()), t.trim_matches('`').to_string()),
            None => (None, name.clone()),
        };
        out.insert(table.to_ascii_lowercase(), (schema.clone(), table.clone()));
        let mut j = i + 2;
        if tokens.get(j).is_some_and(|t| t.eq_ignore_ascii_case("as")) {
            j += 1;
        }
        if let Some(alias) = tokens.get(j) {
            let lower = alias.trim_matches('`').to_ascii_lowercase();
            let is_word = lower.chars().next().is_some_and(|c| c.is_alphabetic() || c == '_');
            if is_word && !NOT_ALIASES.contains(&lower.as_str()) {
                out.insert(lower, (schema, table));
            }
        }
        i = j;
    }
    out
}

/// A comma separates tables only between FROM and the next clause keyword.
fn in_from_list(tokens: &[String], comma: usize) -> bool {
    for t in tokens[..comma].iter().rev() {
        match t.to_ascii_lowercase().as_str() {
            "from" => return true,
            "select" | "where" | "group" | "order" | "having" | "set" | "values" | "(" | ")" => return false,
            _ => {}
        }
    }
    false
}

fn tokenize(sql: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut chars = sql.chars().peekable();
    let flush = |current: &mut String, out: &mut Vec<String>| {
        if !current.is_empty() {
            out.push(std::mem::take(current));
        }
    };
    while let Some(c) = chars.next() {
        match c {
            '\'' | '"' => {
                flush(&mut current, &mut out);
                for d in chars.by_ref() {
                    if d == c {
                        break;
                    }
                }
                out.push("''".into());
            }
            '`' => {
                current.push(c);
                for d in chars.by_ref() {
                    current.push(d);
                    if d == '`' {
                        break;
                    }
                }
            }
            c if c.is_whitespace() => flush(&mut current, &mut out),
            ',' | '(' | ')' | ';' | '=' => {
                flush(&mut current, &mut out);
                out.push(c.to_string());
            }
            _ => current.push(c),
        }
    }
    flush(&mut current, &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plan_without_tables_says_so() {
        let json = r#"{"query_block":{"select_id":1,"table":{"message":"No tables used"}}}"#;
        let plan = parse(json, PlanKind::Estimated, Some("SELECT SLEEP(1)"), Some("shop")).unwrap();
        let mut nodes = vec![&plan.root];
        let mut found = false;
        while let Some(n) = nodes.pop() {
            assert!(n.object.as_deref() != Some("shop."), "no empty table name");
            found |= n.op == "No tables used";
            nodes.extend(n.children.iter());
        }
        assert!(found, "{:?}", plan.root);
    }

    const SCAN: &str = r#"{
      "query_block": {
        "select_id": 1,
        "cost_info": {"query_cost": "50352.75"},
        "ordering_operation": {
          "using_filesort": true,
          "table": {
            "table_name": "o",
            "access_type": "ALL",
            "rows_examined_per_scan": 497280,
            "rows_produced_per_join": 4972,
            "filtered": "1.00",
            "cost_info": {"read_cost": "49855.47", "eval_cost": "497.28", "prefix_cost": "50352.75", "data_read_per_join": "3M"},
            "used_columns": ["id", "promo_code", "total"],
            "attached_condition": "(`shop`.`o`.`promo_code` = 'SPRING')"
          }
        }
      }
    }"#;

    const JOIN: &str = r#"{
      "query_block": {
        "select_id": 1,
        "cost_info": {"query_cost": "120.10"},
        "nested_loop": [
          {"table": {"table_name": "c", "access_type": "const", "key": "PRIMARY", "used_key_parts": ["id"], "ref": ["const"],
                     "rows_examined_per_scan": 1, "rows_produced_per_join": 1, "filtered": "100.00",
                     "cost_info": {"read_cost": "0.00", "eval_cost": "0.10", "prefix_cost": "0.00"}}},
          {"table": {"table_name": "orders", "access_type": "ref", "key": "fk_orders_customer", "used_key_parts": ["customer_id"],
                     "ref": ["const"], "rows_examined_per_scan": 12, "rows_produced_per_join": 12, "filtered": "100.00",
                     "using_index": true,
                     "cost_info": {"read_cost": "118.00", "eval_cost": "2.00", "prefix_cost": "120.10"}}}
        ]
      }
    }"#;

    #[test]
    fn parses_a_sorted_full_scan() {
        let sql = "SELECT o.id, o.total FROM orders o WHERE o.promo_code = 'SPRING' ORDER BY o.total DESC";
        let plan = parse(SCAN, PlanKind::Estimated, Some(sql), Some("shop")).unwrap();
        assert_eq!(plan.engine, Engine::Mysql);
        assert!((plan.total_cost - 50352.75).abs() < 0.01);
        let sort = &plan.root.children[0];
        assert_eq!(sort.op, "Sort");
        assert_eq!(sort.detail.as_deref(), Some("using filesort"));
        let scan = &sort.children[0];
        assert_eq!(scan.kind, NodeKind::Scan);
        assert_eq!(scan.object.as_deref(), Some("shop.orders"));
        assert_eq!(scan.predicate.as_deref(), Some("(shop.o.promo_code = 'SPRING')"));
        assert_eq!(scan.rows_read, Some(497_280.0));
        assert_eq!(scan.est_rows, 4972.0);
        assert!(scan.warnings.contains(&PlanWarning::ResidualPredicate));
        assert_eq!(plan.hottest().unwrap().id, scan.id);
    }

    #[test]
    fn guessed_filters_are_not_flagged() {
        // Two conditions without statistics: MySQL multiplies its 10% guesses into 1.00, which says nothing.
        let guessed = SCAN.replace(
            "(`shop`.`o`.`promo_code` = 'SPRING')",
            "((`shop`.`o`.`promo_code` = 'SPRING') and (`shop`.`o`.`channel` = 'web'))",
        );
        let plan = parse(&guessed, PlanKind::Estimated, None, Some("shop")).unwrap();
        assert!(!plan.root.children[0].children[0].warnings.contains(&PlanWarning::ResidualPredicate));
        // One condition can't make 1.00 from guesses: that share comes from statistics.
        let plan = parse(SCAN, PlanKind::Estimated, None, Some("shop")).unwrap();
        assert!(plan.root.children[0].children[0].warnings.contains(&PlanWarning::ResidualPredicate));
    }

    #[test]
    fn parses_a_join_with_lookups() {
        let sql = "SELECT * FROM customers c JOIN orders ON orders.customer_id = c.id WHERE c.id = 7";
        let plan = parse(JOIN, PlanKind::Estimated, Some(sql), Some("shop")).unwrap();
        let join = &plan.root.children[0];
        assert_eq!(join.kind, NodeKind::Join);
        assert_eq!(join.children.len(), 2);
        let lookup = &join.children[1];
        assert_eq!(lookup.kind, NodeKind::Seek);
        assert_eq!(lookup.object.as_deref(), Some("shop.orders"));
        assert_eq!(lookup.index.as_deref(), Some("fk_orders_customer"));
        assert_eq!(lookup.seek_predicate.as_deref(), Some("customer_id = const"));
        assert_eq!(lookup.detail.as_deref(), Some("covering index"));
        assert_eq!(join.children[0].object.as_deref(), Some("shop.customers"));
    }

    #[test]
    fn joined_tables_count_their_scans() {
        // Each order looks up its customer (1,000 lookups of one row); the items are read once into a hash join buffer.
        let json = r#"{"query_block": {"select_id": 1, "cost_info": {"query_cost": "9000.00"}, "nested_loop": [
          {"table": {"table_name": "o", "access_type": "ALL", "rows_examined_per_scan": 1000,
                     "rows_produced_per_join": 1000, "filtered": "100.00"}},
          {"table": {"table_name": "c", "access_type": "eq_ref", "key": "PRIMARY", "used_key_parts": ["id"],
                     "ref": ["shop.o.customer_id"], "rows_examined_per_scan": 1, "rows_produced_per_join": 1000,
                     "filtered": "100.00"}},
          {"table": {"table_name": "i", "access_type": "ALL", "rows_examined_per_scan": 5000,
                     "rows_produced_per_join": 500000, "filtered": "10.00", "using_join_buffer": "hash join",
                     "attached_condition": "(`shop`.`i`.`order_id` = `shop`.`o`.`id`)"}}
        ]}}"#;
        let plan = parse(json, PlanKind::Estimated, None, Some("shop")).unwrap();
        let join = &plan.root.children[0];
        let (lookup, buffered) = (&join.children[1], &join.children[2]);
        assert_eq!((lookup.est_executions, lookup.est_rows, lookup.rows_read), (1000.0, 1.0, Some(1000.0)));
        assert_eq!((buffered.est_executions, buffered.rows_read), (1.0, Some(5000.0)));
        assert_eq!(buffered.est_rows_total(), 500_000.0);
        assert_eq!(join.est_rows, 500_000.0);
    }

    #[test]
    fn resolves_aliases() {
        let a = aliases(
            "SELECT * FROM `shop`.`orders` AS o, customers c LEFT JOIN products p ON p.id = o.product_id WHERE x = 'from y'",
        );
        assert_eq!(a.get("o"), Some(&(Some("shop".into()), "orders".into())));
        assert_eq!(a.get("c"), Some(&(None, "customers".into())));
        assert_eq!(a.get("p"), Some(&(None, "products".into())));
        assert!(!a.contains_key("y"));
    }
}
