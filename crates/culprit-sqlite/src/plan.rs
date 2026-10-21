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

/// The plan of a statement, never run. A statement with placeholders (`?`, as logs and fingerprints write it) is planned
/// with NULL in each: EXPLAIN QUERY PLAN still runs nothing, and the plan says it had no values.
pub fn explain(conn: &Connection, statement: &str) -> Result<Plan> {
    let mut stmt = conn.prepare(&format!("EXPLAIN QUERY PLAN {statement}")).map_err(|e| map_error(&e))?;
    let parameters = stmt.parameter_count();
    let nulls = std::iter::repeat_n(rusqlite::types::Null, parameters);
    let rows: Vec<(i64, i64, String)> = stmt
        .query_map(rusqlite::params_from_iter(nulls), |r| Ok((r.get(0)?, r.get(1)?, r.get(3)?)))
        .map_err(|e| map_error(&e))?
        .collect::<rusqlite::Result<_>>()
        .map_err(|e| map_error(&e))?;
    let mut plan = plan_from_rows(statement, &rows);
    if parameters > 0 {
        let reason = "Planned without parameter values: with statistics that hold value distributions, the chosen index can depend on them.";
        plan.facts_mut().generic_plan_reason = Some(reason.into());
        plan.props.insert("Generic Plan".into(), reason.into());
    }
    Ok(plan)
}

/// Whether text is `EXPLAIN QUERY PLAN` output as the sqlite3 shell prints it: the tree under a `QUERY PLAN` line
/// (`|--SCAN orders`, `` `--USE TEMP B-TREE FOR ORDER BY ``), or the rows as `id|parent|notused|detail`.
pub fn is_text_plan(text: &str) -> bool {
    let mut lines = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .skip_while(|l| l.replace(' ', "") == "id|parent|notused|detail");
    let Some(first) = lines.next() else { return false };
    if first == "QUERY PLAN" {
        // psql heads a PostgreSQL plan with the same line and a border under it; the shell goes straight to its steps.
        return lines.next().is_some_and(|l| l.starts_with("|--") || l.starts_with("`--"));
    }
    list_row(first).is_some()
}

/// A pasted plan: the shell's tree or its rows. The statement isn't in the text, so a table keeps the alias the
/// statement gave it.
pub fn parse_text(text: &str) -> Result<Plan> {
    let lines: Vec<&str> = text.lines().map(str::trim_end).filter(|l| !l.trim().is_empty()).collect();
    let rows: Vec<(i64, i64, String)> = if lines.first().is_some_and(|l| l.trim() == "QUERY PLAN") {
        // Each level indents three columns ("|  " or "   ") before its "|--" or "`--".
        let mut parents: Vec<i64> = vec![0];
        let mut rows = Vec::new();
        for (n, line) in lines[1..].iter().enumerate() {
            let Some(at) = line.find("|--").into_iter().chain(line.find("`--")).min() else {
                return Err(culprit_core::Error::invalid(format!("Line {} isn't a plan step: {line}", n + 2)));
            };
            let depth = at / 3;
            let id = n as i64 + 1;
            parents.truncate(depth + 1);
            let parent = *parents.last().unwrap_or(&0);
            rows.push((id, parent, line[at + 3..].trim().to_string()));
            parents.push(id);
        }
        rows
    } else {
        lines.iter().filter_map(|l| list_row(l)).collect()
    };
    if rows.is_empty() {
        return Err(culprit_core::Error::invalid("The plan has no steps"));
    }
    let mut plan = plan_from_rows("", &rows);
    plan.statement = None;
    Ok(plan)
}

/// `2|0|0|SCAN orders`: a row of `EXPLAIN QUERY PLAN` in the shell's list mode.
fn list_row(line: &str) -> Option<(i64, i64, String)> {
    let mut parts = line.splitn(4, '|');
    let id = parts.next()?.trim().parse().ok()?;
    let parent = parts.next()?.trim().parse().ok()?;
    parts.next()?.trim().parse::<i64>().ok()?;
    let detail = parts.next()?.trim();
    (!detail.is_empty()).then(|| (id, parent, detail.to_string()))
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
        facts: Some(Default::default()),
        view: None,
        engine: Engine::Sqlite,
        kind: PlanKind::Estimated,
        statement: Some(statement.to_string()),
        total_cost: 0.0,
        root,
        warnings: Vec::new(),
        index_hints: Vec::new(),
        props: Default::default(),
        raw: Some(raw.into()),
        insights: None,
    }
}

fn resolve_aliases(n: &mut PlanNode, aliases: &[(String, String)]) {
    if let Some(object) = n.object.clone()
        && let Some((_, table)) = aliases.iter().find(|(alias, _)| alias.eq_ignore_ascii_case(&object))
    {
        n.facts_mut().table_alias = Some(object.clone());
        if let Some(built) = &mut n.facts_mut().automatic_index {
            built.table = table.clone();
        }
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
        facts: Some(Default::default()),
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
            n.facts_mut().automatic_index = culprit_core::PlanAutomaticIndex::from_description(on);
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

/// `(customer_id=?) LEFT-JOIN` → what's inside the parentheses (`customer_id=?`) and what follows them (`LEFT-JOIN`,
/// a flag SQLite adds to the step of an outer join).
fn parenthesized(text: &str) -> (Option<String>, &str) {
    let Some(open) = text.find('(') else { return (None, text.trim()) };
    let mut depth = 0;
    for (at, ch) in text[open..].char_indices() {
        match ch {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    let close = open + at;
                    return (Some(text[open + 1..close].trim().to_string()), text[close + 1..].trim());
                }
            }
            _ => {}
        }
    }
    (Some(text[open + 1..].trim().to_string()), "")
}

