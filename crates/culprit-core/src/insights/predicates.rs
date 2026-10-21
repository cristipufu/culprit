//! Static predicate and filter inspection shared by plan insight rules: sargability blockers, seekable columns,
//! implicit conversion extraction, and MySQL selectivity-guess detection.

use std::collections::HashMap;

use crate::model::Engine;
use crate::plan::{INDEX_KEEPS_AT_MOST, NodeKind, Plan, PlanNode};
use crate::sql::{balanced_parens as balanced, identifier_parts, split_top_level_and as top_level_and};

use super::{Cx, MANY_ROWS, column_leaf, conditions, rows_read};

const NOT_COLUMNS: &[&str] = &[
    "and",
    "or",
    "not",
    "is",
    "null",
    "true",
    "false",
    "like",
    "in",
    "any",
    "all",
    "between",
    "as",
    "text",
    "date",
    "integer",
    "bigint",
    "numeric",
    "timestamp",
    "timestamptz",
    "varchar",
    "nvarchar",
    "char",
    "bpchar",
    "interval",
    "subplan",
    "hashed",
    "exists",
    "case",
    "when",
    "then",
    "else",
    "end",
    "col1",
    "double",
    "decimal",
    "int",
    "cast",
    "convert_implicit",
    "convert",
    "scalar_operator",
    "getdate",
    "now",
    "current_date",
    "current_timestamp",
    "day",
    "month",
    "year",
    "hour",
    "minute",
    "second",
    "week",
    "quarter",
    "escape",
    "collate",
    // Type names a CONVERT or CAST writes beside its operand.
    "datetime",
    "datetime2",
    "datetimeoffset",
    "smalldatetime",
    "time",
    "money",
    "smallmoney",
    "float",
    "real",
    "bit",
    "smallint",
    "tinyint",
    "uniqueidentifier",
    "binary",
    "varbinary",
    "nchar",
    "ntext",
    "xml",
    "signed",
    "unsigned",
    "json",
    "jsonb",
    "boolean",
    "bool",
    "uuid",
    "bytea",
    "character",
    "varying",
    "precision",
];

/// Identifiers in a condition, in order: bracketed (SQL Server), backticked (MySQL) or bare/qualified (Postgres, MySQL
/// trees). Quoted literals, parameters, numbers, function names and type names are skipped.
pub(super) fn identifiers(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut out: Vec<String> = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '\'' {
            i += 1;
            while i < chars.len() {
                if chars[i] == '\'' {
                    if chars.get(i + 1) == Some(&'\'') {
                        i += 2;
                        continue;
                    }
                    break;
                }
                i += 1;
            }
            i += 1;
            continue;
        }
        if c == '[' || c == '`' || c == '"' || c.is_ascii_alphabetic() || c == '_' {
            // A dotted name: [a].[b].[c], `a`.`b`, a.b.c, "a"."b"
            let start = i;
            loop {
                match chars.get(i) {
                    Some('[') => {
                        while i < chars.len() && chars[i] != ']' {
                            i += 1;
                        }
                        i += 1;
                    }
                    Some('`') | Some('"') => {
                        let q = chars[i];
                        i += 1;
                        while i < chars.len() && chars[i] != q {
                            i += 1;
                        }
                        i += 1;
                    }
                    Some(ch) if ch.is_ascii_alphanumeric() || *ch == '_' || *ch == '$' || *ch == '#' => {
                        while i < chars.len()
                            && (chars[i].is_ascii_alphanumeric() || matches!(chars[i], '_' | '$' | '#'))
                        {
                            i += 1;
                        }
                    }
                    _ => break,
                }
                if chars.get(i) == Some(&'.') && chars.get(i + 1).is_some_and(|n| *n != ' ') {
                    i += 1;
                    continue;
                }
                break;
            }
            let token: String = chars[start..i.min(chars.len())].iter().collect();
            let leaf = column_leaf(&token);
            let previous = chars[..start].iter().rev().find(|c| !c.is_whitespace()).copied();
            let next = chars[i.min(chars.len())..].iter().find(|c| !c.is_whitespace()).copied();
            let is_type = previous == Some(':');
            let is_function = next == Some('(') && !token.starts_with('[') && !token.starts_with('`');
            let is_literal_prefix = chars.get(i) == Some(&'\'');
            let lower = leaf.to_ascii_lowercase();
            if !leaf.is_empty()
                && !leaf.starts_with('@')
                && !leaf.starts_with('$')
                && !is_type
                && !is_function
                && !is_literal_prefix
                && !NOT_COLUMNS.contains(&lower.as_str())
                && !leaf.chars().all(|c| c.is_ascii_digit())
                && !out.iter().any(|o| o.eq_ignore_ascii_case(&leaf))
            {
                out.push(leaf);
            }
            continue;
        }
        i += 1;
    }
    out
}

