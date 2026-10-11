//! MySQL provider for Culprit: read-only snapshots from performance_schema and information_schema, plans via EXPLAIN,
//! live sessions with lock waits.

mod collect;
mod connect;
pub mod deadlocks;
pub mod plan_json;
pub mod plan_tree;
mod pressure;
mod progress;
mod rows;
mod sessions;

use std::collections::HashMap;
use std::time::Instant;

use async_trait::async_trait;
use culprit_core::{
    ConnectionSpec, Engine, Error, ExplainMode, LiveState, Plan, PlanKind, Provider, Result, ResultSet, ServerInfo,
    Session, SessionAction, Snapshot, SnapshotOptions, WaitStat,
};
use mysql_async::consts::ColumnType;
use mysql_async::prelude::Queryable;
use mysql_async::{Conn, Value};
use serde_json::Value as Json;

use connect::{Connector, Scope, map_error};
use rows::{explain_one, query, query_one, tagged, value_text};

pub struct MysqlProvider {
    connector: Connector,
    spec: ConnectionSpec,
}

impl MysqlProvider {
    pub fn new(spec: &ConnectionSpec) -> Result<Self> {
        if let Some(engine) = spec.engine
            && engine != Engine::Mysql
        {
            return Err(Error::invalid(format!("This connection is for {engine}, not MySQL")));
        }
        Ok(MysqlProvider { connector: Connector::new(spec)?, spec: spec.clone() })
    }
}

#[async_trait]
impl Provider for MysqlProvider {
    fn engine(&self) -> Engine {
        Engine::Mysql
    }

    async fn probe(&self) -> Result<ServerInfo> {
        let mut conn = self.connector.connect(Scope::Server).await?;
        let probe = collect::probe(&mut conn, &self.connector).await?;
        let _ = conn.disconnect().await;
        Ok(probe.server_info())
    }

    async fn snapshot(&self, connection: &str, options: &SnapshotOptions) -> Result<Snapshot> {
        collect::snapshot(&self.connector, &self.spec, connection, options).await
    }

    async fn explain(&self, sql: &str, mode: ExplainMode) -> Result<Plan> {
        let statement = single_statement(sql)?;
        let schema = self.connector.database.as_deref();
        let mut conn = self.connector.connect(Scope::Database).await?;
        match mode {
            ExplainMode::Estimated => {
                let _ = conn.query_drop(tagged("SET SESSION explain_json_format_version = 1")).await;
                let row = explain_one(&mut conn, &format!("EXPLAIN FORMAT=JSON {statement}"))
                    .await?
                    .ok_or_else(|| Error::internal("EXPLAIN returned no plan"))?;
                let json = row.text("explain").ok_or_else(|| Error::internal("EXPLAIN returned no plan"))?;
                plan_json::parse(&json, PlanKind::Estimated, Some(statement), schema)
            }
            ExplainMode::Analyze => {
                if !reads_only(statement) {
                    return Err(Error::invalid(
                        "Explain Analyze runs the statement, so Culprit only allows it for SELECT",
                    )
                    .with_hint("Use Explain (estimated) for statements that change data."));
                }
                conn.query_drop(tagged("START TRANSACTION READ ONLY")).await.map_err(map_error)?;
                let result = explain_one(&mut conn, &format!("EXPLAIN ANALYZE {statement}")).await;
                let rollback = conn.query_drop(tagged("ROLLBACK")).await;
                let row = result?.ok_or_else(|| Error::internal("EXPLAIN ANALYZE returned no plan"))?;
                rollback.map_err(map_error)?;
                let tree = row.text("explain").ok_or_else(|| Error::internal("EXPLAIN ANALYZE returned no plan"))?;
                plan_tree::parse(&tree, Some(statement), schema)
            }
        }
    }

    fn session_actions(&self) -> Vec<SessionAction> {
        vec![SessionAction::Cancel, SessionAction::Terminate]
    }

