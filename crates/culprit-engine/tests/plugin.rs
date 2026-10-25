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

#[cfg(feature = "yugabyte")]
#[tokio::test]
async fn retained_native_plans_open_offline_without_borrowing_current_catalog_or_measurements() {
    let home = temp_home("native-cache");
    let ws = Workspace::open_in(&home).with_provider(
        Engine::Yugabyte,
        Arc::new(|_| Err(Error::internal("Opening this saved plan must not resolve a database provider"))),
    );
    ws.add_connection(NewConnection {
        name: "native-cache".into(),
        input: "yugabyte://reader@unavailable.invalid:5433/demo".into(),
        auth: Some(AuthKind::None),
        ..Default::default()
    })
    .unwrap();
    let mut snapshot: Snapshot = serde_json::from_str(include_str!("../../../fixtures/yugabyte-demo.json")).unwrap();
    snapshot.connection = "native-cache".into();
    let q = &mut snapshot.workload.as_mut().unwrap().queries[0];
    let mut cached = culprit_yugabyte::plan::parse(
        include_str!("../../culprit-yugabyte/tests/data/qpm.json"),
        culprit_core::PlanKind::Estimated,
        None,
    )
    .unwrap();
    cached.facts_mut().constants_redacted = true;
    q.plan_history = Some(culprit_core::PlanHistory {
        source: "yb_pg_stat_plans".into(),
        kind: culprit_core::PlanHistoryKind::Cache,
        note: "Retained cache periods are independent of statement statistics".into(),
        captured_plan_id: Some("-123".into()),
        captured_plan: Some(cached),
    });
    let query = q.id.clone();
    let report = culprit_checks::analyze(snapshot);
    culprit_engine::reports::save(&ws.paths().reports_dir("native-cache"), &report).unwrap();
    let plan = ws.query_plan_at("native-cache", &query, Some("-123"), Some(&report.snapshot.id)).await.unwrap();
    assert!(plan.facts().constants_redacted && plan.facts().estimated_cost_unavailable);
    assert!(plan.nodes().iter().all(|n| n.table_rows.is_none()));
    let view = plan.view.as_ref().unwrap();
    assert!(view.query.is_none());
    assert!(view.nodes.iter().all(|n| n.runs.is_none()
        && n.estimated_rows.is_none()
        && n.estimated_rows_for_runs.is_none()
        && n.rows_out.is_none()));
    assert!(plan.insights.as_ref().unwrap().items.iter().all(|i| i.fix_sql.is_none()));
    let _ = std::fs::remove_dir_all(home);
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

    async fn run(&self, _: &str, limit: usize) -> Result<ResultSet> {
        assert!(limit <= culprit_core::limits::MAX_RESULT_ROWS);
        Ok(ResultSet {
            columns: vec![],
            rows: vec![],
            truncated: false,
            elapsed_ms: 0.0,
            messages: vec!["read".into()],
        })
    }

    async fn preview(&self, sql: &str, limit: usize) -> Result<ResultSet> {
        self.run(sql, limit).await
    }

    async fn execute(&self, _: &str, limit: usize) -> Result<ResultSet> {
        assert!(limit <= culprit_core::limits::MAX_RESULT_ROWS);
        Ok(ResultSet {
            columns: vec![],
            rows: vec![],
            truncated: false,
            elapsed_ms: 0.0,
            messages: vec!["wrote".into()],
        })
    }
}

/// [`Replay`], explaining each recorded statement the way an engine would explain it now: its recorded plan, without the
/// marks a report's preparation puts on the plans of a scan from its catalog.
struct Explains;

#[async_trait]
impl Provider for Explains {
    fn engine(&self) -> Engine {
        Engine::Postgres
    }

    async fn probe(&self) -> Result<ServerInfo> {
        Replay.probe().await
    }

    async fn snapshot(&self, connection: &str, options: &SnapshotOptions) -> Result<Snapshot> {
        Replay.snapshot(connection, options).await
    }

    async fn explain(&self, sql: &str, _: ExplainMode) -> Result<Plan> {
        let queries = recorded().workload.map(|w| w.queries).unwrap_or_default();
        queries
            .into_iter()
            .find(|q| q.text == sql)
            .and_then(|q| q.plan)
            .ok_or_else(|| Error::invalid("no plan recorded"))
    }

    async fn live(&self) -> Result<LiveState> {
        Replay.live().await
    }

    async fn run(&self, sql: &str, limit: usize) -> Result<ResultSet> {
        Replay.run(sql, limit).await
    }

