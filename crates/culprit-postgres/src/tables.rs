//! Table sizes for plans. PostgreSQL's estimated plans say how many rows a scan expects to return, not how many it
//! reads: a sequential scan reads the whole table. The planner's own statistics (`pg_class.reltuples`) say how big
//! that is, so a scan expecting five rows of a two-million-row table reads as what it is.
//!
//! Two scans in a generic plan don't read what they show, and get no rows read: a partition under an Append (a generic
//! plan lists every partition; the ones the parameters rule out are pruned only when it runs), and a scan whose filter
//! compares a column with a parameter by a range or a pattern (planned with a default selectivity, where the real value
//! may well have used an index).

use std::collections::HashMap;

use culprit_core::{IndexStat, NodeKind, Plan, PlanNode, TableStat};
use tokio_postgres::Client;

use crate::rows::tagged;

const SIZES_SQL: &str =
    "SELECT n.nspname || '.' || c.relname AS qualified, c.relname AS name, c.reltuples::float8 AS rows,
       c.relispartition AS partition, c.relpages::float8 AS pages,
       ARRAY(SELECT DISTINCT a.attname::text FROM pg_index x
               JOIN pg_attribute a ON a.attrelid = x.indrelid AND a.attnum = ANY(x.indkey)
              WHERE x.indrelid = c.oid) AS indexed
  FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
 WHERE c.relkind IN ('r', 'm', 'p')
   AND ((n.nspname || '.' || c.relname) = ANY($1) OR (c.relname = ANY($1) AND pg_table_is_visible(c.oid)))";

/// What the catalog says about a scanned table.
#[derive(Clone, Debug, PartialEq)]
struct Size {
    /// The planner's count of its rows; `None` where the catalog hasn't counted them.
    rows: Option<f64>,
    /// A partition of a partitioned table.
    partition: bool,
    /// The columns its indexes have in their keys; `None` where that isn't known.
    indexed: Option<Vec<String>>,
}

/// The planner's count of a table's rows from the catalog: `reltuples`, unknown when it is -1 (never analyzed) or when
/// it is 0 with no pages counted either (never vacuumed or analyzed: the planner then estimates from the file's size,
/// and a catalog table it reads that way shows 0 rows where it expects hundreds).
fn counted_rows(reltuples: f64, relpages: Option<f64>) -> Option<f64> {
    (reltuples > 0.0 || (reltuples == 0.0 && relpages.is_some_and(|p| p > 0.0))).then_some(reltuples)
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
    let mut collected: HashMap<String, Option<Size>> = HashMap::new();
    for row in rows {
        let (Ok(qualified), Ok(name), Ok(count)) =
            (row.try_get::<_, String>(0), row.try_get::<_, String>(1), row.try_get::<_, f64>(2))
        else {
            continue;
        };
        let partition = row.try_get::<_, bool>(3).unwrap_or(false);
        let pages = row.try_get::<_, f64>(4).ok();
        let indexed = row.try_get::<_, Vec<String>>(5).ok();
        let size = Size { rows: counted_rows(count, pages), partition, indexed };
        for key in [name, qualified] {
            collected.entry(key).and_modify(|known| *known = None).or_insert_with(|| Some(size.clone()));
        }
    }
    let sizes = collected.into_iter().filter_map(|(key, size)| Some((key, size?))).collect();
    apply(&mut plan.root, &sizes, false);
}

/// The sizes a plan carries from its scan (`table_rows`, and which scans were marked as partitions a run may prune),
/// put back on the same plan read again by a newer parser ([`crate::plan_json::reparse`]): what [`fill`] looked up
/// then, worked out again without a connection. A table the saved plan carries no size for (scans saved before the
/// collector looked them up) takes it from the scan's own catalog, `tables`, and every table its indexes, `indexes`, so
/// the plan reads as one a scan takes now.
pub(crate) fn refill(plan: &mut Plan, saved: &Plan, tables: &[TableStat], indexes: &[IndexStat]) {
    let mut sizes: HashMap<String, Size> = HashMap::new();
    for n in saved.nodes() {
        // A count of 0 where the planner expects rows is one the catalog never made (`counted_rows`): unknown, not 0.
        let counted = n.table_rows.filter(|rows| *rows > 0.0 || n.est_rows <= 1.0);
        if let (Some(object), Some(rows)) = (n.object.as_ref(), counted) {
            let partition = n.facts().partition_pruning.is_some();
            let size = sizes.entry(object.clone()).or_insert(Size { rows: Some(rows), partition, indexed: None });
            size.partition |= partition;
        }
    }
    for n in plan.nodes() {
        let Some(object) = n.object.as_ref() else { continue };
        let (schema, name) = object.rsplit_once('.').unwrap_or(("", object));
        let mut matching = tables.iter().filter(|t| t.name == name && (schema.is_empty() || t.schema == schema));
        let (Some(table), None) = (matching.next(), matching.next()) else { continue };
        let indexed: Vec<String> = indexes
            .iter()
            .filter(|i| i.schema == table.schema && i.table == table.name)
            .flat_map(|i| i.columns.iter().cloned())
            .collect();
        let size = sizes.entry(object.clone()).or_insert(Size { rows: table.rows, partition: false, indexed: None });
        size.rows = size.rows.or(table.rows);
        size.indexed = Some(indexed);
    }
    apply(&mut plan.root, &sizes, false);
}

