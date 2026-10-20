//! `EXPLAIN ANALYZE` output (the TREE format with measured times) into Culprit's plan model.
//!
//! ```text
//! -> Sort: o.total DESC  (cost=50352 rows=497280) (actual time=180..181 rows=12 loops=1)
//!     -> Filter: (o.promo_code = 'SPRING')  (cost=50352 rows=4973) (actual time=0.9..179 rows=12 loops=1)
//!         -> Table scan on o  (cost=50352 rows=497280) (actual time=0.07..141 rows=500000 loops=1)
//! ```
//! Estimates and measurements are per loop; Culprit's model wants totals for actual rows and time.

use std::collections::HashMap;

use culprit_core::{Engine, Error, NodeKind, Plan, PlanKind, PlanNode, PlanWarning, Properties, Result};

use crate::plan_json::{aliases, unquote};

struct Line {
    depth: usize,
    text: String,
}

pub fn parse(tree: &str, statement: Option<&str>, schema: Option<&str>) -> Result<Plan> {
    let lines: Vec<Line> = tree
        .lines()
        .filter_map(|line| {
            let trimmed = line.trim_start();
            let text = trimmed.strip_prefix("-> ")?;
            Some(Line { depth: (line.len() - trimmed.len()) / 4, text: text.trim_end().to_string() })
        })
        .collect();
    if lines.is_empty() {
        return Err(Error::internal("EXPLAIN ANALYZE returned no plan"));
    }
    let names = statement.map(aliases).unwrap_or_default();
    let mut next_id = 0;
    let mut index = 0;
    let mut root = build(&lines, &mut index, &names, schema, &mut next_id);
    // Several top-level lines (rare) hang under one synthetic root.
    while index < lines.len() {
        let sibling = build(&lines, &mut index, &names, schema, &mut next_id);
        if root.op != "Query" {
            let mut wrapper = blank(next_id, "Query", NodeKind::Other);
            next_id += 1;
            wrapper.children.push(root);
            root = wrapper;
        }
        root.children.push(sibling);
    }
    fill_missing_costs(&mut root);
    root.flag_misestimates();
    let mut props = Properties::new();
    // The root's time is the statement's: the same name every engine uses.
    if let Some(ms) = root.act_time_ms.or_else(|| root.children.iter().filter_map(|c| c.act_time_ms).reduce(f64::max)) {
        props.insert("ElapsedMs".into(), ms.to_string());
    }
    let mut plan = Plan {
        view: None,
        engine: Engine::Mysql,
        kind: PlanKind::Actual,
        statement: statement.map(str::to_string),
        total_cost: root.subtree_cost,
        root,
        warnings: Vec::new(),
        index_hints: Vec::new(),
        props,
        raw: Some(tree.to_string()),
        insights: None,
    };
    plan.compute_self_costs();
    Ok(plan)
}

fn build(
    lines: &[Line],
    index: &mut usize,
    names: &HashMap<String, (Option<String>, String)>,
    schema: Option<&str>,
    next_id: &mut u32,
) -> PlanNode {
    let line = &lines[*index];
    *index += 1;
    let mut node = node_from(&line.text, names, schema, *next_id);
    *next_id += 1;
    while *index < lines.len() && lines[*index].depth > line.depth {
        let child = build(lines, index, names, schema, next_id);
        node.children.push(child);
    }
    // MySQL puts the WHERE condition in a Filter above the scan; checks look for it on the scan itself.
    if node.op == "Filter"
        && let [child] = node.children.as_mut_slice()
        && child.kind == NodeKind::Scan
        && child.predicate.is_none()
    {
        child.predicate = node.predicate.clone();
        if let (Some(read), Some(kept)) = (child.act_rows, node.act_rows)
            && read >= 1_000.0
            && read >= 10.0 * kept.max(1.0)
        {
            child.warnings.push(PlanWarning::ResidualPredicate);
        }
    }
    node
}

fn blank(id: u32, op: &str, kind: NodeKind) -> PlanNode {
    PlanNode {
        id,
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
    }
}

