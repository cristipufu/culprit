use crate::probe::{self, node_id};
use culprit_core::{Engine, Environment, Error, ErrorCode, LiveState, LockWait, Result, Session, SessionAction};
use culprit_postgres::wire::{Connector, INTERACTIVE, SessionLimits, query, query_one, tagged};
use serde_json::Value;
use std::collections::HashMap;

fn text(v: &Value, key: &str) -> Option<String> {
    v.get(key)?.as_str().map(str::to_string)
}
fn elapsed(now: &str, then: Option<&str>) -> Option<f64> {
    let seconds = culprit_core::format::epoch_seconds(now)? - culprit_core::format::epoch_seconds(then?)?;
    (seconds >= 0.0).then_some(seconds * 1000.0)
}
pub(crate) fn literal(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

pub(crate) async fn live(connector: &Connector, environment: Environment) -> Result<LiveState> {
    let observed = connector.connect_observed(SessionLimits { catalog_path: true, ..INTERACTIVE }).await?;
    let p = probe::read(&observed.client).await?;
    let relation = if p.global { "pg_catalog.\"gv$pg_stat_activity\"" } else { "pg_catalog.pg_stat_activity" };
    let count = observed.notices();
    let rows=query(&observed.client,&format!("SELECT to_jsonb(a)::text AS sample FROM {relation} a WHERE datname={}
        AND backend_type IN ('client backend','yb-conn-mgr worker connection') AND COALESCE(application_name,'') <> 'culprit' LIMIT 1001",literal(&p.database))).await?;
    let mut notes = vec![];
    if !p.global {
        notes.push(format!(
            "Sessions cover connected YB-TServer {} only. Enable and authorize cluster views to observe other nodes.",
            p.node.as_deref().unwrap_or("unknown")
        ));
    }
    if observed.notices() != count {
        notes.push(
            "A YSQL node was skipped or raised a warning while reading sessions; cluster activity is incomplete."
                .into(),
        );
    }
    if rows.len() > 1000 {
        notes.push("Only the first 1,000 YSQL sessions are retained; activity and blocking totals are partial.".into());
    }
    let mut sessions = Vec::new();
    let mut transactions = HashMap::new();
    let mut backends = HashMap::new();
    for row in rows.into_iter().take(1000) {
        let Some(raw) = row.text("sample") else { continue };
        let v: Value =
            serde_json::from_str(&raw).map_err(|_| Error::invalid("YSQL returned malformed session metadata"))?;
        let Some(pid) = v.get("pid").and_then(Value::as_i64).filter(|n| *n > 0) else {
            notes.push("A YSQL session had no usable backend identity.".into());
            continue;
        };
        let node = if p.global { text(&v, "server_uuid").and_then(|s| node_id(&s)) } else { p.node.clone() };
        if node.is_none() {
            notes.push("A YSQL session's node identity is unknown; it cannot be signaled safely.".into());
        }
        let started = text(&v, "backend_start").unwrap_or_default();
        let id = format!("{}/{pid}/{started}", node.as_deref().unwrap_or("unknown"));
        if let Some(node) = node {
            backends.insert((node, pid.to_string()), id.clone());
        }
        if let Some(tx) = text(&v, "yb_backend_xid") {
            transactions.insert(tx.replace('-', ""), id.clone());
        }
        let sql = text(&v, "query").filter(|s| s != "<insufficient privilege>");
        if sql.is_none() && text(&v, "state").as_deref().is_none_or(|s| s == "active") {
            notes.push("Some active statements are hidden by this login's permissions.".into());
        }
        sessions.push(Session {
            id,
            user: text(&v, "usename"),
            application: text(&v, "application_name"),
            host: text(&v, "client_addr"),
            database: text(&v, "datname"),
            state: text(&v, "state").unwrap_or_else(|| "unknown".into()),
            command: None,
            wait: match (text(&v, "wait_event_type"), text(&v, "wait_event")) {
                (Some(kind), Some(event)) => Some(format!("{kind}: {event}")),
                (kind, event) => event.or(kind),
            },
            wait_ms: None,
            blocked_by: None,
            elapsed_ms: if text(&v, "state").as_deref() == Some("active") {
                elapsed(&p.now, text(&v, "query_start").as_deref())
            } else {
                None
            },
            transaction_ms: elapsed(&p.now, text(&v, "xact_start").as_deref()),
            cpu_ms: None,
            reads: None,
            sql,
        });
    }
    // This session's metadata filter may include young waiters without changing application or cluster settings.
    // Native lock collection still has transaction/per-tablet caps; an empty result cannot establish absence.
    if observed.client.batch_execute(&tagged("SET yb_locks_min_txn_age=0")).await.is_err() {
        notes.push("Short-lived DocDB locks may be omitted by the server's minimum transaction-age filter.".into());
    }
    notes.push("DocDB lock collection follows transaction heartbeats and has transaction/per-tablet limits; missing chains do not establish that no session is blocked.".into());
    let count = observed.notices();
    let locks = query(
        &observed.client,
        &format!(
            "SELECT l.node,l.pid::text AS pid,l.transaction_id::text AS transaction,l.blocked_by::text,
        l.mode::text,l.granted,l.locktype,n.nspname || '.' || c.relname AS object
        FROM yb_lock_status(NULL,NULL) l LEFT JOIN pg_class c ON c.oid=l.relation
        LEFT JOIN pg_namespace n ON n.oid=c.relnamespace WHERE l.database={} AND NOT l.granted LIMIT 2001",
            p.oid
        ),
    )
    .await;
    let mut waits = Vec::<LockWait>::new();
    match locks {
        Ok(rows) => {
            if rows.len() > 2000 || observed.notices() != count {
                notes.push("DocDB lock evidence is partial.".into());
            }
            for row in rows.into_iter().take(2000) {
                let waiter = node_id(&row.string("node"))
                    .and_then(|node| backends.get(&(node, row.string("pid"))).cloned())
                    .or_else(|| row.text("transaction").and_then(|id| transactions.get(&id.replace('-', "")).cloned()));
                let Some(waiter) = waiter else { continue };
                let blockers = row.string("blocked_by");
                let blocker = blockers.trim_matches(['{', '}']).split(',').find_map(|tx| {
                    let tx = tx.trim().trim_matches('"').replace('-', "");
                    (!tx.is_empty())
                        .then(|| transactions.get(&tx).cloned().unwrap_or_else(|| format!("transaction/{tx}")))
                });
                if let Some(s) = sessions.iter_mut().find(|s| s.id == waiter) {
                    s.blocked_by = blocker;
                }
                waits.push(LockWait {
                    session: waiter,
                    resource: row.text("locktype"),
                    object: row.text("object"),
                    index: None,
                    mode: row.text("mode"),
                });
            }
        }
        Err(_) => {
            notes.push("DocDB lock metadata is unavailable; PostgreSQL heap lock views are not substituted.".into())
        }
    }
    notes.sort();
    notes.dedup();
    let blocking = culprit_core::blocking::analyze(Engine::Yugabyte, &sessions, &waits);
    Ok(LiveState {
        taken_at: p.now,
        sessions,
        waits: vec![],
        notes,
        blocking,
        session_actions: if environment.is_production() || p.node.is_none() {
            vec![]
        } else {
            Engine::Yugabyte.spec().session_actions()
        },
        session_actions_note: if environment.is_production() {
            Some("Production connection: Culprit does not cancel or end sessions here.".into())
        } else {
            Some("Actions require this node, the same backend start time, and a direct client backend. Pooled Connection Manager workers are not logical clients.".into())
        },
    })
}

pub(crate) fn target(id: &str) -> Result<(String, i32, String)> {
    culprit_core::ysql_session_target(id)
}

pub(crate) async fn interrupt(connector: &Connector, id: &str, action: SessionAction) -> Result<()> {
    let (node, pid, started) = target(id)?;
    let function = match action {
        SessionAction::Cancel => "pg_cancel_backend",
        SessionAction::Terminate => "pg_terminate_backend",
    };
    let client = connector.connect(INTERACTIVE).await?;
    let row=query_one(&client,&format!("SELECT pg_catalog.{function}(a.pid) AS stopped FROM pg_catalog.pg_stat_activity a
        WHERE a.pid={pid} AND a.datname=pg_catalog.current_database() AND a.backend_type='client backend'
        AND a.pid <> pg_catalog.pg_backend_pid() AND COALESCE(a.application_name,'') <> 'culprit'
        AND a.backend_start={}::timestamptz AND pg_catalog.replace(pg_catalog.yb_get_local_tserver_uuid()::text,'-','')={}",literal(&started),literal(&node))).await?;
    if row.is_some_and(|r| r.flag("stopped")) {
        Ok(())
    } else {
        Err(Error::new(ErrorCode::NotFound, "The YSQL session ended, belongs to another node, or is a pooled backend")
            .with_hint("Refresh Live; session actions never signal an unverified or pooled logical client."))
    }
}