    async fn execute(&self, sql: &str, limit: usize) -> Result<ResultSet> {
        Replay.execute(sql, limit).await
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
async fn testing_an_unsaved_connection_uses_its_provider_without_persisting_it() {
    let home = temp_home("test-connection");
    let ws = Workspace::open_in(&home)
        .with_provider(Engine::Postgres, Arc::new(|_| Ok(Box::new(Replay) as Box<dyn Provider>)));
    let new = NewConnection {
        name: "unsaved".into(),
        input: "postgres://reader@replay.invalid:5432/demo".into(),
        auth: Some(AuthKind::None),
        ..Default::default()
    };
    assert_eq!(ws.test_connection(&new).await.expect("test unsaved connection").engine, Engine::Postgres);
    assert!(ws.connections().unwrap().is_empty());
    assert_eq!(ws.probe("unsaved").await.unwrap_err().code, ErrorCode::NotFound);
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

// A plan fetched apart from the scan (a stored plan, or one explained when it is opened) reads the way the report's own
// plans do: marked with the latest scan's catalog, such as the indexes on a table it scans.
#[tokio::test]
async fn a_plan_opened_from_a_report_reads_with_the_reports_catalog() {
    let home = temp_home("plan");
    let ws = Workspace::open_in(&home)
        .with_provider(Engine::Postgres, Arc::new(|_| Ok(Box::new(Explains) as Box<dyn Provider>)));
    ws.add_connection(NewConnection {
        name: "replayed".into(),
        input: "postgres://reader@replay.invalid:5432/demo".into(),
        auth: Some(AuthKind::None),
        ..Default::default()
    })
    .expect("save connection");
    let report = ws.scan("replayed", &SnapshotOptions::default()).await.expect("scan");
    let marked = |plan: &Plan| plan.nodes().iter().any(|n| n.props.contains_key(culprit_core::insights::TABLE_INDEXES));
    let q = report
        .snapshot
        .workload
        .as_ref()
        .and_then(|w| w.queries.iter().find(|q| q.plan.as_ref().is_some_and(marked)))
        .expect("a scan the report's catalog marks");
    let explained = Explains.explain(&q.text, ExplainMode::Estimated).await.expect("explained");
    assert!(!marked(&explained), "the engine's plan has no catalog marks of its own");
    let plan = ws.query_plan("replayed", &q.id, None).await.expect("plan");
    assert!(marked(&plan), "{:?}", plan.nodes().iter().map(|n| &n.props).collect::<Vec<_>>());
    let _ = std::fs::remove_dir_all(home);
}

#[tokio::test]
async fn advice_and_fetched_plans_follow_the_selected_report_not_the_latest_pointer() {
    let home = temp_home("selected-report");
    let ws = Workspace::open_in(&home)
        .with_provider(Engine::Postgres, Arc::new(|_| Ok(Box::new(Explains) as Box<dyn Provider>)));
    ws.add_connection(NewConnection {
        name: "replayed".into(),
        input: "postgres://reader@replay.invalid:5432/demo".into(),
        auth: Some(AuthKind::None),
        ..Default::default()
    })
    .unwrap();
    let mut snapshot = recorded();
    snapshot.id = "20260828T120000.000Z".into();
    snapshot.connection = "replayed".into();
    let selected = culprit_checks::analyze(snapshot.clone());
    let q = selected.snapshot.queries().iter().find(|q| q.plan.is_some() && q.executions > 1.0).unwrap();
    let dir = ws.paths().reports_dir("replayed");
    culprit_engine::reports::save(&dir, &selected).unwrap();
    snapshot.id = "20260828T130000.000Z".into();
    snapshot.workload = None;
    let latest = culprit_checks::analyze(snapshot);
    culprit_engine::reports::save(&dir, &latest).unwrap();
    let actual = ws.index_advice_at("replayed", &selected.snapshot.id).unwrap();
    let expected = ws.index_advice_for(selected.clone());
    assert_eq!(serde_json::to_value(actual).unwrap(), serde_json::to_value(expected).unwrap());
    assert_ne!(
        serde_json::to_value(ws.index_advice("replayed").unwrap()).unwrap(),
        serde_json::to_value(ws.index_advice_at("replayed", &selected.snapshot.id).unwrap()).unwrap()
    );
    let plan = ws.query_plan_at("replayed", &q.id, None, Some(&selected.snapshot.id)).await.unwrap();
    assert!(plan.insights.is_some());
    assert!(ws.query_plan("replayed", &q.id, None).await.is_err(), "latest has no such query");
    assert!(
        ws.query_plan_at("replayed", &q.id, Some("999999"), Some(&selected.snapshot.id)).await.is_err(),
        "a missing requested variant must not be silently replaced"
    );
    for id in ["missing", "../latest", "../../other/20260828T120000.000Z"] {
        assert!(ws.index_advice_at("replayed", id).is_err(), "{id}");
    }
    assert!(ws.index_advice_at("another-connection", &selected.snapshot.id).is_err());
    std::fs::remove_dir_all(home).unwrap();
}

#[tokio::test]
async fn reads_run_freely_and_production_writes_need_confirmation() {
    assert_eq!(
        Explains.preview("SELECT 1", 1).await.unwrap_err().code,
        ErrorCode::Unsupported,
        "An external provider must opt into short-budget previews, never fall back to long-running execution"
    );
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
    let mut preview = culprit_core::TablePreviewOptions {
        schema: "public".into(),
        table: "orders".into(),
        filter: Some("id > 1".into()),
        order: Some(culprit_core::ColumnSort { column: "id".into(), direction: culprit_core::SortDirection::Desc }),
        limit: 100,
    };
    for name in ["prod", "dev"] {
        let result = ws.preview_table(name, &preview).await.unwrap();
        assert_eq!(path(result.result), "read", "Table previews always use the read-only provider path");
        assert!(result.sql.ends_with("ORDER BY \"id\" DESC LIMIT 100"));
    }
    preview.filter = Some("1=1); DELETE FROM orders; --".into());
    assert!(ws.preview_table("dev", &preview).await.is_err());
    preview.filter = Some("pg_advisory_lock(1) IS NULL".into());
    assert_eq!(ws.preview_table("dev", &preview).await.unwrap_err().code, ErrorCode::PermissionDenied);
    assert_eq!(path(ws.run("prod", "SELECT * FROM orders", 10, false).await.unwrap()), "read");
    assert_eq!(path(ws.run("prod", "SELECT * FROM orders", usize::MAX, false).await.unwrap()), "read");
    assert_eq!(path(ws.run("dev", "DELETE FROM orders WHERE id = 1", usize::MAX, false).await.unwrap()), "wrote");
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