/// The columns of a scan's filter an index could seek on, equality columns first, then ranges: only conditions that
/// compare one plain column with a value. A condition no B-tree index can serve gives no key, which `filter_columns`
/// would take its first name from: an OR across columns (`status = 'x' OR region = 'eu'`), an inequality (`<>`,
/// `NOT LIKE`, `NOT IN`), two columns of the row compared (`shipped_at > created_at`), arithmetic or a cast on the
/// column (`(price * 1.1) > 100`, `(created_at)::date = $1`), and SQL Server's bitmap probe of a hash join
/// (`PROBE([Opt_Bitmap1005], …)`), which names no column at all.
pub(super) fn seekable_columns(text: &str) -> Vec<String> {
    let (mut equality, range) = seekable_keys(text);
    for c in range {
        push_unique(&mut equality, &c);
    }
    equality.truncate(4);
    equality
}

/// [`seekable_columns`] apart: the columns compared with a value by equality, and those compared by range.
pub(super) fn seekable_keys(text: &str) -> (Vec<String>, Vec<String>) {
    let mut equality: Vec<String> = Vec::new();
    let mut range: Vec<String> = Vec::new();
    for part in top_level_and(text) {
        let lower = part.to_ascii_lowercase();
        if lower.contains("probe(") {
            continue;
        }
        let ids = identifiers(part);
        let [column] = ids.as_slice() else { continue };
        if lower.contains(" or ") {
            // `status = 'a' OR status = 'b'` is an IN list on one column.
            if !lower.contains("<>") && !lower.contains("!=") {
                push_unique(&mut equality, column);
            }
            continue;
        }
        if ["<>", "!=", " not like ", " not in ", " not between ", "is not null", "<> all"]
            .iter()
            .any(|n| lower.contains(n))
        {
            continue;
        }
        // The column's side of the comparison holds nothing but the column (a harmless cast to text aside).
        let Some((at, op)) = ["<=", ">=", "=", "<", ">", " like ", " in (", " between ", " is null", " = any"]
            .iter()
            .filter_map(|op| lower.find(op).map(|at| (at, *op)))
            .min_by_key(|(at, _)| *at)
        else {
            continue;
        };
        let (left, right) = (&lower[..at], &lower[at + op.len()..]);
        let side = if identifiers(left).is_empty() { right } else { left };
        let arithmetic = side.split([' ', '(', ')']).any(|t| matches!(t, "%" | "+" | "-" | "*" | "/" | "||"));
        let cast = side.split("::").skip(1).any(|ty| {
            let ty: String = ty.chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == ' ').collect();
            !matches!(ty.trim(), "text" | "varchar" | "character varying" | "name" | "unknown")
        });
        if arithmetic || cast {
            continue;
        }
        let is_eq = (op == "=" || op == " in (" || op == " is null" || op == " = any")
            && !lower.contains(">=")
            && !lower.contains("<=");
        push_unique(if is_eq { &mut equality } else { &mut range }, column);
    }
    (equality, range)
}

