//! Desktop polling uses the same live projection as the CLI, without reading another window's report.
use async_trait::async_trait;
use culprit_core::{
    AuthKind, Dashboard, Engine, Error, ErrorCode, ExplainMode, LiveState, Plan, Provider, Report, Result, ResultSet,
    ServerInfo, Snapshot, SnapshotOptions, TileKey, TileStatus,
};
use culprit_engine::{NewConnection, Workspace};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

#[derive(Clone)]
struct Reader {
    reads: Arc<AtomicUsize>,
    response: Result<LiveState>,
}

#[async_trait]
impl Provider for Reader {
    fn engine(&self) -> Engine {
        Engine::Postgres
    }
    async fn probe(&self) -> Result<ServerInfo> {
        Err(Error::invalid("a live refresh needs no extra probe"))
    }
    async fn snapshot(&self, _: &str, _: &SnapshotOptions) -> Result<Snapshot> {
        Err(Error::invalid("a live refresh must not scan"))
    }
    async fn explain(&self, _: &str, _: ExplainMode) -> Result<Plan> {
        Err(Error::invalid("not needed"))
    }
    async fn live(&self) -> Result<LiveState> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        self.response.clone()
    }
    async fn run(&self, _: &str, _: usize) -> Result<ResultSet> {
        Err(Error::invalid("not needed"))
    }
}

fn recorded() -> LiveState {
    let mut all: std::collections::BTreeMap<Engine, LiveState> =
        serde_json::from_str(include_str!("../../../fixtures/live.sample.json")).unwrap();
    all.remove(&Engine::Postgres).unwrap()
}

fn dashboard() -> Dashboard {
    let report: Report = serde_json::from_str(include_str!("../../../fixtures/pg-demo.report.json")).unwrap();
    let mut dashboard = culprit_checks::dashboard(&report);
    dashboard.connection = "replayed".into();
    dashboard.report_id = "chosen-report".into();
    dashboard.coverage = Some("The selected report's coverage".into());
    dashboard
}

struct Fixture {
    ws: Workspace,
    home: std::path::PathBuf,
    reads: Arc<AtomicUsize>,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.home);
    }
}

fn fixture(tag: &str, response: Result<LiveState>) -> Fixture {
    let home = std::env::temp_dir().join(format!("culprit-dashboard-{tag}-{}", std::process::id()));
    let reads = Arc::new(AtomicUsize::new(0));
    let reader = Reader { reads: reads.clone(), response };
    let ws = Workspace::open_in(&home).with_provider(Engine::Postgres, Arc::new(move |_| Ok(Box::new(reader.clone()))));
    ws.add_connection(NewConnection {
        name: "replayed".into(),
        input: "postgres://reader@replay.invalid/demo".into(),
        auth: Some(AuthKind::None),
        ..Default::default()
    })
    .unwrap();
    Fixture { ws, home, reads }
}

#[tokio::test]
async fn a_live_refresh_preserves_the_selected_report_and_ranks_its_new_session_tile() {
    let mut live = recorded();
    for session in live.sessions.iter_mut().filter(|s| s.blocked_by.is_some()) {
        session.wait_ms = Some(31_000.0);
    }
    live.blocking = culprit_core::blocking::analyze(Engine::Postgres, &live.sessions, &[]);
    let f = fixture("success", Ok(live));
    let before = dashboard();
    let result = f.ws.dashboard_live("replayed", before.clone()).await.unwrap();
    assert_eq!(f.reads.load(Ordering::SeqCst), 1);
    assert!(result.error.is_none());
    assert!(result.live.is_some());
    assert_eq!(result.dashboard.report_id, before.report_id);
    assert_eq!(result.dashboard.coverage, before.coverage);
    for tile in before.tiles.iter().filter(|t| t.key != TileKey::Now) {
        assert_eq!(result.dashboard.tile(tile.key), Some(tile));
    }
    assert_eq!(result.dashboard.attention.first(), Some(&TileKey::Now));
    assert_eq!(result.dashboard.tile(TileKey::Now).unwrap().status, TileStatus::Bad);
    assert_eq!(result.dashboard.headline, result.dashboard.tile(TileKey::Now).unwrap().takeaway);
}

#[tokio::test]
async fn a_failed_live_read_withholds_the_measurements_and_keeps_the_scan() {
    let f = fixture("error", Err(Error::new(ErrorCode::PermissionDenied, "Session visibility was denied")));
    let before = dashboard();
    let result = f.ws.dashboard_live("replayed", before.clone()).await.unwrap();
    assert_eq!(f.reads.load(Ordering::SeqCst), 1);
    assert_eq!(result.error.unwrap().code, ErrorCode::PermissionDenied);
    assert!(result.live.is_none());
    let tile = result.dashboard.tile(TileKey::Now).unwrap();
    assert_eq!(tile.status, TileStatus::Unavailable);
    assert!(tile.metrics.is_empty());
    assert!(tile.takeaway.to_lowercase().contains("session visibility was denied"));
    assert_eq!(result.dashboard.report_id, before.report_id);
}