    async fn interrupt_session(&self, session: &str, action: SessionAction) -> Result<()> {
        sessions::interrupt(&self.connector, session, action).await
    }

    async fn lock_waits(&self) -> Result<Vec<culprit_core::LockWait>> {
        let mut conn = self.connector.connect(Scope::Server).await?;
        let out = deadlocks::lock_waits(&mut conn).await;
        let _ = conn.disconnect().await;
        out
    }

    async fn deadlocks(&self, since_hours: f64) -> Result<culprit_core::DeadlockReport> {
        let mut conn = self.connector.connect(Scope::Server).await?;
        let out = deadlocks::recent(&mut conn, since_hours).await;
        let _ = conn.disconnect().await;
        out
    }

    async fn wait_sample(&self, seconds: u32) -> Result<culprit_core::WaitSample> {
        let mut conn = self.connector.connect(Scope::Server).await?;
        let result = crate::pressure::sample(&mut conn, seconds).await;
        let _ = conn.disconnect().await;
        result
    }

    async fn last_actual_plan(&self, _query_id: &str, _plan_id: Option<&str>) -> Result<Plan> {
        Err(Error::new(culprit_core::ErrorCode::Unsupported, "MySQL doesn't keep the actual plan of past runs")
            .with_hint(
                "Measure it instead: Analyze runs the statement's recorded text once inside a read-only transaction.",
            ))
    }

    async fn query_progress(&self, session: &str, with_plan: bool) -> Result<culprit_core::QueryProgress> {
        progress::query_progress(&self.connector, session, with_plan).await
    }

    async fn live(&self) -> Result<LiveState> {
        let mut conn = self.connector.connect(Scope::Server).await?;
        let rows = query(
            &mut conn,
            "SELECT p.ID AS id, p.USER AS user_name, p.HOST AS host, p.DB AS db, p.COMMAND AS command, p.TIME AS seconds,
                    p.STATE AS state, p.INFO AS info,
                    TIMESTAMPDIFF(MICROSECOND, t.trx_started, NOW(6)) / 1000 AS trx_ms
               FROM information_schema.PROCESSLIST p
               LEFT JOIN information_schema.INNODB_TRX t ON t.trx_mysql_thread_id = p.ID
              WHERE p.ID <> CONNECTION_ID() AND p.COMMAND NOT IN ('Daemon', 'Binlog Dump', 'Binlog Dump GTID')",
        )
        .await?;
        let blockers = lock_waits(&mut conn).await;
        let last_statements = deadlocks::last_statements(&mut conn).await;
        let now = query_one(&mut conn, "SELECT DATE_FORMAT(UTC_TIMESTAMP(), '%Y-%m-%dT%H:%i:%sZ') AS now")
            .await?
            .map(|r| r.string("now"))
            .unwrap_or_default();
        let _ = conn.disconnect().await;

        let sessions: Vec<Session> = rows
            .iter()
            .filter(|r| !r.text("info").is_some_and(|i| i.contains(connect::TAG)))
            .map(|r| {
                let id = r.string("id");
                let command = r.string("command");
                let in_transaction = r.number("trx_ms").is_some();
                let state = match command.as_str() {
                    "Sleep" if in_transaction => "idle in transaction".to_string(),
                    "Sleep" => "idle".to_string(),
                    _ => "active".to_string(),
                };
                let active = state == "active";
                Session {
                    blocked_by: blockers.get(&id).cloned(),
                    user: r.text("user_name"),
                    application: None,
                    host: r.text("host"),
                    database: r.text("db"),
                    command: active.then(|| command.to_uppercase()),
                    wait: if active { r.text("state").filter(|s| !s.is_empty()) } else { None },
                    wait_ms: None,
                    elapsed_ms: r.number("seconds").map(|s| s * 1000.0),
                    transaction_ms: r.number("trx_ms"),
                    cpu_ms: None,
                    reads: None,
                    // An idle session in a transaction shows its last statement: usually what took the locks.
                    sql: r
                        .text("info")
                        .filter(|s| !s.is_empty())
                        .or_else(|| in_transaction.then(|| last_statements.get(&id).cloned()).flatten()),
                    state,
                    id,
                }
            })
            .collect();
        let mut waits: Vec<WaitStat> = Vec::new();
        for session in sessions.iter().filter(|s| s.state == "active") {
            let category = if session.blocked_by.is_some() { "Lock" } else { "Running" }.to_string();
            match waits.iter_mut().find(|w| w.category == category) {
                Some(existing) => existing.tasks = Some(existing.tasks.unwrap_or(0.0) + 1.0),
                None => waits.push(WaitStat { category, wait_type: None, wait_ms: 0.0, tasks: Some(1.0) }),
            }
        }
        Ok(LiveState {
            taken_at: now,
            sessions,
            waits,
            session_actions: Vec::new(),
            session_actions_note: None,
            blocking: None,
        })
    }

