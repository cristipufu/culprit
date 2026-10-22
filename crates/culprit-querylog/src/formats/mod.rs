//! The log formats Culprit reads, one module each. A new framework is a module implementing `Format` and a line in
//! [`formats`].

mod django;
mod ecto;
mod efcore;
mod jsonl;
mod rails;

use crate::{Format, Moment};

/// Every format, in the order they're tried when two recognize the same number of lines.
pub fn formats() -> Vec<Box<dyn Format>> {
    vec![
        Box::new(rails::Rails::default()),
        Box::new(django::Django),
        Box::new(efcore::EfCore::default()),
        Box::new(ecto::Ecto::default()),
        Box::new(jsonl::JsonLines),
    ]
}

/// A line without terminal colour codes (`\e[1m\e[36m…\e[0m`), which development logs are full of.
pub(crate) fn strip_ansi(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' && chars.peek() == Some(&'[') {
            chars.next();
            for c in chars.by_ref() {
                if c.is_ascii_alphabetic() {
                    break;
                }
            }
            continue;
        }
        out.push(c);
    }
    out
}

/// A timestamp at the start of a line (`2026-10-15 12:00:05,120 …`, `2026-10-15T12:00:05.120Z …`) and the rest of the
/// line. Without a zone it is local time, whose zone the log doesn't say.
pub(crate) fn leading_timestamp(line: &str) -> Option<(Moment, &str)> {
    let t = line.trim_start();
    let bytes = t.as_bytes();
    let date_like = bytes.len() >= 19
        && bytes[..4].iter().all(u8::is_ascii_digit)
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && (bytes[10] == b' ' || bytes[10] == b'T')
        && bytes[13] == b':'
        && bytes[16] == b':';
    if !date_like {
        return None;
    }
    // The timestamp runs to the first space after the time (and a zone right after it, as `+0000` or `Z`).
    let after_time = t[11..].find(' ').map_or(t.len(), |p| p + 11);
    if after_time < 19 {
        return None;
    }
    let mut stamp = t[..after_time].replace(',', ".");
    let mut rest = &t[after_time..];
    let zoned_inline = stamp.ends_with('Z') || stamp.get(19..).is_some_and(|s| s.contains(['+', '-']));
    let mut zoned = zoned_inline;
    if !zoned_inline {
        let next = rest.trim_start();
        if let Some(zone) = next.split_whitespace().next().filter(|z| is_zone(z)) {
            stamp.push(' ');
            stamp.push_str(zone);
            rest = &next[zone.len()..];
            zoned = true;
        }
    }
    let epoch = culprit_core::format::epoch_seconds(&stamp)?;
    Some((Moment { epoch, zoned }, rest))
}

fn is_zone(word: &str) -> bool {
    word == "UTC"
        || word == "Z"
        || ((word.starts_with('+') || word.starts_with('-'))
            && word.len() >= 5
            && word[1..].chars().all(|c| c.is_ascii_digit() || c == ':'))
}

/// A duration with its unit (`12.4ms`, `0.012s`, `1.2µs`, `500ns`) in milliseconds.
pub(crate) fn duration_ms(text: &str) -> Option<f64> {
    let t = text.trim();
    for (suffix, scale) in [("ms", 1.0), ("µs", 0.001), ("us", 0.001), ("ns", 0.000_001), ("s", 1000.0)] {
        if let Some(n) = t.strip_suffix(suffix) {
            return n.trim().parse::<f64>().ok().map(|v| v * scale);
        }
    }
    None
}

/// Skip leading whitespace, `-- …` line comments and `/* … */` block comments (`sqlcommenter`, Rails `QueryLogs`,
/// Entity Framework Core `.TagWith()`).
pub(crate) fn skip_leading_comments(mut text: &str) -> &str {
    loop {
        text = text.trim_start();
        if let Some(after) = text.strip_prefix("/*") {
            let Some((_, rest)) = after.split_once("*/") else { return "" };
            text = rest;
        } else if let Some(after) = text.strip_prefix("--") {
            let Some((_, rest)) = after.split_once(['\n', '\r']) else { return "" };
            text = rest;
        } else {
            return text;
        }
    }
}