#[tokio::test]
async fn another_connections_dashboard_is_rejected_before_reading() {
    let f = fixture("scope", Ok(recorded()));
    let mut board = dashboard();
    board.connection = "another".into();
    assert!(f.ws.dashboard_live("replayed", board).await.is_err());
    assert_eq!(f.reads.load(Ordering::SeqCst), 0);
}

#[test]
fn incomplete_lock_visibility_cannot_claim_zero_blocked_or_healthy_sessions() {
    let mut live = recorded();
    live.sessions.clear();
    live.blocking = None;
    live.notes = vec!["Lock instrumentation unavailable".into()];
    let result = culprit_checks::dashboard::refresh(dashboard(), Ok(live));
    let now = result.dashboard.tile(TileKey::Now).unwrap();
    assert_eq!(now.status, TileStatus::Info);
    assert!(now.takeaway.contains("incomplete"));
    assert_eq!(now.metric("blocked").unwrap().value, None);
}

#[tokio::test]
async fn a_dashboard_is_pinned_to_the_displayed_report_when_another_window_finishes() {
    let f = fixture("selected-report", Err(Error::invalid("no live read needed")));
    f.ws.add_connection(NewConnection {
        name: "windowed".into(),
        input: "mssql://reader@replay.invalid/culprit_demo".into(),
        auth: Some(AuthKind::None),
        ..Default::default()
    })
    .unwrap();
    let mut selected: Report = serde_json::from_str(include_str!("../../../fixtures/mssql-demo.report.json")).unwrap();
    selected.snapshot.connection = "windowed".into();
    selected.snapshot.id = "20261010T000000Z".into();
    let mut newer = selected.clone();
    newer.snapshot.id = "20261011T000000Z".into();
    newer.snapshot.options = Some(SnapshotOptions::default().with_window_minutes(60));
    let w = newer.snapshot.workload.as_mut().unwrap();
    w.window_hours = 1.0;
    let end = culprit_core::format::epoch_seconds(w.window_end.as_deref().unwrap()).unwrap();
    w.window_start = Some(culprit_core::format::iso_from_epoch(end - 3600.0));
    let directory = f.ws.paths().reports_dir("windowed");
    culprit_engine::reports::save(&directory, &selected).unwrap();
    culprit_engine::reports::save(&directory, &newer).unwrap();
    let latest = f.ws.dashboard("windowed", None, false).await.unwrap();
    let pinned = f.ws.dashboard_for_report("windowed", &selected.snapshot.id).unwrap();
    assert_eq!(latest.report_id, newer.snapshot.id);
    assert_eq!(pinned.report_id, selected.snapshot.id);
    assert_ne!(latest.coverage, pinned.coverage);
    assert_eq!(f.reads.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn renaming_a_connection_keeps_its_saved_dashboard_refreshable() {
    let f = fixture("renamed", Ok(recorded()));
    let mut report: Report = serde_json::from_str(include_str!("../../../fixtures/pg-demo.report.json")).unwrap();
    report.snapshot.connection = "replayed".into();
    culprit_engine::reports::save(&f.ws.paths().reports_dir("replayed"), &report).unwrap();
    f.ws.rename_connection("replayed", "renamed").unwrap();
    let pinned = f.ws.dashboard_for_report("renamed", &report.snapshot.id).unwrap();
    assert_eq!(pinned.connection, "renamed");
    assert_eq!(f.ws.dashboard("renamed", None, false).await.unwrap().connection, "renamed");
    let refreshed = f.ws.dashboard_live("renamed", pinned).await.unwrap();
    assert!(refreshed.live.is_some());
    assert!(refreshed.error.is_none());
}

#[test]
fn browser_live_views_are_the_shared_backend_assessment_for_each_recorded_state() {
    use serde_json::Value;
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures");
    let samples: std::collections::BTreeMap<String, std::collections::BTreeMap<String, Value>> =
        serde_json::from_slice(&std::fs::read(root.join("dashboard-live.sample.json")).unwrap()).unwrap();
    for (name, cases) in samples {
        let report = culprit_engine::reading::open(&root.join(format!("{name}.report.json"))).unwrap();
        for (key, sample) in cases {
            let result = if key == "unavailable" {
                Err(Error::new(ErrorCode::PermissionDenied, "Session visibility was denied"))
            } else {
                Ok(serde_json::from_value::<LiveState>(sample["live"].clone()).unwrap())
            };
            let current = culprit_checks::dashboard::refresh(culprit_checks::dashboard(&report), result);
            crate::invariants::assert_browser_json(
                &sample,
                &serde_json::to_value(current).unwrap(),
                &format!("{name}/{key}: regenerate browser fixtures"),
            );
        }
    }
}
