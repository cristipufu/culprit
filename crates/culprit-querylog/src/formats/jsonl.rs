//! JSON lines (`.jsonl`, `.ndjson`, `.log`), one statement each, for anything that already logs that way:
//! `{"sql": …, "ms": …, "at": …}`, structured logger envelopes (`fields` in Rust `tracing-subscriber`), or
//! OpenTelemetry JSON spans (`attributes` / `spanAttributes` with `db.statement` or `db.query.text` and
//! `startTimeUnixNano` / `endTimeUnixNano`).

use super::{duration_ms, starts_like_sql};
use crate::{Entry, Format, Moment};
use serde_json::Value;

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

/// Unwrap an OTLP JSON attribute value object (`{"stringValue": "…"}`, `{"intValue": "…"}`, `{"doubleValue": …}`).
fn otlp_value(v: &Value) -> &Value {
    if let Some(obj) = v.as_object() {
        for key in ["stringValue", "intValue", "doubleValue", "boolValue"] {
            if let Some(inner) = obj.get(key) {
                return inner;
            }
        }
    }
    v
}

/// Look up a key at the top level, under `fields` (`tracing-subscriber`), or under `attributes` / `spanAttributes`
/// (OpenTelemetry JSON spans, both flat maps and OTLP key-value arrays).
fn lookup<'a>(v: &'a Value, key: &str) -> Option<&'a Value> {
    if let Some(found) = v.get(key).filter(|f| !f.is_null()) {
        return Some(found);
    }
    if let Some(found) = v.get("fields").and_then(|f| f.get(key)).filter(|f| !f.is_null()) {
        return Some(found);
    }
    for container in ["attributes", "spanAttributes"] {
        let Some(attrs) = v.get(container) else { continue };
        if let Some(found) = attrs.get(key) {
            let unwrapped = otlp_value(found);
            if !unwrapped.is_null() {
                return Some(unwrapped);
            }
        }
        if let Some(arr) = attrs.as_array()
            && let Some(item) = arr.iter().find(|item| item.get("key").and_then(Value::as_str) == Some(key))
            && let Some(val) = item.get("value")
        {
            let unwrapped = otlp_value(val);
            if !unwrapped.is_null() {
                return Some(unwrapped);
            }
        }
    }
    None
}

fn number_value(v: &Value) -> Option<f64> {
    v.as_f64().or_else(|| {
        let s = v.as_str()?;
        duration_ms(s).or_else(|| s.trim().parse::<f64>().ok())
    })
}

fn unix_nanos(v: &Value) -> Option<u128> {
    v.as_u64().map(u128::from).or_else(|| v.as_str()?.trim().parse::<u128>().ok())
}

fn span_duration_ms(v: &Value) -> Option<f64> {
    if let (Some(start), Some(end)) =
        (lookup(v, "startTimeUnixNano").and_then(unix_nanos), lookup(v, "endTimeUnixNano").and_then(unix_nanos))
        && end >= start
    {
        return Some((end - start) as f64 / 1_000_000.0);
    }
    let start = ["start_time", "startTime"]
        .iter()
        .find_map(|k| lookup(v, k)?.as_str())
        .and_then(culprit_core::format::epoch_seconds)?;
    let end = ["end_time", "endTime"]
        .iter()
        .find_map(|k| lookup(v, k)?.as_str())
        .and_then(culprit_core::format::epoch_seconds)?;
    (end >= start).then_some((end - start) * 1_000.0)
}

fn moment_from_value(v: &Value) -> Option<Moment> {
    if let Some(s) = v.as_str() {
        if let Some(epoch) = culprit_core::format::epoch_seconds(s) {
            let zoned = s.ends_with('Z') || s.rfind(['+', '-']).is_some_and(|p| p > 10);
            return Some(Moment { epoch, zoned });
        }
        if let Ok(n) = s.trim().parse::<f64>()
            && let Some(m) = moment_from_epoch_number(n)
        {
            return Some(m);
        }
        return None;
    }
    v.as_f64().and_then(moment_from_epoch_number)
}

