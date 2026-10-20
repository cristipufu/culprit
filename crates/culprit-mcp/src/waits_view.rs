//! Compact JSON for the `waits` tool: categories with their meaning and top queries, and the pressure signals.

use culprit_core::{FileIo, Report, Signal, WaitSample, format};
use serde_json::{Value, json};

fn signal(s: &Signal) -> Value {
    json!({
        "key": s.key,
        "label": s.label,
        "value": s.value,
        "unit": s.unit,
        "scope": s.scope,
        "status": s.status,
        "healthy": s.healthy,
    })
}

/// One file's average latency since startup, in words an agent can quote.
fn file(f: &FileIo) -> Value {
    let avg = |ms: Option<f64>, n: f64| {
        ms.filter(|_| n > 0.0)
            .map(|ms| ms / n)
            .map(|ms| if ms < 0.1 { format!("{:.0} µs", ms * 1000.0) } else { format::duration(ms) })
    };
    json!({
        "file": f.name,
        "kind": f.kind,
        "database": f.database,
        "reads": f.reads,
        "avg_read": avg(f.read_ms, f.reads),
        "writes": f.writes,
        "avg_write": avg(f.write_ms, f.writes),
    })
}

pub fn report(report: &Report) -> Value {
    let snapshot = &report.snapshot;
    let waits = culprit_engine::explain_waits(snapshot);
    let categories: Vec<Value> = waits
        .as_ref()
        .map(|w| {
            w.categories
                .iter()
                .map(|c| {
                    json!({
                        "category": c.label,
                        "key": c.key,
                        "share": format::percent(c.share),
                        "time": format::duration(c.wait_ms),
                        "top_wait": c.top_wait,
                        "meaning": c.meaning,
                        "detail": c.detail,
                        "queries": c.queries.iter().take(5).map(|q| json!({
                            "id": q.id,
                            "share_of_category": format::percent(q.share_of_category),
                            "share_of_query_time": format::percent(q.share_of_query),
                            "text": q.text.chars().take(140).collect::<String>(),
                        })).collect::<Vec<_>>(),
                        "not_attributed": c.attribution,
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    json!({
        "connection": snapshot.connection,
        "source": waits.as_ref().map(|w| w.source.clone()),
        "categories": categories,
        "notes": waits.map(|w| w.notes).unwrap_or_else(|| vec!["The latest scan recorded no waits. Call waits with now=true to sample them.".into()]),
        "signals": snapshot.pressure.as_ref().map(|p| p.signals.iter().map(signal).collect::<Vec<_>>()).unwrap_or_default(),
        "storage": snapshot.pressure.as_ref().map(|p| p.files.iter().take(12).map(file).collect::<Vec<_>>()).unwrap_or_default(),
        "replication": snapshot.pressure.as_ref().map(|p| p.replicas.clone()).unwrap_or_default(),
        "operations": snapshot.operations,
        "oldest_open_transaction": snapshot.pressure.as_ref().and_then(|p| p.oldest_transaction.as_ref()).map(|t| json!({
            "session": t.session, "open_for": format::duration(t.seconds * 1000.0), "state": t.state, "program": t.program,
        })),
    })
}

pub fn sample(s: &WaitSample) -> Value {
    json!({
        "seconds": s.seconds.round(),
        "method": s.method,
        "categories": s.categories.iter().map(|c| {
            let key = culprit_core::waits::category_key(&c.category);
            json!({
                "category": culprit_core::waits::category_info(key, s.engine).0,
                "share": format::percent(if s.total_ms > 0.0 { c.wait_ms / s.total_ms } else { 0.0 }),
                "time": format::duration(c.wait_ms),
                "top_wait": c.wait_type,
            })
        }).collect::<Vec<_>>(),
        // An empty sum is -0.0; a count of sessions is never negative.
        "busy_sessions_on_average": s.active_sessions.map(|v| v.max(0.0)),
        "signals": s.signals.iter().map(signal).collect::<Vec<_>>(),
        "notes": s.notes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use culprit_core::{Engine, SignalScope, SignalUnit};

    #[test]
    fn waits_consumers_receive_the_measured_value_without_display_rounding() {
        let mut report_data: Report =
            serde_json::from_str(include_str!("../../../fixtures/mssql-demo.report.json")).unwrap();
        for (value, unit) in [
            (0.004, SignalUnit::Percent),
            (0.000_001, SignalUnit::PerSecond),
            (0.004, SignalUnit::Milliseconds),
            (1e308, SignalUnit::Bytes),
        ] {
            let measured = Signal::new("measured", "Measured signal", value, unit, SignalScope::Sampled);
            report_data.snapshot.pressure.as_mut().unwrap().signals = vec![measured.clone()];
            let sampled = WaitSample {
                engine: Engine::Mssql,
                taken_at: report_data.snapshot.taken_at.clone(),
                seconds: 30.0,
                method: "Two counter readings".into(),
                total_ms: 0.0,
                categories: vec![],
                signals: vec![measured],
                active_sessions: None,
                notes: vec![],
            };
            for response in [report(&report_data), sample(&sampled)] {
                assert_eq!(response["signals"][0]["value"].as_f64(), Some(value));
                assert_eq!(response["signals"][0]["unit"], serde_json::to_value(unit).unwrap());
            }
        }
    }
}
