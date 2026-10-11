//! Statement fingerprints: the statement with its literal values taken out, so runs of the same statement with
//! different values group together, and a statement from the slow query log can be matched to a performance_schema
//! digest (`SELECT ID , NAME FROM `products` WHERE ID = ?`) to plan it.

/// A statement's fingerprint (canonical tokens, for grouping and matching) and its display text (the statement as
/// written, with every literal replaced by `?`).
pub struct Fingerprint {
    pub key: String,
    pub display: String,
}

pub fn fingerprint(sql: &str) -> Fingerprint {
    let mut tokens: Vec<String> = Vec::new();
    let mut display = String::with_capacity(sql.len());
    let chars: Vec<char> = sql.chars().collect();
    let mut i = 0;
    let space = |display: &mut String| {
        if !display.is_empty() && !display.ends_with(' ') {
            display.push(' ');
        }
    };
    while i < chars.len() {
        let c = chars[i];
        let next = chars.get(i + 1).copied();
        // Comments, including MySQL's executable `/*! … */` ones: the digest drops them too.
        if c == '#' || (c == '-' && next == Some('-') && chars.get(i + 2).is_none_or(|c| c.is_whitespace())) {
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
        // Strings (and hex/bit literals written x'..', b'..', N'..').
        let prefixed = matches!(c, 'x' | 'X' | 'b' | 'B' | 'n' | 'N') && next == Some('\'') && !word_before(&chars, i);
        if c == '\'' || c == '"' || prefixed {
            let quote = if prefixed { '\'' } else { c };
            i += if prefixed { 2 } else { 1 };
            while i < chars.len() {
                if chars[i] == '\\' {
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
            tokens.push("?".into());
            display.push('?');
            continue;
        }
        if c == '`' {
            let start = i + 1;
            i += 1;
            while i < chars.len() && chars[i] != '`' {
                i += 1;
            }
            let name: String = chars[start..i.min(chars.len())].iter().collect();
            i += 1;
            tokens.push(name.to_lowercase());
            display.push('`');
            display.push_str(&name);
            display.push('`');
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
            tokens.push("?".into());
            display.push('?');
            continue;
        }
        if c.is_alphanumeric() || c == '_' || c == '$' || c == '@' {
            let start = i;
            while i < chars.len() && (chars[i].is_alphanumeric() || matches!(chars[i], '_' | '$' | '@')) {
                i += 1;
            }
            let word: String = chars[start..i].iter().collect();
            let lower = word.to_lowercase();
            let canonical = match lower.as_str() {
                "true" | "false" => "?".to_string(),
                "character" => "char".to_string(),
                // The digest names interval units by their internal names (SQL_TSI_DAY).
                w if w.starts_with("sql_tsi_") => w.trim_start_matches("sql_tsi_").to_string(),
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
        if matches!(pair.as_str(), "<=" | ">=" | "<>" | "!=" | ":=" | "||" | "&&" | "<<" | ">>") {
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

/// Lists of values become `...` whatever their length (`IN (1, 2, 3)`, `IN (...)`, `?, ...`), and the digest's
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

/// Whether a statement from the log has the shape of a digest. The digest may be cut at max_digest_length.
pub fn matches_digest(statement_key: &str, digest_text: &str) -> bool {
    let truncated = digest_text.trim_end().ends_with("...") && !digest_text.trim_end().ends_with("(...)");
    let digest = fingerprint(digest_text.trim_end().trim_end_matches("...")).key;
    if truncated {
        return !digest.is_empty() && statement_key.starts_with(&digest);
    }
    statement_key == digest
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
    fn literals_go_and_shapes_match_digests() {
        let a = fingerprint("SELECT id, name, price FROM products WHERE id = 42");
        let b = fingerprint("select id,name,price from `products` where id=7 -- n+1");
        assert_eq!(a.key, b.key);
        assert_eq!(a.display, "SELECT id, name, price FROM products WHERE id = ?");
        assert!(matches_digest(&a.key, "SELECT ID , NAME , `price` FROM `products` WHERE ID = ? "));
        assert_eq!(id(&a.key), id(&b.key));
        assert_ne!(id(&a.key), id(&fingerprint("SELECT 1").key));
    }

    #[test]
    fn lists_negatives_and_odd_literals() {
        let s = fingerprint(
            "SELECT id FROM orders WHERE id IN (1,2,3) AND total > -5 AND status IN ('a','b') AND created_at > NOW() - INTERVAL 3 DAY LIMIT 3",
        );
        assert!(matches_digest(
            &s.key,
            "SELECT ID FROM `orders` WHERE ID IN (...) AND `total` > - ? AND STATUS IN (...) AND `created_at` > NOW ( ) - INTERVAL ? SQL_TSI_DAY LIMIT ? "
        ));
        let hex = fingerprint("SELECT 1 FROM products WHERE price = 1.5e3 OR name IS NULL OR sku = x'41' LIMIT 1");
        assert!(matches_digest(
            &hex.key,
            "SELECT ? FROM `products` WHERE `price` = ? OR NAME IS NULL OR `sku` = (unknown) LIMIT ? "
        ));
        assert_eq!(fingerprint("SELECT 'it''s', \"a\\\"b\"").key, "select ...");
        assert_eq!(fingerprint("SELECT a <> b").key, fingerprint("SELECT a != b").key);
    }

    #[test]
    fn truncated_digests_match_by_prefix() {
        let s = fingerprint("SELECT a, b, c, d FROM t WHERE x = 1 AND y = 2");
        assert!(matches_digest(&s.key, "SELECT `a` , `b` , `c` ..."));
        assert!(!matches_digest(&s.key, "SELECT `a` , `z` ..."));
    }
}
