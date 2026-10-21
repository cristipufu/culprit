//! JSON lines (`.jsonl`, `.ndjson`, `.log`), one statement each, for anything that already logs that way:
//! `{"sql": …, "ms": …, "at": …}`. The statement may also be under `query` or `statement`, the time under
//! `duration_ms`, `elapsed_ms`, `time_ms`, `query_time_ms` (or `duration` / `elapsed` with a unit such as `"12.4ms"`),
//! and the timestamp under `at`, `ts`, `timestamp` or `time`.

use super::{duration_ms, starts_like_sql};
use crate::{Entry, Format, Moment};

pub struct JsonLines;

impl Format for JsonLines {
    fn name(&self) -> &'static str {
        "JSON lines"
    }

    fn recognizes(&self, line: &str) -> bool {
        entry(line).is_some_and(|e| e.ms.is_finite() && e.ms >= 0.0)
    }

    fn line(&mut self, line: &str, out: &mut Vec<Entry>) {
        if let Some(e) = entry(line) {
            out.push(e);
        }
    }
}

fn entry(line: &str) -> Option<Entry> {
    let t = line.trim();
    if !t.starts_with('{') {
        return None;
    }
    let v: serde_json::Value = serde_json::from_str(t).ok()?;
    let sql = ["sql", "query", "statement"].iter().find_map(|k| v.get(*k)?.as_str())?.to_string();
    if !starts_like_sql(&sql) {
        return None;
    }
    let raw_error = v.get("error_code").or_else(|| v.pointer("/error/code")).or_else(|| v.get("error"));
    let error_code = raw_error
        .filter(|v| {
            !v.is_null()
                && **v != serde_json::Value::Bool(false)
                && v.as_u64() != Some(0)
                && v.as_str().is_none_or(|s| !s.is_empty() && !matches!(s, "0" | "SQLITE_OK"))
                && v.as_object().is_none_or(|o| !o.is_empty())
        })
        .map(|value| {
            let code = value
                .as_str()
                .map(str::to_ascii_uppercase)
                .or_else(|| value.as_u64().map(|n| n.to_string()))
                .unwrap_or_default();
            match code.as_str() {
                "SQLITE_BUSY" | "5" => "SQLITE_BUSY",
                "SQLITE_LOCKED" | "6" => "SQLITE_LOCKED",
                "SQLITE_BUSY_SNAPSHOT" | "517" => "SQLITE_BUSY_SNAPSHOT",
                "SQLITE_CONSTRAINT" | "19" => "SQLITE_CONSTRAINT",
                "SQLITE_FULL" | "13" => "SQLITE_FULL",
                "SQLITE_READONLY" | "8" => "SQLITE_READONLY",
                "SQLITE_IOERR" | "10" => "SQLITE_IOERR",
                "SQLITE_CORRUPT" | "11" => "SQLITE_CORRUPT",
                c if c.starts_with("SQLITE_CONSTRAINT_") => "SQLITE_CONSTRAINT",
                c if c.starts_with("SQLITE_READONLY_") => "SQLITE_READONLY",
                c if c.starts_with("SQLITE_IOERR_") => "SQLITE_IOERR",
                c if c.starts_with("SQLITE_BUSY_") => "SQLITE_BUSY",
                c if c.starts_with("SQLITE_LOCKED_") => "SQLITE_LOCKED",
                c if c.parse::<u64>().is_ok_and(|n| n > 255) => match c.parse::<u64>().unwrap() & 255 {
                    5 => "SQLITE_BUSY",
                    6 => "SQLITE_LOCKED",
                    19 => "SQLITE_CONSTRAINT",
                    8 => "SQLITE_READONLY",
                    10 => "SQLITE_IOERR",
                    11 => "SQLITE_CORRUPT",
                    _ => "Logged SQL error",
                },
                _ => "Logged SQL error",
            }
        });
    let timed_ms = ["ms", "duration_ms", "elapsed_ms", "time_ms", "query_time_ms"]
        .iter()
        .find_map(|k| {
            let val = v.get(*k)?;
            val.as_f64().or_else(|| {
                let s = val.as_str()?;
                duration_ms(s).or_else(|| s.trim().parse::<f64>().ok())
            })
        })
        .or_else(|| ["duration", "elapsed"].iter().find_map(|k| duration_ms(v.get(*k)?.as_str()?)))
        .filter(|ms| ms.is_finite() && *ms >= 0.0);
    let ms = timed_ms.or_else(|| error_code.map(|_| f64::NAN))?;
    let at = ["at", "ts", "timestamp", "time"].iter().find_map(|k| v.get(*k)?.as_str()).and_then(|s| {
        let epoch = culprit_core::format::epoch_seconds(s)?;
        let zoned = s.ends_with('Z') || s.rfind(['+', '-']).is_some_and(|p| p > 10);
        Some(Moment { epoch, zoned })
    });
    Some(Entry {
        sql,
        ms,
        at,
        error_code,
        request_id: v.get("request_id").and_then(|v| v.as_str()).filter(|s| !s.trim().is_empty()).map(str::to_string),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statements_and_their_keys() {
        let e = entry(r#"{"sql": "SELECT 1", "ms": 8.5, "at": "2026-10-15T12:00:00Z"}"#).unwrap();
        assert_eq!((e.sql.as_str(), e.ms), ("SELECT 1", 8.5));
        assert!(e.at.is_some_and(|m| m.zoned));
        assert!(entry(r#"{"query": "SELECT 1", "duration_ms": 2}"#).is_some());
        assert!(
            entry(r#"{"statement": "/*app='shop'*/ SELECT * FROM orders WHERE id = ?", "duration": "4.2ms", "ts": "2026-10-15T12:00:00Z"}"#)
                .is_some_and(|e| (e.ms - 4.2).abs() < 1e-9 && e.at.is_some_and(|m| m.zoned))
        );
        assert!(entry(r#"{"level": "info", "msg": "started"}"#).is_none());
        // Non-SQL search/event records that happen to use `query` or `statement` keys are rejected.
        assert!(entry(r#"{"query": "red running shoes", "duration_ms": 12.0}"#).is_none());
        assert!(entry(r#"{"query": "select red shoes", "duration_ms": 12.0}"#).is_none());
        assert!(entry(r#"{"statement": "update profile settings", "ms": 5.0}"#).is_none());
        // Recognition requires both a SQL statement and a non-negative duration.
        assert!(JsonLines.recognizes(r#"{"sql": "SELECT * FROM orders", "ms": 1.2}"#));
        assert!(!JsonLines.recognizes(r#"{"sql": "SELECT * FROM orders"}"#));
        assert!(!JsonLines.recognizes(r#"{"sql": "INSERT INTO orders(id) VALUES (1)", "error_code": 5}"#));
    }
}
