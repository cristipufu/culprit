#[derive(Debug, Default, PartialEq)]
pub struct PredicateColumns {
    pub equality: Vec<String>,
    pub range: Vec<String>,
}

/// Comparisons a default b-tree index can seek, as (operator, equality). Not-equal (`<>`, `!=`) and PostgreSQL's
/// LIKE (`~~`, `~~*`) are left out on purpose: see [`predicate_columns`].
const OPERATORS: [(&str, bool); 10] = [
    (" = ANY ", true),
    (" IN ", true),
    (" BETWEEN ", false),
    (" LIKE ", false),
    (">=", false),
    ("<=", false),
    ("<=>", true),
    ("=", true),
    ("<", false),
    (">", false),
];

/// Comparisons no default b-tree index serves: a condition that has one names no column to index.
const NOT_SEEKABLE: [&str; 5] = ["<>", "!=", "~~", "!~", " NOT "];

/// Columns compared against a parameter or literal in a plan predicate, e.g. `(customer_id = $1)` or
/// `[db].[dbo].[Orders].[CustomerId] as [m].[CustomerId]=[@P1]`: the columns an index on the filter would seek. Only
/// conditions every row must meet count: a branch of an OR (`status = 'open' OR priority > 3`) doesn't, since an index
/// on one branch's column can't find the other branch's rows. Column-to-column comparisons (joins) are skipped, and so
/// are comparisons a default b-tree index can't seek: not-equal on any engine, NOT anything, a LIKE whose pattern
/// starts with a wildcard, and PostgreSQL's LIKE (`~~`), which needs the C collation or a `text_pattern_ops` index.
pub fn predicate_columns(predicate: &str) -> PredicateColumns {
    let cleaned = strip_casts(&predicate.replace(['[', ']', '"', '`'], ""));
    let mut out = PredicateColumns::default();
    for conjunct in conjuncts(&cleaned) {
        let conjunct = conjunct.replace(['(', ')'], " ");
        let conjunct = conjunct.split_whitespace().collect::<Vec<_>>().join(" ");
        let upper = conjunct.to_ascii_uppercase();
        let unquoted = format!(" {} ", upper.split('\'').step_by(2).collect::<Vec<_>>().join("''"));
        if NOT_SEEKABLE.iter().any(|op| unquoted.contains(op)) {
            continue;
        }
        let Some((at, op, equality)) = OPERATORS
            .iter()
            .filter_map(|(op, eq)| find_unquoted(&upper, op).map(|at| (at, *op, *eq)))
            .min_by_key(|(at, op, _)| (*at, std::cmp::Reverse(op.len())))
        else {
            continue;
        };
        let left = drop_alias(conjunct[..at].trim());
        let right = drop_alias(conjunct[at + op.len()..].trim());
        // `LIKE '%abc'` reads every key: only a pattern with a fixed start can seek.
        if op == " LIKE " && starts_with_wildcard(right) {
            continue;
        }
        let column = match (column_name(left), column_name(right)) {
            (Some(c), None) if is_value(right) => c,
            (None, Some(c)) if is_value(left) && op != " LIKE " && op != " BETWEEN " => c,
            _ => continue,
        };
        let list = if equality { &mut out.equality } else { &mut out.range };
        if !list.iter().any(|c| c.eq_ignore_ascii_case(&column)) {
            list.push(column);
        }
    }
    out.range.retain(|r| !out.equality.iter().any(|e| e.eq_ignore_ascii_case(r)));
    out
}

/// The conditions every row must meet: the parts of a predicate joined by AND at its top level, nested ANDs taken
/// apart, and any part that holds an OR dropped (its rows can come from either branch). `x BETWEEN a AND b` stays one
/// condition. Quoted text is never split.
fn conjuncts(text: &str) -> Vec<String> {
    let text = without_outer_parens(text.trim());
    let mut merged: Vec<String> = Vec::new();
    let mut between = false;
    for part in split_top_level(text, " AND ") {
        // BETWEEN's own AND joins its bounds back.
        if between && let Some(last) = merged.last_mut() {
            last.push_str(" AND ");
            last.push_str(part);
            between = false;
            continue;
        }
        between = split_top_level(part, " BETWEEN ").len() == 2;
        merged.push(part.to_string());
    }
    if merged.len() == 1 {
        let only = &merged[0];
        // An OR anywhere in a condition (`a = 1 OR b = 2`, `NOT (a = 1 OR …)`) leaves no column every row must match.
        return if contains_keyword(only, " OR ") { Vec::new() } else { vec![only.clone()] };
    }
    merged.iter().flat_map(|part| conjuncts(part)).collect()
}

/// `((a = 1))` → `a = 1`: parentheses around the whole text only.
fn without_outer_parens(mut text: &str) -> &str {
    while text.starts_with('(') && text.ends_with(')') && closing_paren(text) == Some(text.len() - 1) {
        text = text[1..text.len() - 1].trim();
    }
    text
}

