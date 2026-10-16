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

pub struct SqliteProvider {
    path: PathBuf,
    spec: ConnectionSpec,
}

impl SqliteProvider {
    pub fn new(spec: &ConnectionSpec) -> Result<Self> {
        let path = spec
            .database
            .as_deref()
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .ok_or_else(|| Error::invalid("A SQLite connection needs the database file's path"))?;
        Ok(SqliteProvider { path: culprit_core::expand_home(path), spec: spec.clone() })
    }

    /// The file, opened read-only: Culprit never writes to it, and `query_only` refuses a write even through a
    /// statement the read-only flag would let start. A writer holding the file waits up to two seconds, then the read
    /// fails rather than hanging.
    fn open(&self) -> Result<Connection> {
        open(&self.path)
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
            error.with_hint("Another process held the file's write lock for more than two seconds; try again.")
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

#[async_trait]
impl Provider for SqliteProvider {
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
        let conn = self.open()?;
        blocking(move || plan::explain(&conn, &statement)).await
    }

    async fn live(&self) -> Result<LiveState> {
        Err(Engine::Sqlite.refused(Ability::Sessions))
    }

    async fn run(&self, sql: &str, limit: usize) -> Result<ResultSet> {
        let statement = culprit_core::sql::single_statement(sql, Engine::Sqlite)?.to_string();
        let conn = self.open()?;
        blocking(move || collect::run(&conn, &statement, limit)).await
    }
}
