//! Statement fingerprints: a statement with its literal values taken out, so runs of the same statement with different
//! values group together. One implementation for every source that sees statements as written rather than normalized
//! by the engine: MySQL's slow query log (matched to performance_schema digests), and query logs an application's
//! framework writes (`culprit-querylog`).
//!
//! The engine decides what a quote means: MySQL's double quotes are strings and its `#` starts a comment; elsewhere a
//! double-quoted word is a name, and `#` is part of a name (SQL Server's temporary tables). Placeholders (`?`, `?1`,
//! `$1`, `:name`, `@name`) are values, whatever the driver wrote.

use crate::model::Engine;

/// A statement's fingerprint (canonical tokens, for grouping and matching) and its display text (the statement as
/// written, with every literal replaced by `?`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fingerprint {
    pub key: String,
    pub display: String,
}

struct Rules {
    backslash_escapes: bool,
    double_quote_is_string: bool,
    hash_comments: bool,
    backticks: bool,
    brackets: bool,
    /// `:name`, `@name` and `$name` are placeholders (SQLite; SQL Server's `@name`).
    named_parameters: &'static [char],
}

fn rules(engine: Engine) -> Rules {
    match engine {
        Engine::Mysql => Rules {
            backslash_escapes: true,
            double_quote_is_string: true,
            hash_comments: true,
            backticks: true,
            brackets: false,
            named_parameters: &[],
        },
        Engine::Postgres => Rules {
            backslash_escapes: false,
            double_quote_is_string: false,
            hash_comments: false,
            backticks: false,
            brackets: false,
            named_parameters: &[],
        },
        Engine::Mssql => Rules {
            backslash_escapes: false,
            double_quote_is_string: false,
            hash_comments: false,
            backticks: false,
            brackets: true,
            named_parameters: &['@'],
        },
        Engine::Sqlite => Rules {
            backslash_escapes: false,
            double_quote_is_string: false,
            hash_comments: false,
            backticks: true,
            brackets: true,
            named_parameters: &[':', '@', '$'],
        },
    }
}

