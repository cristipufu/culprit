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
    let activity = waits.as_ref().and_then(|w| w.activity.as_ref()).map(|a| {
        json!({
            "period":a.period,
            "categories":a.categories.iter().take(12).map(|c|json!({
                "key":c.key,"label":c.label,"share_of_sample_weight":c.share,
                "observations":c.observations,"weighted_observations":c.weighted_observations,
                "events":c.events.iter().take(8).collect::<Vec<_>>(),
                "events_retained":c.events.len(),"queries":c.queries.iter().take(5).collect::<Vec<_>>()
            })).collect::<Vec<_>>()
        })
    });
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
    let mut response = json!({
        "connection": snapshot.connection,
        "source": waits.as_ref().map(|w| w.source.clone()),
        "categories": categories,
        "notes": waits.map(|w| w.notes).unwrap_or_else(|| {
            let mut notes=vec!["The latest scan recorded no waits. Call waits with now=true to sample them.".into()];
            if let Some(reason)=snapshot.source_problem(&[culprit_core::source_id::DOCDB_ACTIVITY_HISTORY]) {notes.push(reason);}
            notes
        }),
        "signals": snapshot.pressure.as_ref().map(|p| p.signals.iter().map(signal).collect::<Vec<_>>()).unwrap_or_default(),
        "storage": snapshot.pressure.as_ref().map(|p| p.files.iter().take(12).map(file).collect::<Vec<_>>()).unwrap_or_default(),
        "replication": snapshot.pressure.as_ref().map(|p| p.replicas.clone()).unwrap_or_default(),
        "operations": snapshot.operations.without_retained_history(),
        "oldest_open_transaction": snapshot.pressure.as_ref().and_then(|p| p.oldest_transaction.as_ref()).map(|t| json!({
            "session": t.session, "open_for": format::duration(t.seconds * 1000.0), "state": t.state, "program": t.program,
        })),
    });
    if let Some(activity) = activity {
        response["activity"] = activity;
    }
    if let Some(history) = &snapshot.operations.termination_history {
        let mut by_reason = std::collections::BTreeMap::<String, usize>::new();
        for event in &history.events {
            *by_reason.entry(culprit_core::wire_name(&event.reason)).or_default() += 1;
        }
        response["terminations"] = json!({
            "source":history.source,"requested_start":history.requested_start,"requested_end":history.requested_end,
            "scope":history.scope,"nodes":history.nodes,"visibility":history.visibility,"partial":history.partial,"notes":history.notes,
            "records_retained":history.events.len(),"by_reason":by_reason,"events":history.events.iter().take(12).collect::<Vec<_>>()
        });
    } else if let Some(reason) = snapshot.source_problem(&[culprit_core::source_id::DOCDB_TERMINATIONS]) {
        response["terminations_unavailable"] = json!(reason);
    }
    response
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
    fn termination_output_bounds_records_and_keeps_native_limits_without_repeating_history() {
        let mut data: Report =
            serde_json::from_str(include_str!("../../../fixtures/yugabyte-demo.report.json")).unwrap();
        data.snapshot.operations.termination_history = Some(culprit_core::TerminationHistory {
            source: "native termination buffer".into(),
            requested_start: "2026-10-01T00:00:00Z".into(),
            requested_end: "2026-10-08T00:00:00Z".into(),
            scope: culprit_core::CounterScope::Node,
            nodes: vec!["sample-node".into()],
            visibility: "native login visibility".into(),
            partial: true,
            notes: vec!["No failure-rate denominator".into()],
            events: vec![
                culprit_core::QueryTermination {
                    query_id: None,
                    node: Some("sample-node".into()),
                    started_at: None,
                    ended_at: "2026-10-02T00:00:00Z".into(),
                    reason: culprit_core::TerminationReason::Sigkill
                };
                10_000
            ],
        });
        let history = data.snapshot.operations.termination_history.as_mut().unwrap();
        let mut older = history.events[0].clone();
        older.reason = culprit_core::TerminationReason::Sigsegv;
        history.events.push(older);
        let value = report(&data);
        let history = &value["terminations"];
        assert_eq!(history["events"].as_array().unwrap().len(), 12);
        assert_eq!(history["records_retained"], 10_001);
        assert_eq!(history["by_reason"]["sigkill"], 10_000);
        assert_eq!(
            history["by_reason"]["sigsegv"], 1,
            "older termination categories must survive the event-list limit"
        );
        assert_eq!(history["requested_start"], "2026-10-01T00:00:00Z");
        assert_eq!(history["partial"], true);
        assert!(value["operations"].get("termination_history").is_none());
        assert!(serde_json::to_vec(history).unwrap().len() < 5000);
    }

    #[test]
    fn activity_output_is_bounded_and_does_not_repeat_raw_history_or_invent_durations() {
        let mut data: Report =
            serde_json::from_str(include_str!("../../../fixtures/yugabyte-demo.report.json")).unwrap();
        data.snapshot.operations.activity_history = Some(culprit_core::ActivityHistory {
            period: culprit_core::ActivityPeriod {
                source: "native test samples".into(),
                requested_start: "2026-10-01T00:00:00Z".into(),
                requested_end: "2026-10-08T00:00:00Z".into(),
                first_sample: None,
                last_sample: None,
                scope: culprit_core::CounterScope::Node,
                nodes: vec![],
                observations: 20_000.0,
                weighted_observations: 40_000.0,
                partial: false,
                notes: vec!["Task weights are not elapsed time".into()],
            },
            events: (0..20_000)
                .map(|i| culprit_core::ActivityEvent {
                    node: Some("recorded-node".into()),
                    category: "rpc".into(),
                    component: "YSQL".into(),
                    wait_event: "TableRead".into(),
                    query_id: Some((i + 100).to_string()),
                    observations: 1.0,
                    weighted_observations: 2.0,
                })
                .collect(),
        });
        let result = report(&data);
        assert!(result["activity"]["period"]["partial"].as_bool().unwrap());
        let category = &result["activity"]["categories"][0];
        assert_eq!(category["events"].as_array().unwrap().len(), 8);
        assert_eq!(category["queries"].as_array().unwrap().len(), 5);
        assert_eq!(category["weighted_observations"], 20_000.0);
        assert!(category.get("time").is_none());
        assert!(result["operations"].get("activity_history").is_none());
        assert!(serde_json::to_vec(&result).unwrap().len() < 20_000);
    }

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
