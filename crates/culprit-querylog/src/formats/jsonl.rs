//! JSON lines, one statement each, for anything that already logs that way: `{"sql": …, "ms": …, "at": …}`. The
//! statement may also be under `query` or `statement`, the time under `duration_ms` or `elapsed_ms`; `at` is an ISO
//! 8601 timestamp.

use crate::{Entry, Format, Moment};

pub struct JsonLines;

impl Format for JsonLines {
    fn name(&self) -> &'static str {
        "JSON lines"
    }

    fn recognizes(&self, line: &str) -> bool {
        entry(line).is_some()
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
    let ms = ["ms", "duration_ms", "elapsed_ms"]
        .iter()
        .find_map(|k| v.get(*k)?.as_f64())
        .or_else(|| error_code.map(|_| f64::NAN))?;
    let at = v.get("at").and_then(|a| a.as_str()).and_then(|s| {
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
        assert!(entry(r#"{"level": "info", "msg": "started"}"#).is_none());
    }
}
