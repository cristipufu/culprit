//! Rails (ActiveRecord) at debug level, the default in development:
//!
//! ```text
//! Started GET "/products?category=3" for 66.249.66.1 at 2026-10-15 12:00:01 +0000
//!   Product Load (12.4ms)  SELECT "products".* FROM "products" WHERE "products"."category_id" = ? LIMIT ?  [["category_id", 3], ["LIMIT", 20]]
//!   CACHE Product Load (0.0ms)  SELECT …      (served from Rails' query cache: never reached the database)
//! ```
//!
//! A request's `Started … at` line dates the statements under it.

use super::{duration_ms, starts_like_sql, strip_ansi};
use crate::{Entry, Format, Moment};

#[derive(Default)]
pub struct Rails {
    at: Option<Moment>,
}

impl Format for Rails {
    fn name(&self) -> &'static str {
        "Rails"
    }

    fn recognizes(&self, line: &str) -> bool {
        let line = strip_ansi(line);
        statement(&line).is_some()
    }

    fn line(&mut self, line: &str, out: &mut Vec<Entry>) {
        let line = strip_ansi(line);
        if let Some(at) = started_at(&line) {
            self.at = Some(at);
            return;
        }
        if let Some((cached, ms, sql)) = statement(&line)
            && !cached
        {
            out.push(Entry { sql, ms, at: self.at, ..Default::default() });
        }
    }
}

/// `Started GET "/" for 1.2.3.4 at 2026-10-15 12:00:01 +0000`.
fn started_at(line: &str) -> Option<Moment> {
    let rest = line.trim_start().strip_prefix("Started ")?;
    let at = rest.rfind(" at ")?;
    let stamp = rest[at + 4..].trim();
    let epoch = culprit_core::format::epoch_seconds(stamp)?;
    Some(Moment { epoch, zoned: stamp.contains(['+', 'Z']) || stamp.rfind('-').is_some_and(|p| p > 10) })
}

/// `  Product Load (12.4ms)  SELECT …  [[binds]]`: whether it came from the query cache, how long, and the SQL.
fn statement(line: &str) -> Option<(bool, f64, String)> {
    let t = line.trim_start();
    let close = t.find("ms)")?;
    let open = t[..close].rfind('(')?;
    let ms = duration_ms(&t[open + 1..close + 2])?;
    let name = t[..open].trim();
    if name.is_empty() || name.contains(['(', ')']) {
        return None;
    }
    let mut sql = t[close + 3..].trim();
    if !starts_like_sql(sql) {
        return None;
    }
    // The bind values Rails appends after two spaces: `  [["id", 3], ["LIMIT", 1]]` (before or after a trailing
    // QueryLogs comment `/*application:Shop*/`).
    fn strip_trailing_comments(mut s: &str) -> &str {
        while s.ends_with("*/")
            && let Some(at) = s.rfind("/*")
        {
            s = s[..at].trim_end();
        }
        s
    }
    sql = strip_trailing_comments(sql);
    if sql.ends_with("]]")
        && let Some(at) = sql.rfind("  [[")
    {
        sql = strip_trailing_comments(sql[..at].trim_end());
    }
    Some((name.starts_with("CACHE"), ms, sql.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statements_their_time_and_their_request() {
        let mut f = Rails::default();
        let mut out = Vec::new();
        for line in [
            "Started GET \"/products?category=3\" for 66.249.66.1 at 2026-10-15 12:00:01 +0000",
            "  \u{1b}[1m\u{1b}[36mProduct Load (12.4ms)\u{1b}[0m  \u{1b}[1m\u{1b}[34mSELECT \"products\".* FROM \"products\" WHERE \"products\".\"category_id\" = ? LIMIT ?\u{1b}[0m  [[\"category_id\", 3], [\"LIMIT\", 20]]",
            "  CACHE Product Load (0.0ms)  SELECT \"products\".* FROM \"products\" WHERE \"products\".\"category_id\" = ? LIMIT ?  [[\"category_id\", 3]]",
            "Completed 200 OK in 31ms (Views: 15.2ms | ActiveRecord: 12.7ms | Allocations: 9001)",
        ] {
            f.line(line, &mut out);
        }
        assert_eq!(out.len(), 1, "{out:?}");
        assert_eq!(out[0].ms, 12.4);
        assert_eq!(
            out[0].sql,
            "SELECT \"products\".* FROM \"products\" WHERE \"products\".\"category_id\" = ? LIMIT ?"
        );
        assert!(out[0].at.is_some_and(|m| m.zoned));
        // "Completed … in 31ms (Views: 15.2ms …" and "Started GET …" alone are no SQL statements.
        assert!(!f.recognizes("Completed 200 OK in 31ms (Views: 15.2ms | ActiveRecord: 12.7ms)"));
        assert!(!f.recognizes("Started GET \"/\" for 127.0.0.1 at 2026-10-15 12:00:01 +0000"));
    }

    #[test]
    fn query_log_tags_before_or_after_statements_are_accepted() {
        let mut f = Rails::default();
        let mut out = Vec::new();
        for line in [
            "  Product Load (3.2ms)  /*application='Shop'*/ SELECT \"products\".* FROM \"products\" WHERE \"products\".\"id\" = ?  [[\"id\", 1]]",
            "  Product Load (1.8ms)  SELECT \"products\".* FROM \"products\" WHERE \"products\".\"id\" = ?  [[\"id\", 2]] /*application:Shop*/",
        ] {
            assert!(f.recognizes(line), "{line}");
            f.line(line, &mut out);
        }
        assert_eq!(out.len(), 2);
        assert!(!out[0].sql.contains("[[") && !out[1].sql.contains("[["));
    }
}
