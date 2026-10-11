//! An application embedding the engine can plug in its own checks and its own provider; scans, `checks` and offline analysis
//! all go through them.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use culprit_core::{
    AuthKind, Capability, Category, Check, CheckInfo, Engine, Error, ExplainMode, Finding, LiveState, Plan, Provider,
    Result, ResultSet, ServerInfo, Snapshot, SnapshotOptions,
};
use culprit_engine::{NewConnection, Workspace};

fn fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/postgres-demo.json")
}

fn temp_home(tag: &str) -> PathBuf {
    std::env::temp_dir().join(format!("culprit-plugin-{tag}-{}", std::process::id()))
}

struct Marker;

impl Check for Marker {
    fn info(&self) -> CheckInfo {
        CheckInfo {
            id: "acme.marker".into(),
            title: "Marker".into(),
            category: Category::Configuration,
            engines: vec![Engine::Postgres],
            requires: vec![Capability::TableStats],
            description: "Proves the plug-in ran.".into(),
        }
    }

    fn run(&self, snapshot: &Snapshot) -> Vec<Finding> {
        let report = culprit_checks::analyze(snapshot.clone());
        let mut f = report.findings.into_iter().next().expect("the demo has findings");
        f.id = "acme.marker:demo".into();
        f.check = "acme.marker".into();
        vec![f]
    }
}

/// Stands in for any engine implementation an embedder brings: here it replays a recorded snapshot.
struct Replay;

#[async_trait]
impl Provider for Replay {
    fn engine(&self) -> Engine {
        Engine::Postgres
    }

    async fn probe(&self) -> Result<ServerInfo> {
        Ok(recorded().server)
    }

    async fn snapshot(&self, _connection: &str, _options: &SnapshotOptions) -> Result<Snapshot> {
        Ok(recorded())
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
}

fn recorded() -> Snapshot {
    serde_json::from_slice(&std::fs::read(fixture()).expect("fixture")).expect("snapshot")
}

#[test]
fn plugged_in_checks_reach_listing_and_offline_analysis() {
    let home = temp_home("checks");
    let ws = Workspace::open_in(&home).with_check(Arc::new(Marker));
    assert!(ws.checks().iter().any(|c| c.id == "acme.marker"));
    let report = ws.analyze_file(&fixture()).expect("analyze");
    assert!(report.findings.iter().any(|f| f.id == "acme.marker:demo"));
    let _ = std::fs::remove_dir_all(home);
}

#[tokio::test]
async fn a_plugged_in_provider_serves_a_full_scan() {
    let home = temp_home("provider");
    let ws = Workspace::open_in(&home)
        .with_provider(Engine::Postgres, Arc::new(|_| Ok(Box::new(Replay) as Box<dyn Provider>)))
        .with_check(Arc::new(Marker));
    ws.add_connection(NewConnection {
        name: "replayed".into(),
        input: "postgres://reader@replay.invalid:5432/demo".into(),
        auth: Some(AuthKind::None),
        ..Default::default()
    })
    .expect("save connection");

    assert_eq!(ws.probe("replayed").await.expect("probe").engine, Engine::Postgres);
    let report = ws.scan("replayed", &SnapshotOptions::default()).await.expect("scan through the plugged-in provider");
    assert_eq!(report.snapshot.connection, "replayed");
    assert!(report.findings.iter().any(|f| f.check == "filtered-scan"), "built-in checks ran on the replayed snapshot");
    assert!(report.findings.iter().any(|f| f.id == "acme.marker:demo"), "plugged-in checks ran too");
    let saved = ws.latest_report("replayed", None).expect("read back").expect("report was stored");
    assert_eq!(saved.snapshot.id, report.snapshot.id);
    let _ = std::fs::remove_dir_all(home);
}
