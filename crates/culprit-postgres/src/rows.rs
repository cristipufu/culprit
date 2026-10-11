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

    pub fn text_at(&self, index: usize) -> Option<String> {
        self.0.try_get(index).ok().flatten().map(str::to_string)
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

pub async fn query_one(client: &Client, sql: &str) -> Result<Option<Row>> {
    Ok(query(client, sql).await?.into_iter().next())
}

pub fn tagged(sql: &str) -> String {
    format!("{sql}\n{TAG}")
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arrays_parse() {
        assert_eq!(parse_array("{12,34}"), vec!["12", "34"]);
        assert!(parse_array("{}").is_empty());
    }
}
