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

/// The sizes a plan carries from its scan (`table_rows`, and which scans were marked as partitions a run may prune),
/// put back on the same plan read again by a newer parser ([`crate::plan_json::reparse`]): what [`fill`] looked up
/// then, worked out again without a connection.
pub(crate) fn refill(plan: &mut Plan, saved: &Plan) {
    let mut sizes: HashMap<String, Size> = HashMap::new();
    for n in saved.nodes() {
        if let (Some(object), Some(rows)) = (n.object.as_ref(), n.table_rows) {
            let partition = n.props.contains_key(culprit_core::NOT_READ_IN_FULL);
            let size = sizes.entry(object.clone()).or_insert(Size { rows, partition });
            size.partition |= partition;
        }
    }
    apply(&mut plan.root, &sizes, false);
}

/// Said on a partition's scan under an Append in a generic plan, instead of rows read.
const UNPRUNED: &str = "Listed because the plan has no parameter values: partitions are pruned only when it runs, so this one may never be read.";
/// Said on a scan whose filter compares a column with a parameter by a range, a pattern or ANY, instead of rows read.
const GUESSED: &str = "Planned with a default selectivity for its parameter (a fixed share of the rows for a range or a pattern, 10 values for ANY): with a real value the planner may well use an index instead.";
/// Said on any other step whose condition compares a column with a parameter that way.
const GUESSED_STEP: &str = "Planned with a default selectivity for its parameter (a fixed share of the rows for a range or a pattern, 10 values for ANY): the rows it expects, and the cost of the steps above it, rest on that guess, not on the column's statistics.";

