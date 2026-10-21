//! MySQL and MariaDB provider for Culprit: read-only snapshots from performance_schema, information_schema and the
//! slow query log table, plans via EXPLAIN (and MariaDB's ANALYZE), live sessions with lock waits.

mod collect;
mod connect;
pub mod deadlocks;
pub mod fingerprint;
mod json_members;
mod operations;
pub mod plan_json;
pub mod plan_maria;
pub mod plan_tree;
mod pressure;
mod progress;
mod recorded;
mod rows;
mod sessions;
mod signals;
mod slowlog;
pub mod sources;
mod vars;

use std::time::Instant;

use async_trait::async_trait;
use culprit_core::{
    ConnectionSpec, Engine, Error, ErrorCode, ExplainMode, LiveState, Plan, PlanKind, Provider, Result, ResultSet,
    ServerInfo, Session, SessionAction, Snapshot, SnapshotOptions, WaitStat,
};
use mysql_async::consts::ColumnType;
use mysql_async::prelude::Queryable;
use mysql_async::{Conn, Value};
use serde_json::Value as Json;

use connect::{Connector, Flavor, Scope, map_error};
use rows::{explain_one, query, query_one, tagged, value_text};

/// Identity of this crate's collector and native-plan parser, including a build revision when supplied.
pub fn component_version() -> culprit_core::ComponentVersion {
    culprit_core::ComponentVersion::package(
        "culprit-mysql",
        env!("CARGO_PKG_VERSION"),
        option_env!("CULPRIT_BUILD_REVISION").or(option_env!("GITHUB_SHA")),
    )
}

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

    /// The plan, tagged with the server's version so its notes can say what this version can and can't measure.
    fn versioned(&self, mut plan: Plan) -> Plan {
        if let Some(version) = self.connector.version() {
            plan.facts_mut().server_version = Some(version.to_string());
            plan.props.insert("server_version".into(), version.to_string());
        }
        plan
    }
}

impl MysqlProvider {
    async fn explain_statement(&self, sql: &str, mode: ExplainMode) -> Result<Plan> {
        let statement = single_statement(sql)?;
        let schema = self.connector.database.as_deref();
        let mut conn = self.connector.connect(Scope::Database).await?;
        let flavor = self.connector.flavor();
        match mode {
            ExplainMode::Estimated => {
                let verb = leading_verb(statement);
                if !EXPLAINABLE.contains(&verb.as_str()) {
                    return Err(Error::new(
                        culprit_core::ErrorCode::Unsupported,
                        format!("{} can't EXPLAIN a {} statement", flavor.name(), verb.to_ascii_uppercase()),
                    )
                    .with_hint(
                        "EXPLAIN covers SELECT, INSERT, UPDATE, DELETE and REPLACE. The scan's statement statistics show \
                         this one's measured times.",
                    ));
                }
                if flavor == Flavor::Mysql {
                    let _ = conn.query_drop(tagged("SET SESSION explain_json_format_version = 1")).await;
                }
                let row = match explain_one(&mut conn, &format!("EXPLAIN FORMAT=JSON {statement}")).await {
                    // Culprit's sessions are read-only (EXPLAIN can evaluate stored functions), and the server refuses
                    // to plan a write inside one. Nothing would have been written: say what happened instead.
                    Err(e) if WRITES.contains(&verb.as_str()) && e.message.contains("READ ONLY") => {
                        return Err(Error::new(
                            culprit_core::ErrorCode::Unsupported,
                            format!(
                                "{} won't plan {} {} in a read-only session, and Culprit's sessions are always read-only",
                                flavor.name(),
                                if matches!(verb.as_str(), "insert" | "update") { "an" } else { "a" },
                                verb.to_ascii_uppercase()
                            ),
                        )
                        .with_hint(if lower_starts_values(statement) {
                            "An INSERT … VALUES searches no rows, so there's nothing to plan; its cost is the indexes and \
                             triggers on the table."
                        } else {
                            "Explain the SELECT with the same FROM and WHERE: the server usually finds the rows the \
                             same way."
                        }));
                    }
                    result => result?.ok_or_else(|| Error::internal("EXPLAIN returned no plan"))?,
                };
                let json = row.text("explain").ok_or_else(|| Error::internal("EXPLAIN returned no plan"))?;
                plan_json::parse(&json, PlanKind::Estimated, Some(statement), schema).map(|plan| self.versioned(plan))
            }
            ExplainMode::Analyze => {
                if !self.connector.measures_runs() {
                    let version = self.connector.version().unwrap_or_default();
                    let short = version.split('-').next().unwrap_or(version);
                    return Err(Error::new(
                        culprit_core::ErrorCode::Unsupported,
                        format!("MySQL {short} can't measure a run: EXPLAIN ANALYZE arrived in MySQL 8.0.18"),
                    )
                    .with_hint("Explain shows the estimated plan; the scan's statement statistics show each query's measured times."));
                }
                if !reads_only(statement) {
                    return Err(Error::invalid(
                        "Explain Analyze runs the statement, so Culprit only allows it for SELECT",
                    )
                    .with_hint("Use Explain (estimated) for statements that change data."));
                }
                // MySQL's EXPLAIN ANALYZE prints a tree; MariaDB's ANALYZE prints the EXPLAIN JSON with measurements.
                let sql = match flavor {
                    Flavor::Mysql => format!("EXPLAIN ANALYZE {statement}"),
                    Flavor::Mariadb => format!("ANALYZE FORMAT=JSON {statement}"),
                };
                conn.query_drop(tagged("START TRANSACTION READ ONLY")).await.map_err(map_error)?;
                let result = explain_one(&mut conn, &sql).await;
                let rollback = conn.query_drop(tagged("ROLLBACK")).await;
                let row = result?.ok_or_else(|| Error::internal("ANALYZE returned no plan"))?;
                rollback.map_err(map_error)?;
                match flavor {
                    Flavor::Mysql => {
                        let tree =
                            row.text("explain").ok_or_else(|| Error::internal("EXPLAIN ANALYZE returned no plan"))?;
                        plan_tree::parse(&tree, Some(statement), schema)
                    }
                    Flavor::Mariadb => {
                        let json = row.first_text().ok_or_else(|| Error::internal("ANALYZE returned no plan"))?;
                        plan_json::parse(&json, PlanKind::Actual, Some(statement), schema)
                    }
                }
            }
        }
    }
}

