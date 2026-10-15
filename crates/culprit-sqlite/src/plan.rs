//! `EXPLAIN QUERY PLAN` as the shared plan tree. SQLite reports each step as a line of text under a parent:
//! `SCAN orders`, `SEARCH orders USING INDEX ix_orders_customer (customer_id=?)`, `USE TEMP B-TREE FOR ORDER BY`,
//! `AUTOMATIC COVERING INDEX ON t(x)`. Before 3.36 a table was `SCAN TABLE orders`. There are no rows, costs or
//! timings: what a step does is all the plan says.

use culprit_core::{Engine, NodeKind, Plan, PlanKind, PlanNode, Result};
use rusqlite::Connection;

use crate::map_error;

/// Properties the plan insights read on a SQLite step.
pub const AUTOMATIC_INDEX: &str = "Automatic Index";
pub const TEMP_BTREE: &str = "Temp B-Tree";

pub fn explain(conn: &Connection, statement: &str) -> Result<Plan> {
    let mut stmt = conn.prepare(&format!("EXPLAIN QUERY PLAN {statement}")).map_err(|e| map_error(&e))?;
    let rows: Vec<(i64, i64, String)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(3)?)))
        .map_err(|e| map_error(&e))?
        .collect::<rusqlite::Result<_>>()
        .map_err(|e| map_error(&e))?;
    Ok(plan_from_rows(statement, &rows))
}

/// The plan for `EXPLAIN QUERY PLAN` rows (`id`, `parent`, `detail`), under a root that stands for the statement.
pub fn plan_from_rows(statement: &str, rows: &[(i64, i64, String)]) -> Plan {
    let raw = rows.iter().map(|(_, _, d)| d.as_str()).collect::<Vec<_>>().join("\n");
    let mut root = node(0, "QUERY PLAN", NodeKind::Other);
    root.children = children(0, rows);
    // SQLite names a table by the alias the statement gave it (`SCAN o`): put the table back, keep the alias.
    let aliases = culprit_core::sql::table_aliases(statement, Engine::Sqlite);
    resolve_aliases(&mut root, &aliases);
    Plan {
        engine: Engine::Sqlite,
        kind: PlanKind::Estimated,
        statement: Some(statement.to_string()),
        total_cost: 0.0,
        root,
        warnings: Vec::new(),
        index_hints: Vec::new(),
        props: Default::default(),
        raw: Some(raw),
        insights: None,
    }
}

fn resolve_aliases(n: &mut PlanNode, aliases: &[(String, String)]) {
    if let Some(object) = n.object.clone()
        && let Some((_, table)) = aliases.iter().find(|(alias, _)| alias.eq_ignore_ascii_case(&object))
    {
        n.props.insert("Alias".into(), object);
        n.object = Some(table.clone());
    }
    for child in &mut n.children {
        resolve_aliases(child, aliases);
    }
}

fn children(parent: i64, rows: &[(i64, i64, String)]) -> Vec<PlanNode> {
    rows.iter()
        .filter(|(_, p, _)| *p == parent)
        .map(|(id, _, detail)| {
            let mut n = step(*id, detail);
            n.children = children(*id, rows);
            n
        })
        .collect()
}

fn node(id: i64, op: &str, kind: NodeKind) -> PlanNode {
    PlanNode {
        id: id.max(0) as u32,
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
        props: Default::default(),
        children: Vec::new(),
    }
}