fn node_from(text: &str, names: &HashMap<String, (Option<String>, String)>, schema: Option<&str>, id: u32) -> PlanNode {
    let (head, stats) = match text.find("  (") {
        Some(at) => (&text[..at], &text[at..]),
        None => (text, ""),
    };
    let (op, rest) = split_op(head);
    let mut node = blank(id, &op, kind_of(&op));
    node.props.insert("line".into(), head.to_string());

    match op.as_str() {
        "Filter" => node.predicate = rest.map(|r| unquote(r.trim_start_matches(": ").trim())),
        "Sort" | "Group aggregate" | "Limit" | "Aggregate" | "Stream results" => {
            node.detail = rest.map(|r| r.trim_start_matches(": ").trim().to_string()).filter(|d| !d.is_empty())
        }
        _ => {}
    }
    if let Some(rest) = rest
        && let Some(on) = rest.strip_prefix(" on ")
    {
        let mut parts = on.splitn(2, ' ');
        let alias = parts.next().unwrap_or_default().trim_matches('`');
        node.object = object(alias, names, schema);
        let tail = parts.next().unwrap_or_default();
        if let Some(using) = tail.strip_prefix("using ") {
            let (index, condition) = match using.split_once(' ') {
                Some((index, condition)) => (index, Some(condition.trim())),
                None => (using, None),
            };
            node.index = Some(index.trim_matches('`').to_string());
            node.seek_predicate = condition.map(|c| unquote(c.trim_start_matches("over ")));
        }
    }

    for group in stats.split(") (").map(|g| g.trim().trim_start_matches('(').trim_end_matches(')')) {
        let fields = fields(group);
        if group.starts_with("cost=") || group.starts_with("rows=") {
            node.subtree_cost = fields.get("cost").copied().unwrap_or(0.0);
            node.est_rows = fields.get("rows").copied().unwrap_or(0.0);
        } else if group.starts_with("actual time=") {
            let loops = fields.get("loops").copied().unwrap_or(1.0).max(1.0);
            node.act_executions = Some(loops);
            node.est_executions = loops;
            node.act_rows = fields.get("rows").map(|r| r * loops);
            node.act_time_ms = fields.get("time_end").map(|t| t * loops);
        } else if group.contains("never executed") {
            node.act_executions = Some(0.0);
            node.act_rows = Some(0.0);
        }
    }
    node
}

/// "Table scan on o" → ("Table scan", Some(" on o")); "Filter: (x = 1)" → ("Filter", Some(": (x = 1)")).
fn split_op(head: &str) -> (String, Option<&str>) {
    if let Some(at) = head.find(':') {
        let op = &head[..at];
        if !op.contains(" on ") {
            return (op.trim().to_string(), Some(&head[at..]));
        }
    }
    match head.find(" on ") {
        Some(at) => (head[..at].trim().to_string(), Some(&head[at..])),
        None => (head.trim().to_string(), None),
    }
}

fn object(alias: &str, names: &HashMap<String, (Option<String>, String)>, schema: Option<&str>) -> Option<String> {
    if alias.starts_with('<') || alias.is_empty() {
        return None;
    }
    let (table_schema, table) = match names.get(&alias.to_ascii_lowercase()) {
        Some((s, t)) => (s.clone().or_else(|| schema.map(str::to_string)), t.clone()),
        None => (schema.map(str::to_string), alias.to_string()),
    };
    Some(match table_schema {
        Some(s) => format!("{s}.{table}"),
        None => table,
    })
}

/// `cost=50352 rows=4973` or `actual time=0.9..179 rows=12 loops=1` as named numbers (`time` is split into start/end).
fn fields(group: &str) -> HashMap<&'static str, f64> {
    let mut out = HashMap::new();
    let group = group.trim_start_matches("actual ");
    for part in group.split_whitespace() {
        let Some((key, value)) = part.split_once('=') else { continue };
        match key {
            "cost" => insert(&mut out, "cost", value),
            "rows" => insert(&mut out, "rows", value),
            "loops" => insert(&mut out, "loops", value),
            "time" => {
                if let Some((start, end)) = value.split_once("..") {
                    insert(&mut out, "time_start", start);
                    insert(&mut out, "time_end", end);
                }
            }
            _ => {}
        }
    }
    out
}

fn insert(map: &mut HashMap<&'static str, f64>, key: &'static str, value: &str) {
    if let Ok(v) = value.trim_end_matches(',').parse::<f64>() {
        map.insert(key, v);
    }
}

fn kind_of(op: &str) -> NodeKind {
    let lower = op.to_ascii_lowercase();
    match lower.as_str() {
        "table scan" | "index scan" | "covering index scan" | "full-text index search" => NodeKind::Scan,
        l if l.contains("index lookup") || l.contains("index range scan") || l.contains("index skip scan") => {
            NodeKind::Seek
        }
        "constant row from" | "rows fetched before execution" => NodeKind::Seek,
        l if l.contains("join") || l.starts_with("nested loop") => NodeKind::Join,
        l if l.starts_with("sort") => NodeKind::Sort,
        l if l.contains("aggregate") || l.starts_with("group") || l.contains("deduplicate") => NodeKind::Aggregate,
        l if l.starts_with("materialize") || l.starts_with("temporary table") || l.starts_with("stream results") => {
            NodeKind::Spool
        }
        _ => NodeKind::Other,
    }
}

