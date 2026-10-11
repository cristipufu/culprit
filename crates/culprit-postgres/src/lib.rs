//! Postgres provider for Culprit: read-only snapshots from the statistics views, plans via EXPLAIN, live sessions.

mod collect;
mod connect;
pub mod deadlocks;
mod generic;
pub mod plan_json;
mod pressure;
mod progress;
mod recorded;
mod rows;
mod sessions;
mod signals;
mod tables;

use std::time::Instant;

use async_trait::async_trait;
use culprit_core::{
    ConnectionSpec, Engine, Environment, Error, ErrorCode, ExplainMode, LiveState, Plan, PlanKind, Provider, Result,
    ResultSet, ServerInfo, Session, SessionAction, Snapshot, SnapshotOptions, WaitStat,
};
use serde_json::Value;
use tokio_postgres::types::Type;
use tokio_postgres::{Client, SimpleQueryMessage};

pub use collect::{leading_keyword, plannable};
use connect::{COLLECTOR, Connector, EXECUTION, INTERACTIVE, map_error, sql_state};
use rows::{iso, parse_array, query, query_one, rows_of, tagged};

const CURSOR: &str = "culprit_cursor";

pub struct PostgresProvider {
    connector: Connector,
    spec: ConnectionSpec,
}

impl PostgresProvider {
    pub fn new(spec: &ConnectionSpec) -> Result<Self> {
        if let Some(engine) = spec.engine
            && engine != Engine::Postgres
        {
            return Err(Error::invalid(format!("This connection is for {engine}, not Postgres")));
        }
        Ok(PostgresProvider { connector: Connector::new(spec)?, spec: spec.clone() })
    }

    fn environment(&self) -> Environment {
        self.spec.environment
    }
}

#[async_trait]
impl Provider for PostgresProvider {
    fn engine(&self) -> Engine {
        Engine::Postgres
    }

    async fn probe(&self) -> Result<ServerInfo> {
        let client = self.connector.connect(COLLECTOR).await?;
        let probe = collect::probe(&client, &self.connector.host).await?;
        Ok(probe.server_info())
    }

    async fn snapshot(&self, connection: &str, options: &SnapshotOptions) -> Result<Snapshot> {
        collect::snapshot(&self.connector, &self.spec, connection, options).await
    }

