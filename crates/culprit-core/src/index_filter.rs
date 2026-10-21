//! Exact, deliberately limited static-predicate matching shared by checks and privacy-preserving evidence.
//! Unsupported expressions remain unknown; this is not an engine's general SQL predicate evaluator.
use std::ops::Range;

use crate::Engine;
use crate::sql::{identifier_parts, quoted_end, skip_quoted_bytes, split_top_level_and, strip_outer_parens};

/// Column/literal equalities only. Unsupported predicates remain unknown.
pub fn filter_equalities(filter: &str) -> Option<Vec<(String, String)>> {
    let clauses = split_top_level_and(filter);
    if clauses.is_empty() {
        return None;
    }
    clauses
        .into_iter()
        .map(|clause| {
            let (lhs, rhs) = split_top_level_eq(clause)?;
            let column = column_operand(lhs)?;
            let literal = strip_outer_parens(rhs);
            is_literal(literal).then(|| (column, literal.to_string()))
        })
        .collect::<Option<Vec<_>>>()
        .filter(|c| !c.is_empty())
}

fn split_top_level_eq(clause: &str) -> Option<(&str, &str)> {
    let clause = strip_outer_parens(clause);
    let bytes = clause.as_bytes();
    let mut depth = 0i32;
    let mut eq_at = None;
    let mut i = 0;
    while i < bytes.len() {
        let close = match bytes[i] {
            b'\'' => Some(b'\''),
            b'"' => Some(b'"'),
            b'`' => Some(b'`'),
            b'[' => Some(b']'),
            _ => None,
        };
        if let Some(close) = close {
            i = skip_quoted_bytes(bytes, i, close, false);
            continue;
        }
        match bytes[i] {
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth < 0 {
                    return None;
                }
            }
            b'<' | b'>' | b'!' if depth == 0 => return None,
            b'=' if depth == 0 => {
                if bytes.get(i + 1) == Some(&b'=') || eq_at.is_some() {
                    return None;
                }
                eq_at = Some(i);
            }
            _ => {}
        }
        i += 1;
    }
    if depth != 0 {
        return None;
    }
    eq_at.map(|at| (&clause[..at], &clause[at + 1..]))
}

fn column_operand(raw: &str) -> Option<String> {
    let raw = strip_outer_parens(raw);
    if is_literal(raw) {
        return None;
    }
    let parts = identifier_parts(raw)?;
    let valid_part = |part: &str| {
        !part.is_empty()
            && !is_literal(part)
            && part.chars().all(|c| !c.is_ascii() || c.is_ascii_alphanumeric() || matches!(c, '_' | '@' | '#' | '$'))
    };
    if !parts.iter().all(|p| valid_part(p)) {
        return None;
    }
    parts.into_iter().last()
}

pub fn is_literal(word: &str) -> bool {
    let word = word.trim();
    if word.parse::<f64>().is_ok() && !word.chars().any(char::is_whitespace) {
        return true;
    }
    let quoted = word.strip_prefix(['n', 'N']).filter(|rest| rest.starts_with('\'')).unwrap_or(word);
    quoted.starts_with('\'') && quoted_end(quoted.as_bytes(), 0, b'\'', false) == Ok(quoted.len())
}

/// Numeric equivalence and the string comparison used by the index advisor, without guessing parameters.
pub fn same_literal(a: &str, b: &str) -> bool {
    let plain = |v: &str| {
        let v = strip_outer_parens(v);
        v.strip_prefix(['n', 'N']).filter(|rest| rest.starts_with('\'')).unwrap_or(v).to_ascii_lowercase()
    };
    let (a, b) = (plain(a), plain(b));
    match (a.parse::<f64>(), b.parse::<f64>()) {
        (Ok(x), Ok(y)) => x == y,
        _ => !a.is_empty() && a == b,
    }
}

