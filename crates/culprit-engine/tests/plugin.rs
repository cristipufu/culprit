//! An application embedding the engine can plug in its own checks and its own provider; scans, `checks` and offline analysis
//! all go through them.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use culprit_core::{
    AuthKind, Capability, Category, Check, CheckInfo, Engine, Environment, Error, ErrorCode, ExplainMode, Finding,
    LiveState, Plan, Provider, Result, ResultSet, ServerInfo, Snapshot, SnapshotOptions,
};
use culprit_engine::saved::SaveQuery;
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
        Ok(ResultSet {
            columns: vec![],
            rows: vec![],
            truncated: false,
            elapsed_ms: 0.0,
            messages: vec!["read".into()],
        })
    }

    async fn execute(&self, _: &str, _: usize) -> Result<ResultSet> {
        Ok(ResultSet {
            columns: vec![],
            rows: vec![],
            truncated: false,
            elapsed_ms: 0.0,
            messages: vec!["wrote".into()],
        })
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

#[tokio::test]
async fn reads_run_freely_and_production_writes_need_confirmation() {
    let home = temp_home("run");
    let ws = Workspace::open_in(&home)
        .with_provider(Engine::Postgres, Arc::new(|_| Ok(Box::new(Replay) as Box<dyn Provider>)));
    for (name, environment) in [("prod", Environment::Production), ("dev", Environment::Development)] {
        ws.add_connection(NewConnection {
            name: name.into(),
            input: "postgres://reader@replay.invalid:5432/demo".into(),
            auth: Some(AuthKind::None),
            environment,
            ..Default::default()
        })
        .expect("save connection");
    }
    let path = |r: ResultSet| r.messages[0].clone();
    assert_eq!(path(ws.run("prod", "SELECT * FROM orders", 10, false).await.unwrap()), "read");
    let refused = ws.run("prod", "DELETE FROM orders", 10, false).await.unwrap_err();
    assert_eq!(refused.code, ErrorCode::ConfirmationRequired);
    assert!(refused.hint.unwrap().contains("every row"));
    assert_eq!(path(ws.run("prod", "DELETE FROM orders WHERE id = 1", 10, true).await.unwrap()), "wrote");
    assert_eq!(path(ws.run("dev", "UPDATE orders SET note = '' WHERE id = 1", 10, false).await.unwrap()), "wrote");

    ws.save_query(SaveQuery {
        name: "Purge one".into(),
        sql: "DELETE FROM orders WHERE id = 1".into(),
        connection: Some("prod".into()),
        ..Default::default()
    })
    .unwrap();
    let saved = ws.run_saved("purge ONE", None, 10, false).await.unwrap_err();
    assert_eq!(saved.code, ErrorCode::ConfirmationRequired, "saved queries follow the same rule");
    assert_eq!(path(ws.run_saved("Purge one", Some("dev"), 10, false).await.unwrap()), "wrote");
    assert!(ws.saved_query("Purge one").unwrap().last_run_at.is_some());
    let _ = std::fs::remove_dir_all(home);
}