/// Whether text starts like a SQL statement, so neither a log line that merely contains "(1.2ms)" nor a structured
/// log record whose `query` field is a search term ("select red shoes", "update profile") is mistaken for SQL.
pub(crate) fn starts_like_sql(text: &str) -> bool {
    let sql = skip_leading_comments(text);
    let word: String = sql.chars().take_while(|c| c.is_ascii_alphabetic()).collect::<String>().to_ascii_uppercase();
    if word.is_empty() {
        return false;
    }
    let after_word = &sql[word.len()..];
    if !after_word.is_empty()
        && !after_word.starts_with(|c: char| {
            c.is_whitespace() || matches!(c, ';' | '(' | '/' | '-' | '*' | '"' | '\'' | '`' | '[')
        })
    {
        return false;
    }
    let rest = skip_leading_comments(after_word);
    let has_word = |needle: &str| {
        rest.split(|c: char| !c.is_ascii_alphabetic() && c != '_').any(|w| w.eq_ignore_ascii_case(needle))
    };
    let rest_no_semi = rest.trim_end_matches(';').trim();
    match word.as_str() {
        "SELECT" => {
            !rest_no_semi.is_empty()
                && (has_word("FROM")
                    || has_word("WHERE")
                    || has_word("UNION")
                    || has_word("NULL")
                    || has_word("TRUE")
                    || has_word("FALSE")
                    || has_word("CURRENT_TIMESTAMP")
                    || has_word("CURRENT_DATE")
                    || has_word("CURRENT_TIME")
                    || rest_no_semi.chars().any(|c| {
                        c.is_ascii_digit()
                            || matches!(c, '*' | '(' | ',' | '"' | '\'' | '`' | '[' | '?' | '$' | ':' | '@' | '=')
                    }))
        }
        "INSERT" | "REPLACE" | "UPSERT" => has_word("INTO") || has_word("VALUES") || has_word("DEFAULT"),
        "MERGE" => has_word("INTO") || has_word("USING"),
        "UPDATE" => has_word("SET"),
        "DELETE" => has_word("FROM"),
        "WITH" => has_word("AS") && rest.contains('('),
        "VALUES" => rest.starts_with('('),
        "PRAGMA" => {
            !rest_no_semi.is_empty()
                && (rest_no_semi.contains(['=', '('])
                    || rest_no_semi.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.')))
        }
        "BEGIN" => {
            rest_no_semi.is_empty()
                || ["TRANSACTION", "DEFERRED", "IMMEDIATE", "EXCLUSIVE", "WORK", "TRY", "CATCH", "ATOMIC"]
                    .iter()
                    .any(|w| has_word(w))
        }
        "COMMIT" | "END" => rest_no_semi.is_empty() || has_word("TRANSACTION") || has_word("WORK"),
        "ROLLBACK" => rest_no_semi.is_empty() || has_word("TRANSACTION") || has_word("WORK") || has_word("TO"),
        "SAVEPOINT" | "RELEASE" => {
            !rest_no_semi.is_empty()
                && rest_no_semi
                    .split_whitespace()
                    .all(|w| w.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '"' | '`' | '[' | ']')))
        }
        "CREATE" | "ALTER" | "DROP" | "TRUNCATE" => [
            "TABLE",
            "INDEX",
            "VIEW",
            "TRIGGER",
            "VIRTUAL",
            "SCHEMA",
            "DATABASE",
            "SEQUENCE",
            "TYPE",
            "DOMAIN",
            "EXTENSION",
            "FUNCTION",
            "PROCEDURE",
            "PROC",
            "ROLE",
            "USER",
            "POLICY",
            "PUBLICATION",
            "SUBSCRIPTION",
            "SERVER",
            "STATISTICS",
            "PARTITION",
            "COLUMN",
            "CONSTRAINT",
        ]
        .iter()
        .any(|w| has_word(w)),
        "EXPLAIN" => {
            has_word("QUERY")
                || has_word("PLAN")
                || has_word("ANALYZE")
                || ["SELECT", "INSERT", "UPDATE", "DELETE", "WITH", "REPLACE", "VALUES"].iter().any(|w| has_word(w))
        }
        "SET" => {
            rest_no_semi.contains('=')
                || [
                    "TO",
                    "TRANSACTION",
                    "TIME",
                    "NAMES",
                    "CHARACTER",
                    "NOCOUNT",
                    "LOCAL",
                    "SESSION",
                    "GLOBAL",
                    "ROLE",
                    "CONSTRAINTS",
                    "SEARCH_PATH",
                    "LOCK_TIMEOUT",
                    "STATEMENT_TIMEOUT",
                    "DEADLOCK_PRIORITY",
                    "IDENTITY_INSERT",
                    "QUOTED_IDENTIFIER",
                ]
                .iter()
                .any(|w| has_word(w))
        }
        "VACUUM" | "ANALYZE" | "ANALYSE" | "REINDEX" => {
            rest_no_semi.is_empty()
                || has_word("INTO")
                || rest_no_semi.split_whitespace().all(|w| {
                    w.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '"' | '`' | '[' | ']'))
                })
        }
        "ATTACH" | "DETACH" => has_word("DATABASE") || has_word("AS") || !rest_no_semi.is_empty(),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamps_with_and_without_a_zone() {
        let (m, rest) = leading_timestamp("2026-10-15 12:00:05,120 DEBUG (0.001) SELECT 1").unwrap();
        assert!(!m.zoned);
        assert_eq!(rest.trim_start(), "DEBUG (0.001) SELECT 1");
        let (z, _) = leading_timestamp("2026-10-15 12:00:01 +0000 Started").unwrap();
        assert!(z.zoned);
        assert!(leading_timestamp("Started GET /").is_none());
        assert!(leading_timestamp("2026-10-15 12:00 INFO Starting worker").is_none());
        assert!(leading_timestamp("2026-10-15 12:00:5—foo").is_none());
    }

    #[test]
    fn durations_and_colours() {
        assert_eq!(duration_ms("12.4ms"), Some(12.4));
        assert_eq!(duration_ms("0.012s"), Some(12.0));
        assert_eq!(strip_ansi("\u{1b}[1m\u{1b}[36mProduct Load (1.0ms)\u{1b}[0m"), "Product Load (1.0ms)");
    }
}
