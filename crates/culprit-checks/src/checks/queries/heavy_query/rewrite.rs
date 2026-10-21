use super::*;

/// An enabled index on `schema.table` whose first key is `column`: one that holds every row first, since a filtered
/// index serves only the statements whose conditions match its filter.
fn index_leading_with<'a>(
    snapshot: &'a Snapshot,
    schema: &'a str,
    table: &'a str,
    column: &str,
) -> Option<&'a culprit_core::IndexStat> {
    let leading: Vec<&culprit_core::IndexStat> = snapshot
        .indexes_on(schema, table)
        .filter(|i| !i.is_disabled && i.key_names().first().is_some_and(|k| k.eq_ignore_ascii_case(column)))
        .collect();
    leading.iter().find(|i| i.filter.is_none()).or_else(|| leading.first()).copied()
}

/// Advice for a scan that keeps most of its table while the statement returns far fewer rows: a condition later in the
/// plan narrows them, so the index belongs to that condition, not to the scan's filter. An OR across two tables can use
/// an index on neither; written as a UNION of two queries, each side can use one of its own: the one its table has, or
/// one to add where the index rules allow it.
pub(super) fn narrowed_later_fix(
    snapshot: &Snapshot,
    plan: &Plan,
    _q: &QueryStat,
    table: &str,
    kept: f64,
    returned: f64,
    narrowing: Option<&str>,
) -> Fix {
    // Rounded as the evidence rounds it: 2.5 rows a run are "about 2.5", not "about 3".
    let rows = if returned < 1.0 {
        "less than a row a run".to_string()
    } else {
        let n = format::count(returned);
        let n = n.strip_suffix(".0").unwrap_or(&n);
        format!("about {n} row{} a run", if n == "1" { "" } else { "s" })
    };
    let start = format!(
        "The plan reads all of {table} and keeps about {} of its rows, but the statement returns {rows}: they are narrowed later in the plan",
        format::percent(kept)
    );
    match narrowing {
        Some(condition) if or_across_tables(condition) => or_rewrite_fix(snapshot, plan, condition, &start),
        Some(condition) => advice(format!(
            "{start}, by `{}`. Where that condition can be applied to {table} itself, an index on the columns it compares lets the plan read only the rows needed.",
            super::super::plan_warnings::truncate(condition, 140)
        )),
        None => advice(format!(
            "{start} (by a join or a limit). Open the plan to see where; a condition on {table} itself, with an index on the columns it compares, lets the plan read only the rows needed."
        )),
    }
}

fn or_rewrite_fix(snapshot: &Snapshot, plan: &Plan, condition: &str, start: &str) -> Fix {
    let sides = union_sides(snapshot, plan, condition)
        .map(|sides| format!(" Existing coverage: {}.", sides.join("; ")))
        .unwrap_or_default();
    advice(format!(
        "{start}, by `{}`. This OR spans different tables, so a single index on the scan's filter does not address it. Evaluate a UNION rewrite or an equivalent EXISTS form that lets each branch use its own access path. Preserve the original joins, permission checks, duplicate handling, NULL behavior, grouping and ordering. Compare representative rewritten plans and reads before deciding whether a branch needs another index.{sides} Keep this structural risk under review as table size or call volume grows.",
        super::super::plan_warnings::truncate(condition, 140)
    ))
}

/// Existing index coverage for each branch. Returned rows after joins do not establish a branch's selectivity,
/// so a rewrite must be evaluated before recommending any new index.
fn union_sides(snapshot: &Snapshot, plan: &Plan, condition: &str) -> Option<Vec<String>> {
    if snapshot.notes.iter().any(|n| n.collector == "indexes") && !snapshot.server.has(Capability::IndexUsage) {
        return None;
    }
    let mut sides = Vec::new();
    for branch in or_branches(condition) {
        let mut tables = qualifiers(&unquoted(branch));
        tables.sort();
        tables.dedup();
        let [qualifier] = tables.as_slice() else { return None };
        let cols = predicate_columns(branch);
        let column = cols.equality.first().or(cols.range.first())?;
        let node = table_named(plan, qualifier)?;
        let (schema, table) = crate::indexes::split_object(node.object.as_deref()?)?;
        sides.push(match index_leading_with(snapshot, &schema, &table, column) {
            Some(index) => format!(
                "{} on {schema}.{table} leads with {column}{}",
                index.name,
                index
                    .filter
                    .as_deref()
                    .map(|f| format!(" (partial index: verify the branch implies {f})"))
                    .unwrap_or_default()
            ),
            None => format!("{schema}.{table} has no index that leads with {column}"),
        });
    }
    (sides.len() >= 2).then_some(sides)
}