/// Whether one of the table's indexes has a column that a condition planned on the guess names in its key: only then
/// may a real value for its parameter let the planner read through an index instead. A condition the planner estimated
/// from statistics (`(id % $1) = $2`) says nothing of it. False where the table's indexes aren't known.
fn index_may_serve(filter: &str, indexed: Option<&[String]>) -> bool {
    let Some(indexed) = indexed else { return false };
    let mut words = Vec::new();
    for condition in filter.split(" AND ").filter(|c| guessed_selectivity(c)) {
        let mut word = String::new();
        for c in condition.chars().chain(std::iter::once(' ')) {
            if c.is_alphanumeric() || c == '_' {
                word.push(c);
            } else if !word.is_empty() {
                words.push(std::mem::take(&mut word));
            }
        }
    }
    words.iter().any(|w| indexed.iter().any(|col| col.trim_matches('"').eq_ignore_ascii_case(w)))
}

/// Said on a partition's scan under an Append in a generic plan, instead of rows read.
const UNPRUNED: &str = "Listed because the plan has no parameter values: partitions are pruned only when it runs, so this one may never be read.";
/// Said on a scan whose filter compares a column with a parameter by a range, a pattern or ANY, instead of rows read,
/// where an index of its table has a column the filter names.
const GUESSED: &str = "Planned with a default selectivity for its parameter (a fixed share of the rows for a range or a pattern, 10 values for ANY): with a real value the planner may well use an index instead.";
/// Said on any other step whose condition compares a column with a parameter that way.
const GUESSED_STEP: &str = "Planned with a default selectivity for its parameter (a fixed share of the rows for a range or a pattern, 10 values for ANY): the rows it expects, and the cost of the steps above it, rest on that guess, not on the column's statistics.";