/// EXPLAIN goes through a prepared statement, where a `?` is a parameter MySQL wants a value for before it plans.
fn parameter_error(e: Error) -> Error {
    if !e.message.contains("parameters but") {
        return e;
    }
    Error::invalid("This statement has `?` parameters, which MySQL needs values for to plan it").with_hint(
        "Replace each ? with a sample value of its type (a quoted string for text, a number for an id) and explain again. \
         Queries Culprit captured open from Queries with the plan of a real statement.",
    )
}

#[async_trait]
impl Provider for MysqlProvider {
    fn collector_version(&self) -> culprit_core::ComponentVersion {
        component_version()
    }

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

    /// A statement the scan recorded is digest text (`COUNT ( * )`, `LIMIT ?`): planned as far as it runs, and when its
    /// placeholders stop it, said so instead of the server's syntax error.
    async fn explain_recorded(&self, sql: &str, _query_id: Option<&str>) -> Result<Plan> {
        let text = fingerprint::runnable(sql);
        self.explain(&text, ExplainMode::Estimated).await.map_err(|e| {
            if !fingerprint::has_placeholders(&text) || !matches!(e.code, ErrorCode::QueryFailed | ErrorCode::InvalidInput) {
                return e;
            }
            let name = if self.connector.flavor() == Flavor::Mariadb { "MariaDB" } else { "MySQL" };
            Error::invalid(format!(
                "{name} recorded this statement without its values (each ? stands for one), so it can't be planned as recorded"
            ))
            .with_hint("Open it in the editor, put a real value in place of each ?, and explain it there.")
        })
    }

    async fn explain(&self, sql: &str, mode: ExplainMode) -> Result<Plan> {
        let mut plan = self.explain_statement(sql, mode).await.map_err(parameter_error)?;
        if self.connector.flavor() == Flavor::Mariadb {
            plan.facts_mut().flavor = Some("MariaDB".into());
            plan.props.insert("flavor".into(), "MariaDB".into());
        }
        Ok(plan)
    }

    async fn interrupt_session(&self, session: &str, action: SessionAction) -> Result<()> {
        sessions::interrupt(&self.connector, session, action).await
    }

    async fn lock_waits(&self) -> Result<Vec<culprit_core::LockWait>> {
        let mut conn = self.connector.connect(Scope::Server).await?;
        let out = deadlocks::lock_waits(&mut conn, self.connector.flavor()).await;
        let _ = conn.disconnect().await;
        out
    }

    async fn deadlocks(&self, since_hours: f64) -> Result<culprit_core::DeadlockReport> {
        let mut conn = self.connector.connect(Scope::Server).await?;
        let out = deadlocks::recent(&mut conn, since_hours).await;
        let _ = conn.disconnect().await;
        out
    }