/// Where the parenthesis opening `text` closes, skipping quoted text.
fn closing_paren(text: &str) -> Option<usize> {
    let mut depth = 0usize;
    let mut quoted = false;
    for (i, c) in text.char_indices() {
        match c {
            '\'' => quoted = !quoted,
            '(' if !quoted => depth += 1,
            ')' if !quoted => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
    }
    None
}

/// `text` split at `separator` (an ASCII keyword with its spaces, matched case-insensitively) where it stands outside
/// parentheses and quotes.
fn split_top_level<'a>(text: &'a str, separator: &str) -> Vec<&'a str> {
    let upper = text.to_ascii_uppercase();
    let bytes = upper.as_bytes();
    let mut parts = Vec::new();
    let (mut depth, mut quoted, mut start, mut i) = (0i32, false, 0, 0);
    while i < bytes.len() {
        match bytes[i] {
            b'\'' => quoted = !quoted,
            b'(' if !quoted => depth += 1,
            b')' if !quoted => depth -= 1,
            // Bytes, not a string slice: `i` can be inside a multi-byte character, where slicing would panic.
            _ if !quoted && depth == 0 && bytes[i..].starts_with(separator.as_bytes()) => {
                parts.push(&text[start..i]);
                i += separator.len();
                start = i;
                continue;
            }
            _ => {}
        }
        i += 1;
    }
    parts.push(&text[start..]);
    parts
}

/// Whether `keyword` (with its spaces) appears outside quotes, at any depth.
fn contains_keyword(text: &str, keyword: &str) -> bool {
    let unquoted: String = text.split('\'').step_by(2).collect::<Vec<_>>().join("''");
    unquoted.replace(['(', ')'], " ").to_ascii_uppercase().contains(keyword)
}

/// First byte index in `upper` where `needle` appears outside single-quoted literals.
fn find_unquoted(upper: &str, needle: &str) -> Option<usize> {
    let bytes = upper.as_bytes();
    let needle = needle.as_bytes();
    let mut quoted = false;
    for i in 0..bytes.len() {
        if bytes[i] == b'\'' {
            quoted = !quoted;
            continue;
        }
        if !quoted && bytes[i..].starts_with(needle) {
            return Some(i);
        }
    }
    None
}

/// `'%abc'`, `N'_bc'`: a LIKE pattern that doesn't start with fixed text.
fn starts_with_wildcard(pattern: &str) -> bool {
    let p = pattern.trim();
    let p = p.strip_prefix(['N', 'n']).filter(|rest| rest.starts_with('\'')).unwrap_or(p);
    p.strip_prefix('\'').is_some_and(|rest| rest.starts_with(['%', '_']))
}

/// A predicate without PostgreSQL's casts. A cast of a column to another type (`(created_at)::date`) hides it from its index like a function would: it stays
/// as ` ::cast` so the side is no bare column and gives no key. Casts of values, and to text (how PostgreSQL prints a
/// varchar column compared with a string), are harmless and go.
fn strip_casts(text: &str) -> String {
    const MULTIWORD: [&str; 4] = [" with time zone", " without time zone", " varying", " precision"];
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(pos) = rest.find("::") {
        out.push_str(&rest[..pos]);
        let after = &rest[pos + 2..];
        let ty: String = after.chars().take_while(|c| c.is_alphanumeric() || *c == '_').collect();
        rest = &after[ty.len()..];
        while let Some(suffix) = MULTIWORD.iter().find(|m| rest.to_ascii_lowercase().starts_with(*m)) {
            rest = &rest[suffix.len()..];
        }
        if let Some(stripped) = rest.strip_prefix("[]") {
            rest = stripped;
        }
        let harmless = matches!(
            ty.to_ascii_lowercase().as_str(),
            "text" | "varchar" | "character" | "bpchar" | "name" | "unknown"
        );
        if !harmless && cast_operand_is_column(&out) {
            out.push_str(" ::cast");
        }
    }
    out.push_str(rest);
    out
}

/// Whether what `before` ends with, the operand of a cast that follows it, is a column: `(created_at)` or
/// `o.created_at`, not `'2026-01-01'`, `$1`, `now()` or a number.
fn cast_operand_is_column(before: &str) -> bool {
    let trimmed = before.trim_end();
    let operand = if let Some(inner) = trimmed.strip_suffix(')') {
        let mut depth = 1;
        let open = inner.char_indices().rev().find(|(_, c)| {
            match c {
                ')' => depth += 1,
                '(' => depth -= 1,
                _ => {}
            }
            depth == 0
        });
        match open {
            // `now()`: a function call's parentheses, not a parenthesized column.
            Some((at, _)) if inner[..at].ends_with(|c: char| c.is_alphanumeric() || c == '_') => return false,
            Some((at, _)) => inner[at + 1..].trim(),
            None => return false,
        }
    } else {
        let start = trimmed
            .char_indices()
            .rev()
            .find(|(_, c)| !(c.is_alphanumeric() || matches!(c, '_' | '.' | '$')))
            .map_or(0, |(i, c)| i + c.len_utf8());
        &trimmed[start..]
    };
    !is_value(operand) && column_name(operand).is_some()
}

