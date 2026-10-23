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
    assert_eq!(escalation(&first).actionability, culprit_core::Actionability::NeedsAttention);
    assert!(escalation(&first).title.contains("2 times since SQL Server started"), "{}", escalation(&first).title);

    // A second scan 5ms later does not cover the 7d window, so zero delta over 5ms does not silence the finding.
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    let second = scan().await;
    assert_eq!(second.snapshot.escalations_before.as_ref().map(|b| &b.taken_at), Some(&first.snapshot.taken_at));
    assert_eq!(escalation(&second).severity, Severity::High);
    assert_eq!(escalation(&second).actionability, culprit_core::Actionability::NeedsAttention);
    assert!(!escalation(&second).title.contains(", none since "), "{}", escalation(&second).title);

    // One more since the earlier scan.
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

    // A job keeps no scans: the first digest grades by the rate since server start and includes it in `top`, and the
    // next digests compare with the digest before.
    let first = ws.digest(None, Some(&report("a.json", "2026-10-12T06:00:00Z", 2, started)), None, 10, 5).unwrap();
    assert_eq!(first.server_started_at.as_deref(), Some(started));
    assert_eq!(finding(&first).severity, Severity::High);
    assert_eq!(finding(&first).actionability, culprit_core::Actionability::NeedsAttention);
    assert!(first.top.iter().any(|f| f.id == ID), "first digest must surface high-rate escalations in top");
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