pub fn fingerprint(sql: &str, engine: Engine) -> Fingerprint {
    let r = rules(engine);
    let mut tokens: Vec<String> = Vec::new();
    let mut display = String::with_capacity(sql.len());
    let chars: Vec<char> = sql.chars().collect();
    let mut i = 0;
    let space = |display: &mut String| {
        if !display.is_empty() && !display.ends_with(' ') {
            display.push(' ');
        }
    };
    let value = |tokens: &mut Vec<String>, display: &mut String| {
        tokens.push("?".into());
        display.push('?');
    };
    while i < chars.len() {
        let c = chars[i];
        let next = chars.get(i + 1).copied();
        // Comments, including MySQL's executable `/*! … */` ones: the digest drops them too.
        if (r.hash_comments && c == '#')
            || (c == '-' && next == Some('-') && chars.get(i + 2).is_none_or(|c| c.is_whitespace() || !r.hash_comments))
        {
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
            continue;
        }
        if c == '/' && next == Some('*') {
            i += 2;
            while i + 1 < chars.len() && !(chars[i] == '*' && chars[i + 1] == '/') {
                i += 1;
            }
            i += 2;
            continue;
        }
        if c.is_whitespace() {
            space(&mut display);
            i += 1;
            continue;
        }
        // Strings (and hex/bit literals written x'..', b'..', N'..'); in MySQL, double-quoted ones too.
        let prefixed = (matches!(c, 'x' | 'X' | 'b' | 'B' | 'n' | 'N')
            || (engine == Engine::Postgres && matches!(c, 'e' | 'E')))
            && next == Some('\'')
            && !word_before(&chars, i);
        if c == '\'' || (c == '"' && r.double_quote_is_string) || prefixed {
            let quote = if prefixed { '\'' } else { c };
            let escapes = r.backslash_escapes || (prefixed && matches!(c, 'e' | 'E'));
            i += if prefixed { 2 } else { 1 };
            while i < chars.len() {
                if escapes && chars[i] == '\\' {
                    i += 2;
                    continue;
                }
                if chars[i] == quote {
                    if chars.get(i + 1) == Some(&quote) {
                        i += 2;
                        continue;
                    }
                    break;
                }
                i += 1;
            }
            i += 1;
            value(&mut tokens, &mut display);
            continue;
        }
        // Quoted names: kept as written for display, lower-cased for the key.
        let close = match c {
            '`' if r.backticks => Some('`'),
            '[' if r.brackets => Some(']'),
            '"' if !r.double_quote_is_string => Some('"'),
            _ => None,
        };
        if let Some(close) = close {
            let start = i + 1;
            i += 1;
            while i < chars.len() && chars[i] != close {
                i += 1;
            }
            let name: String = chars[start..i.min(chars.len())].iter().collect();
            i += 1;
            tokens.push(name.to_lowercase());
            display.push(c);
            display.push_str(&name);
            display.push(close);
            continue;
        }
        // Placeholders: `?` and `?1`, `$1`, and named ones where the engine has them.
        if c == '?' {
            i += 1;
            while i < chars.len() && chars[i].is_ascii_digit() {
                i += 1;
            }
            value(&mut tokens, &mut display);
            continue;
        }
        if c == '$' && engine != Engine::Mysql && next.is_some_and(|n| n.is_ascii_digit()) {
            i += 1;
            while i < chars.len() && chars[i].is_ascii_digit() {
                i += 1;
            }
            value(&mut tokens, &mut display);
            continue;
        }
        if r.named_parameters.contains(&c)
            && next.is_some_and(|n| n.is_alphabetic() || n == '_')
            && !(c == ':' && i > 0 && chars[i - 1] == ':')
        {
            i += 1;
            while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
                i += 1;
            }
            value(&mut tokens, &mut display);
            continue;
        }
        if c.is_ascii_digit() || (c == '.' && next.is_some_and(|n| n.is_ascii_digit())) {
            // 12, 1.5, 1e3, 0x1F
            i += 1;
            while i < chars.len()
                && (chars[i].is_ascii_alphanumeric()
                    || chars[i] == '.'
                    || ((chars[i] == '-' || chars[i] == '+') && matches!(chars[i - 1], 'e' | 'E')))
            {
                i += 1;
            }
            value(&mut tokens, &mut display);
            continue;
        }
        let word_char =
            |ch: char| ch.is_alphanumeric() || ch == '_' || ch == '$' || ch == '@' || (ch == '#' && !r.hash_comments);
        if word_char(c) {
            let start = i;
            while i < chars.len() && word_char(chars[i]) {
                i += 1;
            }
            let word: String = chars[start..i].iter().collect();
            let lower = word.to_lowercase();
            let canonical = match lower.as_str() {
                "true" | "false" => "?".to_string(),
                "character" => "char".to_string(),
                // The MySQL digest names interval units by their internal names (SQL_TSI_DAY).
                w if engine == Engine::Mysql && w.starts_with("sql_tsi_") => {
                    w.trim_start_matches("sql_tsi_").to_string()
                }
                _ => lower,
            };
            if canonical == "?" {
                display.push('?');
            } else {
                display.push_str(&word);
            }
            tokens.push(canonical);
            continue;
        }
        // Operators: two-character ones stay together, `<>` is `!=`, and `...` is the digest's list marker.
        let pair: String = [Some(c), next].iter().flatten().collect();
        if c == '.' && next == Some('.') && chars.get(i + 2) == Some(&'.') {
            tokens.push("...".into());
            display.push_str("...");
            i += 3;
            continue;
        }
        if matches!(pair.as_str(), "<=" | ">=" | "<>" | "!=" | ":=" | "||" | "&&" | "<<" | ">>" | "::") {
            tokens.push(if pair == "<>" { "!=".into() } else { pair.clone() });
            display.push_str(&pair);
            i += 2;
            continue;
        }
        if c == ';' {
            i += 1;
            continue;
        }
        tokens.push(c.to_string());
        display.push(c);
        i += 1;
    }
    Fingerprint { key: collapse(tokens).join(" "), display: display.trim().to_string() }
}

