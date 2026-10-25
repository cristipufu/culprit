//! SQLite provider. A SQLite database is a file, not a server: Culprit opens it read-only and reads its schema,
//! settings and plans. SQLite keeps no statement statistics, sessions or waits, so those parts of a snapshot are
//! missing capabilities with the reason, never zeros. See `docs/providers/sqlite.md`.

mod collect;
pub mod plan;
pub mod sources;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use culprit_core::{
    Ability, ConnectionSpec, Engine, Error, ErrorCode, ExplainMode, LiveState, Plan, Provider, Result, ResultSet,
    ServerInfo, Snapshot, SnapshotOptions,
};
use rusqlite::{Connection, OpenFlags};

pub use plan::plan_from_rows;

/// Identity of this crate's collector and native-plan parser, including a build revision when supplied.
pub fn component_version() -> culprit_core::ComponentVersion {
    culprit_core::ComponentVersion::package(
        "culprit-sqlite",
        env!("CARGO_PKG_VERSION"),
        option_env!("CULPRIT_BUILD_REVISION").or(option_env!("GITHUB_SHA")),
    )
}

pub struct SqliteProvider {
    path: PathBuf,
    spec: ConnectionSpec,
}

impl SqliteProvider {
    pub fn new(spec: &ConnectionSpec) -> Result<Self> {
        if spec.ssh.is_some() || spec.transport.is_some() {
            return Err(Error::new(ErrorCode::Unsupported, "SQLite opens a local file and cannot use SSH forwarding"));
        }
        let path = spec
            .database
            .as_deref()
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .ok_or_else(|| Error::invalid("A SQLite connection needs the database file's path"))?;
        Ok(SqliteProvider { path: culprit_core::expand_home(path), spec: spec.clone() })
    }
}

fn open(path: &PathBuf) -> Result<Connection> {
    if !path.exists() {
        return Err(Error::new(ErrorCode::NotFound, format!("No SQLite file at {}", path.display()))
            .with_hint("Check the path; Culprit opens existing files only, read-only."));
    }
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX)
        .map_err(|e| map_error(&e))?;
    conn.busy_timeout(Duration::from_secs(2)).map_err(|e| map_error(&e))?;
    conn.execute_batch("PRAGMA query_only = 1;").map_err(|e| map_error(&e))?;
    Ok(conn)
}

pub(crate) fn map_error(e: &rusqlite::Error) -> Error {
    if e.sqlite_error_code() == Some(rusqlite::ErrorCode::OperationInterrupted) {
        return Error::new(ErrorCode::Timeout, "The SQLite statement was cancelled or reached its execution deadline")
            .with_hint("Add a more selective filter or simplify the query.");
    }
    let message = e.to_string();
    let code = match e.sqlite_error_code() {
        Some(rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked) => ErrorCode::Timeout,
        Some(rusqlite::ErrorCode::ReadOnly) => ErrorCode::PermissionDenied,
        Some(rusqlite::ErrorCode::NotADatabase) => ErrorCode::InvalidInput,
        Some(rusqlite::ErrorCode::CannotOpen) => ErrorCode::ConnectionFailed,
        _ => ErrorCode::QueryFailed,
    };
    let error = Error::new(code, message);
    match code {
        ErrorCode::Timeout => {
            error.with_hint("SQLite reported a busy or locked file. Another connection or a conflicting transaction can cause this; the error does not measure how long it held the lock. Culprit's read waits at most two seconds where SQLite permits waiting.")
        }
        ErrorCode::PermissionDenied => {
            error.with_hint("Culprit opens SQLite files read-only and never writes to them.")
        }
        _ => error,
    }
}

/// Run SQLite's blocking calls off the async runtime.
async fn blocking<T: Send + 'static>(f: impl FnOnce() -> Result<T> + Send + 'static) -> Result<T> {
    tokio::task::spawn_blocking(f).await.map_err(|e| Error::internal(format!("The SQLite task stopped: {e}")))?
}

const STATEMENT_TIMEOUT: Duration = culprit_core::limits::QUERY_TIMEOUT;
static STATEMENTS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(4);

/// Dropping an async caller cannot abort spawn_blocking: make SQLite observe the cancellation inside its VM instead.
async fn interactive<T: Send + 'static>(
    path: PathBuf,
    budget: Duration,
    operation: impl FnOnce(&Connection) -> Result<T> + Send + 'static,
) -> Result<T> {
    let guard = culprit_core::limits::OperationGuard::new(budget)?;
    let handle = guard.handle();
    let permit = tokio::time::timeout(handle.remaining(), STATEMENTS.acquire())
        .await
        .map_err(|_| culprit_core::limits::timeout_error())?
        .map_err(|_| Error::internal("SQLite execution is unavailable"))?;
    blocking(move || {
        let _permit = permit;
        handle.check()?;
        let conn = open(&path)?;
        handle.check()?;
        conn.progress_handler(1_000, Some(move || handle.stopped())).map_err(|e| map_error(&e))?;
        operation(&conn)
    })
    .await
}

#[async_trait]
impl Provider for SqliteProvider {
    fn collector_version(&self) -> culprit_core::ComponentVersion {
        component_version()
    }