#[test]
fn selecting_any_timespan_window_after_a_recent_scan_keeps_escalations_in_that_window() {
    let home = temp_home("all-windows");
    let reports_dir = home.join("reports");
    std::fs::create_dir_all(&reports_dir).unwrap();
    let ws = Workspace::open_in(&home);
    let started = "2026-10-19T12:41:00Z";

    // Case 1: The user opened the app at 06:05 UTC (which ran a 7d scan) and then selected any timespan window
    // (1h, 6h, 24h, 7d, 30d) at 06:06 UTC. Because SnapshotOptions::with_window_minutes clears window_minutes on whole
    // days (24h, 7d, 30d), every window must be respected generically. The 1-minute baseline from 06:05 must not
    // silence the 6 escalations into History for any window.
    let mut scan_0605 = demo(6);
    scan_0605.id = "20261023T060500.000Z".into();
    scan_0605.taken_at = "2026-10-23T06:05:00Z".into();
    scan_0605.server.started_at = Some(started.into());
    scan_0605.options = Some(SnapshotOptions::default().with_window_minutes(7 * 24 * 60));
    let r_0605 = culprit_checks::analyze(scan_0605);
    std::fs::write(reports_dir.join("20261023T060500.000Z.json"), serde_json::to_vec(&r_0605).unwrap()).unwrap();

    for window_minutes in [60_u32, 360, 1440, 7 * 1440, 30 * 1440] {
        let mut selected = demo(6);
        selected.id = format!("20261023T060600.{window_minutes:05}Z");
        selected.taken_at = "2026-10-23T06:06:00Z".into();
        selected.server.started_at = Some(started.into());
        selected.options = Some(SnapshotOptions::default().with_window_minutes(window_minutes));
        let path = reports_dir.join(format!("selected-{window_minutes}.json"));
        std::fs::write(&path, serde_json::to_vec(&selected).unwrap()).unwrap();
        let loaded = ws.analyze_file(&path).unwrap();
        let f = escalation(&loaded);
        assert_eq!(f.severity, Severity::High, "window {window_minutes}m");
        assert_eq!(f.actionability, culprit_core::Actionability::NeedsAttention, "window {window_minutes}m");
        assert!(!f.title.contains(", none since "), "window {window_minutes}m: {}", f.title);
        let _ = std::fs::remove_file(&path);
    }

    // Case 2: The user also scanned 10 hours ago (2026-10-22T20:00:00Z) when the count was already 6 (escalations
    // happened 11 hours ago). Now at 06:06 UTC:
    // - For 1h and 6h, the 10h-old scan proves 0 escalations happened in the last 1h or 6h -> History.
    // - For 24h, 7d and 30d, the 10h-old scan does not cover the window -> stays High / NeedsAttention.
    let mut scan_10h_ago = demo(6);
    scan_10h_ago.id = "20261022T200000.000Z".into();
    scan_10h_ago.taken_at = "2026-10-22T20:00:00Z".into();
    scan_10h_ago.server.started_at = Some(started.into());
    scan_10h_ago.options = Some(SnapshotOptions::default().with_window_minutes(7 * 24 * 60));
    let r_10h_ago = culprit_checks::analyze(scan_10h_ago);
    std::fs::write(reports_dir.join("20261022T200000.000Z.json"), serde_json::to_vec(&r_10h_ago).unwrap()).unwrap();

    for (window_minutes, expect_active) in
        [(60_u32, false), (360, false), (1440, true), (7 * 1440, true), (30 * 1440, true)]
    {
        let mut selected = demo(6);
        selected.id = format!("20261023T060600.{window_minutes:05}Z");
        selected.taken_at = "2026-10-23T06:06:00Z".into();
        selected.server.started_at = Some(started.into());
        selected.options = Some(SnapshotOptions::default().with_window_minutes(window_minutes));
        let path = reports_dir.join(format!("selected-10h-{window_minutes}.json"));
        std::fs::write(&path, serde_json::to_vec(&selected).unwrap()).unwrap();
        let loaded = ws.analyze_file(&path).unwrap();
        let f = escalation(&loaded);
        assert_eq!(f.is_actionable(), expect_active, "window {window_minutes}m: {}", f.title);
        let _ = std::fs::remove_file(&path);
    }

    // Case 3: An earlier scan from 12 hours ago (2026-10-22T18:00:00Z) before the escalations happened (count = 0)
    // is also present. Now 24h, 7d and 30d walk past both the 06:05 scan and the 20:00 scan to report 6 new
    // escalations since Oct 22, 18:00 UTC.
    let mut scan_12h_ago = demo(0);
    scan_12h_ago.id = "20261022T180000.000Z".into();
    scan_12h_ago.taken_at = "2026-10-22T18:00:00Z".into();
    scan_12h_ago.server.started_at = Some(started.into());
    scan_12h_ago.options = Some(SnapshotOptions::default().with_window_minutes(7 * 24 * 60));
    let r_12h_ago = culprit_checks::analyze(scan_12h_ago);
    std::fs::write(reports_dir.join("20261022T180000.000Z.json"), serde_json::to_vec(&r_12h_ago).unwrap()).unwrap();

    for window_minutes in [1440_u32, 7 * 1440, 30 * 1440] {
        let mut selected = demo(6);
        selected.id = format!("20261023T060600.{window_minutes:05}Z");
        selected.taken_at = "2026-10-23T06:06:00Z".into();
        selected.server.started_at = Some(started.into());
        selected.options = Some(SnapshotOptions::default().with_window_minutes(window_minutes));
        let path = reports_dir.join(format!("selected-12h-{window_minutes}.json"));
        std::fs::write(&path, serde_json::to_vec(&selected).unwrap()).unwrap();
        let loaded = ws.analyze_file(&path).unwrap();
        let f = escalation(&loaded);
        assert_eq!(f.severity, Severity::High, "window {window_minutes}m");
        assert_eq!(f.actionability, culprit_core::Actionability::NeedsAttention, "window {window_minutes}m");
        assert!(f.title.contains("6 times since Oct 22, 18:00 UTC"), "window {window_minutes}m: {}", f.title);
        let _ = std::fs::remove_file(&path);
    }

    let _ = std::fs::remove_dir_all(home);
}