/// `LEFT-JOIN` after a search: the step is the inner side of an outer join.
fn join_flag(n: &mut PlanNode, after: &str) {
    if let Some(kind) =
        after.split_whitespace().find_map(|w| w.to_ascii_uppercase().strip_suffix("-JOIN").map(str::to_string))
    {
        n.props.insert("Join".into(), kind.to_ascii_lowercase());
    }
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
        let (by, after) = parenthesized(using);
        join_flag(n, after);
        let columns: Vec<String> = by
            .as_deref()
            .unwrap_or("")
            .split(" AND ")
            .filter_map(|c| {
                let name: String = c.trim().chars().take_while(|ch| ch.is_alphanumeric() || *ch == '_').collect();
                (!name.is_empty()).then_some(name)
            })
            .collect();
        n.facts_mut().automatic_index = Some(culprit_core::PlanAutomaticIndex {
            table: n.object.clone().unwrap_or_default(),
            columns: columns.clone(),
        });
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
            let name = tail.find('(').map_or(tail, |at| tail[..at].trim());
            let (by, after) = parenthesized(tail);
            join_flag(n, after);
            n.index = Some(if name.is_empty() { prefix.trim().to_ascii_lowercase() } else { name.to_string() });
            if upper.starts_with("COVERING") {
                n.props.insert("Covering".into(), "true".into());
            }
            n.seek_predicate = by;
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

    #[test]
    fn automatic_index_advice_uses_typed_keys_after_the_raw_description_is_removed() {
        let mut plan = plan_from_rows(
            "SELECT * FROM orders o JOIN items i ON i.order_id = o.id",
            &rows(&[(1, 0, "SCAN o"), (2, 0, "SEARCH i USING AUTOMATIC COVERING INDEX (order_id=?)")]),
        );
        let expected = culprit_core::insights::engine_built_indexes(&plan);
        assert_eq!(expected.len(), 1);
        assert_eq!(plan.root.children[1].facts().automatic_index.as_ref().unwrap().table, "items");
        assert_eq!(
            (expected[0].table.as_str(), expected[0].keys.as_slice()),
            ("items", ["order_id".to_string()].as_slice())
        );
        for node in &mut plan.root.children {
            node.props.clear();
        }
        assert_eq!(culprit_core::insights::engine_built_indexes(&plan), expected);
    }

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
    fn an_outer_join_flag_isnt_part_of_the_search() {
        let plan = plan_from_rows(
            "SELECT …",
            &rows(&[
                (3, 0, "SCAN customers"),
                (7, 0, "SEARCH orders USING AUTOMATIC COVERING INDEX (customer_id=?) LEFT-JOIN"),
                (9, 0, "SEARCH items USING INDEX ix_items (order_id=? AND product_id>?) LEFT-JOIN"),
            ]),
        );
        let kids = &plan.root.children;
        assert_eq!(kids[1].seek_predicate.as_deref(), Some("customer_id=?"));
        assert_eq!(kids[1].props.get("Join").map(String::as_str), Some("left"));
        assert_eq!(kids[1].props.get(AUTOMATIC_INDEX).map(String::as_str), Some("orders(customer_id)"));
        assert_eq!(kids[2].seek_predicate.as_deref(), Some("order_id=? AND product_id>?"));
        assert_eq!(kids[2].index.as_deref(), Some("ix_items"));
    }

    #[test]
    fn the_shell_tree_and_list_rows_read_back() {
        let tree = "QUERY PLAN\n|--SCAN c\n`--CORRELATED SCALAR SUBQUERY 1\n   |--SEARCH o USING INDEX ix_orders_customer (customer_id=?)\n   `--USE TEMP B-TREE FOR ORDER BY\n";
        assert!(is_text_plan(tree));
        let plan = parse_text(tree).unwrap();
        let kids = &plan.root.children;
        assert_eq!(kids.len(), 2);
        assert_eq!((kids[0].kind, kids[0].object.as_deref()), (NodeKind::Scan, Some("c")));
        let sub = &kids[1].children;
        assert_eq!(sub.len(), 2, "{:?}", kids[1]);
        assert_eq!(sub[0].index.as_deref(), Some("ix_orders_customer"));
        assert_eq!(sub[1].kind, NodeKind::Sort);
        assert_eq!(plan.statement, None);
        let list = "2|0|0|SCAN orders\n14|0|0|USE TEMP B-TREE FOR ORDER BY";
        assert!(is_text_plan(list));
        assert_eq!(parse_text(list).unwrap().root.children.len(), 2);
        // SQL Server's SHOWPLAN_TEXT isn't SQLite's.
        assert!(!is_text_plan("  |--Clustered Index Scan(OBJECT:([db].[dbo].[t].[pk]))"));
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