    fn engine(&self) -> Engine {
        Engine::Sqlite
    }

    async fn probe(&self) -> Result<ServerInfo> {
        let path = self.path.clone();
        blocking(move || {
            let conn = open(&path)?;
            let (have, missing) = collect::capabilities(&conn, collect::whole_file_read(&conn).map(|_| ()));
            Ok(collect::server_info(&conn, &path, have, missing))
        })
        .await
    }

    async fn snapshot(&self, connection: &str, _options: &SnapshotOptions) -> Result<Snapshot> {
        let path = self.path.clone();
        let (connection, environment) = (connection.to_string(), self.spec.environment);
        blocking(move || {
            let started = Instant::now();
            let conn = open(&path)?;
            collect::snapshot(&conn, &path, &connection, environment, started)
        })
        .await
    }

    async fn explain(&self, sql: &str, mode: ExplainMode) -> Result<Plan> {
        if mode == ExplainMode::Analyze {
            return Err(Engine::Sqlite.refused(Ability::MeasuresRuns));
        }
        let statement = culprit_core::sql::single_statement(sql, Engine::Sqlite)?.to_string();
        interactive(self.path.clone(), culprit_core::limits::OBSERVE_TIMEOUT, move |conn| {
            plan::explain(conn, &statement)
        })
        .await
    }

    async fn live(&self) -> Result<LiveState> {
        Err(Engine::Sqlite.refused(Ability::Sessions))
    }

    async fn run(&self, sql: &str, limit: usize) -> Result<ResultSet> {
        let statement = culprit_core::sql::single_statement(sql, Engine::Sqlite)?.to_string();
        interactive(self.path.clone(), STATEMENT_TIMEOUT, move |conn| collect::run(conn, &statement, limit)).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use culprit_core::{NewIndex, Online};

    // Expensive before its first row, but finite: a broken interrupt must fail the test without hanging runtime shutdown.
    const SLOW: &str =
        "WITH RECURSIVE c(x) AS (VALUES(0) UNION ALL SELECT x+1 FROM c WHERE x < 10000000) SELECT count(*) FROM c";

    fn query_file(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("culprit-query-{name}-{}.sqlite", std::process::id()));
        drop(Connection::open(&path).unwrap());
        path
    }

    #[tokio::test]
    async fn execution_deadlines_interrupt_work_before_the_first_result_row() {
        let path = query_file("deadline");
        let result = interactive(path.clone(), Duration::from_millis(10), |conn| collect::run(conn, SLOW, 1));
        let error = tokio::time::timeout(Duration::from_secs(2), result).await.unwrap().unwrap_err();
        assert_eq!(error.code, ErrorCode::Timeout);
        std::fs::remove_file(path).unwrap();
    }

    #[tokio::test]
    async fn execution_deadline_includes_waiting_for_a_worker() {
        let workers = STATEMENTS.acquire_many(4).await.unwrap();
        let path = query_file("queued-deadline");
        let result = interactive(path.clone(), Duration::from_millis(10), |_| -> Result<()> {
            panic!("expired queued work must not start");
        });
        let error = tokio::time::timeout(Duration::from_secs(2), result).await.unwrap().unwrap_err();
        assert_eq!(error.code, ErrorCode::Timeout);
        drop(workers);
        std::fs::remove_file(path).unwrap();
    }

