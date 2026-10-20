//! Entity Framework Core's `Microsoft.EntityFrameworkCore.Database.Command` at Information: the time, the parameters,
//! then the statement on the lines below (the console logger indents them), or on the same line in single-line
//! loggers.
//!
//! ```text
//! info: Microsoft.EntityFrameworkCore.Database.Command[20101]
//!       Executed DbCommand (12ms) [Parameters=[@__p_0='?' (DbType = Int32)], CommandType='Text', CommandTimeout='30']
//!       SELECT "p"."Id", "p"."Name"
//!       FROM "Products" AS "p"
//!       WHERE "p"."CategoryId" = @__p_0
//! ```

use super::{leading_timestamp, starts_like_sql};
use crate::{Entry, Format, Moment};

const MARKER: &str = "Executed DbCommand (";

#[derive(Default)]
pub struct EfCore {
    pending: Option<Pending>,
}

struct Pending {
    ms: f64,
    at: Option<Moment>,
    sql: Vec<String>,
}

impl EfCore {
    fn flush(&mut self, out: &mut Vec<Entry>) {
        if let Some(p) = self.pending.take() {
            let sql = p.sql.join("\n");
            if starts_like_sql(&sql) {
                out.push(Entry { sql, ms: p.ms, at: p.at, ..Default::default() });
            }
        }
    }
}

impl Format for EfCore {
    fn name(&self) -> &'static str {
        "Entity Framework Core"
    }

    fn recognizes(&self, line: &str) -> bool {
        executed(line).is_some()
    }

    fn line(&mut self, line: &str, out: &mut Vec<Entry>) {
        if let Some((ms, rest)) = executed(line) {
            self.flush(out);
            let mut sql = Vec::new();
            if !rest.trim().is_empty() {
                sql.push(rest.trim().to_string());
            }
            self.pending = Some(Pending { ms, at: leading_timestamp(line).map(|(m, _)| m), sql });
            return;
        }
        // The statement continues on indented lines; anything else (a new entry, a blank line) ends it.
        match &mut self.pending {
            Some(p) if line.starts_with([' ', '\t']) && !line.trim().is_empty() => p.sql.push(line.trim().to_string()),
            Some(_) => self.flush(out),
            None => {}
        }
    }

    fn finish(&mut self, out: &mut Vec<Entry>) {
        self.flush(out);
    }
}

/// `Executed DbCommand (12ms) [Parameters=[…], CommandType='Text', CommandTimeout='30']` and what follows it.
fn executed(line: &str) -> Option<(f64, &str)> {
    let at = line.find(MARKER)?;
    let rest = &line[at + MARKER.len()..];
    let close = rest.find(')')?;
    let ms: f64 = rest[..close].trim_end_matches("ms").trim().parse().ok()?;
    let after = &rest[close + 1..];
    // Past the bracketed parameters: they end with CommandTimeout='…'].
    let body = match after.find("CommandTimeout=") {
        Some(t) => after[t..].find(']').map_or("", |e| &after[t + e + 1..]),
        None => after,
    };
    Some((ms, body))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_statement_below_the_marker() {
        let mut f = EfCore::default();
        let mut out = Vec::new();
        for line in [
            "info: Microsoft.EntityFrameworkCore.Database.Command[20101]",
            "      Executed DbCommand (12ms) [Parameters=[@__p_0='?' (DbType = Int32)], CommandType='Text', CommandTimeout='30']",
            "      SELECT \"p\".\"Id\", \"p\".\"Name\"",
            "      FROM \"Products\" AS \"p\"",
            "      WHERE \"p\".\"CategoryId\" = @__p_0",
            "info: Microsoft.EntityFrameworkCore.Database.Command[20101]",
            "      Executed DbCommand (3ms) [Parameters=[], CommandType='Text', CommandTimeout='30'] SELECT 1",
        ] {
            f.line(line, &mut out);
        }
        f.finish(&mut out);
        assert_eq!(out.len(), 2, "{out:?}");
        assert_eq!(out[0].ms, 12.0);
        assert!(out[0].sql.starts_with("SELECT \"p\".\"Id\"") && out[0].sql.ends_with("= @__p_0"));
        assert_eq!(out[1].sql, "SELECT 1");
    }
}
