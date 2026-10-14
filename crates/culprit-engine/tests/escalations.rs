//! SQL Server counts lock escalations from when it started and keeps no time for each. A scan tells new ones from old
//! ones by comparing with the scan before it, and a scheduled digest, which keeps no scans, with the digest before it.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use async_trait::async_trait;
use culprit_core::{
    AuthKind, Digest, Engine, Error, ExplainMode, IndexContention, LiveState, Plan, Provider, Report, Result,
    ResultSet, ServerInfo, Severity, Snapshot, SnapshotOptions,
};
use culprit_engine::{NewConnection, Workspace};

const ID: &str = "lock-escalations:shop.orders";

fn temp_home(tag: &str) -> PathBuf {
    std::env::temp_dir().join(format!("culprit-escalations-{tag}-{}", std::process::id()))
}

/// The demo with `escalations` counted on one index of shop.orders.
fn demo(escalations: u32) -> Snapshot {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/mssql-demo.json");
    let mut s: Snapshot = serde_json::from_slice(&std::fs::read(path).expect("fixture")).expect("snapshot");
    let orders = s.indexes.iter_mut().find(|i| i.schema == "shop" && i.table == "orders").expect("an orders index");
    orders.contention = Some(IndexContention { lock_escalations: f64::from(escalations), ..Default::default() });
    // Started twelve hours before the scan: without an earlier scan, the count is graded on its rate since the start.
    let started = chrono::Utc::now() - chrono::Duration::hours(12);
    s.server.started_at = Some(started.to_rfc3339_opts(chrono::SecondsFormat::Secs, true));
    s
}

/// Replays the demo with however many escalations the test has set.
struct Replay(Arc<AtomicU32>);

#[async_trait]
impl Provider for Replay {
    fn engine(&self) -> Engine {
        Engine::Mssql
    }

    async fn probe(&self) -> Result<ServerInfo> {
        Ok(demo(0).server)
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

fn escalation(report: &Report) -> &culprit_core::Finding {
    report.findings.iter().find(|f| f.id == ID).expect("the escalation finding")
}

#[tokio::test]
async fn each_scan_compares_escalations_with_the_scan_before() {
    let home = temp_home("scans");
    let count = Arc::new(AtomicU32::new(2));
    let replay = count.clone();
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

    // The first scan has nothing to compare with: it can't tell whether the two are recent, only that two in the 12
    // hours since the start is more than one a day.
    let first = scan().await;
    assert!(first.snapshot.escalations_before.is_none());
    assert_eq!(escalation(&first).severity, Severity::High);
    assert!(escalation(&first).title.contains("2 times since SQL Server started"), "{}", escalation(&first).title);

    // Nothing new since: history.
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    let second = scan().await;
    assert_eq!(second.snapshot.escalations_before.as_ref().map(|b| &b.taken_at), Some(&first.snapshot.taken_at));
    assert_eq!(escalation(&second).severity, Severity::Low);
    assert!(escalation(&second).title.contains(", none since "), "{}", escalation(&second).title);

    // One more since the second scan.
    count.store(3, Ordering::SeqCst);
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    let third = scan().await;
    assert_eq!(escalation(&third).severity, Severity::High);
    assert!(escalation(&third).title.contains("escalated to a table lock once since "), "{}", escalation(&third).title);
    let _ = std::fs::remove_dir_all(home);
}

#[test]
fn a_scheduled_digest_compares_escalations_with_the_previous_digest() {
    let home = temp_home("digest");
    std::fs::create_dir_all(&home).unwrap();
    let ws = Workspace::open_in(&home);
    let write = |name: &str, json: Vec<u8>| {
        let path = home.join(name);
        std::fs::write(&path, json).unwrap();
        path
    };
    let report = |name: &str, taken_at: &str, escalations: u32, started_at: &str| {
        let mut s = demo(escalations);
        s.taken_at = taken_at.into();
        s.server.started_at = Some(started_at.into());
        write(name, serde_json::to_vec(&s).unwrap())
    };
    let started = "2026-10-11T12:13:03Z";
    let finding = |d: &Digest| d.findings.iter().find(|f| f.id == ID).cloned().expect("the escalation in the digest");

    // A job keeps no scans: the first digest can't tell, the next ones compare with the digest before.
    let first = ws.digest(None, Some(&report("a.json", "2026-10-12T06:00:00Z", 2, started)), None, 10, 5).unwrap();
    assert_eq!(first.server_started_at.as_deref(), Some(started));
    assert_eq!(finding(&first).severity, Severity::High);
    let first_path = write("first.json", serde_json::to_vec(&first).unwrap());

    let same =
        ws.digest(None, Some(&report("b.json", "2026-10-12T12:00:00Z", 2, started)), Some(&first_path), 10, 5).unwrap();
    assert_eq!(finding(&same).severity, Severity::Low);
    assert!(finding(&same).title.ends_with(", none since Oct 12, 06:00 UTC"), "{}", finding(&same).title);
    assert!(same.since.as_ref().unwrap().again.is_empty());
    let same_path = write("same.json", serde_json::to_vec(&same).unwrap());

    let grew =
        ws.digest(None, Some(&report("c.json", "2026-10-12T18:00:00Z", 5, started)), Some(&same_path), 10, 5).unwrap();
    assert_eq!(finding(&grew).severity, Severity::High);
    assert!(finding(&grew).title.contains("3 times since Oct 12, 12:00 UTC"), "{}", finding(&grew).title);
    let since = grew.since.as_ref().unwrap();
    assert_eq!(since.again.iter().map(|f| f.count_since).collect::<Vec<_>>(), [Some(3.0)]);
    // Low to High is the count growing, which `again` already says.
    assert!(since.worse.iter().all(|f| f.id != ID));

    // The server restarted in between: the one counted since is new, though the count is lower.
    let restarted = ws
        .digest(
            None,
            Some(&report("d.json", "2026-10-13T06:00:00Z", 1, "2026-10-13T01:00:00Z")),
            Some(&same_path),
            10,
            5,
        )
        .unwrap();
    assert_eq!(finding(&restarted).severity, Severity::High);
    assert!(finding(&restarted).title.contains("once since Oct 12, 12:00 UTC"), "{}", finding(&restarted).title);
    let _ = std::fs::remove_dir_all(home);
}