/// Whether `text` compares `table`'s `column` with `=` to a value `wanted` accepts, on either side, the column perhaps
/// bracketed or qualified: `[column] = @p`, `t.column = 0`, `N'open' = column`. A column qualified by another table's
/// name or alias (`[app].[call_sessions].[is_deleted] = 0` in a join) is that table's, not this one's. Text is compared
/// lower-cased, its aliases read as SQL Server writes them (the optimizer's requests and Azure's are its own).
pub fn compared_to(text: &str, table: &str, column: &str, wanted: impl Fn(&str) -> bool) -> bool {
    let (plain, literals) = normalize_for_comparison(text);
    let column = column.to_ascii_lowercase();
    let table = table.to_ascii_lowercase();
    let aliases = crate::sql::table_aliases(text, Engine::Mssql);
    let ours = |qualifier: &str| {
        qualifier == table
            || aliases.iter().any(|(alias, name)| alias == qualifier && name.rsplit('.').next() == Some(table.as_str()))
    };
    // Unknown non-ASCII identifier characters must stay with their token, never become a boundary inside it.
    let part = |c: char| !c.is_ascii() || c.is_ascii_alphanumeric() || matches!(c, '_' | '@' | '#' | '$');
    plain.match_indices(&column).any(|(at, _)| {
        if literals.iter().any(|range| range.contains(&at)) {
            return false;
        }
        let end = at + column.len();
        if plain[..at].chars().next_back().is_some_and(part)
            || plain[end..].chars().next().is_some_and(part)
            || plain[..at].ends_with('\'')
            || plain[end..].starts_with('\'')
        {
            return false;
        }
        // The column perhaps qualified: by this table, or an alias the statement gives it.
        let mut before = &plain[..at];
        if let Some(qualified) = before.strip_suffix('.') {
            let unqualified = qualified.trim_end_matches(|c: char| c.is_ascii_alphanumeric() || c == '_');
            if !ours(&qualified[unqualified.len()..]) {
                return false;
            }
            before = unqualified;
        }
        // `column = value`
        if let Some(value) = plain[end..].trim_start().strip_prefix('=')
            && !value.starts_with('=')
        {
            return wanted(operand_after(value));
        }
        // `value = column`
        before.trim_end().strip_suffix('=').is_some_and(|lhs| {
            // `<=`, `>=` and `!=` aren't equality.
            !lhs.ends_with(['<', '>', '!', '=']) && wanted(operand_before(lhs))
        })
    })
}

/// Lowercases `text`, strips identifier delimiters (`[`, `]`, `"`) and SQL comments outside string literals, and
/// returns the byte ranges of single-quoted string literals in the normalized output so column names inside literals
/// are never mistaken for predicate columns.
fn normalize_for_comparison(text: &str) -> (String, Vec<Range<usize>>) {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut literals = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\'' {
            let start = out.len();
            let end = skip_quoted_bytes(bytes, i, b'\'', false);
            out.push_str(&text[i..end].to_ascii_lowercase());
            literals.push(start..out.len());
            i = end;
            continue;
        }
        if bytes[i] == b'-' && bytes.get(i + 1) == Some(&b'-') {
            out.push(' ');
            i += 2;
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if bytes[i] == b'/' && bytes.get(i + 1) == Some(&b'*') {
            out.push(' ');
            i += 2;
            while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                i += 1;
            }
            i = (i + 2).min(bytes.len());
            continue;
        }
        let ch = text[i..].chars().next().unwrap();
        if !matches!(ch, '[' | ']' | '"') {
            out.push(ch.to_ascii_lowercase());
        }
        i += ch.len_utf8();
    }
    (out, literals)
}

/// The value right after `=`: a parameter, a number or a quoted string, past any opening parentheses.
pub fn operand_after(text: &str) -> &str {
    let text = text.trim_start().trim_start_matches(|c: char| c == '(' || c.is_whitespace());
    let quote = if text.starts_with('\'') {
        Some(0)
    } else if text.starts_with("n'") || text.starts_with("N'") {
        Some(1)
    } else {
        None
    };
    match quote {
        Some(open) => match quoted_end(text.as_bytes(), open, b'\'', false) {
            Ok(end) => &text[..end],
            Err(_) => text,
        },
        None => {
            let end = text
                .find(|c: char| {
                    c.is_ascii() && !(c.is_ascii_alphanumeric() || matches!(c, '_' | '@' | '#' | '$' | '.' | '-' | '+'))
                })
                .unwrap_or(text.len());
            &text[..end]
        }
    }
}

