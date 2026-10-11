use std::collections::BTreeMap;

use culprit_core::{Error, ErrorCode, LiveState, Result, Session, SessionAction, WaitStat, session_number};

use crate::conn::{MsClient, Pool, execute, query};
use crate::schema::{BENIGN_WAITS, wait_type_category};
use crate::value::Rec;

/// Running requests, plus idle sessions that hold a transaction open or block someone: the usual culprits in a blocking chain.
const SESSIONS_SQL: &str = r#"
SELECT
  CAST(s.session_id AS varchar(10)) AS id,
  s.login_name, s.program_name, s.host_name,
  DB_NAME(COALESCE(r.database_id, s.database_id)) AS database_name,
  COALESCE(r.status, s.status) AS state,
  r.command, r.wait_type, r.wait_time,
  CAST(NULLIF(r.blocking_session_id, 0) AS varchar(10)) AS blocked_by,
  COALESCE(r.total_elapsed_time, DATEDIFF_BIG(millisecond, s.last_request_start_time, GETDATE())) AS elapsed_ms,
  (SELECT DATEDIFF_BIG(millisecond, MIN(at.transaction_begin_time), GETDATE())
     FROM sys.dm_tran_session_transactions st
     JOIN sys.dm_tran_active_transactions at ON at.transaction_id = st.transaction_id
     WHERE st.session_id = s.session_id) AS transaction_ms,
  COALESCE(r.cpu_time, s.cpu_time) AS cpu_ms,
  COALESCE(r.logical_reads, s.logical_reads) AS reads,
  CASE WHEN r.sql_handle IS NOT NULL THEN
    SUBSTRING(t.text, r.statement_start_offset / 2 + 1,
      (CASE r.statement_end_offset WHEN -1 THEN DATALENGTH(t.text) ELSE r.statement_end_offset END - r.statement_start_offset) / 2 + 1)
  ELSE lt.text END AS sql_text
FROM sys.dm_exec_sessions s
LEFT JOIN sys.dm_exec_requests r ON r.session_id = s.session_id
OUTER APPLY sys.dm_exec_sql_text(r.sql_handle) t
LEFT JOIN sys.dm_exec_connections c ON c.session_id = s.session_id AND c.parent_connection_id IS NULL
OUTER APPLY sys.dm_exec_sql_text(c.most_recent_sql_handle) lt
WHERE s.is_user_process = 1
  AND s.session_id <> @@SPID
  AND (r.session_id IS NOT NULL
       OR s.open_transaction_count > 0
       OR s.session_id IN (SELECT blocking_session_id FROM sys.dm_exec_requests WHERE blocking_session_id <> 0))
ORDER BY elapsed_ms DESC
"#;

/// Fallback for servers where the connection DMV is not visible: no last statement for idle sessions.
const SESSIONS_BASIC_SQL: &str = r#"
SELECT
  CAST(s.session_id AS varchar(10)) AS id,
  s.login_name, s.program_name, s.host_name,
  DB_NAME(COALESCE(r.database_id, s.database_id)) AS database_name,
  COALESCE(r.status, s.status) AS state,
  r.command, r.wait_type, r.wait_time,
  CAST(NULLIF(r.blocking_session_id, 0) AS varchar(10)) AS blocked_by,
  r.total_elapsed_time AS elapsed_ms,
  COALESCE(r.cpu_time, s.cpu_time) AS cpu_ms,
  COALESCE(r.logical_reads, s.logical_reads) AS reads,
  SUBSTRING(t.text, r.statement_start_offset / 2 + 1,
    (CASE r.statement_end_offset WHEN -1 THEN DATALENGTH(t.text) ELSE r.statement_end_offset END - r.statement_start_offset) / 2 + 1) AS sql_text
FROM sys.dm_exec_sessions s
LEFT JOIN sys.dm_exec_requests r ON r.session_id = s.session_id
OUTER APPLY sys.dm_exec_sql_text(r.sql_handle) t
WHERE s.is_user_process = 1
  AND s.session_id <> @@SPID
  AND (r.session_id IS NOT NULL OR s.open_transaction_count > 0)
