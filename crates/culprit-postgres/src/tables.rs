//! Table sizes for plans. PostgreSQL's estimated plans say how many rows a scan expects to return, not how many it
//! reads: a sequential scan reads the whole table. The planner's own statistics (`pg_class.reltuples`) say how big
//! that is, so a scan expecting five rows of a two-million-row table reads as what it is.
//!
//! Two scans in a generic plan don't read what they show, and get no rows read: a partition under an Append (a generic
//! plan lists every partition; the ones the parameters rule out are pruned only when it runs), and a scan whose filter
//! compares a column with a parameter by a range or a pattern (planned with a default selectivity, where the real value
//! may well have used an index).

use std::collections::HashMap;

use culprit_core::{NodeKind, Plan, PlanNode};
use tokio_postgres::Client;

use crate::rows::tagged;

const SIZES_SQL: &str =
    "SELECT n.nspname || '.' || c.relname AS qualified, c.relname AS name, c.reltuples::float8 AS rows,
       c.relispartition AS partition
  FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
 WHERE c.relkind IN ('r', 'm', 'p')
   AND ((n.nspname || '.' || c.relname) = ANY($1) OR (c.relname = ANY($1) AND pg_table_is_visible(c.oid)))";

/// What the catalog says about a scanned table.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Size {
    rows: f64,
    /// A partition of a partitioned table.
    partition: bool,
}

/// Fill `table_rows` on the plan's scans, and `rows_read` on sequential scans an estimated plan left without one.
/// Best effort: a failed lookup leaves the plan as it was.
pub async fn fill(client: &Client, plan: &mut Plan) {
    let mut names: Vec<String> = plan
        .nodes()
        .into_iter()
        .filter(|n| matches!(n.kind, NodeKind::Scan | NodeKind::Seek))
        .filter_map(|n| n.object.clone())
        .collect();
    names.sort();
    names.dedup();
    if names.is_empty() {
        return;
    }
    // The names are bound as one text[] parameter, never spliced into the SQL.
    let Ok(rows) = client.query(tagged(SIZES_SQL).as_str(), &[&names]).await else { return };
    let mut sizes: HashMap<String, Size> = HashMap::new();
    for row in rows {
        let (Ok(qualified), Ok(name), Ok(count)) =
            (row.try_get::<_, String>(0), row.try_get::<_, String>(1), row.try_get::<_, f64>(2))
        else {
            continue;
        };
        let partition = row.try_get::<_, bool>(3).unwrap_or(false);
        // -1 means never analyzed: no estimate to give.
        if count >= 0.0 {
            let size = Size { rows: count, partition };
            sizes.insert(qualified, size);
            sizes.entry(name).or_insert(size);
        }
    }
    apply(&mut plan.root, &sizes, false);
}

/// Said on a partition's scan under an Append in a generic plan, instead of rows read.
const UNPRUNED: &str = "Listed because the plan has no parameter values: partitions are pruned only when it runs, so this one may never be read.";
/// Said on a scan whose filter compares a column with a parameter by a range or a pattern, instead of rows read.
const GUESSED: &str = "Planned with a default selectivity for its parameter: with a real value the planner may well use an index instead.";

fn apply(node: &mut PlanNode, sizes: &HashMap<String, Size>, under_append: bool) {
    if let Some(size) = node.object.as_ref().and_then(|o| sizes.get(o)).copied() {
        node.table_rows = Some(size.rows);
        if node.kind == NodeKind::Scan
            && node.op.contains("Seq Scan")
            && node.rows_read.is_none()
            && node.act_rows.is_none()
        {
            if under_append && size.partition {
                node.props.insert("Partition Pruning".into(), UNPRUNED.into());
            } else if node.predicate.as_deref().is_some_and(guessed_selectivity) {
                node.props.insert("Selectivity".into(), GUESSED.into());
            } else {
                node.rows_read = Some(size.rows);
            }
        }
    }
    let append = under_append || node.op.ends_with("Append");
    for child in node.children.iter_mut() {
        apply(child, sizes, append);
    }
}

/// Operators whose estimate for an unknown value is a fixed default (a third for a range, a fraction of a percent for
/// a pattern), not the column's statistics: `col > $1`, `col ~~ $1` (LIKE), `col ~ $1`.
const DEFAULT_SELECTIVITY: &[&str] =
    &["<", ">", "<=", ">=", "~~", "~~*", "!~~", "!~~*", "~", "~*", "!~", "!~*", "@>", "<@", "&&"];

/// Whether a filter compares a column with a generic plan's parameter (`$1`) by a range or a pattern.
pub fn guessed_selectivity(filter: &str) -> bool {
    let bytes = filter.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'$' || !bytes.get(i + 1).is_some_and(u8::is_ascii_digit) {
            i += 1;
            continue;
        }
        let end = i + 1 + bytes[i + 1..].iter().take_while(|b| b.is_ascii_digit()).count();
        let before = operator_before(&filter[..i]);
        let after = operator_after(&filter[end..]);
        if [before, after].iter().flatten().any(|op| DEFAULT_SELECTIVITY.contains(&op.as_str())) {
            return true;
        }
        i = end;
    }
    false
}

fn is_operator_char(c: char) -> bool {
    "<>=~!*@&|#%^-+/".contains(c)
}

/// The operator right before a parameter: `created_at > ($1)` → `>`; `= ANY ($1)` → `=`.
fn operator_before(text: &str) -> Option<String> {
    let mut rest = text.trim_end_matches(|c: char| c.is_whitespace() || c == '(');
    for word in ["ANY", "ALL", "any", "all"] {
        if let Some(stripped) = rest.strip_suffix(word) {
            rest = stripped.trim_end_matches(|c: char| c.is_whitespace() || c == '(');
        }
    }
    let op: String =
        rest.chars().rev().take_while(|c| is_operator_char(*c)).collect::<Vec<_>>().into_iter().rev().collect();
    (!op.is_empty()).then_some(op)
}