#[test]
fn daily_ci_digest_compares_across_growing_query_store_cron_jitter_and_legacy_digests() {
    let home = temp_home("daily-ci");
    std::fs::create_dir_all(&home).unwrap();
    let ws = Workspace::open_in(&home);
    let started = "2026-10-19T12:41:00Z";
    let write = |name: &str, json: Vec<u8>| {
        let path = home.join(name);
        std::fs::write(&path, json).unwrap();
        path
    };

    // Day 1: 7d requested, Query Store has 24h of history so far (partial growing coverage).
    let mut day1_snap = demo(2);
    day1_snap.taken_at = "2026-10-22T06:00:00Z".into();
    day1_snap.server.started_at = Some(started.into());
    day1_snap.options = Some(SnapshotOptions::default().with_window_minutes(7 * 24 * 60));
    day1_snap.workload.as_mut().unwrap().window_hours = 24.0;
    day1_snap.workload.as_mut().unwrap().window_start = Some("2026-10-21T06:00:00Z".into());
    day1_snap.workload.as_mut().unwrap().window_end = Some("2026-10-22T06:00:00Z".into());
    day1_snap.bind_identity(day1_snap.database_identity());
    let day1_snap_path = write("day1-snap.json", serde_json::to_vec(&day1_snap).unwrap());
    let day1_digest = ws.digest(None, Some(&day1_snap_path), None, 10, 5).unwrap();
    let day1_digest_path = write("day1-digest.json", serde_json::to_vec(&day1_digest).unwrap());

    // Day 2: 7d requested, Query Store now has 48h of history ("the last 2 days"), and 4 more escalations occurred.
    let mut day2_snap = demo(6);
    day2_snap.taken_at = "2026-10-23T06:01:00Z".into();
    day2_snap.server.started_at = Some(started.into());
    day2_snap.options = Some(SnapshotOptions::default().with_window_minutes(7 * 24 * 60));
    day2_snap.workload.as_mut().unwrap().window_hours = 48.0;
    day2_snap.workload.as_mut().unwrap().window_start = Some("2026-10-21T06:01:00Z".into());
    day2_snap.workload.as_mut().unwrap().window_end = Some("2026-10-23T06:01:00Z".into());
    day2_snap.bind_identity(day2_snap.database_identity());
    let day2_snap_path = write("day2-snap.json", serde_json::to_vec(&day2_snap).unwrap());
    let day2_digest = ws.digest(None, Some(&day2_snap_path), Some(&day1_digest_path), 10, 5).unwrap();
    assert!(day2_digest.since.is_some(), "growing Query Store window (24h -> 48h) must allow digest comparison");
    assert!(day2_digest.top.iter().any(|f| f.id == ID && f.title.contains("4 times since Oct 22, 06:00 UTC")));

    // Cron jitter on a 24h window: 24h 05m between Day 1 and Day 2 must still be NeedsAttention.
    let mut day1_24h = day1_snap.clone();
    day1_24h.options = Some(SnapshotOptions::default().with_window_minutes(24 * 60));
    day1_24h.bind_identity(day1_24h.database_identity());
    let day1_24h_path = write("day1-24h.json", serde_json::to_vec(&day1_24h).unwrap());
    let day1_24h_digest = ws.digest(None, Some(&day1_24h_path), None, 10, 5).unwrap();
    let day1_24h_digest_path = write("day1-24h-digest.json", serde_json::to_vec(&day1_24h_digest).unwrap());

    let mut day2_24h = day2_snap.clone();
    day2_24h.taken_at = "2026-10-23T06:05:00Z".into();
    day2_24h.options = Some(SnapshotOptions::default().with_window_minutes(24 * 60));
    day2_24h.workload.as_mut().unwrap().window_hours = 24.0;
    day2_24h.workload.as_mut().unwrap().window_start = Some("2026-10-22T06:05:00Z".into());
    day2_24h.workload.as_mut().unwrap().window_end = Some("2026-10-23T06:05:00Z".into());
    day2_24h.bind_identity(day2_24h.database_identity());
    let day2_24h_path = write("day2-24h.json", serde_json::to_vec(&day2_24h).unwrap());
    let day2_24h_digest = ws.digest(None, Some(&day2_24h_path), Some(&day1_24h_digest_path), 10, 5).unwrap();
    assert!(
        day2_24h_digest.top.iter().any(|f| f.id == ID),
        "24h digest with 5m cron jitter must keep new escalations in top"
    );

    // Pre-0.4.0 legacy digest (identity: None, measurement: None) must also compare cleanly.
    let mut legacy = day1_digest;
    legacy.identity = None;
    legacy.measurement = None;
    let legacy_path = write("legacy-digest.json", serde_json::to_vec(&legacy).unwrap());
    let from_legacy = ws.digest(None, Some(&day2_snap_path), Some(&legacy_path), 10, 5).unwrap();
    assert!(from_legacy.since.is_some(), "legacy digest without identity/measurement must still compare");
    assert!(from_legacy.top.iter().any(|f| f.id == ID && f.title.contains("4 times since Oct 22, 06:00 UTC")));

    let _ = std::fs::remove_dir_all(home);
}
