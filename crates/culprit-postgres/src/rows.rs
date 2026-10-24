use culprit_core::Result;
use tokio_postgres::{Client, SimpleQueryMessage, SimpleQueryRow};

use crate::connect::map_error;

/// Appended to every statement Culprit sends, so its own queries can be told apart in pg_stat_statements
/// (which keeps trailing comments but drops leading ones).
pub const TAG: &str = "/* culprit */";

/// Separator used by `string_agg` in catalog queries; it cannot appear in identifiers.
pub const LIST_SEPARATOR: char = '\u{1f}';

/// A row from the simple-query protocol, where every value arrives as text.
pub struct Row(SimpleQueryRow);

impl Row {
    pub fn text(&self, column: &str) -> Option<String> {
        self.0.try_get(column).ok().flatten().map(str::to_string)
    }

    pub fn string(&self, column: &str) -> String {
        self.text(column).unwrap_or_default()
    }

    pub fn number(&self, column: &str) -> Option<f64> {
        let value = self.0.try_get(column).ok().flatten()?;
        value.trim().parse::<f64>().ok().filter(|v| v.is_finite())
    }

    pub fn flag(&self, column: &str) -> bool {
        matches!(self.0.try_get(column).ok().flatten(), Some("t" | "true" | "on" | "1"))
    }

    pub fn list(&self, column: &str) -> Vec<String> {
        self.text(column)
            .map(|v| v.split(LIST_SEPARATOR).filter(|s| !s.is_empty()).map(str::to_string).collect())
            .unwrap_or_default()
    }
}

pub async fn query(client: &Client, sql: &str) -> Result<Vec<Row>> {
    let messages = client.simple_query(&tagged(sql)).await.map_err(map_error)?;
    Ok(rows_of(messages))
}

/// Retain a bounded prefix of collector rows, draining completion and late errors after retention stops. The caller
/// must record partial coverage when truncated; dropping rows never establishes absence or a complete catalog.
pub async fn query_bounded(client: &Client, sql: &str, limit: usize) -> Result<(Vec<Row>, bool)> {
    use futures_util::TryStreamExt;
    let stream = client.simple_query_raw(&tagged(sql)).await.map_err(map_error)?;
    tokio::pin!(stream);
    let mut budget = culprit_core::limits::ResultBudget::new(limit);
    let mut rows = Vec::new();
    while let Some(message) = stream.try_next().await.map_err(map_error)? {
        if let SimpleQueryMessage::Row(row) = message {
            if budget.truncated() {
                continue;
            }
            let bytes = (0..row.len()).fold(0usize, |total, i| total.saturating_add(row.get(i).map_or(0, str::len)));
            if budget.retain(bytes) {
                rows.push(Row(row));
            }
        }
    }
    Ok((rows, budget.truncated()))
}

pub async fn query_one(client: &Client, sql: &str) -> Result<Option<Row>> {
    Ok(query(client, sql).await?.into_iter().next())
}

pub fn tagged(sql: &str) -> String {
    // YSQL's QPM has its own supported opt-out; comments are inert on PostgreSQL. Keep TAG unchanged so reads also
    // exclude collector statements recorded by earlier Culprit versions.
    format!("{sql}\n{TAG} /* __YB_STAT_PLANS_SKIP */")
}

pub fn rows_of(messages: Vec<SimpleQueryMessage>) -> Vec<Row> {
    messages
        .into_iter()
        .filter_map(|m| match m {
            SimpleQueryMessage::Row(row) => Some(Row(row)),
            _ => None,
        })
        .collect()
}

/// UTC ISO-8601 text for a timestamptz expression, matching the format other providers emit.
pub fn iso(expression: &str) -> String {
    format!("to_char(({expression}) AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"')")
}

/// Postgres array literal text (`{1,2,3}`) to its elements.
pub fn parse_array(text: &str) -> Vec<String> {
    let inner = text.trim().trim_start_matches('{').trim_end_matches('}');
    if inner.is_empty() {
        return Vec::new();
    }
    inner.split(',').map(|s| s.trim().trim_matches('"').to_string()).collect()
}

pub fn quote_ident(name: &str) -> String {
    let plain = name.chars().next().is_some_and(|c| c.is_ascii_lowercase() || c == '_')
        && name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    if plain {
        return name.to_string();
    }
    format!("\"{}\"", name.replace('"', "\"\""))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arrays_parse() {
        assert_eq!(parse_array("{12,34}"), vec!["12", "34"]);
        assert!(parse_array("{}").is_empty());
    }

    #[test]
    fn identifiers_quote_only_when_needed() {
        assert_eq!(quote_ident("app_user"), "app_user");
        assert_eq!(quote_ident("AppUser"), "\"AppUser\"");
        assert_eq!(quote_ident("we\"ird"), "\"we\"\"ird\"");
    }
}