/// Lines without a cost (Limit, Stream results) cost what their children cost.
fn fill_missing_costs(node: &mut PlanNode) {
    node.children.iter_mut().for_each(fill_missing_costs);
    let children: f64 = node.children.iter().map(|c| c.subtree_cost).sum();
    if node.subtree_cost < children {
        node.subtree_cost = children;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TREE: &str = "-> Sort: o.total DESC  (cost=50352 rows=4973) (actual time=180..181 rows=12 loops=1)
    -> Filter: (o.promo_code = 'SPRING')  (cost=50352 rows=4973) (actual time=0.9..179 rows=12 loops=1)
        -> Table scan on o  (cost=50352 rows=497280) (actual time=0.07..141 rows=500000 loops=1)
";

    const LOOKUP: &str = "-> Nested loop inner join  (cost=12.5 rows=10) (actual time=0.1..0.4 rows=12 loops=1)
    -> Single-row index lookup on c using PRIMARY (id=7)  (cost=0.35 rows=1) (actual time=0.02..0.02 rows=1 loops=1)
    -> Index lookup on orders using fk_orders_customer (customer_id=7)  (cost=12.1 rows=10) (actual time=0.05..0.3 rows=12 loops=1)
";

    #[test]
    fn parses_measured_scan_under_filter() {
        let sql = "SELECT o.id FROM orders o WHERE o.promo_code = 'SPRING' ORDER BY o.total DESC";
        let plan = parse(TREE, Some(sql), Some("shop")).unwrap();
        assert_eq!(plan.kind, PlanKind::Actual);
        assert_eq!(plan.root.op, "Sort");
        assert_eq!(plan.root.detail.as_deref(), Some("o.total DESC"));
        let filter = &plan.root.children[0];
        assert_eq!(filter.op, "Filter");
        let scan = &filter.children[0];
        assert_eq!(scan.kind, NodeKind::Scan);
        assert_eq!(scan.object.as_deref(), Some("shop.orders"));
        assert_eq!(scan.predicate.as_deref(), Some("(o.promo_code = 'SPRING')"));
        assert_eq!(scan.act_rows, Some(500_000.0));
        assert_eq!(scan.act_time_ms, Some(141.0));
        assert!(scan.warnings.contains(&PlanWarning::ResidualPredicate));
    }

    #[test]
    fn misestimates_are_per_loop_and_need_a_run() {
        let tree = "-> Limit: 20 row(s)  (cost=20000 rows=20) (actual time=0.1..0.5 rows=20 loops=1)
    -> Nested loop inner join  (cost=20000 rows=200000) (actual time=0.1..0.5 rows=20 loops=1)
        -> Covering index scan on c using ix_customers_region  (cost=20000 rows=200000) (actual time=0.1..0.2 rows=20 loops=1)
        -> Index lookup on o using fk_orders_customer (customer_id=c.id)  (cost=0.6 rows=2.5) (actual time=0.01..0.01 rows=2.5 loops=20)
            -> Filter: (o.status = 'x')  (cost=0.6 rows=900) (never executed)
";
        let plan = parse(tree, None, Some("shop")).unwrap();
        // The scan stopped because the Limit had its 20 rows; the lookup returned 2.5 a loop, as expected per loop.
        assert!(plan.nodes().iter().all(|n| !n.warnings.contains(&PlanWarning::Misestimate)), "{:?}", plan.root);
        assert_eq!(plan.root.children[0].children[0].kind, NodeKind::Scan, "a covering index scan reads it all");
    }

    #[test]
    fn parses_index_lookups() {
        let sql = "SELECT * FROM customers c JOIN orders ON orders.customer_id = c.id WHERE c.id = 7";
        let plan = parse(LOOKUP, Some(sql), Some("shop")).unwrap();
        assert_eq!(plan.root.kind, NodeKind::Join);
        let first = &plan.root.children[0];
        assert_eq!(first.kind, NodeKind::Seek);
        assert_eq!(first.object.as_deref(), Some("shop.customers"));
        assert_eq!(first.index.as_deref(), Some("PRIMARY"));
        assert_eq!(first.seek_predicate.as_deref(), Some("(id=7)"));
        let second = &plan.root.children[1];
        assert_eq!(second.index.as_deref(), Some("fk_orders_customer"));
        assert_eq!(second.act_rows, Some(12.0));
    }
}
