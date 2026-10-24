use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use culprit_core::{
    AuthKind, Engine, Environment, Error, ErrorCode, ExplainMode, LiveState, Plan, Provider, Result, ResultSet,
    ServerInfo, Snapshot, SnapshotOptions,
};
use culprit_engine::{
    EventsRequest, NewConnection, Workspace,
    secrets::{Memory, SecretStore},
};

fn home(tag: &str) -> PathBuf {
    std::env::temp_dir().join(format!("culprit-services-{tag}-{}", std::process::id()))
}

#[cfg(all(feature = "postgres", feature = "yugabyte"))]
#[tokio::test]
#[ignore = "requires dev/yugabyte and CULPRIT_YSQL_TEST_PORT"]
async fn live_native_workspace_session_progress_and_cancel() {
    use culprit_core::{ConnectionSpec, SessionAction};
    use culprit_postgres::wire::{Connector, INTERACTIVE, query_one, tagged};
    let dir = home("native-session-routing");
    let port: u16 = std::env::var("CULPRIT_YSQL_TEST_PORT").expect("set CULPRIT_YSQL_TEST_PORT").parse().unwrap();
    let mut spec = ConnectionSpec {
        engine: Some(Engine::Yugabyte),
        host: "127.0.0.1".into(),
        port: Some(port),
        user: Some("yugabyte".into()),
        database: Some("culprit_yugabyte".into()),
        environment: Environment::Local,
        ..Default::default()
    };
    spec.options.insert("sslmode".into(), "disable".into());
    let connector = Connector::new(&spec).unwrap();
    let client = connector.connect(INTERACTIVE).await.unwrap();
    client.batch_execute(&tagged("SET application_name='culprit_native_workspace_test'")).await.unwrap();
    let row = query_one(
        &client,
        "SELECT pg_backend_pid()::text AS pid, yb_get_local_tserver_uuid()::text AS node,
        to_char(backend_start AT TIME ZONE 'UTC','YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"') AS started
        FROM pg_stat_activity WHERE pid=pg_backend_pid()",
    )
    .await
    .unwrap()
    .unwrap();
    let id = format!("{}/{}/{}", row.string("node"), row.string("pid"), row.string("started"));
    let ws = Workspace::open_in(&dir).with_secrets(Arc::new(Memory::default()));
    ws.add_connection(NewConnection {
        name: "native-session-test".into(),
        input: format!("yugabyte://yugabyte@127.0.0.1:{port}/culprit_yugabyte?sslmode=disable"),
        environment: Environment::Local,
        auth: Some(AuthKind::None),
        ..Default::default()
    })
    .unwrap();
    let sleeping = tokio::spawn(async move { client.batch_execute(&tagged("SELECT pg_sleep(10)")).await });
    let observed = async {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(3);
        let progress = loop {
            let progress = ws.query_progress("native-session-test", &id, false).await?;
            if progress.running || tokio::time::Instant::now() >= deadline {
                break progress;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        };
        ws.interrupt_session("native-session-test", &id, SessionAction::Cancel).await?;
        Ok::<_, Error>(progress)
    }
    .await;
    // Clean up this test's verified backend even when a Workspace regression rejects its native id.
    let direct = culprit_yugabyte::YugabyteProvider::new(&spec).unwrap();
    let _ = direct.interrupt_session(&id, SessionAction::Cancel).await;
    let stopped = sleeping.await.unwrap();
    drop(ws);
    let _ = std::fs::remove_dir_all(dir);
    let progress = observed.unwrap();
    assert_eq!(progress.session, id);
    assert_eq!(progress.engine, Engine::Yugabyte);
    assert!(progress.running && progress.statement.as_deref().is_some_and(|s| s.contains("pg_sleep")));
    assert!(
        stopped.is_err_and(|e| e.code().is_some_and(|code| code.code() == "57014")),
        "only the verified test backend should be canceled"
    );
}

#[derive(Clone)]
struct ParserProvider {
    types: Vec<String>,
    reject_read: bool,
    calls: Arc<Mutex<Vec<(String, usize)>>>,
}

#[async_trait]
impl Provider for ParserProvider {
    fn engine(&self) -> Engine {
        Engine::Mssql
    }
    async fn probe(&self) -> Result<ServerInfo> {
        Err(Error::invalid("not used by these operations"))
    }
    async fn snapshot(&self, _: &str, _: &SnapshotOptions) -> Result<Snapshot> {
        Err(Error::invalid("not used"))
    }
    async fn live(&self) -> Result<LiveState> {
        Err(Error::invalid("not used"))
    }
    async fn explain(&self, sql: &str, _: ExplainMode) -> Result<Plan> {
        self.calls.lock().unwrap().push((format!("explain: {sql}"), 0));
        Err(Error::invalid("a refused measurement must never reach this method"))
    }
    async fn statement_types(&self, _: &str) -> Result<Option<Vec<String>>> {
        Ok(Some(self.types.clone()))
    }
    fn session_actions(&self) -> Vec<culprit_core::SessionAction> {
        vec![culprit_core::SessionAction::Terminate]
    }
    async fn interrupt_session(&self, _: &str, _: culprit_core::SessionAction) -> Result<()> {
        Err(Error::new(ErrorCode::AuthFailed, "test authentication failure"))
    }
    async fn period_stats(
        &self,
        baseline: &culprit_core::Period,
        recent: &culprit_core::Period,
    ) -> Result<Option<culprit_core::changes::PeriodData>> {
        Ok(Some(culprit_core::changes::PeriodData {
            source: culprit_core::TimelineSource::QueryStore,
            baseline: baseline.clone(),
            recent: recent.clone(),
            baseline_total: Default::default(),
            recent_total: Default::default(),
            baseline_coverage: 1.0,
            queries: Vec::new(),
            notes: Vec::new(),
            unavailable: None,
        }))
    }
    async fn recorded_changes(&self, _: &str) -> Result<Vec<culprit_core::ChangeEvent>> {
        Err(Error::new(ErrorCode::AuthFailed, "test authentication failure"))
    }
    async fn run(&self, sql: &str, limit: usize) -> Result<ResultSet> {
        self.calls.lock().unwrap().push((sql.into(), limit));
        if self.reject_read {
            return Err(Error::new(ErrorCode::AuthFailed, "test authentication failure"));
        }
        Ok(ResultSet { columns: Vec::new(), rows: Vec::new(), truncated: false, elapsed_ms: 0.0, messages: Vec::new() })
    }
}

fn workspace(tag: &str, types: &[&str], reject_read: bool) -> (Workspace, ParserProvider, PathBuf) {
    let dir = home(tag);
    let provider = ParserProvider {
        types: types.iter().map(|s| (*s).into()).collect(),
        reject_read,
        calls: Arc::new(Mutex::new(Vec::new())),
    };
    let replay = provider.clone();
    let ws = Workspace::open_in(&dir)
        .with_provider(Engine::Mssql, Arc::new(move |_| Ok(Box::new(replay.clone()) as Box<dyn Provider>)));
    (ws, provider, dir)
}

fn add(ws: &Workspace, name: &str, password: Option<&str>) {
    ws.add_connection(NewConnection {
        name: name.into(),
        input: "mssql://reader@service.invalid:1433/shop".into(),
        environment: Environment::Production,
        auth: Some(if password.is_some() { AuthKind::Password } else { AuthKind::None }),
        password: password.map(str::to_string),
        ..Default::default()
    })
    .unwrap();
}

#[tokio::test]
async fn server_parser_refusal_never_reaches_a_read_or_measured_execution() {
    for (tag, types, explain_error) in [
        ("several-statements", vec!["SELECT", "UPDATE"], ErrorCode::InvalidInput),
        ("write-shaped-read", vec!["SELECT INTO"], ErrorCode::PermissionDenied),
    ] {
        let (ws, provider, dir) = workspace(tag, &types, false);
        add(&ws, "shop", None);
        assert_eq!(
            ws.run_read("shop", "SELECT id FROM orders", 10).await.unwrap_err().code,
            ErrorCode::PermissionDenied
        );
        assert_eq!(
            ws.explain("shop", "SELECT id FROM orders", ExplainMode::Analyze, true).await.unwrap_err().code,
            explain_error
        );
        assert!(provider.calls.lock().unwrap().is_empty());
        std::fs::remove_dir_all(dir).unwrap();
    }
}

#[tokio::test]
async fn a_confirmed_server_read_on_production_keeps_the_rollback_boundary_and_limit() {
    let (ws, provider, dir) = workspace("read-transaction", &["SELECT"], false);
    add(&ws, "shop", None);
    ws.run("shop", "SELECT id FROM orders", 17, false).await.unwrap();
    let calls = provider.calls.lock().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].1, 17);
    assert!(calls[0].0.contains("BEGIN TRANSACTION"));
    assert!(calls[0].0.contains("SELECT id FROM orders"));
    assert!(calls[0].0.contains("ROLLBACK TRANSACTION"));
    drop(calls);
    std::fs::remove_dir_all(dir).unwrap();
}