pub(super) fn push_unique(list: &mut Vec<String>, column: &str) {
    if !list.iter().any(|c| c.eq_ignore_ascii_case(column)) {
        list.push(column.to_string());
    }
}

/// Columns compared in a condition, suitable as index keys: equality columns first, then ranges.
pub(super) fn filter_columns(text: &str) -> Vec<String> {
    let mut equality = Vec::new();
    let mut other = Vec::new();
    for part in top_level_and(text) {
        let lower = part.to_ascii_lowercase();
        let ids = identifiers(part);
        let Some(first) = ids.first() else { continue };
        let is_eq = lower.contains('=') && !lower.contains(">=") && !lower.contains("<=") && !lower.contains("<>")
            || lower.contains(" in (");
        let list = if is_eq { &mut equality } else { &mut other };
        if !list.iter().any(|c: &String| c.eq_ignore_ascii_case(first)) {
            list.push(first.clone());
        }
    }
    let mut cols = equality;
    for c in other {
        if !cols.iter().any(|x| x.eq_ignore_ascii_case(&c)) {
            cols.push(c);
        }
    }
    cols.truncate(4);
    cols
}

/// MySQL's fixed guesses for the share of rows a condition keeps when it has neither index statistics nor a histogram
/// for the column: 10% for an equality, 11.11% for a range or LIKE, 33.33% for an inequality, multiplied together for
/// several conditions. A `filtered` that is exactly such a product (to the two decimals MySQL prints) says nothing about
/// the data, so it is never read as the share a filter keeps; a histogram that happens to give one of these values is
/// taken as a guess too. `conditions`: how many conditions are ANDed in the filter.
pub fn mysql_guessed_filtered(filtered: f64, conditions: usize) -> bool {
    const GUESSES: [f64; 3] = [0.1, 1.0 / 9.0, 1.0 / 3.0];
    let mut products = vec![1.0];
    let mut all = Vec::new();
    for _ in 0..conditions.clamp(1, 3) {
        products = products.iter().flat_map(|p| GUESSES.iter().map(move |g| p * g)).collect();
        all.extend(products.iter().copied());
    }
    all.into_iter().any(|g| (g * 100.0 - filtered).abs() < 0.006)
}

/// How many conditions a filter ANDs together, for [`mysql_guessed_filtered`].
pub fn condition_count(filter: &str) -> usize {
    top_level_and(filter).len().max(1)
}

/// The share of the rows a scan reads that its filter keeps, from the plan's own numbers: measured counts when the
/// plan has them (MySQL's EXPLAIN ANALYZE keeps the filter in a Filter node above the scan), else the optimizer's
/// estimate ([`PlanNode::kept_share`]). None when the plan can't say: no rows-read figure (a PostgreSQL estimate without
/// the table's size), or one of MySQL's fixed guesses ([`mysql_guessed_filtered`]).
pub(super) fn filter_keeps(cx: &Cx, n: &PlanNode) -> Option<f64> {
    if let (Some(read), Some(filter)) = (n.act_rows, cx.parent(n).filter(|p| p.op == "Filter"))
        && n.rows_read.is_none()
    {
        let kept = filter.act_rows?;
        return (read > 0.0).then(|| (kept / read).min(1.0));
    }
    n.kept_share()
}

