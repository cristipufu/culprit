//! Phoenix/Ecto's debug log, on by default in development: the times on one line, the statement and its parameters on
//! the next, then where it was called from.
//!
//! ```text
//! [debug] QUERY OK source="products" db=1.2ms decode=0.1ms queue=0.3ms idle=12.3ms
//! SELECT p0."id", p0."name" FROM "products" AS p0 WHERE (p0."category_id" = ?) [3]
//! ↳ Shop.Products.list/1, at: lib/shop/products.ex:12
//! ```
//!
//! The time counted is `db`: the database's own; `queue` is waiting for a pool connection, which isn't database time.

use super::{duration_ms, leading_timestamp, starts_like_sql};
use crate::{Entry, Format, Moment};

#[derive(Default)]
pub struct Ecto {
    pending: Option<(f64, Option<Moment>)>,
}

impl Format for Ecto {
    fn name(&self) -> &'static str {
        "Phoenix/Ecto"
    }

    fn recognizes(&self, line: &str) -> bool {
        query_ok(line).is_some()
    }

    fn line(&mut self, line: &str, out: &mut Vec<Entry>) {
        if let Some(ms) = query_ok(line) {
            self.pending = Some((ms, leading_timestamp(line).map(|(m, _)| m)));
            return;
        }
        let text = line.trim();
        if text.is_empty() || text.starts_with('↳') {
            return;
        }
        if let Some((ms, at)) = self.pending.take()
            && starts_like_sql(text)
        {
            out.push(Entry { sql: without_parameters(text).to_string(), ms, at });
        }
    }
}

/// `QUERY OK … db=1.2ms …`: the database's time.
fn query_ok(line: &str) -> Option<f64> {
    let at = line.find("QUERY OK")?;
    line[at..].split_whitespace().find_map(|w| w.strip_prefix("db=")).and_then(duration_ms)
}

/// The statement without the parameter list Ecto appends: `… = ?) [3, "red"]`.
fn without_parameters(text: &str) -> &str {
    if text.ends_with(']')
        && let Some(at) = text.rfind(" [")
    {
        return text[..at].trim_end();
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_database_time_and_the_statement_below() {
        let mut f = Ecto::default();
        let mut out = Vec::new();
        for line in [
            "[debug] QUERY OK source=\"products\" db=1.2ms decode=0.1ms queue=40.3ms idle=12.3ms",
            "SELECT p0.\"id\" FROM \"products\" AS p0 WHERE (p0.\"category_id\" = ?) [3]",
            "↳ Shop.Products.list/1, at: lib/shop/products.ex:12",
        ] {
            f.line(line, &mut out);
        }
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].ms, 1.2);
        assert_eq!(out[0].sql, "SELECT p0.\"id\" FROM \"products\" AS p0 WHERE (p0.\"category_id\" = ?)");
    }
}
