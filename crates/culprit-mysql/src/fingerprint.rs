//! Statement fingerprints for the slow query log: `culprit_core::fingerprint` with MySQL's rules (double quotes are
//! strings, `#` starts a comment), and matching a logged statement to a performance_schema digest
//! (`SELECT ID , NAME FROM `products` WHERE ID = ?`) to plan it.

use culprit_core::Engine;
pub use culprit_core::fingerprint::{Fingerprint, id};

pub fn fingerprint(sql: &str) -> Fingerprint {
    culprit_core::fingerprint::fingerprint(sql, Engine::Mysql)
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

/// Digest text as the server normalized it (`SUM ( `total` )`, `LIMIT ?`), as runnable as it can be made: each function
/// name next to its parenthesis again, since MariaDB without IGNORE_SPACE reads `COUNT (` as a name, not a call.
/// Placeholders stay; [`has_placeholders`] says whether any are left.
pub fn runnable(digest: &str) -> String {
    let mut out = String::with_capacity(digest.len());
    let mut quote: Option<char> = None;
    let mut chars = digest.chars().peekable();
    while let Some(c) = chars.next() {
        if let Some(q) = quote {
            out.push(c);
            if c == q {
                quote = None;
            }
            continue;
        }
        if matches!(c, '\'' | '"' | '`') {
            quote = Some(c);
            out.push(c);
            continue;
        }
        if c.is_whitespace() {
            let mut rest = chars.clone();
            while rest.peek().is_some_and(|n| n.is_whitespace()) {
                rest.next();
            }
            let after_name = out.chars().last().is_some_and(|p| p.is_alphanumeric() || p == '_' || p == '`');
            if after_name && rest.peek() == Some(&'(') {
                chars = rest;
                continue;
            }
        }
        out.push(c);
    }
    out
}

/// Whether text keeps a `?` where a value was, outside quotes.
pub fn has_placeholders(text: &str) -> bool {
    let mut quote: Option<char> = None;
    for c in text.chars() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => {}
            None if matches!(c, '\'' | '"' | '`') => quote = Some(c),
            None if c == '?' => return true,
            None => {}
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digest_text_runs_again_where_it_can() {
        let digest = "SELECT `customer_id` , SUM ( `total` ) AS `spent` , COUNT ( * ) FROM `orders` WHERE `id` IN ( ... ) LIMIT ?";
        let text = runnable(digest);
        assert!(text.contains("SUM( `total` )") && text.contains("COUNT( * )") && text.contains("IN( ... )"), "{text}");
        assert!(has_placeholders(&text));
        assert!(!has_placeholders("SELECT 'why?' FROM `t?`"));
        // Inside quotes nothing moves.
        assert_eq!(runnable("SELECT 'a (b)' , f ( 1 )"), "SELECT 'a (b)' , f( 1 )");
    }

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