#[derive(Default)]
struct ObservedSecrets {
    values: Memory,
    forgotten: Mutex<Vec<String>>,
}

impl SecretStore for ObservedSecrets {
    fn get(&self, name: &str) -> Result<Option<String>> {
        self.values.get(name)
    }
    fn set(&self, name: &str, value: &str) -> Result<()> {
        self.values.set(name, value)
    }
    fn delete(&self, name: &str) -> Result<()> {
        self.values.delete(name)
    }
    fn forget(&self, name: &str) {
        self.forgotten.lock().unwrap().push(name.into());
    }
}

#[tokio::test]
async fn an_execution_authentication_failure_forgets_only_that_connections_cached_secret() {
    let (ws, _, dir) = workspace("rejected-secret", &["SELECT"], true);
    let secrets = Arc::new(ObservedSecrets::default());
    let ws = ws.with_secrets(secrets.clone());
    add(&ws, "first", Some("first-test-secret"));
    add(&ws, "second", Some("second-test-secret"));
    assert_eq!(ws.run_read("first", "SELECT 1", 1).await.unwrap_err().code, ErrorCode::AuthFailed);
    assert_eq!(*secrets.forgotten.lock().unwrap(), vec!["first"]);
    // Invalidating an in-process cache must not delete either backing credential.
    assert!(secrets.get("first").unwrap().is_some());
    assert!(secrets.get("second").unwrap().is_some());
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn session_and_optional_event_authentication_failures_invalidate_only_the_rejected_cache() {
    for events in [false, true] {
        let (ws, _, dir) = workspace(if events { "event-secret" } else { "session-secret" }, &["SELECT"], false);
        let secrets = Arc::new(ObservedSecrets::default());
        let ws = ws.with_secrets(secrets.clone());
        ws.add_connection(NewConnection {
            name: "first".into(),
            input: "mssql://reader@service.invalid:1433/shop".into(),
            environment: Environment::Development,
            auth: Some(AuthKind::Password),
            password: Some("first-test-secret".into()),
            ..Default::default()
        })
        .unwrap();
        add(&ws, "second", Some("second-test-secret"));
        if events {
            let out = ws
                .changes("first", &culprit_engine::changes::ChangeRequest { cloud: Some(false), ..Default::default() })
                .await
                .unwrap();
            assert!(out.notes.iter().any(|n| n.contains("Database change events were unavailable")));
        } else {
            assert_eq!(
                ws.interrupt_session("first", "123", culprit_core::SessionAction::Terminate).await.unwrap_err().code,
                ErrorCode::AuthFailed
            );
        }
        assert_eq!(*secrets.forgotten.lock().unwrap(), vec!["first"]);
        assert!(secrets.get("first").unwrap().is_some());
        assert!(secrets.get("second").unwrap().is_some());
        std::fs::remove_dir_all(dir).unwrap();
    }
}

#[cfg(any(feature = "mssql", feature = "mysql", feature = "postgres"))]
#[test]
fn failed_auxiliary_sessions_leave_checks_unassessed_even_without_a_capability_lock() {
    let groups = [
        #[cfg(feature = "postgres")]
        (include_str!("../../../fixtures/postgres-demo.json"), culprit_postgres::sources::MISC, "config"),
        #[cfg(feature = "mysql")]
        (include_str!("../../../fixtures/mysql-demo.json"), culprit_mysql::sources::SIGNAL_READS, "config"),
        #[cfg(feature = "mysql")]
        (include_str!("../../../fixtures/mysql-demo.json"), culprit_mysql::sources::MISC, "config"),
        #[cfg(feature = "mssql")]
        (include_str!("../../../fixtures/mssql-demo.json"), culprit_mssql::sources::MISC, "config"),
        #[cfg(feature = "mssql")]
        (
            include_str!("../../../fixtures/mssql-demo.json"),
            culprit_mssql::sources::SIGNAL_READS,
            "untrusted-constraint",
        ),
    ];
    for (fixture, sources, check) in groups {
        let mut snapshot: Snapshot = serde_json::from_str(fixture).unwrap();
        let mut coverage = culprit_core::Coverage::default();
        coverage.sources_failed(sources, &Error::internal("auxiliary session was lost"));
        coverage.append_to(&mut snapshot);
        for source in sources {
            assert_eq!(snapshot.source_outcome(source.name).unwrap().state, culprit_core::SourceState::Unavailable);
        }
        let report = culprit_checks::analyze(snapshot);
        assert!(report.check_absence_problem(check).is_some(), "{check} cannot claim a clean result");
    }
}

#[tokio::test]
async fn event_services_keep_identical_aliases_in_separate_workspace_stores() {
    let (a, _, dir_a) = workspace("events-a", &["SELECT"], false);
    let (b, _, dir_b) = workspace("events-b", &["SELECT"], false);
    add(&a, "shop", None);
    add(&b, "shop", None);
    let at_a = (chrono::Utc::now() - chrono::Duration::minutes(2)).to_rfc3339();
    let at_b = (chrono::Utc::now() - chrono::Duration::minutes(1)).to_rfc3339();
    let mark_a = a.mark("shop", "Deployment A", Some(&at_a), "test").unwrap();
    let mark_b = b.mark("shop", "Deployment B", Some(&at_b), "test").unwrap();
    let request = EventsRequest { cloud: Some(false), ..Default::default() };
    let events_a = a.events("shop", &request).await.unwrap().events;
    let events_b = b.events("shop", &request).await.unwrap().events;
    assert!(events_a.iter().any(|e| e.id == mark_a.event().id));
    assert!(!events_a.iter().any(|e| e.id == mark_b.event().id));
    assert!(events_b.iter().any(|e| e.id == mark_b.event().id));
    assert!(!events_b.iter().any(|e| e.id == mark_a.event().id));
    std::fs::remove_dir_all(dir_a).unwrap();
    std::fs::remove_dir_all(dir_b).unwrap();
}

#[test]
fn all_provider_sources_are_registered_in_the_canonical_source_contract() {
    use culprit_core::{same_source, source_id};
    for (i, a) in source_id::ALL.iter().enumerate() {
        for b in &source_id::ALL[i + 1..] {
            assert!(!same_source(a, b), "duplicate canonical source id: {a} vs {b}");
        }
    }
    let provider_sources: &[&[culprit_core::Source]] = &[
        #[cfg(feature = "postgres")]
        culprit_postgres::sources::ALL,
        #[cfg(feature = "yugabyte")]
        culprit_yugabyte::sources::ALL,
        #[cfg(feature = "mysql")]
        culprit_mysql::sources::ALL,
        #[cfg(feature = "mssql")]
        culprit_mssql::sources::ALL,
        #[cfg(feature = "sqlite")]
        culprit_sqlite::sources::ALL,
    ];
    for list in provider_sources {
        for source in *list {
            assert!(
                source_id::ALL.iter().any(|canonical| same_source(canonical, source.name)),
                "provider source {:?} is missing from culprit_core::source_id::ALL",
                source.name
            );
        }
    }
}