/// Whether an index could pay off for a scan, judged from the plan's own numbers. Every rule that advises an index, or
/// a rewrite that lets one be used, from a scan passes this: a scan (a seek already uses an index) of rowstore data (a
/// columnstore index is read by segments and batches, not seeks), reading at least `MANY_ROWS` rows, whose filter keeps
/// at most `INDEX_KEEPS_AT_MOST` of them. When the plan can't tell how much the filter keeps, none of them speaks.
///
/// A SQL Server scan whose filter probes a hash join's bitmap (`PROBE([Opt_Bitmap1005], …)`) keeps what the join lets
/// through: the share it keeps is the join's doing, not a filter an index could serve. So is a MySQL table joined
/// through a join buffer: its condition holds the join's, and the share MySQL expects it to keep (`filtered`) counts
/// the join in. The join-buffer rule speaks for it.
pub(super) fn index_could_pay(cx: &Cx, n: &PlanNode) -> bool {
    n.kind == NodeKind::Scan
        && !n.facts().storage.as_ref().is_some_and(|s| s.eq_ignore_ascii_case("ColumnStore"))
        && !n.facts().join_buffer.is_some()
        && rows_read(n) >= MANY_ROWS
        && !conditions(n).to_ascii_lowercase().contains("probe(")
        && filter_keeps(cx, n).is_some_and(|k| k <= INDEX_KEEPS_AT_MOST)
}

/// What keeps a scan's filter from an index, when the filter itself does: one insight per scan, from the first that
/// applies (a converted column, a leading wildcard, then a function or cast around a column).
#[derive(PartialEq)]
pub(super) enum Blocker {
    Conversion,
    Wildcard(usize),
    Function(String, String),
}

pub(super) fn blocker(cx: &Cx, n: &PlanNode, filter: &str) -> Option<Blocker> {
    if !current_conversions(cx.plan, &conditions(n)).is_empty() {
        return Some(Blocker::Conversion);
    }
    if let Some(at) = wildcard_at(filter) {
        return Some(Blocker::Wildcard(at));
    }
    wrapped_column(filter, cx.engine()).map(|(f, c)| Blocker::Function(f, c))
}

/// MySQL's possible keys for a scan that could serve its filter. MySQL lists keys for the WHERE clause and for joins
/// alike, and names them without their columns: with one table in the query every key comes from the filter. In a
/// join, a key counts when it leads with a filtered column, by the catalog ([`super::mark_possible_keys`]), so an index
/// on the join column isn't taken for one on the filter; on a plan the catalog didn't mark, when its name carries a
/// filtered column (`ix_orders_promo_code` for `promo_code`).
pub(super) fn possible_keys_for_filter(cx: &Cx, n: &PlanNode) -> Vec<String> {
    let facts = n.facts();
    let keys = facts.possible_index_names.clone();
    let mut up = cx.parent(n);
    let mut joined = false;
    while let Some(p) = up {
        joined |= p.kind == NodeKind::Join;
        up = cx.parent(p);
    }
    if !joined {
        return keys;
    }
    let columns: Vec<String> =
        n.predicate.as_deref().map(filter_columns).unwrap_or_default().iter().map(|c| c.to_ascii_lowercase()).collect();
    if let Some(described) = &facts.possible_index_columns {
        let leading: HashMap<&str, String> = described
            .iter()
            .map(|i| (i.name.as_str(), i.columns.first().map_or(String::new(), |c| c.to_ascii_lowercase())))
            .collect();
        return keys
            .into_iter()
            .filter(|k| leading.get(k.as_str()).is_some_and(|lead| columns.iter().any(|c| c == lead)))
            .collect();
    }
    keys.into_iter()
        .filter(|k| {
            let name = k.to_ascii_lowercase();
            columns.iter().any(|c| {
                name.match_indices(c.as_str()).any(|(at, _)| {
                    let before = name[..at].chars().last();
                    let after = name[at + c.len()..].chars().next();
                    before.is_none_or(|b| !b.is_ascii_alphanumeric())
                        && after.is_none_or(|a| !a.is_ascii_alphanumeric())
                })
            })
        })
        .collect()
}

/// Each CONVERT_IMPLICIT operand in a predicate as written: `[db].[s].[t].[c]`, `[@1]`, `N'x'`, `[Expr1004]`.
pub(super) fn convert_operands(text: &str) -> Vec<&str> {
    convert_calls(text).into_iter().map(|(_, operand)| operand).collect()
}