fn drop_alias(side: &str) -> &str {
    let upper = side.to_ascii_uppercase();
    match upper.find(" AS ") {
        Some(pos) => side[..pos].trim(),
        None => side,
    }
}

fn column_name(side: &str) -> Option<String> {
    let side = side.trim();
    if side.is_empty() || is_value(side) || side.contains(' ') {
        return None;
    }
    let last = side.rsplit('.').next()?.trim();
    let valid = last.chars().next().is_some_and(|c| c.is_alphabetic() || c == '_')
        && last.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '$');
    (valid && !last.contains("CONVERT_IMPLICIT")).then(|| last.to_string())
}

pub(super) fn is_value(side: &str) -> bool {
    let s = side.trim();
    let upper = s.to_ascii_uppercase();
    s.starts_with(['$', '@', '\'', '-', '?'])
        || s.chars().next().is_some_and(|c| c.is_ascii_digit())
        || upper.starts_with("NULL")
        || upper.starts_with("N'")
        // MySQL writes typed literals in plans: TIMESTAMP'2026-09-05 00:00:00'.
        || ["TIMESTAMP'", "DATE'", "TIME'"].iter().any(|p| upper.starts_with(p))
        || upper.starts_with("ARRAY")
        || upper.starts_with("{")
        || upper.starts_with("CONVERT_IMPLICIT") && (upper.contains("@") || upper.contains("$"))
        || upper.starts_with("NOW")
        || upper.starts_with("CURRENT_")
        || upper.starts_with("GETDATE")
        || upper.starts_with("SYSUTCDATETIME")
        || is_value_call(s)
}

/// A function of values alone is a value: `dateadd(day,@P1,'2026-01-01')` (or the same with its parentheses gone, as a
/// condition is read), `coalesce($1, 0)`, `date_sub(?, INTERVAL ? DAY)`. Every argument is a parameter, a literal, a
/// date part (`day`) or such a call, and at least one is a parameter or a literal. A function of a column
/// (`upper(name)`, `dateadd(day, 1, created_at)`) isn't.
fn is_value_call(s: &str) -> bool {
    let s = s.trim();
    let name_len = s.find(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '.')).unwrap_or(s.len());
    if name_len == 0 || name_len == s.len() || !s.starts_with(|c: char| c.is_ascii_alphabetic()) {
        return false;
    }
    // A call's name is followed by its parenthesis (or the space left where it was): a name that goes on with other
    // characters is a name, such as a column qualified by a database called `app-db-01` (not `app` minus a value).
    if !s[name_len..].starts_with(['(', ' ']) {
        return false;
    }
    let rest = s[name_len..].trim();
    let inner = rest.strip_prefix('(').and_then(|r| r.strip_suffix(')')).unwrap_or(rest);
    let mut any_value = false;
    for arg in top_level_args(inner) {
        let arg = arg.trim();
        let upper = arg.to_ascii_uppercase();
        let interval = upper.strip_prefix("INTERVAL ").and_then(|r| r.split_whitespace().next()).is_some_and(is_value);
        if is_value(arg) || interval {
            any_value = true;
        } else if !DATE_PARTS.contains(&upper.as_str()) {
            return false;
        }
    }
    any_value
}

/// The date parts SQL Server's date functions take as a bare word (`dateadd(day, …)`), and the units of an interval.
const DATE_PARTS: [&str; 38] = [
    "YEAR",
    "YY",
    "YYYY",
    "QUARTER",
    "QQ",
    "Q",
    "MONTH",
    "MM",
    "M",
    "DAYOFYEAR",
    "DY",
    "Y",
    "DAY",
    "DD",
    "D",
    "WEEK",
    "WK",
    "WW",
    "WEEKDAY",
    "DW",
    "W",
    "HOUR",
    "HH",
    "MINUTE",
    "MI",
    "N",
    "SECOND",
    "SS",
    "S",
    "MILLISECOND",
    "MS",
    "MICROSECOND",
    "MCS",
    "NANOSECOND",
    "NS",
    "ISO_WEEK",
    "ISOWK",
    "ISOWW",
];

/// `text` split at its commas outside quotes and parentheses: a call's arguments.
fn top_level_args(text: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let (mut depth, mut quoted, mut start) = (0i32, false, 0);
    for (i, c) in text.char_indices() {
        match c {
            '\'' => quoted = !quoted,
            '(' if !quoted => depth += 1,
            ')' if !quoted => depth -= 1,
            ',' if !quoted && depth == 0 => {
                parts.push(&text[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    parts.push(&text[start..]);
    parts
}