/// One rewrite observation shared by Findings, Plans and the advisor, independent of today's table size.
pub(crate) fn late_filter_rewrite(
    snapshot: &Snapshot,
    q: &QueryStat,
    plan: &Plan,
) -> Option<culprit_core::insights::PlanInsight> {
    if is_monitoring_query(q)
        || !matches!(kind_of(q), StatementKind::Read | StatementKind::Aggregate)
        || plan.nodes().iter().any(|n| n.kind == NodeKind::Modify)
    {
        return None;
    }
    let returned = q.rows_handled_per_execution();
    let mut scans: Vec<_> = plan
        .nodes()
        .into_iter()
        .filter(|n| {
            n.kind == NodeKind::Scan
                && n.reads_table_whole()
                && n.object.as_deref().is_some_and(|o| !culprit_core::insights::is_system_object(o))
        })
        .collect();
    scans.sort_by(|a, b| b.table_rows.unwrap_or(0.0).total_cmp(&a.table_rows.unwrap_or(0.0)));
    for node in scans {
        let Some(kept) = node.kept_share().filter(|k| *k > culprit_core::INDEX_KEEPS_AT_MOST) else { continue };
        let Some(condition) =
            ancestors(plan, node.id).into_iter().filter_map(|n| n.predicate.as_deref()).find(|p| or_across_tables(p))
        else {
            continue;
        };
        let table = node.object.as_deref()?;
        let fix = match returned {
            Some(returned) => narrowed_later_fix(snapshot, plan, q, table, kept, returned, Some(condition)),
            None => or_rewrite_fix(
                snapshot,
                plan,
                condition,
                &format!("The captured plan reads {table} before a filter across tables is applied"),
            ),
        };
        return Some(culprit_core::insights::PlanInsight {
            rule: "cross-table-or".into(), area: culprit_core::insights::InsightArea::Query,
            severity: Severity::Low, impact: 0.0, fixed_weight: true, nodes: vec![node.id],
            title: "A filter across tables can require a whole-table read".into(),
            detail: format!("The captured plan scans {table} before an OR across tables narrows its rows. This structure can become expensive as the tables grow; the plan does not measure the benefit or semantics of a rewrite."),
            fix: fix.summary, fix_sql: None,
            verify: "Compare results, representative parameters, plans, reads and execution time for the original and rewritten query. Preserve permission checks and duplicate/NULL behavior. Evaluate index keys only from the rewritten branch predicates and measured selectivity.".into(),
            index_hint: None,
        });
    }
    None
}

/// The branches of a condition's top-level OR, outer parentheses taken off: `((a.x = $1) OR (s.y = $2))` →
/// `(a.x = $1)`, `(s.y = $2)`. Quoted text is never split.
fn or_branches(condition: &str) -> Vec<&str> {
    let mut text = condition.trim();
    while text.starts_with('(') && text.ends_with(')') && closes_at_end(text) {
        text = text[1..text.len() - 1].trim();
    }
    let bytes = text.as_bytes();
    let word = |c: Option<&u8>| c.is_some_and(|c| c.is_ascii_alphanumeric() || *c == b'_');
    let (mut depth, mut quoted, mut start, mut at) = (0i32, false, 0, 0);
    let mut parts = Vec::new();
    while at < bytes.len() {
        match bytes[at] {
            b'\'' => quoted = !quoted,
            b'(' if !quoted => depth += 1,
            b')' if !quoted => depth -= 1,
            b'o' | b'O'
                if !quoted
                    && depth == 0
                    && bytes.get(at + 1).is_some_and(|c| c.eq_ignore_ascii_case(&b'r'))
                    && !word(at.checked_sub(1).and_then(|i| bytes.get(i)))
                    && !word(bytes.get(at + 2)) =>
            {
                parts.push(text[start..at].trim());
                at += 2;
                start = at;
                continue;
            }
            _ => {}
        }
        at += 1;
    }
    parts.push(text[start..].trim());
    parts
}