/// Each CONVERT_IMPLICIT as (the type it converts to, its operand): `nvarchar(64)`, `[db].[s].[t].[c]`.
pub(super) fn convert_calls(text: &str) -> Vec<(&str, &str)> {
    const CALL: &str = "CONVERT_IMPLICIT(";
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(i) = rest.find(CALL) {
        let after = &rest[i + CALL.len()..];
        // The target type comes first and may have its own parentheses: nvarchar(200).
        let mut depth = 0i32;
        let start = after.char_indices().find_map(|(j, ch)| {
            match ch {
                '(' => depth += 1,
                ')' => depth -= 1,
                ',' if depth == 0 => return Some(j + 1),
                _ => {}
            }
            None
        });
        if let Some(start) = start {
            let operand = &after[start..];
            let (mut depth, mut bracket, mut quote) = (0i32, false, false);
            let mut end = None;
            for (j, ch) in operand.char_indices() {
                match ch {
                    '[' if !quote => bracket = true,
                    ']' if !quote => bracket = false,
                    '\'' if !bracket => quote = !quote,
                    '(' if !bracket && !quote => depth += 1,
                    ')' if !bracket && !quote => {
                        if depth == 0 {
                            end = Some(j);
                            break;
                        }
                        depth -= 1;
                    }
                    ',' if depth == 0 && !bracket && !quote => {
                        end = Some(j);
                        break;
                    }
                    _ => {}
                }
            }
            // An incomplete saved expression is not a column name. Older parsers shortened predicates in the
            // middle of operands; retain no advice from those fragments when the raw plan is unavailable.
            if let Some(end) = end {
                out.push((after[..start - 1].trim(), operand[..end].trim()));
            }
        }
        rest = after;
    }
    out
}

/// The columns `text` implicitly converts, as ("table.column", the type SQL Server converts it to), leaving out those
/// `plan` was captured before they changed (see [`super::CHANGED_SINCE_CAPTURE`]): the mismatches the database still has.
/// `CONVERT_IMPLICIT(nvarchar(64),[db].[s].[jobs].[name],0)=[@p1]` → [("jobs.name", "nvarchar(64)")].
pub fn current_conversions(plan: &Plan, text: &str) -> Vec<(String, String)> {
    let facts = plan.facts();
    convert_calls(text)
        .into_iter()
        .filter(|(_, operand)| is_column_operand(operand))
        .filter_map(|(target, operand)| {
            let reference = column_reference(plan, operand)?;
            let changed = facts.changed_columns.iter().any(|c| {
                reference.table.as_deref().is_some_and(|table| c.table.eq_ignore_ascii_case(table))
                    && c.column.eq_ignore_ascii_case(&reference.column)
                    && same_type(&c.data_type, target)
                    && c.schema
                        .as_deref()
                        .is_none_or(|s| reference.schema.as_deref().is_some_and(|other| s.eq_ignore_ascii_case(other)))
            });
            (!changed).then(|| (reference.label, target.to_string()))
        })
        .collect()
}

pub(super) struct ColumnReference {
    pub(super) label: String,
    pub(super) schema: Option<String>,
    pub(super) table: Option<String>,
    pub(super) column: String,
}

