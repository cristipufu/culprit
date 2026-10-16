//! What the app's file pickers hand over, end to end: a SQLite file chosen as a new connection, and the log its
//! application writes chosen as the connection's query log. The native dialog only returns a path; everything after
//! it (the connection's input, adding it, checking and keeping the log, the scan that reads it) runs here.

use std::path::{Path, PathBuf};

use culprit_core::{Engine, Environment, SnapshotOptions, WorkloadKind};
use culprit_engine::{NewConnection, Workspace, database_file_input};

/// A fresh folder with a small application database and the Rails log beside it.
fn app_folder(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("culprit-pickers-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("home")).unwrap();
    let db = rusqlite::Connection::open(dir.join("app.db")).unwrap();
    db.execute_batch(
        "CREATE TABLE customers (id INTEGER PRIMARY KEY, email TEXT NOT NULL, region TEXT);
         CREATE TABLE orders (id INTEGER PRIMARY KEY, customer_id INTEGER NOT NULL REFERENCES customers(id),
                              status TEXT NOT NULL, created_at TEXT NOT NULL);
         WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 500)
         INSERT INTO customers (email, region) SELECT 'c' || i || '@example.com', 'eu' FROM n;
         WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 2000)
         INSERT INTO orders (customer_id, status, created_at) SELECT 1 + i % 500, 'paid', '2026-10-15' FROM n;",
    )
    .unwrap();
    let mut log = String::new();
    for minute in 0..30 {
        log.push_str(&format!(
            "Started GET \"/customers/{minute}/orders\" for 10.0.0.1 at 2026-10-15 09:{minute:02}:00 +0000\n  \
             Order Load (4.{minute}ms)  SELECT \"orders\".* FROM \"orders\" WHERE \"orders\".\"customer_id\" = ? ORDER BY \"orders\".\"created_at\" DESC LIMIT ?  [[\"customer_id\", {minute}], [\"LIMIT\", 20]]\n\
             Completed 200 OK in 9ms (ActiveRecord: 4.0ms | Allocations: 900)\n"
        ));
    }
    std::fs::write(dir.join("development.log"), log).unwrap();
    std::fs::write(dir.join("notes.txt"), "Nothing here is a SQL statement.\n").unwrap();
    dir
}

fn add_file(ws: &Workspace, name: &str, path: &Path) -> culprit_core::Result<culprit_engine::ConnectionInfo> {
    ws.add_connection(NewConnection {
        name: name.into(),
        input: database_file_input(path),
        environment: Environment::Development,
        engine: None,
        database: None,
        user: None,
        password: None,
        password_env: None,
        auth: None,
    })
}

// The database picker returns the file's path; a file without a SQLite extension is still read as SQLite.
#[test]
fn a_chosen_database_file_is_a_sqlite_connection_whatever_its_name() {
    let dir = app_folder("names");
    assert_eq!(database_file_input(Path::new("/srv/app.db")), "/srv/app.db");
    assert_eq!(database_file_input(Path::new("/srv/APP.SQLITE3")), "/srv/APP.SQLITE3");
    assert_eq!(database_file_input(Path::new("/srv/data.bin")), "sqlite:/srv/data.bin");
    std::fs::copy(dir.join("app.db"), dir.join("data.bin")).unwrap();
    let ws = Workspace::open_in(dir.join("home"));
    for (name, file) in [("app", "app.db"), ("renamed", "data.bin")] {
        let info = add_file(&ws, name, &dir.join(file)).expect("added");
        assert_eq!(info.engine, Engine::Sqlite, "{name}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

// The log picker returns the log's path: kept on the connection, a file that holds no statements refused with why,
// removed on request (and not found again), and read by the next scan.
#[tokio::test]
async fn a_chosen_query_log_is_checked_kept_and_read_by_the_scan() {
    let dir = app_folder("log");
    let ws = Workspace::open_in(dir.join("home"));
    let info = add_file(&ws, "app", &dir.join("app.db")).expect("added");
    // The picker starts beside the database file, or beside the log once there is one.
    assert_eq!(
        Workspace::query_log_folder(&info).map(|p| p.canonicalize().unwrap()),
        Some(dir.canonicalize().unwrap())
    );

    let refused = ws.set_query_log("app", Some(&dir.join("notes.txt"))).expect_err("not a log");
    assert!(!refused.message.is_empty());
    let info = ws.set_query_log("app", Some(&dir.join("development.log"))).expect("kept");
    let kept = info.query_log.clone().expect("a log");
    assert!(kept.ends_with("development.log") && Path::new(&kept).is_absolute(), "{kept}");
    assert_eq!(
        Workspace::query_log_folder(&info).map(|p| p.canonicalize().unwrap()),
        Some(dir.canonicalize().unwrap())
    );

    let report = ws.scan("app", &SnapshotOptions::default()).await.expect("scanned");
    let w = report.snapshot.workload.as_ref().expect("the log's statements");
    assert_eq!(w.kind(), Some(WorkloadKind::QueryLog));
    assert!(w.queries.iter().any(|q| q.text.contains("\"orders\"")), "{:?}", w.queries);

    let info = ws.set_query_log("app", None).expect("removed");
    assert_eq!(info.query_log, None);
    let report = ws.scan("app", &SnapshotOptions::default()).await.expect("scanned");
    assert!(report.snapshot.workload.is_none(), "a removed log isn't found again beside the file");
    let _ = std::fs::remove_dir_all(&dir);
}