/// Whether the parenthesis opening `text` is the one its last character closes.
fn closes_at_end(text: &str) -> bool {
    let (mut depth, mut quoted) = (0i32, false);
    for (i, c) in text.char_indices() {
        match c {
            '\'' => quoted = !quoted,
            '(' if !quoted => depth += 1,
            ')' if !quoted => {
                depth -= 1;
                if depth == 0 {
                    return i == text.len() - 1;
                }
            }
            _ => {}
        }
    }
    false
}

/// The step of a plan that reads the table a condition calls `name`: by its alias (PostgreSQL's), or by its name.
fn table_named<'a>(plan: &'a Plan, name: &str) -> Option<&'a PlanNode> {
    let mut matches = plan.nodes().into_iter().filter(|n| {
        let Some(object) = n.object.as_deref() else { return false };
        let leaf = object.rsplit('.').next().unwrap_or(object).trim_matches(['[', ']', '"', '`']);
        n.facts().table_alias.as_ref().is_some_and(|a| a.eq_ignore_ascii_case(name)) || leaf.eq_ignore_ascii_case(name)
    });
    let first = matches.next()?;
    matches.all(|n| n.object == first.object).then_some(first)
}

/// The steps above the one with `id`, nearest first.
pub(super) fn ancestors(plan: &Plan, id: u32) -> Vec<&PlanNode> {
    fn path<'a>(n: &'a PlanNode, id: u32, out: &mut Vec<&'a PlanNode>) -> bool {
        if n.id == id {
            return true;
        }
        let found = n.children.iter().any(|c| path(c, id, out));
        if found {
            out.push(n);
        }
        found
    }
    let mut out = Vec::new();
    path(&plan.root, id, &mut out);
    out
}

/// Whether a condition ORs a comparison on one table's column with a comparison on another's (`a.owner_id = $1 OR
/// s.email = $2`): an index on either table serves only its own side, so the plan reads both in full.
pub(super) fn or_across_tables(condition: &str) -> bool {
    let text = unquoted(condition);
    // Each OR as a word (`… $1) OR ((s.email …`), and the comparisons on either side of it: the last column before it
    // and the first after it.
    let bytes = text.as_bytes();
    let word = |c: &u8| c.is_ascii_alphanumeric() || *c == b'_';
    (0..bytes.len().saturating_sub(1)).any(|at| {
        bytes[at] == b'o'
            && bytes[at + 1] == b'r'
            && (at == 0 || !word(&bytes[at - 1]))
            && bytes.get(at + 2).is_none_or(|c| !word(c))
            && matches!(
                (qualifiers(&text[..at]).pop(), qualifiers(&text[at + 2..]).into_iter().next()),
                (Some(a), Some(b)) if a != b
            )
    })
}

/// A condition lowercased, with its names unquoted and its quoted literals left out: `'black or white'` is a value, not
/// a condition.
fn unquoted(condition: &str) -> String {
    let mut text = String::new();
    let mut quoted = false;
    for c in condition.to_ascii_lowercase().chars() {
        match c {
            '\'' => quoted = !quoted,
            '`' | '[' | ']' | '"' => {}
            _ if !quoted => text.push(c),
            _ => {}
        }
    }
    text
}

/// The table each qualified column of [`unquoted`] text names, in order: `a.owner_id` → `a`,
/// `db.app.messages.tenant_id` → `messages`.
fn qualifiers(text: &str) -> Vec<String> {
    text.split(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '$' | '@')))
        .filter_map(|word| {
            let (qualifier, column) = word.rsplit_once('.')?;
            let table = qualifier.rsplit('.').next()?;
            let starts = |s: &str| s.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_');
            (starts(table) && starts(column)).then(|| table.to_string())
        })
        .collect()
}