fn moment_from_epoch_number(n: f64) -> Option<Moment> {
    if !n.is_finite() {
        return None;
    }
    if (1_000_000_000.0..100_000_000_000.0).contains(&n) {
        Some(Moment { epoch: n, zoned: true })
    } else if (1_000_000_000_000.0..100_000_000_000_000.0).contains(&n) {
        Some(Moment { epoch: n / 1_000.0, zoned: true })
    } else if (1_000_000_000_000_000.0..100_000_000_000_000_000.0).contains(&n) {
        Some(Moment { epoch: n / 1_000_000.0, zoned: true })
    } else if n >= 1_000_000_000_000_000_000.0 {
        Some(Moment { epoch: n / 1_000_000_000.0, zoned: true })
    } else {
        None
    }
}

fn is_non_ok_error(v: &&Value) -> bool {
    !v.is_null()
        && **v != Value::Bool(false)
        && v.as_u64() != Some(0)
        && v.as_str().is_none_or(|s| {
            let u = s.trim().to_ascii_uppercase();
            !u.is_empty()
                && !matches!(
                    u.as_str(),
                    "0" | "00000" | "SQLITE_OK" | "OK" | "STATUS_CODE_OK" | "UNSET" | "STATUS_CODE_UNSET" | "NO_ERROR"
                )
        })
        && v.as_object().is_none_or(|o| !o.is_empty())
}