/// One `EXPLAIN QUERY PLAN` line as a step.
fn step(id: i64, detail: &str) -> PlanNode {
    let text = detail.trim();
    let upper = text.to_ascii_uppercase();
    let mut n = if let Some(rest) = strip_word(text, &upper, "SCAN") {
        let mut n = node(id, "Scan", NodeKind::Scan);
        access(&mut n, rest);
        n
    } else if let Some(rest) = strip_word(text, &upper, "SEARCH") {
        let mut n = node(id, "Search", NodeKind::Seek);
        access(&mut n, rest);
        n
    } else if upper.starts_with("USE TEMP B-TREE") {
        let mut n = node(id, "Temp B-Tree", NodeKind::Sort);
        n.props.insert(TEMP_BTREE.into(), text["USE TEMP B-TREE".len()..].trim().to_string());
        n
    } else if let Some(rest) = strip_word(text, &upper, "BLOOM FILTER ON") {
        // `BLOOM FILTER ON i (product_id=?)`: a quick membership test before each probe of the table.
        let mut n = node(id, "Bloom Filter", NodeKind::Other);
        n.object = rest.split_whitespace().next().map(str::to_string);
        n
    } else if upper.starts_with("AUTOMATIC") {
        // SQLite built a throwaway index for this statement because none fits: `AUTOMATIC COVERING INDEX ON t(x)`.
        let mut n = node(id, "Automatic Index", NodeKind::Other);
        if let Some(at) = upper.find(" ON ") {
            let on = text[at + 4..].trim();
            n.object = on.split('(').next().map(|t| t.trim().to_string());
            n.props.insert(AUTOMATIC_INDEX.into(), on.to_string());
        }
        n
    } else {
        node(id, &title(text), NodeKind::Other)
    };
    n.detail = Some(text.to_string());
    n
}

/// `rest` after a leading keyword (`SCAN`), past the `TABLE` older versions write.
fn strip_word<'a>(text: &'a str, upper: &str, word: &str) -> Option<&'a str> {
    let after = upper.strip_prefix(word)?;
    if !after.starts_with(' ') {
        return None;
    }
    let mut rest = text[word.len()..].trim_start();
    if rest.to_ascii_uppercase().starts_with("TABLE ") {
        rest = rest[6..].trim_start();
    }
    Some(rest)
}

/// `orders USING INDEX ix (customer_id=?)` → the table, the index and what it searches by.
fn access(n: &mut PlanNode, rest: &str) {
    let upper = rest.to_ascii_uppercase();
    let (table, using) = match upper.find(" USING ") {
        Some(at) => (rest[..at].trim(), Some(rest[at + 7..].trim())),
        None => (rest.trim(), None),
    };
    // `orders AS o`: the table, not its alias.
    n.object = Some(table.split_whitespace().next().unwrap_or(table).to_string());
    let Some(using) = using else { return };
    let upper = using.to_ascii_uppercase();
    // `USING AUTOMATIC COVERING INDEX (product_id=?)`: SQLite built this index for the statement because none fits.
    if upper.starts_with("AUTOMATIC") {
        let by = using.find('(').map(|at| using[at..].trim().trim_start_matches('(').trim_end_matches(')').to_string());
        let columns: Vec<String> = by
            .as_deref()
            .unwrap_or("")
            .split(" AND ")
            .filter_map(|c| {
                let name: String = c.trim().chars().take_while(|ch| ch.is_alphanumeric() || *ch == '_').collect();
                (!name.is_empty()).then_some(name)
            })
            .collect();
        n.index = Some("automatic index".into());
        n.props.insert(
            AUTOMATIC_INDEX.into(),
            format!("{}({})", n.object.clone().unwrap_or_default(), columns.join(", ")),
        );
        n.seek_predicate = by;
        return;
    }
    for prefix in ["COVERING INDEX ", "INDEX ", "INTEGER PRIMARY KEY", "PRIMARY KEY"] {
        if upper.starts_with(prefix) {
            let tail = using[prefix.len()..].trim();
            let (name, by) = match tail.find('(') {
                Some(at) => (tail[..at].trim(), Some(tail[at..].trim())),
                None => (tail, None),
            };
            n.index = Some(if name.is_empty() { prefix.trim().to_ascii_lowercase() } else { name.to_string() });
            if upper.starts_with("COVERING") {
                n.props.insert("Covering".into(), "true".into());
            }
            n.seek_predicate = by.map(|b| b.trim_start_matches('(').trim_end_matches(')').to_string());
            return;
        }
    }
}

