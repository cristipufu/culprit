use crate::{
    probe,
    sessions::{literal, target},
};
use culprit_core::{Engine, QueryProgress, Result, format};
use culprit_postgres::wire::{Connector, INTERACTIVE, ObservedSession, Row, SessionLimits, elevated_login, query_one};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Command {
    Index,
    Analyze,
    Copy,
}

impl Command {
    fn for_statement(text: &str) -> Option<Self> {
        let statement = culprit_core::sql::check(text, Engine::Yugabyte);
        match statement.verb.as_str() {
            "CREATE" if culprit_core::sql::creates_index(text, Engine::Yugabyte) => Some(Self::Index),
            "REINDEX" => Some(Self::Index),
            "ANALYZE" => Some(Self::Analyze),
            "COPY" => Some(Self::Copy),
            _ => None,
        }
    }

    fn view(self) -> &'static str {
        match self {
            Self::Index => "pg_stat_progress_create_index",
            Self::Analyze => "pg_stat_progress_analyze",
            Self::Copy => "pg_stat_progress_copy",
        }
    }

    fn columns(self) -> &'static str {
        match self {
            Self::Index => "command,phase,tuples_done,tuples_total",
            Self::Analyze => "phase",
            Self::Copy => "command,type,yb_status,tuples_processed,bytes_processed,bytes_total",
        }
    }

    fn describe(self, row: &Row) -> Option<(String, &'static str)> {
        let count = |key| row.number(key).filter(|n| *n >= 0.0 && n.fract() == 0.0);
        match self {
            Self::Index => {
                let phase = row.text("phase")?;
                let mut text = format!("{}: {phase}", row.text("command")?);
                if phase == "backfilling" {
                    if let Some(done) = count("tuples_done") {
                        text.push_str(&format!(" · {} table rows processed", format::count(done)));
                    }
                    if let Some(total) = count("tuples_total").filter(|n| *n > 0.0) {
                        text.push_str(&format!(" · about {} table rows estimated", format::count(total)));
                    }
                }
                Some((
                    text,
                    "Index backfill counts describe table rows, including for partial indexes. The total comes from an estimate; it is not a measured completion percentage.",
                ))
            }
            Self::Analyze => Some((
                format!("ANALYZE: {}", row.text("phase")?),
                "ANALYZE reports its current native phase. PostgreSQL heap-block counters do not measure DocDB progress.",
            )),
            Self::Copy => {
                // YugabyteDB retains the last COPY result on this backend after SUCCESS or ERROR.
                if row.text("yb_status").as_deref() != Some("IN PROGRESS") {
                    return None;
                }
                let command = row.text("command")?;
                let mut text = format!("{command} {}: IN PROGRESS", row.text("type").unwrap_or_default());
                if let Some(rows) = count("tuples_processed") {
                    text.push_str(&format!(" · {} rows reported processed", format::count(rows)));
                }
                if let Some(bytes) = count("bytes_processed") {
                    text.push_str(&format!(" · {} processed", format::bytes(bytes)));
                }
                if let Some(total) = count("bytes_total").filter(|n| *n > 0.0) {
                    text.push_str(&format!(" · {} source file", format::bytes(total)));
                }
                Some((
                    text,
                    "Native row counters follow the source's transaction mode. Batched COPY FROM can leave completed transactions persisted after a later error. A zero or missing file size is unknown; counters do not prove whole-command completion or commit.",
                ))
            }
        }
    }
}

pub(crate) async fn read(connector: &Connector, id: &str, with_plan: bool) -> Result<(QueryProgress, bool)> {
    target(id)?;
    let session = connector.connect_observed(SessionLimits { catalog_path: true, ..INTERACTIVE }).await?;
    read_session(&session, id, with_plan).await
}

pub(crate) async fn read_session(
    session: &ObservedSession,
    id: &str,
    with_plan: bool,
) -> Result<(QueryProgress, bool)> {
    let (node, pid, started) = target(id)?;
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
            "SELECT a.state,a.query,a.backend_type,a.wait_event_type,a.wait_event,{} AS query_start,
        extract(epoch FROM clock_timestamp()-a.query_start)*1000 AS elapsed_ms,
        (pg_get_userbyid(a.usesysid)=session_user AND current_user=session_user) AS own
        FROM {relation} a WHERE a.datid={} AND a.pid={pid} AND a.backend_start={}::timestamptz AND {node_guard}",
            probe::timestamp("a.query_start"),
            p.oid,
            literal(&started)
        ),
    )
    .await?;
    let mut progress = QueryProgress::idle(id, Engine::Yugabyte, p.now);
    progress.note = Some("YSQL reports current activity and supported command phases; it does not expose completed-work percentages or live rows for every operator. Requested plans are estimates and never execute the statement.".into());
    let Some(row) = activity else {
        progress.note =
            Some("The YSQL backend ended, changed identity, belongs to another node, or is hidden from this login. Refresh Live.".into());
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
    // Native YSQL progress views can also return retained COPY counters for an unrelated command. Select the view
    // with the shared SQL lexer (including comments), then require its own native command/status metadata.
    // Pin query_start as well as backend_start: one backend can start a different command between these reads.
    if progress.running
        && !worker
        && !partial
        && let (Some(query_start), Some(command)) =
            (row.text("query_start"), progress.statement.as_deref().and_then(Command::for_statement))
    {
        let view = command.view();
        let command_relation =
            if p.global { format!("pg_catalog.\"gv${view}\"") } else { format!("pg_catalog.{view}") };
        let guard =
            if p.global { format!(" AND replace(server_uuid::text,'-','')={}", literal(&node)) } else { String::new() };
        let before = session.notices();
        let phase = query_one(
            &session.client,
            &format!(
                "SELECT {} FROM {command_relation} WHERE pid={pid}{guard} AND datid={} AND EXISTS(
                    SELECT 1 FROM {relation} a WHERE a.datid={} AND a.pid={pid}
                    AND a.backend_start={}::timestamptz AND {node_guard}
                    AND a.query_start={}::timestamptz AND a.state='active') LIMIT 1",
                command.columns(),
                p.oid,
                p.oid,
                literal(&started),
                literal(&query_start)
            ),
        )
        .await;
        match phase {
            Ok(Some(row)) if session.notices() == before => {
                if let Some((text, note)) = command.describe(&row) {
                    progress.phase = Some(text);
                    progress.note = Some(format!("{} {note}", progress.note.as_deref().unwrap_or_default()));
                }
            }
            Ok(None) if session.notices() == before => {}
            _ => {
                progress.note = Some("Current activity was read, but the native command-progress view was unavailable or skipped a node. No completion percentage is inferred.".into());
            }
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

#[cfg(test)]
mod tests {
    use super::Command;

    #[test]
    fn native_progress_requires_an_unambiguous_current_command() {
        assert_eq!(Command::for_statement("/* CREATE INDEX fake */ COPY t FROM STDIN"), Some(Command::Copy));
        assert_eq!(
            Command::for_statement("-- application\nCREATE /* nested /* comment */ */ UNIQUE INDEX i ON t(c)"),
            Some(Command::Index)
        );
        assert_eq!(Command::for_statement("REINDEX INDEX i"), Some(Command::Index));
        assert_eq!(Command::for_statement("ANALYZE t"), Some(Command::Analyze));
        for text in [
            "SELECT 'COPY t FROM STDIN'",
            "CREATE TABLE t (copy text)",
            "COPY t FROM STDIN; SELECT 1",
            "COPY t FROM 'truncated",
        ] {
            assert_eq!(Command::for_statement(text), None, "{text}");
        }
    }
}