    #[tokio::test]
    async fn dropping_a_caller_stops_its_blocking_query() {
        let path = query_file("cancellation");
        let (started, ready) = tokio::sync::oneshot::channel();
        let (finished, stopped) = tokio::sync::oneshot::channel();
        let query = tokio::spawn(interactive(path.clone(), STATEMENT_TIMEOUT, move |conn| {
            let _ = started.send(());
            let result = collect::run(conn, SLOW, 1);
            let _ = finished.send(result.as_ref().err().map(|e| e.code));
            result
        }));
        ready.await.unwrap();
        query.abort();
        let _ = query.await;
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(2), stopped).await.unwrap().unwrap(),
            Some(ErrorCode::Timeout)
        );
        // The aborted caller no longer owns a JoinHandle: wait until blocking workers have released their files too.
        let _workers = tokio::time::timeout(Duration::from_secs(2), STATEMENTS.acquire_many(4)).await.unwrap().unwrap();
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn file_identity_uses_the_canonical_path_and_detects_replacements() {
        let unique = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let dir = std::env::temp_dir().join(format!("culprit-sqlite-identity-{}-{unique}", std::process::id()));
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("shop.sqlite");
        let read = |path: &std::path::Path| {
            let conn = open(&path.to_path_buf()).unwrap();
            collect::snapshot(&conn, path, "shop", culprit_core::Environment::Local, Instant::now()).unwrap()
        };
        drop(Connection::open(&path).unwrap());
        let first = read(&path);
        let alias = read(&dir.join(".").join("shop.sqlite"));
        assert!(first.identity.is_some());
        assert!(first.same_database(&alias));
        #[cfg(unix)]
        {
            let link = dir.join("alias.sqlite");
            std::os::unix::fs::symlink(&path, &link).unwrap();
            assert!(first.same_database(&read(&link)));
        }
        let other = dir.join("other.sqlite");
        drop(Connection::open(&other).unwrap());
        assert!(!first.same_database(&read(&other)));
        // Keep the old inode alive so the new file cannot reuse it.
        std::fs::rename(&path, dir.join("original.sqlite")).unwrap();
        drop(Connection::open(&path).unwrap());
        let replacement = read(&path);
        #[cfg(unix)]
        assert!(!first.same_database(&replacement));
        #[cfg(not(unix))]
        assert!(first.same_database(&replacement), "only path identity is available on this platform");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn collection_outcomes_distinguish_empty_catalog_and_inapplicable_wal() {
        let conn = Connection::open_in_memory().unwrap();
        let read = || {
            collect::snapshot(
                &conn,
                std::path::Path::new("example.sqlite"),
                "example",
                culprit_core::Environment::Local,
                Instant::now(),
            )
            .unwrap()
        };
        let empty = read();
        assert_eq!(empty.source_outcome("catalog").unwrap().state, culprit_core::SourceState::Empty);
        assert_eq!(empty.source_outcome("wal_file").unwrap().state, culprit_core::SourceState::NotApplicable);
        conn.execute_batch(
            "CREATE TABLE items (id INTEGER PRIMARY KEY, name TEXT); CREATE INDEX items_name ON items(name);",
        )
        .unwrap();
        let populated = read();
        assert_eq!(populated.source_outcome("catalog").unwrap().state, culprit_core::SourceState::Collected);
        assert!(populated.tables.iter().any(|t| t.name == "items"));
        assert!(populated.indexes.iter().any(|i| i.name == "items_name"));
        assert!(populated.source_problem(&["catalog"]).is_none());
    }

    #[test]
    fn an_unreadable_statistics_source_locks_only_its_capability() {
        use rusqlite::hooks::{AuthAction, Authorization};
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE items (name TEXT); CREATE INDEX items_name ON items(name); ANALYZE;").unwrap();
        conn.authorizer(Some(|context: rusqlite::hooks::AuthContext<'_>| {
            if matches!(context.action, AuthAction::Read { table_name: "sqlite_stat1", .. }) {
                Authorization::Deny
            } else {
                Authorization::Allow
            }
        }))
        .unwrap();
        let snapshot = collect::snapshot(
            &conn,
            std::path::Path::new("example.sqlite"),
            "example",
            culprit_core::Environment::Local,
            Instant::now(),
        )
        .unwrap();
        assert_eq!(snapshot.source_outcome("statistics").unwrap().state, culprit_core::SourceState::Unavailable);
        assert!(!snapshot.server.has(culprit_core::Capability::Statistics));
        assert!(snapshot.server.has(culprit_core::Capability::TableStats));
        assert!(snapshot.tables.iter().any(|t| t.name == "items"));
        assert!(snapshot.server.missing.iter().any(|m| m.capability == culprit_core::Capability::Statistics));
    }

    #[test]
    fn index_fixes_execute_in_main_and_attached_schemas() {
        let conn = Connection::open_in_memory().unwrap();
        let dialect = Engine::Sqlite.dialect();
        let attached = "audit.\"logs";
        conn.execute_batch(&format!("ATTACH ':memory:' AS {};", dialect.quote(attached))).unwrap();
        let table = "or.\"ders";
        let column = "customer.\"id";
        let keys = vec![format!("{column} DESC")];
        let filter = format!("{} IS NOT NULL", dialect.quote(column));
        for schema in ["main", attached] {
            conn.execute_batch(&format!(
                "CREATE TABLE {} ({} INTEGER);",
                dialect.qualified(&[schema, table]),
                dialect.quote(column)
            ))
            .unwrap();
        }
        // Main, attached, and unqualified targets all use SQLite's grammar. Dots and quotes inside identifiers must
        // survive the move of the schema from the table to the index, as must unique, descending and partial keys.
        for (schema, qualified, name) in
            [("main", true, "ix.\"main"), (attached, true, "ix.\"attached"), ("main", false, "ix.\"unqualified")]
        {
            let target = if qualified { dialect.qualified(&[schema, table]) } else { dialect.quote(table) };
            let sql = dialect.create_index(&NewIndex {
                target: &target,
                unique: true,
                name,
                keys: &keys,
                include: &[],
                filter: Some(&filter),
                online: Online::No,
            });
            conn.execute_batch(&sql).unwrap_or_else(|error| panic!("{sql}\n{error}"));
            let found: (String, bool) = conn
                .query_row(
                    &format!(
                        "SELECT tbl_name, sql LIKE 'CREATE UNIQUE INDEX%WHERE%' FROM {}.sqlite_schema WHERE name = ?",
                        dialect.quote(schema)
                    ),
                    [name],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .unwrap();
            assert_eq!(found, (table.to_string(), true), "{sql}");
            let descending: bool = conn
                .query_row(
                    &format!("PRAGMA {}.index_xinfo({});", dialect.quote(schema), dialect.quote(name)),
                    [],
                    |row| row.get(3),
                )
                .unwrap();
            assert!(descending, "{sql}");
        }
    }
}