fn title(text: &str) -> String {
    let words: Vec<&str> = text.split_whitespace().take(3).collect();
    let mut s = words.join(" ").to_ascii_lowercase();
    if let Some(first) = s.get_mut(0..1) {
        first.make_ascii_uppercase();
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(lines: &[(i64, i64, &str)]) -> Vec<(i64, i64, String)> {
        lines.iter().map(|(i, p, d)| (*i, *p, d.to_string())).collect()
    }

    #[test]
    fn steps_become_the_shared_tree() {
        let plan = plan_from_rows(
            "SELECT …",
            &rows(&[
                (2, 0, "SCAN orders"),
                (5, 0, "SEARCH customers USING INTEGER PRIMARY KEY (rowid=?)"),
                (9, 0, "SEARCH order_items USING INDEX ix_items_order (order_id=?)"),
                (14, 0, "USE TEMP B-TREE FOR ORDER BY"),
            ]),
        );
        let kids = &plan.root.children;
        assert_eq!(kids.len(), 4);
        assert_eq!((kids[0].kind, kids[0].object.as_deref()), (NodeKind::Scan, Some("orders")));
        assert_eq!((kids[1].kind, kids[1].index.as_deref()), (NodeKind::Seek, Some("integer primary key")));
        assert_eq!(kids[2].index.as_deref(), Some("ix_items_order"));
        assert_eq!(kids[2].seek_predicate.as_deref(), Some("order_id=?"));
        assert_eq!(kids[3].kind, NodeKind::Sort);
    }

    #[test]
    fn older_wording_and_automatic_indexes() {
        let plan = plan_from_rows(
            "SELECT …",
            &rows(&[(3, 0, "SCAN TABLE events AS e"), (7, 0, "AUTOMATIC COVERING INDEX ON visits(event_id)")]),
        );
        assert_eq!(plan.root.children[0].object.as_deref(), Some("events"));
        let auto = &plan.root.children[1];
        assert_eq!(auto.object.as_deref(), Some("visits"));
        assert_eq!(auto.props.get(AUTOMATIC_INDEX).map(String::as_str), Some("visits(event_id)"));
    }

    #[test]
    fn an_automatic_index_inside_a_search_is_found() {
        let plan = plan_from_rows(
            "SELECT o.id FROM orders o JOIN order_items i ON i.product_id = o.customer_id",
            &rows(&[
                (2, 0, "SCAN o"),
                (4, 0, "BLOOM FILTER ON i (product_id=?)"),
                (7, 0, "SEARCH i USING AUTOMATIC COVERING INDEX (product_id=?)"),
            ]),
        );
        let bloom = &plan.root.children[1];
        assert_eq!((bloom.op.as_str(), bloom.object.as_deref()), ("Bloom Filter", Some("order_items")));
        let search = &plan.root.children[2];
        assert_eq!(search.object.as_deref(), Some("order_items"));
        assert_eq!(search.props.get(AUTOMATIC_INDEX).map(String::as_str), Some("i(product_id)"));
    }

    #[test]
    fn a_step_names_the_table_not_its_alias() {
        let plan = plan_from_rows(
            "SELECT o.id FROM orders AS o JOIN customers c ON c.id = o.customer_id WHERE o.customer_id = 42",
            &rows(&[(2, 0, "SEARCH c USING INTEGER PRIMARY KEY (rowid=?)"), (5, 0, "SCAN o")]),
        );
        assert_eq!(plan.root.children[0].object.as_deref(), Some("customers"));
        assert_eq!(plan.root.children[1].object.as_deref(), Some("orders"));
        assert_eq!(plan.root.children[1].props.get("Alias").map(String::as_str), Some("o"));
    }

    #[test]
    fn nested_steps_keep_their_parent() {
        let plan = plan_from_rows(
            "SELECT …",
            &rows(&[
                (2, 0, "SCAN a"),
                (4, 0, "CORRELATED SCALAR SUBQUERY 1"),
                (6, 4, "SEARCH b USING INDEX ix_b (a_id=?)"),
            ]),
        );
        assert_eq!(plan.root.children[1].children[0].object.as_deref(), Some("b"));
    }
}