/// The operator right after a parameter: `($1)::date < created_at` → `<`.
fn operator_after(text: &str) -> Option<String> {
    let mut rest = text.trim_start_matches(|c: char| c.is_whitespace() || c == ')');
    while let Some(cast) = rest.strip_prefix("::") {
        let type_end = cast.find(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '"')).unwrap_or(cast.len());
        rest = cast[type_end..].trim_start_matches(|c: char| c.is_whitespace() || c == ')');
    }
    let op: String = rest.chars().take_while(|c| is_operator_char(*c)).collect();
    (!op.is_empty()).then_some(op)
}

#[cfg(test)]
mod tests {
    use super::*;
    use culprit_core::PlanKind;

    fn sizes(entries: &[(&str, f64, bool)]) -> HashMap<String, Size> {
        entries
            .iter()
            .map(|(n, rows, partition)| (n.to_string(), Size { rows: *rows, partition: *partition }))
            .collect()
    }

    #[test]
    fn a_sequential_scan_reads_the_whole_table() {
        let json = r#"[{"Plan": {"Node Type": "Seq Scan", "Relation Name": "orders", "Schema": "public",
            "Plan Rows": 5, "Total Cost": 4500.0, "Filter": "(customer_id = $1)"}}]"#;
        let mut plan = crate::plan_json::parse(json, PlanKind::Generic, Some("SELECT 1")).unwrap();
        apply(&mut plan.root, &sizes(&[("public.orders", 2_000_000.0, false)]), false);
        assert_eq!(plan.root.table_rows, Some(2_000_000.0));
        assert_eq!(plan.root.rows_read, Some(2_000_000.0));
        // And the plan's insights now see a scan that throws almost everything away.
        let found = culprit_core::insights::insights(&plan);
        assert!(
            found.iter().any(|i| i.rule == "scan-filter"),
            "{:?}",
            found.iter().map(|i| &i.rule).collect::<Vec<_>>()
        );
    }

    #[test]
    fn partitions_a_generic_plan_lists_are_not_read_in_full() {
        // A generic plan can't prune partitions: it lists every one under the Append, though only the partition the
        // parameter picks is read when it runs.
        let json = r#"[{"Plan": {"Node Type": "Append", "Plan Rows": 10, "Total Cost": 9000.0, "Plans": [
            {"Node Type": "Seq Scan", "Relation Name": "events_2026_09", "Schema": "public", "Plan Rows": 5,
             "Total Cost": 4500.0, "Filter": "(tenant_id = $1)"},
            {"Node Type": "Seq Scan", "Relation Name": "events_2026_10", "Schema": "public", "Plan Rows": 5,
             "Total Cost": 4500.0, "Filter": "(tenant_id = $1)"}]}}]"#;
        let mut plan = crate::plan_json::parse(json, PlanKind::Generic, Some("SELECT 1")).unwrap();
        let known =
            sizes(&[("public.events_2026_09", 2_000_000.0, true), ("public.events_2026_10", 1_500_000.0, true)]);
        apply(&mut plan.root, &known, false);
        for scan in &plan.root.children {
            assert!(scan.table_rows.is_some());
            assert_eq!(scan.rows_read, None);
            assert!(scan.props.contains_key("Partition Pruning"));
        }
        let found = culprit_core::insights::insights(&plan);
        assert!(
            !found.iter().any(|i| i.rule == "scan-filter"),
            "{:?}",
            found.iter().map(|i| &i.rule).collect::<Vec<_>>()
        );

        // A UNION ALL of two plain tables is an Append too; its scans do read their tables.
        let mut plan = crate::plan_json::parse(json, PlanKind::Generic, Some("SELECT 1")).unwrap();
        let tables =
            sizes(&[("public.events_2026_09", 2_000_000.0, false), ("public.events_2026_10", 1_500_000.0, false)]);
        apply(&mut plan.root, &tables, false);
        assert_eq!(plan.root.children[0].rows_read, Some(2_000_000.0));
    }

    #[test]
    fn a_range_on_a_parameter_is_a_guess_in_a_generic_plan() {
        let json = r#"[{"Plan": {"Node Type": "Seq Scan", "Relation Name": "orders", "Schema": "public",
            "Plan Rows": 5, "Total Cost": 4500.0, "Filter": "(created_at > ($1)::timestamp with time zone)"}}]"#;
        let mut plan = crate::plan_json::parse(json, PlanKind::Generic, Some("SELECT 1")).unwrap();
        apply(&mut plan.root, &sizes(&[("public.orders", 2_000_000.0, false)]), false);
        assert_eq!(plan.root.rows_read, None);
        assert!(plan.root.props.contains_key("Selectivity"));
    }

    #[test]
    fn which_filters_on_parameters_are_guesses() {
        assert!(guessed_selectivity("(created_at > $1)"));
        assert!(guessed_selectivity("((name)::text ~~ $2)"));
        assert!(guessed_selectivity("(($1)::date <= created_at)"));
        assert!(guessed_selectivity("((status = $1) AND (amount >= $2))"));
        assert!(!guessed_selectivity("(customer_id = $1)"));
        assert!(!guessed_selectivity("(status = ANY ($1))"));
        assert!(!guessed_selectivity("(status <> $1)"));
        assert!(!guessed_selectivity("(created_at > now())"));
        assert!(!guessed_selectivity("(price > 10)"));
    }
}
