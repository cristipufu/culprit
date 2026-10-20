//! Django's `django.db.backends` logger at DEBUG: the time in seconds, the statement with its values filled in, then
//! the arguments.
//!
//! ```text
//! (0.012) SELECT "shop_product"."id" FROM "shop_product" WHERE "shop_product"."category_id" = 3 LIMIT 21; args=(3,); alias=default
//! 2026-10-15 12:00:05,120 DEBUG (0.001) SELECT …; args=('shoes',); alias=default
//! ```

use super::{leading_timestamp, starts_like_sql};
use crate::{Entry, Format};

pub struct Django;

impl Format for Django {
    fn name(&self) -> &'static str {
        "Django"
    }

    fn recognizes(&self, line: &str) -> bool {
        statement(line).is_some()
    }

    fn line(&mut self, line: &str, out: &mut Vec<Entry>) {
        if let Some((ms, sql)) = statement(line) {
            out.push(Entry { sql, ms, at: leading_timestamp(line).map(|(m, _)| m), ..Default::default() });
        }
    }
}

/// `(0.012) SELECT …; args=(…); alias=default`.
fn statement(line: &str) -> Option<(f64, String)> {
    let body = leading_timestamp(line).map_or(line, |(_, rest)| rest);
    let open = body.find('(')?;
    let close = open + body[open..].find(") ")?;
    let seconds: f64 = body[open + 1..close].parse().ok()?;
    // Django writes the time with three decimals; a number in parentheses that isn't one isn't its.
    if !body[open + 1..close].contains('.') {
        return None;
    }
    let rest = &body[close + 2..];
    let sql = match rest.rfind("; args=") {
        Some(at) => &rest[..at],
        None => rest.trim_end().trim_end_matches(';'),
    };
    starts_like_sql(sql).then(|| (seconds * 1000.0, sql.trim().to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statements_with_and_without_a_timestamp() {
        let mut out = Vec::new();
        let mut f = Django;
        f.line("(0.012) SELECT \"shop_product\".\"id\" FROM \"shop_product\" WHERE \"shop_product\".\"category_id\" = 3 LIMIT 20; args=(3,); alias=default", &mut out);
        f.line("2026-10-15 12:00:05,120 DEBUG (0.001) SELECT 1 FROM \"t\" WHERE \"slug\" = 'a; b' LIMIT 21; args=('a; b',); alias=default", &mut out);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].ms, 12.0);
        assert!(out[0].sql.ends_with("LIMIT 20"));
        // A semicolon inside a value doesn't end the statement: only the last "; args=" does.
        assert!(out[1].sql.ends_with("= 'a; b' LIMIT 21"), "{}", out[1].sql);
        assert!(out[1].at.is_some_and(|m| !m.zoned));
        assert!(!f.recognizes("GET /products (200) 12ms"));
    }
}