    async fn recorded_changes(&self, since: &str) -> Result<Vec<culprit_core::ChangeEvent>> {
        let Some(database) = self.connector.database.clone() else { return Ok(Vec::new()) };
        // No default database: these catalog reads must not count as the application's statements.
        let mut conn = self.connector.connect(Scope::Server).await?;
        let out = recorded::changes(&mut conn, &database, since).await;
        let _ = conn.disconnect().await;
        out
    }

    async fn wait_sample(&self, seconds: u32) -> Result<culprit_core::WaitSample> {
        let mut conn = self.connector.connect(Scope::Server).await?;
        let result = crate::pressure::sample(&mut conn, seconds).await;
        let _ = conn.disconnect().await;
        result
    }

    /// Servers whose workload comes from the slow query log table compare any two periods straight from the log.
    async fn period_stats(
        &self,
        baseline: &culprit_core::changes::Period,
        recent: &culprit_core::changes::Period,
    ) -> Result<Option<culprit_core::changes::PeriodData>> {
        let mut conn = self.connector.connect(Scope::Server).await?;
        let probe = collect::probe(&mut conn, &self.connector).await?;
        if probe.performance_schema || !probe.slow_log_table() {
            let _ = conn.disconnect().await;
            return Ok(None);
        }
        let runs = slowlog::read(
            &mut conn,
            &probe.database,
            &slowlog::sql_time(&baseline.start),
            Some(&slowlog::sql_time(&recent.end)),
        )
        .await;
        let _ = conn.disconnect().await;
        let runs = runs?;
        let logged_since = runs.iter().map(|r| r.at).reduce(f64::min);
        Ok(Some(slowlog::periods(&runs, baseline, recent, logged_since)))
    }

    async fn query_progress(&self, session: &str, with_plan: bool) -> Result<culprit_core::QueryProgress> {
        progress::query_progress(&self.connector, session, with_plan).await
    }

