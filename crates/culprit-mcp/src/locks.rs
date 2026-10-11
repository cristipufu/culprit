//! Compact views for the `blocking` and `deadlocks` tools. Database text (statements, names) is returned as data.

use culprit_core::{BlockNode, DeadlockReport, LiveState};
use serde_json::{Value, json};

const SQL_CLIP: usize = 300;

fn clip(text: &str) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= SQL_CLIP {
        flat
    } else {
        format!("{}…", flat.chars().take(SQL_CLIP).collect::<String>())
    }
}

fn round(v: f64) -> f64 {
    v.round()
}

fn node(live: &LiveState, n: &BlockNode) -> Value {
    let s = live.sessions.iter().find(|s| s.id == n.session);
    json!({
        "session": n.session,
        "state": s.map(|s| s.state.clone()),
        "user": s.and_then(|s| s.user.clone()),
        "host": s.and_then(|s| s.host.clone()),
        "application": s.and_then(|s| s.application.clone()),
        "waits_for": n.waits_for,
        "wait_type": s.and_then(|s| s.wait.clone()),
        "wait_ms": s.and_then(|s| s.wait_ms).map(round),
        "elapsed_ms": s.and_then(|s| s.elapsed_ms).map(round),
        "transaction_ms": s.and_then(|s| s.transaction_ms).map(round),
        "sql": s.and_then(|s| s.sql.as_deref()).map(clip),
        "blocks": n.children.iter().map(|c| node(live, c)).collect::<Vec<_>>(),
    })
}

pub fn blocking(live: &LiveState) -> Value {
    let Some(b) = live.blocking.as_ref() else {
        return json!({ "taken_at": live.taken_at, "waiting": 0, "chains": [], "summary": "Nothing is blocked." });
    };
    json!({
        "taken_at": live.taken_at,
        "waiting": b.waiting,
        "chains": b.chains.iter().map(|c| json!({
            "head": c.head,
            "head_kind": c.head_kind,
            "headline": c.headline,
            "advice": c.advice,
            "waiters": c.waiters,
            "longest_wait_ms": c.longest_wait_ms.map(round),
            "tree": node(live, &c.tree),
        })).collect::<Vec<_>>(),
    })
}

pub fn deadlocks(report: &DeadlockReport) -> Value {
    json!({
        "source": report.source,
        "since_hours": report.since_hours,
        "count": report.deadlocks.len(),
        "counted": report.counted,
        "counted_since": report.counted_since,
        "note": report.note,
        "deadlocks": report.deadlocks.iter().take(20).map(|d| json!({
            "id": d.id,
            "at": d.at,
            "pattern": d.pattern,
            "objects": d.objects,
            "headline": d.headline,
            "explanation": d.explanation,
            "processes": d.processes.iter().map(|p| json!({
                "session": p.session,
                "rolled_back": p.victim,
                "application": p.application,
                "login": p.login,
                "isolation": p.isolation,
                "statement": p.statement.as_deref().map(clip),
            })).collect::<Vec<_>>(),
            "locks": d.resources.iter().map(|r| json!({
                "kind": r.kind,
                "object": r.object,
                "index": r.index,
                "held_by": r.holders.iter().map(|h| json!({ "process": h.process, "mode": h.mode })).collect::<Vec<_>>(),
                "wanted_by": r.waiters.iter().map(|h| json!({ "process": h.process, "mode": h.mode })).collect::<Vec<_>>(),
            })).collect::<Vec<_>>(),
            "fixes": d.fixes,
        })).collect::<Vec<_>>(),
    })
}
