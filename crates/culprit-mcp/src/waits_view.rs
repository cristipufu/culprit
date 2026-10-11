//! Compact JSON for the `waits` tool: categories with their meaning and top queries, and the pressure signals.

use culprit_core::{Report, Signal, WaitSample, format};
use serde_json::{Value, json};

fn signal(s: &Signal) -> Value {
    json!({
        "key": s.key,
        "label": s.label,
        "value": (s.value * 100.0).round() / 100.0,
        "unit": s.unit,
        "scope": s.scope,
        "status": s.status,
        "healthy": s.healthy,
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
        "busy_sessions_on_average": s.active_sessions,
        "signals": s.signals.iter().map(signal).collect::<Vec<_>>(),
        "notes": s.notes,
    })
}