"#;

pub(crate) async fn collect(client: &mut MsClient) -> tiberius::Result<LiveState> {
    let rows = match query(client, SESSIONS_SQL).await {
        Ok(rows) => rows,
        Err(_) => query(client, SESSIONS_BASIC_SQL).await?,
    };
    let sessions: Vec<Session> = rows.iter().map(session_of).collect();
    Ok(LiveState {
        taken_at: chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string(),
        waits: current_waits(&sessions),
        sessions,
        session_actions: Vec::new(),
        session_actions_note: None,
    })
}

/// SQL Server has no way to stop another session's statement short of `KILL`, which ends the session and rolls back its
/// transaction. Culprit's own sessions and system sessions are refused.
pub(crate) async fn interrupt(pool: &Pool, session: &str, action: SessionAction) -> Result<()> {
    let id = session_number(session)?;
    if action == SessionAction::Cancel {
        return Err(Error::new(
            ErrorCode::Unsupported,
            "SQL Server can't stop another session's statement without ending the session",
        )
        .with_hint("End the session instead; its open transaction rolls back."));
    }
    let target = format!(
        "SELECT CAST(is_user_process AS int) AS user_process, program_name, CASE WHEN session_id = @@SPID THEN 1 ELSE 0 END AS own \
         FROM sys.dm_exec_sessions WHERE session_id = {id}"
    );
    let rows = pool
        .with(|c| {
            let sql = target.clone();
            Box::pin(async move { query(c, &sql).await })
        })
        .await?;
    let Some(row) = rows.first() else {
        return Err(Error::not_found(format!("Session {id} is gone")));
    };
    if row.f64("own") == Some(1.0) || row.str("program_name").is_some_and(|p| p.eq_ignore_ascii_case("culprit")) {
        return Err(Error::invalid(format!("Session {id} is Culprit's own")));
    }
    if row.f64("user_process") != Some(1.0) {
        return Err(Error::invalid(format!("Session {id} is a SQL Server system session")));
    }
    pool.with(|c| Box::pin(async move { execute(c, &format!("KILL {id}")).await })).await
}

fn session_of(r: &Rec) -> Session {
    Session {
        id: r.string("id"),
        user: r.str("login_name"),
        application: r.str("program_name").filter(|s| !s.is_empty()),
        host: r.str("host_name").filter(|s| !s.is_empty()),
        database: r.str("database_name"),
        state: r.string("state"),
        command: r.str("command"),
        wait: r.str("wait_type"),
        wait_ms: r.f64("wait_time").filter(|_| r.str("wait_type").is_some()),
        blocked_by: r.str("blocked_by"),
        elapsed_ms: r.f64("elapsed_ms"),
        transaction_ms: r.f64("transaction_ms"),
        cpu_ms: r.f64("cpu_ms"),
        reads: r.f64("reads"),
        sql: r.str("sql_text").map(|s| s.trim().to_string()).filter(|s| !s.is_empty()),
    }
}

fn current_waits(sessions: &[Session]) -> Vec<WaitStat> {
    let mut by: BTreeMap<&'static str, (f64, f64, String)> = BTreeMap::new();
    for s in sessions {
        let Some(wait) = s.wait.as_deref() else { continue };
        if BENIGN_WAITS.contains(&wait) {
            continue;
        }
        let entry = by.entry(wait_type_category(wait)).or_default();
        entry.0 += s.wait_ms.unwrap_or(0.0);
        entry.1 += 1.0;
        if entry.2.is_empty() {
            entry.2 = wait.to_string();
        }
    }
    let mut out: Vec<WaitStat> = by
        .into_iter()
        .map(|(category, (ms, tasks, wait_type))| WaitStat {
            category: category.into(),
            wait_type: Some(wait_type),
            wait_ms: ms,
            tasks: Some(tasks),
        })
        .collect();
    out.sort_by(|a, b| b.wait_ms.total_cmp(&a.wait_ms));
    out
}
