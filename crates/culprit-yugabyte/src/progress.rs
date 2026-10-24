use crate::{
    probe,
    sessions::{literal, target},
};
use culprit_core::{Engine, QueryProgress, Result};
use culprit_postgres::wire::{Connector, INTERACTIVE, SessionLimits, elevated_login, query_one};

pub(crate) async fn read(connector: &Connector, id: &str, with_plan: bool) -> Result<(QueryProgress, bool)> {
    let (node, pid, started) = target(id)?;
    let session = connector.connect_observed(SessionLimits { catalog_path: true, ..INTERACTIVE }).await?;
    let p = probe::read(&session.client).await?;
    let relation = if p.global { "pg_catalog.\"gv$pg_stat_activity\"" } else { "pg_catalog.pg_stat_activity" };
    let node_guard = if p.global {
        format!("replace(a.server_uuid::text,'-','')={}", literal(&node))
    } else {
        format!("replace(yb_get_local_tserver_uuid()::text,'-','')={}", literal(&node))
    };
    let notices = session.notices();
    let activity = query_one(
        &session.client,
        &format!(
            "SELECT a.state,a.query,a.backend_type,a.wait_event_type,a.wait_event,
        extract(epoch FROM clock_timestamp()-a.query_start)*1000 AS elapsed_ms,
        (pg_get_userbyid(a.usesysid)=session_user AND current_user=session_user) AS own
        FROM {relation} a WHERE a.datid={} AND a.pid={pid} AND a.backend_start={}::timestamptz AND {node_guard}",
            p.oid,
            literal(&started)
        ),
    )
    .await?;
    let mut progress = QueryProgress::idle(id, Engine::Yugabyte, p.now);
    progress.note = Some("YSQL reports current activity and supported command phases; it does not expose completed-work percentages or live rows for every operator. Requested plans are estimates and never execute the statement.".into());
    let Some(row) = activity else {
        progress.note =
            Some("The YSQL backend ended, changed identity, or belongs to another node. Refresh Live.".into());
        return Ok((progress, false));
    };
    progress.state = row.text("state");
    progress.running = row.text("state").as_deref() == Some("active");
    progress.statement = row.text("query").filter(|s| s != "<insufficient privilege>");
    progress.elapsed_ms = row.number("elapsed_ms").filter(|n| *n >= 0.0 && progress.running);
    progress.wait = match (row.text("wait_event_type"), row.text("wait_event")) {
        (Some(kind), Some(event)) => Some(format!("{kind}: {event}")),
        (kind, event) => event.or(kind),
    };
    let worker = row.text("backend_type").as_deref() == Some("yb-conn-mgr worker connection");
    let partial = session.notices() != notices;
    if worker {
        progress.note = Some("This is a reusable Connection Manager worker, not a stable logical-client identity. Its latest observed statement can change between reads; automatic planning is withheld.".into());
    } else if partial || progress.state.is_none() {
        progress.note = Some(
            "Some activity was hidden or a node was skipped; this observation cannot establish complete progress."
                .into(),
        );
    }
    // Progress views report native command phases. Heap block counts and guessed percentages are not substituted.
    let text = progress.statement.as_deref().unwrap_or_default().trim_start().to_ascii_lowercase();
    let phase = if text.starts_with("create index") || text.starts_with("create unique index") {
        Some("pg_stat_progress_create_index")
    } else if text.starts_with("analyze") {
        Some("pg_stat_progress_analyze")
    } else {
        None
    };
    let activity_relation = relation;
    if let Some(view) = phase.filter(|_| progress.running) {
        let relation = if p.global { format!("pg_catalog.\"gv${view}\"") } else { format!("pg_catalog.{view}") };
        let guard =
            if p.global { format!(" AND replace(server_uuid::text,'-','')={}", literal(&node)) } else { String::new() };
        if let Ok(Some(phase)) = query_one(
            &session.client,
            &format!(
                "SELECT phase FROM {relation} WHERE pid={pid}{guard} AND datid={} AND EXISTS(
                SELECT 1 FROM {} a WHERE a.pid={pid} AND a.backend_start={}::timestamptz AND {}) LIMIT 1",
                p.oid,
                activity_relation,
                literal(&started),
                node_guard
            ),
        )
        .await
        {
            progress.phase = phase.text("phase");
        }
    }
    let allowed = with_plan
        && progress.running
        && !worker
        && !partial
        && progress.statement.is_some()
        && (row.flag("own") || !elevated_login(&session.client).await?);
    Ok((progress, allowed))
}
