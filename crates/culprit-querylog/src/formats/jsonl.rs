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
    let ms = ["ms", "duration_ms", "elapsed_ms"].iter().find_map(|k| v.get(*k)?.as_f64())?;
    let at = v.get("at").and_then(|a| a.as_str()).and_then(|s| {
        let epoch = culprit_core::format::epoch_seconds(s)?;
        let zoned = s.ends_with('Z') || s.rfind(['+', '-']).is_some_and(|p| p > 10);
        Some(Moment { epoch, zoned })
    });
    Some(Entry { sql, ms, at })
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