fn apply(node: &mut PlanNode, sizes: &HashMap<String, Size>, under_append: bool) {
    // Rows a run measured aren't a guess, whatever the plan was planned with.
    let guessed = node.act_rows.is_none()
        && [node.predicate.as_deref(), node.seek_predicate.as_deref()].into_iter().flatten().any(guessed_selectivity);
    let size = node.object.as_ref().and_then(|o| sizes.get(o));
    if node.act_rows.is_some() || (!guessed && (node.predicate.is_some() || node.seek_predicate.is_some())) {
        node.facts_mut().selectivity_guess = None;
        node.props.remove(culprit_core::GUESSED_SELECTIVITY);
    }
    if node.act_rows.is_some() || size.is_some_and(|s| !under_append || !s.partition) {
        node.facts_mut().partition_pruning = None;
        node.props.remove(culprit_core::NOT_READ_IN_FULL);
    }
    if let Some(size) = size {
        if size.rows.is_some() {
            node.table_rows = size.rows;
        }
        if node.kind == NodeKind::Scan
            && node.op.contains("Seq Scan")
            && node.rows_read.is_none()
            && node.act_rows.is_none()
        {
            if under_append && size.partition {
                node.facts_mut().partition_pruning = Some(UNPRUNED.into());
                node.props.insert(culprit_core::NOT_READ_IN_FULL.into(), UNPRUNED.into());
            } else if guessed {
                // A note never promises an index the table doesn't have.
                let filter = node.predicate.as_deref().unwrap_or_default();
                let text = if index_may_serve(filter, size.indexed.as_deref()) { GUESSED } else { GUESSED_STEP };
                node.facts_mut().selectivity_guess = Some(text.into());
                node.props.insert(culprit_core::GUESSED_SELECTIVITY.into(), text.into());
            } else if let Some(rows) = size.rows {
                // A total over its runs: a scan on a nested loop's inner side reads the table once per outer row.
                node.rows_read = Some(rows * node.runs());
            }
        }
    }
    if guessed && node.facts().selectivity_guess.is_none() {
        node.facts_mut().selectivity_guess = Some(GUESSED_STEP.into());
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

    #[test]
    fn measured_rows_withdraw_saved_generic_pruning_and_default_guess_annotations() {
        let json = r#"[{"Plan":{"Node Type":"Seq Scan","Relation Name":"orders","Schema":"public",
            "Plan Rows":10,"Total Cost":100,"Filter":"(created_at > $1)"}}]"#;
        let mut plan = crate::plan_json::parse(json, PlanKind::Generic, None).unwrap();
        apply(&mut plan.root, &sizes(&[("public.orders", 100000.0, true)]), true);
        assert!(plan.root.facts().partition_pruning.is_some());
        assert!(plan.root.facts().selectivity_guess.is_some());
        plan.kind = PlanKind::Actual;
        plan.root.act_rows = Some(10.0);
        plan.root.act_executions = Some(1.0);
        apply(&mut plan.root, &sizes(&[("public.orders", 100000.0, true)]), true);
        assert_eq!(plan.root.facts().partition_pruning, None);
        assert_eq!(plan.root.facts().selectivity_guess, None);
        assert!(!plan.root.props.contains_key(culprit_core::NOT_READ_IN_FULL));
        assert!(!plan.root.props.contains_key(culprit_core::GUESSED_SELECTIVITY));
    }

    #[test]
    fn an_unqualified_saved_plan_does_not_borrow_an_arbitrary_schemas_table_size() {
        let json = r#"[{"Plan":{"Node Type":"Seq Scan","Relation Name":"orders","Plan Rows":10,"Total Cost":100}}]"#;
        let mut plan = crate::plan_json::parse(json, PlanKind::Estimated, None).unwrap();
        let saved = plan.clone();
        refill(&mut plan, &saved, &[table("public", "orders", 100.0), table("archive", "orders", 1000000.0)], &[]);
        assert_eq!(plan.root.table_rows, None);
        assert_eq!(plan.root.rows_read_total(), None);
    }

    fn sizes(entries: &[(&str, f64, bool)]) -> HashMap<String, Size> {
        entries
            .iter()
            .map(|(n, rows, partition)| {
                (n.to_string(), Size { rows: Some(*rows), partition: *partition, indexed: None })
            })
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
        assert!(plan.root.facts().selectivity_guess.is_some());
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
        assert!(plan.root.facts().selectivity_guess.is_some());
        assert_eq!(plan.step_share(index_scan), None);
        assert_eq!(plan.step_share(&plan.root), None);
        // An equality on a parameter is estimated from statistics: no mark.
        let json = r#"[{"Plan": {"Node Type": "Index Scan", "Relation Name": "apps", "Schema": "public",
            "Index Name": "PK_apps", "Total Cost": 8.3, "Plan Rows": 1, "Index Cond": "((id)::text = $1)"}}]"#;
        let mut plan = crate::plan_json::parse(json, PlanKind::Generic, Some("SELECT 1")).unwrap();
        apply(&mut plan.root, &sizes(&[("public.apps", 6_285.0, false)]), false);
        assert!(plan.root.facts().selectivity_guess.is_none());
    }

    // Saved scans re-read by a newer parser get the sizes the scan looked up back from the saved plan, with the marks
    // that follow from them, and nothing is looked up.
    #[test]
    fn a_saved_plan_read_again_keeps_the_sizes_its_scan_looked_up() {
        let json = r#"[{"Plan": {"Node Type": "Seq Scan", "Relation Name": "orders", "Schema": "public",
            "Plan Rows": 5, "Total Cost": 4500.0, "Filter": "(customer_id = $1)"}}]"#;
        let mut saved = crate::plan_json::parse(json, PlanKind::Generic, Some("SELECT 1")).unwrap();
        apply(&mut saved.root, &sizes(&[("public.orders", 2_000_000.0, false)]), false);
        let again = crate::plan_json::reparse(&saved, None, &[], &[]).unwrap();
        assert_eq!(again.root.table_rows, Some(2_000_000.0));
        assert_eq!(again.root.rows_read, Some(2_000_000.0));
    }

    fn table(schema: &str, name: &str, rows: f64) -> TableStat {
        serde_json::from_value(
            serde_json::json!({"schema": schema, "name": name, "rows": rows, "has_primary_key": true}),
        )
        .unwrap()
    }

    fn index(schema: &str, table: &str, name: &str, columns: &[&str]) -> IndexStat {
        serde_json::from_value(serde_json::json!({
            "schema": schema, "table": table, "name": name, "kind": "btree", "columns": columns, "include": [],
            "is_unique": false, "is_primary_key": false, "is_clustered": false, "is_disabled": false
        }))
        .unwrap()
    }

    // The demo's saved scans kept plans without table sizes: read again, a plan takes them from the scan's own catalog,
    // so a scan that reads 2M rows to keep 12 reads as one (and its plan's insight says so).
    #[test]
    fn a_saved_plan_without_sizes_takes_them_from_the_catalog() {
        let json = r#"[{"Plan": {"Node Type": "Seq Scan", "Relation Name": "orders", "Schema": "public",
            "Plan Rows": 12, "Total Cost": 45000.0, "Filter": "(customer_id = $1)"}}]"#;
        let saved = crate::plan_json::parse(json, PlanKind::Generic, Some("SELECT 1")).unwrap();
        assert_eq!(saved.root.table_rows, None);
        let again = crate::plan_json::reparse(&saved, None, &[table("public", "orders", 2_000_000.0)], &[]).unwrap();
        assert_eq!(again.root.table_rows, Some(2_000_000.0));
        assert_eq!(again.root.rows_read, Some(2_000_000.0));
        let rules: Vec<String> = culprit_core::insights::insights(&again).into_iter().map(|i| i.rule).collect();
        assert!(rules.contains(&"scan-filter".to_string()), "{rules:?}");
    }

    // A guessed scan's note promises an index only where one of the table's indexes has a column the filter names.
    #[test]
    fn a_guess_note_never_promises_an_index_the_table_lacks() {
        let json = r#"[{"Plan": {"Node Type": "Seq Scan", "Relation Name": "orders", "Schema": "public",
            "Plan Rows": 10000, "Total Cost": 45000.0, "Filter": "((o.note)::text ~~ $1)"}}]"#;
        let saved = crate::plan_json::parse(json, PlanKind::Generic, Some("SELECT 1")).unwrap();
        let tables = [table("public", "orders", 2_000_000.0)];
        let note = |indexes: &[IndexStat]| {
            let plan = crate::plan_json::reparse(&saved, None, &tables, indexes).unwrap();
            plan.root.props[culprit_core::GUESSED_SELECTIVITY].clone()
        };
        let none = note(&[index("public", "orders", "orders_status_created_idx", &["status", "created_at"])]);
        assert!(!none.contains("use an index"), "{none}");
        assert!(none.contains("rest on that guess"), "{none}");
        let some = note(&[index("public", "orders", "orders_note_idx", &["note"])]);
        assert!(some.contains("may well use an index"), "{some}");
        // Only the conditions planned on the guess count: the primary key on id serves none of these ranges.
        let json = r#"[{"Plan": {"Node Type": "Seq Scan", "Relation Name": "orders", "Schema": "public",
            "Plan Rows": 1111, "Total Cost": 45000.0,
            "Filter": "((o.total >= $3) AND (o.customer_id > $4) AND ((o.id % $1) = $2))"}}]"#;
        let saved = crate::plan_json::parse(json, PlanKind::Generic, Some("SELECT 1")).unwrap();
        let plan =
            crate::plan_json::reparse(&saved, None, &tables, &[index("public", "orders", "orders_pkey", &["id"])])
                .unwrap();
        let text = &plan.root.props[culprit_core::GUESSED_SELECTIVITY];
        assert!(!text.contains("use an index"), "{text}");
    }

    // reltuples 0 with no pages counted isn't a count: the planner reads the file's size instead (a catalog table it
    // expects 232 rows of). A saved plan's 0 where the planner expects rows is unknown too.
    #[test]
    fn a_zero_the_catalog_never_counted_is_unknown() {
        assert_eq!(counted_rows(0.0, Some(0.0)), None);
        assert_eq!(counted_rows(-1.0, Some(10.0)), None);
        assert_eq!(counted_rows(0.0, Some(3.0)), Some(0.0));
        assert_eq!(counted_rows(4580.0, Some(60.0)), Some(4580.0));
        let json = r#"[{"Plan": {"Node Type": "Seq Scan", "Relation Name": "pg_attrdef", "Schema": "pg_catalog",
            "Plan Rows": 232, "Total Cost": 47.0}}]"#;
        let mut saved = crate::plan_json::parse(json, PlanKind::Generic, Some("SELECT 1")).unwrap();
        saved.root.table_rows = Some(0.0);
        saved.root.rows_read = Some(0.0);
        let again = crate::plan_json::reparse(&saved, None, &[], &[]).unwrap();
        assert_eq!((again.root.table_rows, again.root.rows_read), (None, None));
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