/// The value right before `=`, the same way.
pub fn operand_before(text: &str) -> &str {
    let text = text.trim_end().trim_end_matches(|c: char| c == ')' || c.is_whitespace());
    if text.ends_with('\'') {
        let bytes = text.as_bytes();
        let mut cursor = bytes.len() - 1;
        let open = loop {
            let Some(pos) = text[..cursor].rfind('\'') else { return text };
            if pos > 0 && bytes[pos - 1] == b'\'' {
                cursor = pos - 1;
                continue;
            }
            break pos;
        };
        let is_national_prefix = open > 0
            && matches!(bytes[open - 1], b'n' | b'N')
            && !text[..open - 1]
                .chars()
                .next_back()
                .is_some_and(|c| !c.is_ascii() || c.is_ascii_alphanumeric() || matches!(c, '_' | '@' | '#' | '$'));
        let start = if is_national_prefix { open - 1 } else { open };
        return &text[start..];
    }
    let start = text
        .char_indices()
        .rfind(|(_, c)| {
            c.is_ascii() && !(c.is_ascii_alphanumeric() || matches!(c, '_' | '@' | '#' | '$' | '.' | '-' | '+'))
        })
        .map_or(0, |(at, c)| at + c.len_utf8());
    &text[start..]
}

/// Whether the statement text establishes every literal equality in this index predicate.
pub fn matches(table: &str, filter: &str, text: &str) -> bool {
    let Some(conditions) = filter_equalities(filter) else {
        return false;
    };
    conditions.iter().all(|(column, literal)| compared_to(text, table, column, |value| same_literal(value, literal)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unicode_column_operands_remain_unknown_without_panicking_or_hiding_a_supported_equality() {
        for column in ["名称", "café", "名称1", "café1"] {
            let query = format!("SELECT * FROM shop.orders WHERE [{column}] = tenant");
            for literal in ["'acme'", "1"] {
                let filter = format!("tenant = {literal}");
                assert!(!matches("orders", &filter, &query));
                assert!(matches("orders", &filter, &format!("{query} AND tenant = {literal}")));
            }
        }
        for column in ["名tenant", "tenant名"] {
            let query = format!("SELECT * FROM shop.orders WHERE [{column}] = 1");
            assert!(!matches("orders", "tenant = 1", &query));
        }
        assert!(matches("orders", "名称 = 1", "SELECT * FROM shop.orders WHERE [名称] = 1"));
    }

    #[test]
    fn string_literals_with_spaces_keywords_operators_and_escaped_quotes_match_cleanly() {
        assert_eq!(
            filter_equalities("([status]=N'in progress' AND [kind]='a and b' AND [tag]='x=y' AND [note]='it''s ok')"),
            Some(vec![
                ("status".into(), "N'in progress'".into()),
                ("kind".into(), "'a and b'".into()),
                ("tag".into(), "'x=y'".into()),
                ("note".into(), "'it''s ok'".into()),
            ])
        );
        let query = "SELECT * FROM dbo.orders WHERE status = N'in progress' AND 'a and b' = kind AND tag = 'x=y' AND ('it''s ok') = note";
        assert!(matches(
            "orders",
            "([status]=N'in progress' AND [kind]='a and b' AND [tag]='x=y' AND [note]='it''s ok')",
            query
        ));
    }

    #[test]
    fn non_equality_operators_without_spaces_and_comment_or_string_decoys_do_not_match() {
        for filter in ["status!=1", "status<=1", "status>=1", "status<>1"] {
            assert_eq!(filter_equalities(filter), None, "{filter} is not an equality predicate");
        }
        assert!(!matches("orders", "status = 0", "SELECT 'status = 0' FROM dbo.orders WHERE status != 0"));
        assert!(!matches("orders", "status = 0", "SELECT * FROM dbo.orders /* status = 0 */ WHERE status = 1"));
        assert!(!matches("orders", "status = 0", "SELECT * FROM dbo.orders -- status = 0\nWHERE status = 1"));
        assert!(!matches("orders", "n = 'x'", "SELECT * FROM dbo.orders WHERE 'x' = N'open'"));
        assert!(matches(
            "orders",
            "status = 'open'",
            "SELECT * FROM dbo.orders WHERE CASE WHEN'open' = status THEN 1 ELSE 0 END = 1"
        ));
    }
}