    async fn live(&self) -> Result<LiveState> {
        let mut conn = self.connector.connect(Scope::Server).await?;
        let flavor = self.connector.flavor();
        // MariaDB's process list also has the time in milliseconds and progress for ALTER TABLE, LOAD DATA and others.
        let extra = match flavor {
            Flavor::Mariadb => "p.TIME_MS AS ms, p.PROGRESS AS progress, p.STAGE AS stage, p.MAX_STAGE AS max_stage",
            Flavor::Mysql => "NULL AS ms, NULL AS progress, NULL AS stage, NULL AS max_stage",
        };
        let rows = query(
            &mut conn,
            &format!(
                "SELECT p.ID AS id, p.USER AS user_name, p.HOST AS host, p.DB AS db, p.COMMAND AS command, p.TIME AS seconds,
                        p.STATE AS state, p.INFO AS info, {extra},
                        TIMESTAMPDIFF(MICROSECOND, t.trx_started, NOW(6)) / 1000 AS trx_ms
                   FROM information_schema.PROCESSLIST p
                   LEFT JOIN information_schema.INNODB_TRX t ON t.trx_mysql_thread_id = p.ID
                  WHERE p.ID <> CONNECTION_ID() AND p.COMMAND NOT IN ('Daemon', 'Binlog Dump', 'Binlog Dump GTID')
                    AND p.USER <> 'system user'"
            ),
        )
        .await?;
        let (blockers, locks, mut notes) = deadlocks::blocking_evidence(&mut conn, flavor).await;
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
                let wait = if active { r.text("state").filter(|s| !s.is_empty()) } else { None };
                Session {
                    blocked_by: blockers.get(&id).cloned(),
                    user: r.text("user_name"),
                    application: None,
                    host: r.text("host"),
                    database: r.text("db"),
                    command: active.then(|| command.to_uppercase()),
                    wait: wait.map(|w| with_progress(w, r)),
                    wait_ms: None,
                    elapsed_ms: r.number("ms").or_else(|| r.number("seconds").map(|s| s * 1000.0)),
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
            let waiting = session.wait.as_deref().unwrap_or("").to_ascii_lowercase();
            let category = if session.blocked_by.is_some() || waiting.contains("lock") {
                "Lock"
            } else if waiting.starts_with("waiting") {
                "Waiting"
            } else {
                "Running"
            }
            .to_string();
            match waits.iter_mut().find(|w| w.category == category) {
                Some(existing) => existing.tasks = Some(existing.tasks.unwrap_or(0.0) + 1.0),
                None => waits.push(WaitStat { category, wait_type: None, wait_ms: 0.0, tasks: Some(1.0) }),
            }
        }
        if sessions.iter().any(|s| {
            s.blocked_by.is_none()
                && s.wait.as_deref().is_some_and(|w| w.to_ascii_lowercase().contains("metadata lock"))
        }) {
            notes.push("A metadata-lock wait is visible but its holder could not be resolved. Check sys.schema_table_lock_waits, metadata-lock instrumentation and this login's visibility; an unresolved blocker is not an unblocked session.".into());
        }
        let blocking = culprit_core::blocking::analyze(Engine::Mysql, &sessions, &locks);
        Ok(LiveState {
            notes,
            taken_at: now,
            sessions,
            waits,
            session_actions: Vec::new(),
            session_actions_note: None,
            blocking,
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
        // MySQL's timeout only stops SELECTs; MariaDB's stops anything, so it is lifted for a change the person ran.
        if let Some(lift) = self.connector.flavor().no_timeout() {
            conn.query_drop(tagged(lift)).await.map_err(map_error)?;
        }
        let result = run_in_transaction(&mut conn, statement, limit).await;
        let _ = conn.disconnect().await;
        let mut result = result?;
        result.elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;
        // "3 rows changed, committed." for a write; a statement that returned rows just says it committed.
        match result.messages.last_mut().filter(|_| result.columns.is_empty()) {
            Some(last) => last.push_str(" changed, committed."),
            None => result.messages.push("Committed.".to_string()),
        }
        Ok(result)
    }
}

/// A process-list state with MariaDB's progress report, when the statement has one (ALTER TABLE, LOAD DATA, …).
fn with_progress(state: String, r: &rows::Rec) -> String {
    let Some(progress) = r.number("progress").filter(|p| *p > 0.0) else { return state };
    match (r.number("stage"), r.number("max_stage")) {
        (Some(stage), Some(max)) if max > 1.0 => format!("{state} · stage {stage:.0} of {max:.0}, {progress:.0}%"),
        _ => format!("{state} · {progress:.0}%"),
    }
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

/// An `INSERT`/`REPLACE … VALUES` (no SELECT behind it): nothing to search, so nothing to plan.
fn lower_starts_values(statement: &str) -> bool {
    let lower = statement.to_ascii_lowercase();
    matches!(leading_verb(statement).as_str(), "insert" | "replace")
        && (lower.contains(" values") || lower.contains(" value ") || lower.contains(" set "))
        && !lower.contains("select")
}

/// What MySQL and MariaDB can EXPLAIN (`(`, `with`, `table`, `values` start reads too).
const EXPLAINABLE: [&str; 9] = ["select", "with", "table", "values", "(", "insert", "update", "delete", "replace"];
const WRITES: [&str; 4] = ["insert", "update", "delete", "replace"];

/// The statement's first word after any comments, lowercased (`(` for a parenthesized read).
fn leading_verb(statement: &str) -> String {
    let mut text = statement.trim_start();
    loop {
        if let Some(rest) = text.strip_prefix("/*") {
            text = rest.split_once("*/").map_or("", |(_, after)| after).trim_start();
        } else if text.starts_with("-- ") || text.starts_with('#') {
            text = text.split_once(['\n', '\r']).map_or("", |(_, after)| after).trim_start();
        } else {
            break;
        }
    }
    if text.starts_with('(') {
        return "(".into();
    }
    text.chars().take_while(|c| c.is_ascii_alphabetic()).collect::<String>().to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_leading_verb_skips_comments() {
        assert_eq!(super::leading_verb("/* app */ SELECT 1"), "select");
        assert_eq!(super::leading_verb("-- note\nUPDATE t SET a = 1"), "update");
        assert_eq!(super::leading_verb("# note\r\n DO `SLEEP` (?)"), "do");
        assert_eq!(super::leading_verb("(SELECT 1) UNION (SELECT 2)"), "(");
    }

    use super::*;

    #[test]
    fn placeholders_in_explain_get_a_plain_hint() {
        let raw = Error::new(
            culprit_core::ErrorCode::QueryFailed,
            "Driver error: `Statement takes 1 parameters but 0 was supplied.'",
        );
        let mapped = parameter_error(raw);
        assert!(mapped.message.contains("`?` parameters"), "{}", mapped.message);
        assert!(mapped.hint.as_deref().is_some_and(|h| h.contains("sample value")));
        assert_eq!(parameter_error(Error::invalid("other")).message, "other");
    }
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