fn apply(node: &mut PlanNode, sizes: &HashMap<String, Size>, under_append: bool) {
    // Rows a run measured aren't a guess, whatever the plan was planned with.
    let guessed = node.act_rows.is_none()
        && [node.predicate.as_deref(), node.seek_predicate.as_deref()].into_iter().flatten().any(guessed_selectivity);
    if let Some(size) = node.object.as_ref().and_then(|o| sizes.get(o)).copied() {
        node.table_rows = Some(size.rows);
        if node.kind == NodeKind::Scan
            && node.op.contains("Seq Scan")
            && node.rows_read.is_none()
            && node.act_rows.is_none()
        {
            if under_append && size.partition {
                node.props.insert(culprit_core::NOT_READ_IN_FULL.into(), UNPRUNED.into());
            } else if guessed {
                node.props.insert(culprit_core::GUESSED_SELECTIVITY.into(), GUESSED.into());
            } else {
                // A total over its runs: a scan on a nested loop's inner side reads the table once per outer row.
                node.rows_read = Some(size.rows * node.runs());
            }
        }
    }
    if guessed && !node.props.contains_key(culprit_core::GUESSED_SELECTIVITY) {
        node.props.insert(culprit_core::GUESSED_SELECTIVITY.into(), GUESSED_STEP.into());
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

/// Operators that combine a parameter with another value before any comparison sees it: `now() - $1`, `'%' || $1`.
const COMBINING: &[&str] = &["+", "-", "*", "/", "%", "||"];

/// Whether a filter compares a column with a generic plan's parameter (`$1`) by a range or a pattern, the parameter
/// perhaps inside arithmetic: `created_at > (now() - $1)` compares with a value the planner doesn't know either. Or
/// with every value of an array parameter (`= ANY ($1)`, `<> ALL ($1)`): the planner doesn't know how many it holds and
/// takes 10, so the rows it expects are that guess times a value's.
pub fn guessed_selectivity(filter: &str) -> bool {
    let bytes = filter.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'$' || !bytes.get(i + 1).is_some_and(u8::is_ascii_digit) {
            i += 1;
            continue;
        }
        let end = i + 1 + bytes[i + 1..].iter().take_while(|b| b.is_ascii_digit()).count();
        // The operators around the parameter and, while those combine it with something (PostgreSQL parenthesizes
        // every such expression), the operators around the parentheses that hold it.
        let (mut start, mut stop) = (i, end);
        loop {
            let around = [operator_before(&filter[..start]), operator_after(&filter[stop..])];
            if around.iter().flatten().any(|op| DEFAULT_SELECTIVITY.contains(&op.as_str()))
                || array_argument(&filter[..start])
            {
                return true;
            }
            if !around.iter().flatten().any(|op| COMBINING.contains(&op.as_str())) {
                break;
            }
            let Some((open, close)) = enclosing(filter, start, stop) else { break };
            (start, stop) = (open, close + 1);
        }
        i = end;
    }
    false
}

/// The parentheses around `filter[start..end]`: the nearest `(` before it that isn't closed before it, and the `)`
/// that closes it.
fn enclosing(filter: &str, start: usize, end: usize) -> Option<(usize, usize)> {
    let bytes = filter.as_bytes();
    let mut depth = 0;
    let open = (0..start).rev().find(|&at| {
        match bytes[at] {
            b')' => depth += 1,
            b'(' if depth == 0 => return true,
            b'(' => depth -= 1,
            _ => {}
        }
        false
    })?;
    let mut depth = 0;
    let close = (end..bytes.len()).find(|&at| {
        match bytes[at] {
            b'(' => depth += 1,
            b')' if depth == 0 => return true,
            b')' => depth -= 1,
            _ => {}
        }
        false
    })?;
    Some((open, close))
}

fn is_operator_char(c: char) -> bool {
    "<>=~!*@&|#%^-+/".contains(c)
}

/// Whether what comes right before a parameter makes it the array of `ANY (…)` or `ALL (…)`: `= ANY (($1)::text[])`.
fn array_argument(before: &str) -> bool {
    let rest = before.trim_end_matches(|c: char| c.is_whitespace() || c == '(');
    let word: Vec<char> = rest.chars().rev().take_while(|c| c.is_alphanumeric() || *c == '_').collect();
    let word: String = word.into_iter().rev().collect();
    word.eq_ignore_ascii_case("ANY") || word.eq_ignore_ascii_case("ALL")
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
        // An array parameter's length is a guess (10 values), whatever the operator.
        assert!(guessed_selectivity("(status = ANY ($1))"));
        assert!(guessed_selectivity("((a.id)::text = ANY (($1)::text[]))"));
        assert!(guessed_selectivity("(id <> ALL ($2))"));
        assert!(!guessed_selectivity("(status = ANY ('{a,b}'::text[]))"));
        assert!(!guessed_selectivity("(company = $1)"));
        assert!(!guessed_selectivity("(status <> $1)"));
        assert!(!guessed_selectivity("(created_at > now())"));
        assert!(!guessed_selectivity("(price > 10)"));
    }

    // `WHERE created_at > now() - interval $1`: the parameter sits in arithmetic, and the range compares with a value
    // the generic plan doesn't know. Equality with such an expression isn't a default guess.
    #[test]
    fn a_parameter_inside_arithmetic_is_still_a_guess() {
        assert!(guessed_selectivity("(orders.created_at > (now() - $1))"));
        assert!(guessed_selectivity("(created_at > (now() - ($1)::interval))"));
        assert!(guessed_selectivity("((amount * $2) >= 100)"));
        assert!(guessed_selectivity("((name)::text ~~ (('%'::text || $1) || '%'::text))"));
        assert!(!guessed_selectivity("(id = ($1 + 1))"));
        assert!(!guessed_selectivity("((data ->> $1) = 'x'::text)"));

        let json = r#"[{"Plan": {"Node Type": "Seq Scan", "Relation Name": "orders", "Schema": "public",
            "Plan Rows": 277777, "Total Cost": 4500.0, "Filter": "(orders.created_at > (now() - $1))"}}]"#;
        let mut plan = crate::plan_json::parse(json, PlanKind::Generic, Some("SELECT 1")).unwrap();
        apply(&mut plan.root, &sizes(&[("public.orders", 2_000_000.0, false)]), false);
        assert_eq!(plan.root.rows_read, None);
        assert!(plan.root.props.contains_key(culprit_core::GUESSED_SELECTIVITY));
    }

    // Aptabase EU P7: `= ANY($1)` in a generic plan rests on a guessed array length (10 values): the bitmap scan's 10
    // rows, and every share above it, are that guess. Marked on the steps whatever they are, so no share is stated.
    #[test]
    fn an_array_parameter_is_a_guess_on_any_step() {
        let json = r#"[{"Plan": {"Node Type": "Bitmap Heap Scan", "Relation Name": "apps", "Schema": "public",
            "Total Cost": 70.54, "Plan Rows": 10, "Recheck Cond": "((id)::text = ANY ($1))", "Plans": [
              {"Node Type": "Bitmap Index Scan", "Index Name": "PK_apps", "Total Cost": 38.90, "Plan Rows": 10,
               "Index Cond": "((id)::text = ANY ($1))"}]}}]"#;
        let mut plan = crate::plan_json::parse(json, PlanKind::Generic, Some("SELECT 1")).unwrap();
        apply(&mut plan.root, &sizes(&[("public.apps", 6_285.0, false)]), false);
        let index_scan = &plan.root.children[0];
        assert!(index_scan.props[culprit_core::GUESSED_SELECTIVITY].contains("10 values for ANY"));
        assert!(plan.root.props.contains_key(culprit_core::GUESSED_SELECTIVITY));
        assert_eq!(plan.step_share(index_scan), None);
        assert_eq!(plan.step_share(&plan.root), None);
        // An equality on a parameter is estimated from statistics: no mark.
        let json = r#"[{"Plan": {"Node Type": "Index Scan", "Relation Name": "apps", "Schema": "public",
            "Index Name": "PK_apps", "Total Cost": 8.3, "Plan Rows": 1, "Index Cond": "((id)::text = $1)"}}]"#;
        let mut plan = crate::plan_json::parse(json, PlanKind::Generic, Some("SELECT 1")).unwrap();
        apply(&mut plan.root, &sizes(&[("public.apps", 6_285.0, false)]), false);
        assert!(!plan.root.props.contains_key(culprit_core::GUESSED_SELECTIVITY));
    }

    // Saved scans re-read by a newer parser get the sizes the scan looked up back from the saved plan, with the marks
    // that follow from them, and nothing is looked up.
    #[test]
    fn a_saved_plan_read_again_keeps_the_sizes_its_scan_looked_up() {
        let json = r#"[{"Plan": {"Node Type": "Seq Scan", "Relation Name": "orders", "Schema": "public",
            "Plan Rows": 5, "Total Cost": 4500.0, "Filter": "(customer_id = $1)"}}]"#;
        let mut saved = crate::plan_json::parse(json, PlanKind::Generic, Some("SELECT 1")).unwrap();
        apply(&mut saved.root, &sizes(&[("public.orders", 2_000_000.0, false)]), false);
        let again = crate::plan_json::reparse(&saved, None).unwrap();
        assert_eq!(again.root.table_rows, Some(2_000_000.0));
        assert_eq!(again.root.rows_read, Some(2_000_000.0));
    }

    // A sort above a scan planned on a guess sorts the guess's rows: the plan can't say how many it will sort, and
    // states no share of its work for it.
    #[test]
    fn a_sort_of_a_guessed_scan_says_nothing_about_its_size() {
        let json = r#"[{"Plan": {"Node Type": "Gather Merge", "Plan Rows": 666665, "Total Cost": 90000.0, "Plans": [
            {"Node Type": "Sort", "Plan Rows": 277777, "Total Cost": 80000.0, "Startup Cost": 79000.0,
             "Sort Key": ["note"], "Plans": [
              {"Node Type": "Seq Scan", "Relation Name": "orders", "Schema": "public", "Plan Rows": 277777,
               "Total Cost": 30000.0, "Filter": "(orders.created_at > (now() - $1))"}]}]}}]"#;
        let mut plan = crate::plan_json::parse(json, PlanKind::Generic, Some("SELECT 1")).unwrap();
        let rules = |plan: &culprit_core::Plan| {
            culprit_core::insights::insights(plan).into_iter().map(|i| i.rule).collect::<Vec<_>>()
        };
        assert!(rules(&plan).contains(&"big-sort".to_string()), "{:?}", rules(&plan));
        apply(&mut plan.root, &sizes(&[("public.orders", 2_000_000.0, false)]), false);
        assert!(!rules(&plan).contains(&"big-sort".to_string()), "{:?}", rules(&plan));
    }
}
