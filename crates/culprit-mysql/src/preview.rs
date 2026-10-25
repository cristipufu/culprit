//! Short, owned previews. A control session cancels our tagged query even when server timeouts are unavailable.
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use culprit_core::{
    Error, ErrorCode, Result, ResultSet,
    limits::{PREVIEW_TIMEOUT, timeout_error},
};
use mysql_async::{Conn, prelude::Queryable};
use tokio::sync::{Semaphore, oneshot};

use crate::connect::{Connector, Scope, TAG, map_error};
use crate::rows::tagged;

// Include retiring workers: rapid navigation cannot accumulate cancellation connections behind the UI's gate.
static PREVIEWS: Semaphore = Semaphore::const_new(2);
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(1);

fn marker() -> String {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let epoch = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos();
    format!("{TAG} /* preview:{:x}-{epoch:x}-{:x} */", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed))
}

pub(crate) async fn read(connector: Arc<Connector>, sql: String, limit: usize) -> Result<ResultSet> {
    let permit = PREVIEWS.acquire().await.map_err(|_| Error::internal("The preview worker gate closed"))?;
    let conn = connector.connect(Scope::Database).await?;
    let control = connector.connect(Scope::Server).await?;
    let setting = connector.flavor().timeout_for(PREVIEW_TIMEOUT);
    let (owner, canceled) = oneshot::channel::<()>();
    let task = tokio::spawn(async move {
        // Retain the SSH route and worker slot through cancellation and bounded disconnects.
        let _transport = connector;
        let _permit = permit;
        worker(conn, control, &sql, limit, &setting, canceled).await
    });
    let result = task.await.map_err(|_| Error::internal("The table preview worker stopped"))?;
    drop(owner);
    result
}

async fn worker(
    mut conn: Conn,
    mut control: Conn,
    sql: &str,
    limit: usize,
    setting: &str,
    canceled: oneshot::Receiver<()>,
) -> Result<ResultSet> {
    let id = conn.id();
    let marker = marker();
    let started = Instant::now();
    let result = tokio::select! {
        biased;
        _ = canceled => None,
        result = tokio::time::timeout(PREVIEW_TIMEOUT, execute(&mut conn, sql, limit, setting, &marker)) => Some(result),
    };
    let result = match result {
        Some(Ok(result)) => result,
        _ => {
            let accepted =
                tokio::time::timeout(CLEANUP_TIMEOUT, cancel(&mut control, id, &marker)).await.unwrap_or(false);
            Err(if accepted {
                timeout_error()
            } else {
                Error::new(
                    ErrorCode::Timeout,
                    "Culprit stopped waiting and could not confirm server cancellation. Check Live before retrying.",
                )
            })
        }
    };
    // mysql_async's Drop normally drains pending results. Disconnect marks the connection closed before awaiting I/O,
    // so timing out these disconnects closes the sockets instead of leaving an unbounded background drain.
    tokio::join!(disconnect(conn), disconnect(control));
    result.map(|mut result| {
        result.elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;
        result
    })
}

async fn execute(conn: &mut Conn, sql: &str, limit: usize, setting: &str, marker: &str) -> Result<ResultSet> {
    // Older servers and proxies may refuse this optional setting. The owned worker still enforces its deadline.
    let _ = conn.query_drop(format!("{marker} {setting}")).await;
    conn.query_drop(format!("{marker} START TRANSACTION READ ONLY")).await.map_err(map_error)?;
    let result = crate::read_result(conn, &format!("{marker} {sql}"), limit).await;
    let rollback = conn.query_drop(format!("{marker} ROLLBACK")).await;
    let result = result?;
    rollback.map_err(map_error)?;
    Ok(result)
}

async fn cancel(control: &mut Conn, id: u32, marker: &str) -> bool {
    // A proxy or load balancer may route the control session elsewhere. Never kill an unrelated reused/server-local ID.
    let current: mysql_async::Result<Option<(Option<String>,)>> =
        control.exec_first(tagged("SELECT INFO FROM information_schema.PROCESSLIST WHERE ID = ?"), (id,)).await;
    match current {
        Ok(Some((Some(sql),))) if sql.starts_with(marker) => {
            control.query_drop(tagged(&format!("KILL QUERY {id}"))).await.is_ok()
        }
        _ => false,
    }
}