fn word_before(chars: &[char], i: usize) -> bool {
    i > 0 && (chars[i - 1].is_alphanumeric() || chars[i - 1] == '_')
}

/// Lists of values become `...` whatever their length (`IN (1, 2, 3)`, `IN (...)`, `?, ...`), and the MySQL digest's
/// `(unknown)` placeholder is a value.
fn collapse(tokens: Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(tokens.len());
    let mut i = 0;
    while i < tokens.len() {
        if tokens[i] == "("
            && tokens.get(i + 1).is_some_and(|t| t == "unknown")
            && tokens.get(i + 2).is_some_and(|t| t == ")")
        {
            out.push("?".into());
            i += 3;
            continue;
        }
        if tokens[i] == "?" || tokens[i] == "..." {
            let mut j = i + 1;
            let mut values = 1;
            while j + 1 < tokens.len() && tokens[j] == "," && (tokens[j + 1] == "?" || tokens[j + 1] == "...") {
                values += 1;
                j += 2;
            }
            if values > 1 || tokens[i] == "..." {
                out.push("...".into());
            } else {
                out.push("?".into());
            }
            i = j;
            continue;
        }
        out.push(tokens[i].clone());
        i += 1;
    }
    out
}

/// A short stable id for a fingerprint (FNV-1a, 64 bits, hex).
pub fn id(key: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in key.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{hash:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_double_quoted_word_is_a_name_outside_mysql() {
        let a = fingerprint(
            r#"SELECT "products".* FROM "products" WHERE "products"."category_id" = 3 LIMIT 20"#,
            Engine::Sqlite,
        );
        let b = fingerprint(
            r#"SELECT "products".* FROM "products" WHERE "products"."category_id" = 9 LIMIT 20"#,
            Engine::Sqlite,
        );
        assert_eq!(a.key, b.key);
        assert_eq!(a.display, r#"SELECT "products".* FROM "products" WHERE "products"."category_id" = ? LIMIT ?"#);
        // In MySQL the same quotes are strings.
        assert_eq!(fingerprint(r#"SELECT "a""#, Engine::Mysql).key, "select ?");
    }

    #[test]
    fn placeholders_are_values() {
        let key = |s: &str, e| fingerprint(s, e).key;
        assert_eq!(
            key("SELECT * FROM t WHERE a = ? AND b = ?2", Engine::Sqlite),
            key("SELECT * FROM t WHERE a = 1 AND b = 'x'", Engine::Sqlite)
        );
        assert_eq!(
            key("SELECT * FROM t WHERE a = :id", Engine::Sqlite),
            key("SELECT * FROM t WHERE a = $id", Engine::Sqlite)
        );
        assert_eq!(
            key("SELECT * FROM t WHERE a = $1", Engine::Postgres),
            key("SELECT * FROM t WHERE a = 7", Engine::Postgres)
        );
        // A cast is not a placeholder.
        assert!(key("SELECT a::text FROM t", Engine::Postgres).contains(":: text"));
        assert_eq!(
            key("SELECT * FROM [t] WHERE id = @p0", Engine::Mssql),
            key("SELECT * FROM [t] WHERE id = 5", Engine::Mssql)
        );
        // SQL Server's temporary tables keep their #.
        assert!(key("SELECT * FROM #work", Engine::Mssql).contains("#work"));
    }

    #[test]
    fn lists_of_any_length_are_one_shape() {
        let key = |s: &str| fingerprint(s, Engine::Sqlite).key;
        assert_eq!(key("SELECT * FROM t WHERE id IN (1, 2, 3)"), key("SELECT * FROM t WHERE id IN (?, ?)"));
    }
}