    async fn run(&self, sql: &str, limit: usize) -> Result<ResultSet> {
        let statement = single_statement(sql)?;
        let mut conn = self.connector.connect(Scope::Database).await?;
        let started = Instant::now();
        conn.query_drop(tagged("START TRANSACTION READ ONLY")).await.map_err(map_error)?;
        let result = run_in_transaction(&mut conn, statement, limit).await;
        let rollback = conn.query_drop(tagged("ROLLBACK")).await;
        let mut result = result?;
        rollback.map_err(map_error)?;
        let _ = conn.disconnect().await;
        result.elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;
        Ok(result)
    }

    async fn execute(&self, sql: &str, limit: usize) -> Result<ResultSet> {
        let statement = single_statement(sql)?;
        let mut conn = self.connector.connect(Scope::Database).await?;
        let started = Instant::now();
        // Sessions open read-only; a write the person chose to run gets a read-write session and commits (autocommit).
        conn.query_drop(tagged("SET SESSION TRANSACTION READ WRITE")).await.map_err(map_error)?;
        let result = run_in_transaction(&mut conn, statement, limit).await;
        let _ = conn.disconnect().await;
        let mut result = result?;
        result.elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;
        result.messages.push("Committed.".to_string());
        Ok(result)
    }
}

/// Which session blocks which, from InnoDB's lock waits (MySQL 8.0+). Empty when the login can't see them.
async fn lock_waits(conn: &mut Conn) -> HashMap<String, String> {
    query(
        conn,
        "SELECT r.PROCESSLIST_ID AS waiting, b.PROCESSLIST_ID AS blocking
           FROM performance_schema.data_lock_waits w
           JOIN performance_schema.threads r ON r.THREAD_ID = w.REQUESTING_THREAD_ID
           JOIN performance_schema.threads b ON b.THREAD_ID = w.BLOCKING_THREAD_ID",
    )
    .await
    .unwrap_or_default()
    .into_iter()
    .filter_map(|r| Some((r.text("waiting")?, r.text("blocking")?)))
    .collect()
}

/// Trim and reject empty or multi-statement input (the server would refuse the second statement anyway).
/// One statement, checked by Culprit's lexer under both backslash modes (and refusing executable comments).
fn single_statement(sql: &str) -> Result<&str> {
    culprit_core::sql::single_statement(sql, Engine::Mysql)
}

fn reads_only(statement: &str) -> bool {
    culprit_core::sql::reads_only(statement, Engine::Mysql)
}