async fn disconnect(conn: Conn) {
    let _ = tokio::time::timeout(CLEANUP_TIMEOUT, conn.disconnect()).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use culprit_core::{ConnectionSpec, Engine, ErrorCode};

    #[test]
    fn preview_markers_are_unique_tagged_comments() {
        let first = marker();
        let second = marker();
        assert_ne!(first, second);
        assert!(first.starts_with(TAG) && first.ends_with("*/") && first.len() < 100);
        assert!(culprit_core::sql::reads_only(&format!("{first} SELECT 1"), Engine::Mysql));
    }

    fn connector() -> Arc<Connector> {
        Arc::new(
            Connector::new(&ConnectionSpec {
                engine: Some(Engine::Mysql),
                host: "127.0.0.1".into(),
                port: Some(
                    std::env::var("CULPRIT_MYSQL_TEST_PORT").expect("set the disposable MySQL port").parse().unwrap(),
                ),
                user: Some(std::env::var("CULPRIT_MYSQL_TEST_USER").unwrap_or("root".into())),
                password: Some(
                    std::env::var("CULPRIT_MYSQL_TEST_PASSWORD").expect("set the disposable MySQL password"),
                ),
                ..Default::default()
            })
            .unwrap(),
        )
    }

    async fn active(connector: &Connector, id: u32) -> u64 {
        let mut conn = connector.connect(Scope::Server).await.unwrap();
        let count = conn.exec_first::<u64, _, _>(
            tagged("SELECT COUNT(*) FROM information_schema.PROCESSLIST WHERE ID = ? AND INFO LIKE '%culprit_preview_cancel_test%'"), (id,),
        ).await.unwrap().unwrap();
        disconnect(conn).await;
        count
    }

    #[tokio::test]
    #[ignore = "needs a disposable MySQL/MariaDB server and CULPRIT_MYSQL_TEST_PORT/PASSWORD"]
    async fn live_deadline_and_dropped_callers_cancel_without_a_server_timeout() {
        let connector = connector();
        let sql = "SELECT SLEEP(30) AS culprit_preview_cancel_test";
        let conn = connector.connect(Scope::Database).await.unwrap();
        let control = connector.connect(Scope::Server).await.unwrap();
        let (owner, canceled) = oneshot::channel();
        // An unsupported setting must not prevent ordinary previews.
        let result = worker(conn, control, "SELECT 42", 1, "SET SESSION culprit_unsupported_timeout = 5", canceled)
            .await
            .unwrap();
        assert_eq!(result.rows[0][0], serde_json::json!(42));
        drop(owner);
        let mut conn = connector.connect(Scope::Database).await.unwrap();
        // Explicitly disable the server timer; cancellation must come from the worker's KILL QUERY.
        let disable = match connector.flavor() {
            crate::connect::Flavor::Mysql => "SET SESSION max_execution_time = 0",
            crate::connect::Flavor::Mariadb => "SET SESSION max_statement_time = 0",
        };
        conn.query_drop(tagged(disable)).await.unwrap();
        let control = connector.connect(Scope::Server).await.unwrap();
        let (owner, canceled) = oneshot::channel();
        let id = conn.id();
        let started = Instant::now();
        let error =
            worker(conn, control, sql, 1, "SET SESSION culprit_unsupported_timeout = 5", canceled).await.unwrap_err();
        drop(owner);
        assert_eq!(error.code, ErrorCode::Timeout);
        assert!(started.elapsed() < Duration::from_secs(8), "{:?}", started.elapsed());
        assert_eq!(active(&connector, id).await, 0, "deadline left a server query running");

        let mut conn = connector.connect(Scope::Database).await.unwrap();
        conn.query_drop(tagged(disable)).await.unwrap();
        let control = connector.connect(Scope::Server).await.unwrap();
        let (owner, canceled) = oneshot::channel();
        let id = conn.id();
        let task = tokio::spawn(async move {
            worker(conn, control, sql, 1, "SET SESSION culprit_unsupported_timeout = 5", canceled).await
        });
        for _ in 0..100 {
            if active(&connector, id).await == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(active(&connector, id).await, 1, "query never became active");
        drop(owner);
        assert_eq!(task.await.unwrap().unwrap_err().code, ErrorCode::Timeout);
        assert_eq!(active(&connector, id).await, 0, "caller cancellation left a server query running");
        assert_eq!(read(connector, "SELECT 42".into(), 1).await.unwrap().rows[0][0], serde_json::json!(42));
    }

    #[tokio::test]
    #[ignore = "needs a disposable MySQL/MariaDB server and CULPRIT_MYSQL_TEST_PORT/PASSWORD"]
    async fn live_cancellation_never_kills_a_different_tag() {
        let connector = connector();
        let mut conn = connector.connect(Scope::Database).await.unwrap();
        let mut control = connector.connect(Scope::Server).await.unwrap();
        let id = conn.id();
        let task = tokio::spawn(async move {
            let result = conn.query_drop(tagged("SELECT SLEEP(30) AS culprit_preview_cancel_test")).await;
            disconnect(conn).await;
            result
        });
        for _ in 0..100 {
            if active(&connector, id).await == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(!cancel(&mut control, id, &marker()).await, "mismatched query was killed");
        assert_eq!(active(&connector, id).await, 1, "different-tag cancellation stopped the query");
        control.query_drop(tagged(&format!("KILL QUERY {id}"))).await.unwrap();
        let _ = task.await.unwrap();
        disconnect(control).await;
    }
}