pub(super) fn column_reference(plan: &Plan, operand: &str) -> Option<ColumnReference> {
    // Showplan can append `as [alias].[column]` to its physical column. An AS inside a delimited name is a name.
    let mut quote = None;
    let mut end = operand.len();
    let mut chars = operand.char_indices().peekable();
    while let Some((at, c)) = chars.next() {
        if let Some(close) = quote {
            if c == close {
                if chars.peek().is_some_and(|(_, next)| *next == close) {
                    chars.next();
                } else {
                    quote = None;
                }
            }
        } else if operand.get(at..at + 4).is_some_and(|part| part.eq_ignore_ascii_case(" as ")) {
            end = at;
            break;
        } else {
            quote = match c {
                '[' => Some(']'),
                '"' => Some('"'),
                '`' => Some('`'),
                _ => None,
            };
        }
    }
    let parts = identifier_parts(&operand[..end])?;
    let column = parts.last()?.clone();
    let label = match parts.as_slice() {
        [.., table, column] => format!("{table}.{column}"),
        [column] => column.clone(),
        _ => return None,
    };
    let (mut schema, mut table) = match parts.as_slice() {
        [.., schema, table, _] => (Some(schema.clone()), Some(table.clone())),
        [table, _] => (None, Some(table.clone())),
        _ => (None, None),
    };
    if let [alias, _] = parts.as_slice() {
        let aliases: Vec<_> = plan
            .nodes()
            .into_iter()
            .filter(|n| n.facts().table_alias.as_ref().is_some_and(|a| a.eq_ignore_ascii_case(alias)))
            .collect();
        if !aliases.is_empty() {
            let objects: Option<Vec<_>> = aliases.iter().map(|n| identifier_parts(n.object.as_deref()?)).collect();
            let unique =
                objects.as_ref().and_then(|all| all.first().filter(|first| all.iter().all(|other| other == *first)));
            match unique.map(Vec::as_slice) {
                Some([s, t]) => {
                    schema = Some(s.clone());
                    table = Some(t.clone());
                }
                Some([t]) => {
                    schema = None;
                    table = Some(t.clone());
                }
                _ => {
                    schema = None;
                    table = None;
                }
            }
        }
    }
    Some(ColumnReference { label, schema, table, column })
}

/// Whether a column of type `now` needs no conversion to `converted_to`: the same type, or text of the same kind.
pub(super) fn same_type(now: &str, converted_to: &str) -> bool {
    let norm = |t: &str| t.to_ascii_lowercase().replace(' ', "");
    let (now, to) = (norm(now), norm(converted_to));
    let base = |t: &str| t.split('(').next().unwrap_or("").to_string();
    now == to || (matches!(base(&now).as_str(), "varchar" | "nvarchar" | "char" | "nchar") && base(&now) == base(&to))
}

/// How to fix a column SQL Server converts to `target` to compare it with a value of that type: make the types match,
/// from either side. Which side is right depends on the schema (a team that keeps text as Unicode changes the column),
/// so both are offered. The one wording for every type mismatch, in plan insights and in findings.
pub fn mismatch_fix(column: Option<&str>, target: Option<&str>) -> String {
    let col = column.unwrap_or("the column");
    let change = match target {
        Some(t) => format!("change {col} to {t}"),
        None => format!("change {col} to the type the application sends"),
    };
    let unicode = target.is_some_and(|t| {
        let t = t.to_ascii_lowercase();
        t.starts_with("nvarchar") || t.starts_with("nchar")
    });
    if unicode {
        format!(
            "Make the types match: send the value as non-Unicode text (SqlDbType.VarChar in .NET, sql.VarChar in Node's mssql, a non-Unicode mapping in your ORM), or, if your schema keeps text as Unicode, {change}. Changing a column rewrites it and the indexes on it."
        )
    } else {
        format!(
            "Make the types match: send the value with {col}'s own type, or {change} if that's what it should hold. Changing a column rewrites it and the indexes on it."
        )
    }
}

/// A CONVERT_IMPLICIT operand that is a column, not a parameter (`@1`), variable, literal or a value the optimizer made
/// up (`[Expr1002]`, `[Union1006]`, `[Bmk1000]`: letters then a number, never qualified by a table).
pub(super) fn is_column_operand(op: &str) -> bool {
    let last = op.rsplit('.').next().unwrap_or(op).trim_matches(|c| c == '[' || c == ']' || c == '"');
    let digits = last.trim_start_matches(|c: char| c.is_ascii_alphabetic());
    let computed = !op.contains('.')
        && digits.len() >= 3
        && digits.len() < last.len()
        && digits.chars().all(|c| c.is_ascii_digit());
    !last.is_empty()
        && !op.contains('(')
        && !op.starts_with('\'')
        && !op.starts_with("N'")
        && !last.starts_with('@')
        && !last.starts_with(|c: char| c.is_ascii_digit() || c == '-')
        && !computed
}