    async fn explain(&self, sql: &str, mode: ExplainMode) -> Result<Plan> {
        let statement = single_statement(sql)?;
        match mode {
            ExplainMode::Estimated => {
                let client = self.connector.connect(INTERACTIVE).await?;
                let parameters = parameter_count(&client, statement).await?;
                let mut plan = if parameters == 0 {
                    explain_text(
                        &client,
                        &format!("EXPLAIN (FORMAT JSON, VERBOSE) {statement}"),
                        PlanKind::Estimated,
                        statement,
                    )
                    .await?
                } else {
                    // Placeholders ($1, $2… as pg_stat_statements records statements): the generic plan, never run.
                    let json = generic::plan_json(&client, statement, server_version(&client).await?).await?;
                    plan_json::parse(&json, PlanKind::Generic, Some(statement))?
                };
                tables::fill(&client, &mut plan).await;
                Ok(plan)
            }
            ExplainMode::Analyze => {
                let client = self.connector.connect(EXECUTION).await?;
                let begin = if self.environment().is_production() { "BEGIN READ ONLY" } else { "BEGIN READ WRITE" };
                client.batch_execute(&tagged(begin)).await.map_err(map_error)?;
                let result = async {
                    if parameter_count(&client, statement).await? > 0 {
                        return Err(Error::invalid(
                            "Explain Analyze runs the statement, so $n parameters need real values",
                        ));
                    }
                    explain_text(
                        &client,
                        &format!("EXPLAIN (ANALYZE, BUFFERS, VERBOSE, FORMAT JSON) {statement}"),
                        PlanKind::Actual,
                        statement,
                    )
                    .await
                }
                .await;
                let rollback = client.batch_execute(&tagged("ROLLBACK")).await;
                let plan = result?;
                rollback.map_err(map_error)?;
                Ok(plan)
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
        let client = self.connector.connect(INTERACTIVE).await?;
        deadlocks::lock_waits(&client).await
    }

    async fn deadlocks(&self, since_hours: f64) -> Result<culprit_core::DeadlockReport> {
        let client = self.connector.connect(INTERACTIVE).await?;
        deadlocks::recent(&client, since_hours).await
    }

    async fn recorded_changes(&self, since: &str) -> Result<Vec<culprit_core::ChangeEvent>> {
        let client = self.connector.connect(INTERACTIVE).await?;
        recorded::changes(&client, since).await
    }

    async fn wait_sample(&self, seconds: u32) -> Result<culprit_core::WaitSample> {
        let client = self.connector.connect(INTERACTIVE).await?;
        crate::pressure::sample(&client, seconds).await
    }

    async fn last_actual_plan(&self, _query_id: &str, _plan_id: Option<&str>) -> Result<Plan> {
        Err(Error::new(ErrorCode::Unsupported, "Postgres doesn't keep the actual plan of past runs").with_hint(
            "auto_explain can write them to the server log, which Culprit can't read over SQL. Open the query in the \
             editor with real values and use Analyze: it runs inside a transaction that is rolled back.",
        ))
    }

    async fn query_progress(&self, session: &str, with_plan: bool) -> Result<culprit_core::QueryProgress> {
        let mut progress = progress::query_progress(&self.connector, session).await?;
        if with_plan
            && progress.running
            && let Some(statement) = progress.statement.clone()
        {
            // EXPLAIN without ANALYZE: plans the statement's text again, never runs it.
            match self.explain(&statement, ExplainMode::Estimated).await {
                Ok(mut plan) => {
                    plan.raw = None;
                    progress.plan = Some(plan);
                }
                Err(e) => {
                    let base = progress.note.take().unwrap_or_default();
                    progress.note =
                        Some(format!("{base} Its plan couldn't be shown: {}", e.message).trim().to_string());
                }
            }
        }
        Ok(progress)
    }

    async fn live(&self) -> Result<LiveState> {
        let client = self.connector.connect(INTERACTIVE).await?;
        let sql = "SELECT pid::text AS id, usename AS username, application_name,
                    coalesce(client_hostname, host(client_addr)) AS host, datname,
                    coalesce(state, backend_type) AS state, wait_event_type, wait_event,
                    extract(epoch FROM now() - CASE WHEN state = 'active' THEN query_start ELSE state_change END) * 1000 AS elapsed_ms,
                    extract(epoch FROM now() - xact_start) * 1000 AS xact_ms,
                    pg_blocking_pids(pid)::text AS blockers, query
             FROM pg_stat_activity
             WHERE pid <> pg_backend_pid() AND backend_type = 'client backend'
               AND application_name <> 'culprit'
             ORDER BY (state = 'active') DESC NULLS LAST, query_start";
        let rows = query(&client, sql).await?;
        let now = query_one(&client, &format!("SELECT {} AS now", iso("now()")))
            .await?
            .map(|r| r.string("now"))
            .unwrap_or_default();
        let sessions: Vec<Session> = rows
            .iter()
            .map(|r| {
                let state = r.string("state");
                let sql = r.text("query").filter(|q| !q.is_empty());
                let active = state == "active";
                let wait = match (r.text("wait_event_type"), r.text("wait_event")) {
                    (Some(kind), Some(event)) => Some(format!("{kind}: {event}")),
                    (Some(kind), None) => Some(kind),
                    _ => None,
                };
                Session {
                    id: r.string("id"),
                    user: r.text("username"),
                    application: r.text("application_name").filter(|a| !a.is_empty()),
                    host: r.text("host"),
                    database: r.text("datname"),
                    command: if active {
                        sql.as_deref().and_then(leading_keyword).map(|k| k.to_uppercase())
                    } else {
                        None
                    },
                    wait: if active { wait } else { None },
                    wait_ms: None,
                    blocked_by: r.text("blockers").and_then(|b| parse_array(&b).into_iter().next()),
                    elapsed_ms: r.number("elapsed_ms"),
                    transaction_ms: r.number("xact_ms"),
                    cpu_ms: None,
                    reads: None,
                    sql,
                    state,
                }
            })
            .collect();
        let mut waits: Vec<WaitStat> = Vec::new();
        for session in sessions.iter().filter(|s| s.state == "active") {
            let category = session.wait.as_deref().and_then(|w| w.split(':').next()).unwrap_or("CPU").to_string();
            let wait_type = session.wait.as_deref().and_then(|w| w.split_once(": ")).map(|(_, e)| e.to_string());
            match waits.iter_mut().find(|w| w.category == category && w.wait_type == wait_type) {
                Some(existing) => existing.tasks = Some(existing.tasks.unwrap_or(0.0) + 1.0),
                None => waits.push(WaitStat { category, wait_type, wait_ms: 0.0, tasks: Some(1.0) }),
            }
        }
        waits.sort_by(|a, b| b.tasks.unwrap_or(0.0).total_cmp(&a.tasks.unwrap_or(0.0)));
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
        let client = self.connector.connect(EXECUTION).await?;
        let started = Instant::now();
        client.batch_execute(&tagged("BEGIN READ ONLY")).await.map_err(map_error)?;
        let result = run_in_transaction(&client, statement, limit, true).await;
        let rollback = client.batch_execute(&tagged("ROLLBACK")).await;
        let mut result = result?;
        rollback.map_err(map_error)?;
        result.elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;
        Ok(result)
    }

    async fn execute(&self, sql: &str, limit: usize) -> Result<ResultSet> {
        let statement = single_statement(sql)?;
        let client = self.connector.connect(EXECUTION).await?;
        let started = Instant::now();
        // Sessions open read-only; a write the person chose to run gets a read-write session and commits on its own,
        // so statements that can't run in a transaction block (CREATE INDEX CONCURRENTLY, VACUUM) work too.
        client.batch_execute(&tagged("SET default_transaction_read_only = off")).await.map_err(map_error)?;
        let mut result = run_in_transaction(&client, statement, limit, false).await?;
        result.elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;
        result.messages.push("Committed.".to_string());
        Ok(result)
    }
}

/// Trim and reject empty input. Single-statement enforcement happens when the server parses it.
/// One statement, checked by Culprit's lexer under both standard_conforming_strings settings.
fn single_statement(sql: &str) -> Result<&str> {
    culprit_core::sql::single_statement(sql, Engine::Postgres)
}

/// Parse the statement on the server without running it. This also rejects several statements in one string.
async fn parameter_count(client: &Client, statement: &str) -> Result<usize> {
    match client.prepare(statement).await {
        Ok(prepared) => Ok(prepared.params().len()),
        // Parameters whose type cannot be inferred: still a single statement, with placeholders.
        Err(e) if sql_state(&e) == Some("42P18") => Ok(1),
        Err(e) => Err(single_statement_error(e)),
    }
}

fn single_statement_error(error: tokio_postgres::Error) -> Error {
    if sql_state(&error) == Some("42601")
        && error.as_db_error().is_some_and(|d| d.message().contains("multiple commands"))
    {
        return Error::invalid("Run one statement at a time").with_hint("Remove everything after the first statement.");
    }
    map_error(error)
}

async fn server_version(client: &Client) -> Result<u32> {
    let row = query_one(client, "SELECT current_setting('server_version_num') AS v").await?;
    Ok(row.and_then(|r| r.number("v")).unwrap_or(0.0) as u32)
}

async fn explain_text(client: &Client, sql: &str, kind: PlanKind, statement: &str) -> Result<Plan> {
    let rows = query(client, sql).await?;
    let json =
        rows.first().and_then(|r| r.text("QUERY PLAN")).ok_or_else(|| Error::internal("EXPLAIN returned no plan"))?;
    plan_json::parse(&json, kind, Some(statement))
}

/// Run one statement and collect up to `limit` rows; `cursor` fetches reads through a cursor (needs a transaction block).
async fn run_in_transaction(client: &Client, statement: &str, limit: usize, cursor: bool) -> Result<ResultSet> {
    let prepared = client.prepare(statement).await.map_err(single_statement_error)?;
    if !prepared.params().is_empty() {
        return Err(Error::invalid("This statement has $n parameters").with_hint("Replace them with values to run it."));
    }
    let columns: Vec<String> = prepared.columns().iter().map(|c| c.name().to_string()).collect();
    let types: Vec<Type> = prepared.columns().iter().map(|c| c.type_().clone()).collect();
    let cursor_friendly = cursor
        && !columns.is_empty()
        && leading_keyword(statement).is_some_and(|k| matches!(k.as_str(), "select" | "with" | "values" | "table"));

    let messages = if cursor_friendly {
        client
            .batch_execute(&tagged(&format!("DECLARE {CURSOR} NO SCROLL CURSOR FOR {statement}")))
            .await
            .map_err(map_error)?;
        client
            .simple_query(&tagged(&format!("FETCH FORWARD {} FROM {CURSOR}", limit.saturating_add(1))))
            .await
            .map_err(map_error)?
    } else {
        client.simple_query(&tagged(statement)).await.map_err(map_error)?
    };

    let completions: Vec<u64> = messages
        .iter()
        .filter_map(|m| match m {
            SimpleQueryMessage::CommandComplete(count) => Some(*count),
            _ => None,
        })
        .collect();
    let raw_rows = rows_of(messages);
    let truncated = raw_rows.len() > limit;
    let rows: Vec<Vec<Value>> = raw_rows
        .iter()
        .take(limit)
        .map(|row| (0..columns.len()).map(|i| json_value(row.text_at(i), types.get(i))).collect())
        .collect();
    let affected = if columns.is_empty() { completions.last().copied().unwrap_or(0) } else { rows.len() as u64 };
    let mut message = format!("{affected} row{}", if affected == 1 { "" } else { "s" });
    if truncated {
        message.push_str(&format!(" (stopped at {limit})"));
    }
    Ok(ResultSet { columns, rows, truncated, elapsed_ms: 0.0, messages: vec![message] })
}

fn json_value(text: Option<String>, kind: Option<&Type>) -> Value {
    let Some(text) = text else { return Value::Null };
    match kind {
        Some(t) if *t == Type::BOOL => Value::Bool(text == "t"),
        Some(t) if [Type::INT2, Type::INT4, Type::INT8, Type::OID].contains(t) => {
            text.parse::<i64>().map(Value::from).unwrap_or(Value::String(text))
        }
        Some(t) if [Type::FLOAT4, Type::FLOAT8].contains(t) => text
            .parse::<f64>()
            .ok()
            .and_then(serde_json::Number::from_f64)
            .map(Value::Number)
            .unwrap_or(Value::String(text)),
        _ => Value::String(text),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use culprit_core::ErrorCode;

    #[test]
    fn values_follow_column_types() {
        assert_eq!(json_value(Some("t".into()), Some(&Type::BOOL)), Value::Bool(true));
        assert_eq!(json_value(Some("42".into()), Some(&Type::INT8)), Value::from(42));
        assert_eq!(json_value(Some("1.5".into()), Some(&Type::FLOAT8)), Value::from(1.5));
        assert_eq!(json_value(Some("1.50".into()), Some(&Type::NUMERIC)), Value::String("1.50".into()));
        assert_eq!(json_value(None, Some(&Type::TEXT)), Value::Null);
    }

    #[test]
    fn statements_are_trimmed() {
        assert_eq!(single_statement("  select 1 ;  ").unwrap(), "select 1");
        assert!(single_statement(" ; ").is_err());
    }

    #[test]
    fn rejects_other_engines() {
        let spec = ConnectionSpec { engine: Some(Engine::Mssql), host: "h".into(), ..Default::default() };
        assert_eq!(PostgresProvider::new(&spec).err().unwrap().code, ErrorCode::InvalidInput);
    }
}