fn entry(line: &str) -> Option<Entry> {
    let t = line.trim();
    if !t.starts_with('{') {
        return None;
    }
    let v: Value = serde_json::from_str(t).ok()?;
    let sql = ["sql", "query", "statement", "db.statement", "db.query.text"]
        .iter()
        .find_map(|k| lookup(&v, k)?.as_str())?
        .to_string();
    if !starts_like_sql(&sql) {
        return None;
    }
    let otel_error = v.get("status").and_then(|s| {
        let code = s.get("code")?;
        let is_err = code.as_u64() == Some(2)
            || code
                .as_str()
                .is_some_and(|c| c.eq_ignore_ascii_case("ERROR") || c.eq_ignore_ascii_case("STATUS_CODE_ERROR"));
        is_err.then(|| s.get("message").or_else(|| s.get("description")).unwrap_or(code))
    });
    let raw_error = lookup(&v, "error_code")
        .filter(is_non_ok_error)
        .or_else(|| v.pointer("/error/code").filter(is_non_ok_error))
        .or_else(|| lookup(&v, "db.response.status_code").filter(is_non_ok_error))
        .or_else(|| lookup(&v, "error").filter(is_non_ok_error))
        .or_else(|| otel_error.filter(is_non_ok_error));
    let error_code = raw_error.map(|value| {
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
            c if c.contains("SQLITE_CONSTRAINT") => "SQLITE_CONSTRAINT",
            c if c.contains("SQLITE_READONLY") => "SQLITE_READONLY",
            c if c.contains("SQLITE_IOERR") => "SQLITE_IOERR",
            c if c.contains("SQLITE_BUSY_SNAPSHOT") => "SQLITE_BUSY_SNAPSHOT",
            c if c.contains("SQLITE_BUSY") || c.contains("DATABASE IS LOCKED") => "SQLITE_BUSY",
            c if c.contains("SQLITE_LOCKED") => "SQLITE_LOCKED",
            c if c.contains("SQLITE_FULL") => "SQLITE_FULL",
            c if c.contains("SQLITE_CORRUPT") => "SQLITE_CORRUPT",
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
    let timed_ms = ["ms", "duration_ms", "elapsed_ms", "time_ms", "query_time_ms", "durationMs", "elapsedMs"]
        .iter()
        .find_map(|k| number_value(lookup(&v, k)?))
        .or_else(|| {
            ["duration_us", "elapsed_us", "time_us"]
                .iter()
                .find_map(|k| number_value(lookup(&v, k)?).map(|us| us / 1_000.0))
        })
        .or_else(|| {
            ["duration_ns", "elapsed_ns", "durationNano"]
                .iter()
                .find_map(|k| number_value(lookup(&v, k)?).map(|ns| ns / 1_000_000.0))
        })
        .or_else(|| ["duration", "elapsed"].iter().find_map(|k| duration_ms(lookup(&v, k)?.as_str()?)))
        .or_else(|| span_duration_ms(&v))
        .filter(|ms| ms.is_finite() && *ms >= 0.0);
    let ms = timed_ms.or_else(|| error_code.map(|_| f64::NAN))?;
    let at = ["at", "ts", "timestamp", "time", "start_time", "startTime"]
        .iter()
        .find_map(|k| moment_from_value(lookup(&v, k)?))
        .or_else(|| {
            let nanos =
                lookup(&v, "startTimeUnixNano").and_then(unix_nanos).filter(|&n| n >= 1_000_000_000_000_000_000)?;
            Some(Moment { epoch: nanos as f64 / 1_000_000_000.0, zoned: true })
        });
    let request_id = ["request_id", "requestId", "http.request_id", "trace_id", "traceId"]
        .iter()
        .find_map(|k| lookup(&v, k)?.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty() && !s.chars().all(|c| c == '0' || c == '-'))
        .map(str::to_string);
    Some(Entry { sql, ms, at, error_code, request_id })
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

    #[test]
    fn opentelemetry_spans_and_tracing_fields_are_parsed() {
        // OpenTelemetry flat attributes + startTimeUnixNano/endTimeUnixNano + traceId
        let otel_flat = r#"{"traceId":"4bf92f3577b34da6a3ce929d0e0e4736","startTimeUnixNano":"1760529600000000000","endTimeUnixNano":"1760529600012500000","attributes":{"db.system":"sqlite","db.statement":"SELECT * FROM orders WHERE customer_id = ?"}}"#;
        assert!(JsonLines.recognizes(otel_flat));
        let e = entry(otel_flat).unwrap();
        assert_eq!(e.sql, "SELECT * FROM orders WHERE customer_id = ?");
        assert!((e.ms - 12.5).abs() < 1e-6);
        assert_eq!(e.request_id.as_deref(), Some("4bf92f3577b34da6a3ce929d0e0e4736"));
        assert!(e.at.is_some_and(|m| m.zoned && (m.epoch - 1_760_529_600.0).abs() < 1e-3));

        // OTLP JSON key-value array format with `db.query.text` and error status
        let otlp_array = r#"{"traceId":"00000000000000000000000000000000","startTimeUnixNano":1760529600000000000,"endTimeUnixNano":1760529600003200000,"attributes":[{"key":"db.query.text","value":{"stringValue":"INSERT INTO orders (id) VALUES (?)"}}],"status":{"code":"STATUS_CODE_ERROR","message":"database is locked"}}"#;
        let e = entry(otlp_array).unwrap();
        assert_eq!(e.sql, "INSERT INTO orders (id) VALUES (?)");
        assert!((e.ms - 3.2).abs() < 1e-6);
        assert_eq!(e.error_code, Some("SQLITE_BUSY"));
        assert_eq!(e.request_id, None, "all-zero traceId is ignored");

        // Rust tracing-subscriber JSON format (`fields` envelope + microseconds)
        let tracing_line = r#"{"timestamp":"2026-10-15T12:00:05Z","level":"INFO","fields":{"db.statement":"UPDATE orders SET status = 'shipped' WHERE id = $1","duration_us":1500,"request_id":"req-99"}}"#;
        let e = entry(tracing_line).unwrap();
        assert_eq!(e.sql, "UPDATE orders SET status = 'shipped' WHERE id = $1");
        assert!((e.ms - 1.5).abs() < 1e-6);
        assert_eq!(e.request_id.as_deref(), Some("req-99"));

        // Node.js pino / Go zerolog numeric epoch millisecond timestamps and nanosecond durations
        let pino_line = r#"{"level":30,"time":1760529600500,"sql":"DELETE FROM sessions WHERE expired_at < ?","duration_ns":850000,"requestId":"req-100"}"#;
        let e = entry(pino_line).unwrap();
        assert!((e.ms - 0.85).abs() < 1e-6);
        assert!(e.at.is_some_and(|m| m.zoned && (m.epoch - 1_760_529_600.5).abs() < 1e-3));
        assert_eq!(e.request_id.as_deref(), Some("req-100"));

        // Null error_code or SQLSTATE "00000" does not shadow a real error or trigger a false error
        let fallback_err =
            r#"{"sql":"INSERT INTO orders (id) VALUES (?)","ms":1.1,"error_code":null,"error":"SQLITE_BUSY"}"#;
        assert_eq!(entry(fallback_err).unwrap().error_code, Some("SQLITE_BUSY"));
        let sqlstate_ok = r#"{"sql":"SELECT 1","ms":0.4,"attributes":{"db.response.status_code":"00000"}}"#;
        assert_eq!(entry(sqlstate_ok).unwrap().error_code, None);
    }
}