async fn run_in_transaction(conn: &mut Conn, statement: &str, limit: usize) -> Result<ResultSet> {
    let mut result = conn.query_iter(tagged(statement)).await.map_err(map_error)?;
    let columns_meta = result.columns();
    let columns: Vec<String> = columns_meta
        .as_deref()
        .map(|cols| cols.iter().map(|c| c.name_str().into_owned()).collect())
        .unwrap_or_default();
    let types: Vec<ColumnType> =
        columns_meta.as_deref().map(|cols| cols.iter().map(|c| c.column_type()).collect()).unwrap_or_default();
    let mut rows: Vec<Vec<Json>> = Vec::new();
    let mut truncated = false;
    while let Some(row) = result.next().await.map_err(map_error)? {
        if rows.len() == limit {
            truncated = true;
            break;
        }
        let values = row.unwrap();
        rows.push(values.iter().enumerate().map(|(i, v)| json_value(v, types.get(i))).collect());
    }
    let affected = result.affected_rows();
    result.drop_result().await.map_err(map_error)?;
    let count = if columns.is_empty() { affected } else { rows.len() as u64 };
    let mut message = format!("{count} row{}", if count == 1 { "" } else { "s" });
    if truncated {
        message.push_str(&format!(" (stopped at {limit})"));
    }
    Ok(ResultSet { columns, rows, truncated, elapsed_ms: 0.0, messages: vec![message] })
}

fn json_value(value: &Value, kind: Option<&ColumnType>) -> Json {
    match value {
        Value::NULL => Json::Null,
        Value::Int(i) => Json::from(*i),
        Value::UInt(u) => Json::from(*u),
        Value::Float(f) => serde_json::Number::from_f64(f64::from(*f)).map(Json::Number).unwrap_or(Json::Null),
        Value::Double(d) => serde_json::Number::from_f64(*d).map(Json::Number).unwrap_or(Json::Null),
        Value::Bytes(bytes) => {
            let text = String::from_utf8_lossy(bytes).into_owned();
            match kind {
                Some(
                    ColumnType::MYSQL_TYPE_TINY
                    | ColumnType::MYSQL_TYPE_SHORT
                    | ColumnType::MYSQL_TYPE_LONG
                    | ColumnType::MYSQL_TYPE_LONGLONG
                    | ColumnType::MYSQL_TYPE_INT24
                    | ColumnType::MYSQL_TYPE_YEAR,
                ) => text.parse::<i64>().map(Json::from).unwrap_or(Json::String(text)),
                Some(ColumnType::MYSQL_TYPE_FLOAT | ColumnType::MYSQL_TYPE_DOUBLE) => text
                    .parse::<f64>()
                    .ok()
                    .and_then(serde_json::Number::from_f64)
                    .map(Json::Number)
                    .unwrap_or(Json::String(text)),
                _ => Json::String(text),
            }
        }
        other => Json::String(value_text(other)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use culprit_core::ErrorCode;

    #[test]
    fn values_follow_column_types() {
        assert_eq!(json_value(&Value::Bytes(b"42".to_vec()), Some(&ColumnType::MYSQL_TYPE_LONGLONG)), Json::from(42));
        assert_eq!(json_value(&Value::Bytes(b"1.5".to_vec()), Some(&ColumnType::MYSQL_TYPE_DOUBLE)), Json::from(1.5));
        assert_eq!(
            json_value(&Value::Bytes(b"1.50".to_vec()), Some(&ColumnType::MYSQL_TYPE_NEWDECIMAL)),
            Json::String("1.50".into())
        );
        assert_eq!(json_value(&Value::NULL, None), Json::Null);
    }

    #[test]
    fn only_reads_are_analyzed() {
        assert!(reads_only("select 1"));
        assert!(reads_only(" WITH x AS (SELECT 1) SELECT * FROM x"));
        assert!(!reads_only("DELETE FROM orders"));
        assert_eq!(single_statement("  select 1 ;  ").unwrap(), "select 1");
        assert!(single_statement(" ; ").is_err());
    }

    #[test]
    fn rejects_other_engines() {
        let spec = ConnectionSpec { engine: Some(Engine::Postgres), host: "h".into(), ..Default::default() };
        assert_eq!(MysqlProvider::new(&spec).err().unwrap().code, ErrorCode::InvalidInput);
    }
}