/// Whether SQL Server text (a predicate, or a conversion warning's expression) implicitly converts a table's column to
/// compare it with a value of another type: the mismatch that costs an index seek or a good estimate. A conversion the
/// query writes itself (`CONVERT(varchar(36), id)` in a select list) is asked for, and converting a parameter, literal
/// or the optimizer's own value costs nothing: neither is a mismatch. The one rule the plan parser, insights and checks
/// share.
pub fn converts_column(text: &str) -> bool {
    convert_operands(text).into_iter().any(is_column_operand)
}

/// Functions that hide a column from its index when wrapped around it.
const WRAPPERS: &[&str] = &[
    "year",
    "month",
    "day",
    "datepart",
    "datediff",
    "dateadd",
    "date",
    "date_trunc",
    "date_format",
    "date_part",
    "extract",
    "to_char",
    "convert",
    "cast",
    "isnull",
    "ifnull",
    "coalesce",
    "nvl",
    "upper",
    "lower",
    "ltrim",
    "rtrim",
    "trim",
    "substring",
    "substr",
    "left",
    "right",
    "len",
    "length",
    "char_length",
    "abs",
    "replace",
    "format",
    "round",
];

/// `(function, column)` for the first function wrapped around a column in a filter, or a cast on a column. SQL Server
/// seeks through a cast of a date and time column to `date` (a dynamic seek), so that one doesn't count there.
pub(super) fn wrapped_column(text: &str, engine: Engine) -> Option<(String, String)> {
    let lower = text.to_ascii_lowercase();
    for f in WRAPPERS {
        let mut from = 0;
        while let Some(at) = lower[from..].find(&format!("{f}(")) {
            let start = from + at;
            from = start + f.len() + 1;
            let before = lower[..start].chars().last();
            if before.is_some_and(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.') {
                continue;
            }
            let args_start = start + f.len() + 1;
            let args = balanced(&text[args_start..]);
            let args_lower = args.to_ascii_lowercase();
            if args_lower.contains("convert_implicit") {
                continue;
            }
            let to_date = args_lower.trim_start().starts_with("date,") || args_lower.trim_end().ends_with(" as date");
            if engine == Engine::Mssql && matches!(*f, "convert" | "cast") && to_date {
                continue;
            }
            // The value converted, not the type it is converted to: CONVERT(datetime, @from, 0) converts a parameter,
            // CAST(? AS datetime) a value; neither hides a column.
            let operand = match *f {
                "convert" => args.split_once(',').map_or(args, |(_, rest)| rest),
                "cast" => args_lower.rfind(" as ").map_or(args, |at| &args[..at]),
                _ => args,
            };
            if let Some(col) = identifiers(operand).into_iter().next() {
                return Some((f.to_ascii_uppercase(), col));
            }
        }
    }
    // Postgres casts on a column: `(created_at)::date`, `(o.total)::integer`. Casts to text are harmless (varchar).
    let mut from = 0;
    while let Some(at) = lower[from..].find(")::") {
        let end = from + at;
        from = end + 3;
        let ty: String = lower[end + 3..].chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '_').collect();
        if matches!(ty.as_str(), "text" | "varchar" | "bpchar" | "name" | "character" | "unknown" | "regclass") {
            continue;
        }
        let open = lower[..end].rfind('(')?;
        let inner = &text[open + 1..end];
        if let Some(col) = identifiers(inner).into_iter().next() {
            return Some((format!("::{ty}"), col));
        }
    }
    None
}

/// Where a LIKE with a leading wildcard starts in a condition.
pub(super) fn wildcard_at(filter: &str) -> Option<usize> {
    let lower = filter.to_ascii_lowercase();
    ["like n'%", "like '%", "~~ '%", "~~* '%", "ilike '%", "like _utf8mb4'%"].iter().find_map(|p| lower.find(p))
}
