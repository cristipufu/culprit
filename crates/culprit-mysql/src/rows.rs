use culprit_core::Result;
use mysql_async::prelude::Queryable;
use mysql_async::{Conn, Row, Value};

use crate::connect::{TAG, map_error};

/// One result row with case-insensitive access by column name, tolerant of whatever protocol and type the server used.
pub struct Rec {
    columns: Vec<String>,
    values: Vec<Value>,
}

impl Rec {
    fn from_row(row: Row) -> Rec {
        let columns = row.columns_ref().iter().map(|c| c.name_str().to_ascii_lowercase()).collect();
        Rec { columns, values: row.unwrap() }
    }

    fn value(&self, name: &str) -> Option<&Value> {
        let name = name.to_ascii_lowercase();
        let index = self.columns.iter().position(|c| *c == name)?;
        self.values.get(index).filter(|v| !matches!(v, Value::NULL))
    }

    pub fn text(&self, name: &str) -> Option<String> {
        self.value(name).map(value_text)
    }

    /// The first column, whatever the server named it (`ANALYZE`, `EXPLAIN`).
    pub fn first_text(&self) -> Option<String> {
        self.values.first().filter(|v| !matches!(v, Value::NULL)).map(value_text)
    }

    pub fn string(&self, name: &str) -> String {
        self.text(name).unwrap_or_default()
    }

    pub fn number(&self, name: &str) -> Option<f64> {
        match self.value(name)? {
            Value::Int(i) => Some(*i as f64),
            Value::UInt(u) => Some(*u as f64),
            Value::Float(f) => Some(f64::from(*f)),
            Value::Double(d) => Some(*d),
            Value::Bytes(b) => String::from_utf8_lossy(b).trim().parse().ok(),
            _ => None,
        }
    }

    pub fn flag(&self, name: &str) -> bool {
        self.number(name).is_some_and(|n| n != 0.0)
            || self.text(name).is_some_and(|t| matches!(t.to_ascii_uppercase().as_str(), "YES" | "ON" | "TRUE"))
    }
}

pub fn value_text(value: &Value) -> String {
    match value {
        Value::NULL => String::new(),
        Value::Bytes(b) => String::from_utf8_lossy(b).into_owned(),
        Value::Int(i) => i.to_string(),
        Value::UInt(u) => u.to_string(),
        Value::Float(f) => f.to_string(),
        Value::Double(d) => d.to_string(),
        Value::Date(y, mo, d, h, mi, s, us) => {
            if *us > 0 {
                format!("{y:04}-{mo:02}-{d:02} {h:02}:{mi:02}:{s:02}.{us:06}")
            } else {
                format!("{y:04}-{mo:02}-{d:02} {h:02}:{mi:02}:{s:02}")
            }
        }
        Value::Time(negative, days, h, mi, s, us) => {
            let hours = u32::from(*h) + days * 24;
            let sign = if *negative { "-" } else { "" };
            if *us > 0 {
                format!("{sign}{hours:02}:{mi:02}:{s:02}.{us:06}")
            } else {
                format!("{sign}{hours:02}:{mi:02}:{s:02}")
            }
        }
    }
}

pub fn tagged(sql: &str) -> String {
    format!("{TAG} {sql}")
}

pub async fn query(conn: &mut Conn, sql: &str) -> Result<Vec<Rec>> {
    let rows: Vec<Row> = conn.query(tagged(sql)).await.map_err(map_error)?;
    Ok(rows.into_iter().map(Rec::from_row).collect())
}

pub async fn query_one(conn: &mut Conn, sql: &str) -> Result<Option<Rec>> {
    Ok(query(conn, sql).await?.into_iter().next())
}

/// EXPLAIN through the binary protocol: a prepared statement can only ever hold one statement, whatever the text says.
/// Servers that can't prepare EXPLAIN (ER_UNSUPPORTED_PS) get the text protocol; callers have already checked the text
/// is a single statement.
pub async fn explain_one(conn: &mut Conn, sql: &str) -> Result<Option<Rec>> {
    match conn.exec_first::<Row, _, _>(tagged(sql), ()).await {
        Ok(row) => Ok(row.map(Rec::from_row)),
        Err(mysql_async::Error::Server(e)) if e.code == 1295 => query_one(conn, sql).await,
        Err(e) => Err(map_error(e)),
    }
}

/// A query with bound parameters (the schema name, mostly), so names are never spliced into SQL.
pub async fn query_with(conn: &mut Conn, sql: &str, params: Vec<Value>) -> Result<Vec<Rec>> {
    let rows: Vec<Row> = conn.exec(tagged(sql), params).await.map_err(map_error)?;
    Ok(rows.into_iter().map(Rec::from_row).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_render_as_text() {
        assert_eq!(value_text(&Value::Bytes(b"abc".to_vec())), "abc");
        assert_eq!(value_text(&Value::Date(2026, 1, 2, 3, 4, 5, 0)), "2026-01-02 03:04:05");
        assert_eq!(value_text(&Value::Time(false, 1, 2, 3, 4, 0)), "26:03:04");
        assert_eq!(value_text(&Value::UInt(7)), "7");
    }
}
