//! Exact, deliberately limited static-predicate matching shared by checks and privacy-preserving evidence.
//! Unsupported expressions remain unknown; this is not an engine's general SQL predicate evaluator.
use crate::Engine;

/// Column/literal equalities only. Unsupported predicates remain unknown.
pub fn filter_equalities(filter: &str) -> Option<Vec<(String, String)>> {
    let plain: String = filter.chars().filter(|c| !matches!(c, '[' | ']' | '"' | '(' | ')')).collect();
    let spaced = plain.replace('=', " = ");
    let words: Vec<&str> = spaced.split_whitespace().collect();
    words
        .split(|w| w.eq_ignore_ascii_case("and"))
        .map(|condition| match condition {
            [column, "=", literal] if is_literal(literal) => {
                Some((column.rsplit('.').next().unwrap_or(column).to_string(), literal.to_string()))
            }
            _ => None,
        })
        .collect::<Option<Vec<_>>>()
        .filter(|c| !c.is_empty())
}

pub fn is_literal(word: &str) -> bool {
    word.parse::<f64>().is_ok() || (word.ends_with('\'') && (word.starts_with('\'') || word.starts_with("N'")))
}

/// Numeric equivalence and the string comparison used by the index advisor, without guessing parameters.
pub fn same_literal(a: &str, b: &str) -> bool {
    let plain = |v: &str| {
        let v = v.trim().trim_matches(|c: char| c == '(' || c == ')' || c.is_whitespace());
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
    let plain = text.chars().filter(|c| !matches!(c, '[' | ']' | '"')).collect::<String>().to_ascii_lowercase();
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
        let end = at + column.len();
        if plain[..at].chars().next_back().is_some_and(part) || plain[end..].chars().next().is_some_and(part) {
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
        if let Some(value) = plain[end..].trim_start().strip_prefix('=') {
            return wanted(operand_after(value));
        }
        // `value = column`
        before.trim_end().strip_suffix('=').is_some_and(|lhs| {
            // `<=`, `>=` and `!=` aren't equality.
            !lhs.ends_with(['<', '>', '!']) && wanted(operand_before(lhs))
        })
    })
}

/// The value right after `=`: a parameter, a number or a quoted string, past any opening parentheses.
pub fn operand_after(text: &str) -> &str {
    let text = text.trim_start().trim_start_matches(|c: char| c == '(' || c.is_whitespace());
    let quote = if text.starts_with('\'') {
        Some(0)
    } else if text.starts_with("n'") {
        Some(1)
    } else {
        None
    };
    match quote {
        Some(open) => match text[open + 1..].find('\'') {
            Some(close) => &text[..open + 1 + close + 1],
            None => text,
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
    if let Some(inner) = text.strip_suffix('\'') {
        let Some(open) = inner.rfind('\'') else { return text };
        let start = if text[..open].ends_with('n') { open - 1 } else { open };
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
}
