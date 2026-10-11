//! A running statement on another connection: performance_schema's current statement and stage events (time so far,
//! rows examined, stage work done) and the plan MySQL is executing for it (EXPLAIN FOR CONNECTION, which reads the
//! running statement's plan and never runs anything).

use culprit_core::{Engine, PlanKind, QueryProgress, Result, session_number};

use crate::connect::{Connector, Scope};
use crate::plan_json;
use crate::rows::{explain_one, query_one};

pub(crate) const NO_OPERATOR_PROGRESS: &str = "MySQL doesn't report how far a running query has got, operator by \
operator. Culprit shows the plan MySQL is executing, how long it has run and how many rows it has examined so far. \
ALTER TABLE and other long statements report stage progress when performance_schema stage events are on.";

/// TIMER_* columns are picoseconds; for a statement still running, TIMER_WAIT is the time so far.
const STATEMENT_SQL: &str = r#"
SELECT t.PROCESSLIST_STATE AS state, t.PROCESSLIST_COMMAND AS command, t.PROCESSLIST_DB AS db,
       s.SQL_TEXT AS sql_text, s.TIMER_WAIT / 1000000000 AS elapsed_ms, s.LOCK_TIME / 1000000000 AS lock_ms,
       s.ROWS_EXAMINED AS rows_examined, s.ROWS_SENT AS rows_sent, s.CREATED_TMP_DISK_TABLES AS tmp_disk,
       g.EVENT_NAME AS stage, g.WORK_COMPLETED AS work_completed, g.WORK_ESTIMATED AS work_estimated
  FROM performance_schema.threads t
  LEFT JOIN performance_schema.events_statements_current s ON s.THREAD_ID = t.THREAD_ID AND s.END_EVENT_ID IS NULL
  LEFT JOIN performance_schema.events_stages_current g ON g.THREAD_ID = t.THREAD_ID AND g.END_EVENT_ID IS NULL
 WHERE t.PROCESSLIST_ID = {id}
"#;

/// The same from the process list, for servers where performance_schema is off or hidden from this login.
const PROCESSLIST_SQL: &str = "SELECT STATE AS state, COMMAND AS command, DB AS db, INFO AS sql_text, TIME * 1000 AS elapsed_ms FROM information_schema.PROCESSLIST WHERE ID = {id}";

pub(crate) async fn query_progress(connector: &Connector, session: &str, with_plan: bool) -> Result<QueryProgress> {
    let id = session_number(session)?;
    let mut conn = connector.connect(Scope::Server).await?;
    let taken_at = query_one(&mut conn, "SELECT DATE_FORMAT(UTC_TIMESTAMP(3), '%Y-%m-%dT%H:%i:%s.%fZ') AS now")
        .await?
        .map(|r| r.string("now"))
        .unwrap_or_default();
    let mut progress = QueryProgress::idle(session, Engine::Mysql, taken_at);
    let detailed = query_one(&mut conn, &STATEMENT_SQL.replace("{id}", &id.to_string())).await;
    let row = match detailed {
        Ok(Some(row)) if row.text("command").is_some() => Some(row),
        _ => query_one(&mut conn, &PROCESSLIST_SQL.replace("{id}", &id.to_string())).await?,
    };
    let Some(row) = row else {
        let _ = conn.disconnect().await;
        return Ok(progress);
    };
    let statement = row.text("sql_text").map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    progress.running = statement.is_some() && row.text("command").as_deref() == Some("Query");
    progress.state = row.text("state").filter(|s| !s.is_empty()).or_else(|| row.text("command"));
    progress.statement = statement;
    if !progress.running {
        let _ = conn.disconnect().await;
        return Ok(progress);
    }
    progress.elapsed_ms = row.number("elapsed_ms");
    // MySQL fills ROWS_EXAMINED in as the statement goes for some plans and only at the end for others: 0 means unknown.
    progress.rows_examined = row.number("rows_examined").filter(|r| *r > 0.0);
    progress.wait = row.number("lock_ms").filter(|ms| *ms >= 1.0).map(|ms| format!("Waited {ms:.0} ms for locks"));
    if let Some(stage) = row.text("stage") {
        let name = stage.trim_start_matches("stage/").replace('/', ": ");
        progress.phase = Some(name);
        if let (Some(done), Some(total)) = (row.number("work_completed"), row.number("work_estimated"))
            && total > 0.0
        {
            let fraction = (done / total).clamp(0.0, 1.0);
            progress.phase_percent = Some(fraction);
            progress.percent = Some(fraction);
            progress.percent_basis = Some("MySQL's own stage progress (WORK_COMPLETED of WORK_ESTIMATED)".into());
        }
    }
    if with_plan {
        // The session id is digits only (session_number), so splicing it in is safe.
        match explain_one(&mut conn, &format!("EXPLAIN FORMAT=JSON FOR CONNECTION {id}")).await {
            Ok(Some(rec)) => {
                if let Some(json) = rec.text("EXPLAIN").or_else(|| rec.text("explain"))
                    && let Ok(mut plan) = plan_json::parse(
                        &json,
                        PlanKind::Estimated,
                        progress.statement.as_deref(),
                        row.text("db").as_deref(),
                    )
                {
                    plan.raw = None;
                    progress.plan = Some(plan);
                }
            }
            Ok(None) => {}
            Err(e) => progress.note = Some(format!("{NO_OPERATOR_PROGRESS} Its plan couldn't be shown: {}", e.message)),
        }
    }
    if progress.note.is_none() && progress.percent.is_none() {
        progress.note = Some(NO_OPERATOR_PROGRESS.to_string());
    }
    let _ = conn.disconnect().await;
    Ok(progress)
}
