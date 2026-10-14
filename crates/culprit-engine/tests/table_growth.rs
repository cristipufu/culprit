//! How fast a table grows is told by comparing its size with an earlier scan of the same database, a day or more
//! before: each scan keeps the sizes that one read.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use culprit_core::{
    AuthKind, Engine, Error, ExplainMode, LiveState, Plan, Provider, Result, ResultSet, ServerInfo, Snapshot,
    SnapshotOptions,
};
use culprit_engine::{NewConnection, Workspace};

const MB: f64 = 1024.0 * 1024.0;

/// The demo with shop.support_tickets at `megabytes`, almost all of it large values.
fn demo(megabytes: u64) -> Snapshot {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/mssql-demo.json");
    let mut s: Snapshot = serde_json::from_slice(&std::fs::read(path).expect("fixture")).expect("snapshot");
    let t = s.tables.iter_mut().find(|t| t.schema == "shop" && t.name == "support_tickets").expect("support_tickets");
    let size = megabytes as f64 * MB;
    (t.rows, t.in_row_bytes, t.lob_bytes, t.overflow_bytes) =
        (Some(9_728.0), Some(31.0 * MB), Some(size - 31.0 * MB), Some(0.0));
    t.data_bytes = Some(size);
    s
}

/// Replays the demo at whatever size the test has set.
struct Replay(Arc<AtomicU64>);

#[async_trait]
impl Provider for Replay {
    fn engine(&self) -> Engine {
        Engine::Mssql
    }

    async fn probe(&self) -> Result<ServerInfo> {
        Ok(demo(1_000).server)
    }

    async fn snapshot(&self, _connection: &str, _options: &SnapshotOptions) -> Result<Snapshot> {
        Ok(demo(self.0.load(Ordering::SeqCst)))
    }

    async fn explain(&self, _: &str, _: ExplainMode) -> Result<Plan> {
        Err(Error::invalid("replay has no plans"))
    }

    async fn live(&self) -> Result<LiveState> {
        Err(Error::invalid("replay has no live state"))
    }

    async fn run(&self, _: &str, _: usize) -> Result<ResultSet> {
        Err(Error::invalid("replay runs nothing"))
    }

    async fn execute(&self, _: &str, _: usize) -> Result<ResultSet> {
        Err(Error::invalid("replay runs nothing"))
    }
}

#[tokio::test]
async fn each_scan_keeps_the_sizes_a_scan_a_day_before_read() {
    let home = std::env::temp_dir().join(format!("culprit-table-growth-{}", std::process::id()));
    let size = Arc::new(AtomicU64::new(2_331));
    let replay = size.clone();
    let ws = Workspace::open_in(&home)
        .with_provider(Engine::Mssql, Arc::new(move |_| Ok(Box::new(Replay(replay.clone())) as Box<dyn Provider>)));
    ws.add_connection(NewConnection {
        name: "shop".into(),
        input: "mssql://reader@replay.invalid:1433/culprit_demo".into(),
        auth: Some(AuthKind::None),
        ..Default::default()
    })
    .expect("save connection");
    let scan = || async { ws.scan("shop", &SnapshotOptions::default()).await.expect("scan") };
    let finding = |r: &culprit_core::Report| {
        r.findings.iter().find(|f| f.id == "large-rows:shop.support_tickets").cloned().expect("the large-rows finding")
    };

    // The first scan has nothing to compare with.
    let first = scan().await;
    assert!(first.snapshot.sizes_before.is_none());
    assert!(!finding(&first).summary.contains("previous scan"));

    // A scan moments later has nothing old enough to measure growth from: minutes of noise aren't a rate.
    size.store(2_931, Ordering::SeqCst);
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    let second = scan().await;
    assert!(second.snapshot.sizes_before.is_none(), "the scan before is moments old");
    assert!(!finding(&second).metrics.iter().any(|m| m.label == "Growth a day"));

    // A scan a day and more before, of the same database, is the baseline; a newer one of another database isn't.
    let dir = home.join("reports").join("shop");
    let backdated = |hours: i64, database: &str| {
        let mut r = first.clone();
        let at = chrono::Utc::now() - chrono::Duration::hours(hours);
        r.snapshot.id = at.format("%Y%m%dT%H%M%S%.3fZ").to_string();
        r.snapshot.taken_at = at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        r.snapshot.server.database = database.to_string();
        culprit_engine::reports::save(&dir, &r).expect("save a backdated scan");
        r.snapshot.taken_at
    };
    backdated(22, "another_database");
    let day_before = backdated(30, &first.snapshot.server.database);
    let third = scan().await;
    let before = third.snapshot.sizes_before.as_ref().expect("the sizes of the scan a day before");
    assert_eq!(before.taken_at, day_before);
    assert_eq!(before.bytes("shop.support_tickets"), Some(2_331.0 * MB));
    assert!(finding(&third).metrics.iter().any(|m| m.label == "Growth a day"));
    let _ = std::fs::remove_dir_all(home);
}
