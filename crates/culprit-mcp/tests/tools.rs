//! The MCP tools against a saved demo report and a replaying provider: shapes, policies, approvals and the audit log.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use culprit_core::{
    AuthKind, Engine, ExplainMode, LiveState, Plan, Provider, Report, Result, ResultSet, ServerInfo, Snapshot,
    SnapshotOptions,
};
use culprit_engine::{AgentAccess, AuditOutcome, NewConnection, Workspace, reports};
use culprit_mcp::{CulpritMcp, Options};
use serde_json::{Value, json};

fn report() -> Report {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/mssql-demo.report.json");
    serde_json::from_slice(&std::fs::read(path).expect("fixture")).expect("report")
}

/// Replays the demo report; `run` echoes the batch it was given so tests can see exactly what would execute.
struct Replay;

#[async_trait]
impl Provider for Replay {
    fn engine(&self) -> Engine {
        Engine::Mssql
    }

    async fn probe(&self) -> Result<ServerInfo> {
        Ok(report().snapshot.server)
    }

    async fn snapshot(&self, _: &str, _: &SnapshotOptions) -> Result<Snapshot> {
        Ok(report().snapshot)
    }

    async fn explain(&self, _: &str, mode: ExplainMode) -> Result<Plan> {
        assert_eq!(mode, ExplainMode::Estimated, "agents only ever get estimated plans");
        let report = report();
        Ok(report.snapshot.workload.unwrap().queries.into_iter().find_map(|q| q.plan).unwrap())
    }

    async fn live(&self) -> Result<LiveState> {
        Ok(LiveState {
            taken_at: "2026-10-11T12:00:00Z".into(),
            sessions: vec![],
            waits: vec![],
            session_actions: vec![],
            session_actions_note: None,
            blocking: None,
        })
    }

    async fn run(&self, sql: &str, _: usize) -> Result<ResultSet> {
        Ok(ResultSet {
            columns: vec!["batch".into()],
            rows: vec![vec![json!(sql)]],
            truncated: false,
            elapsed_ms: 1.0,
            messages: vec![],
        })
    }

    async fn stored_plan(&self, _: &str, _: &str) -> Result<Option<Plan>> {
        Ok(None)
    }
}

struct Fixture {
    ws: Arc<Workspace>,
    dir: PathBuf,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn fixture(tag: &str) -> Fixture {
    let dir = std::env::temp_dir().join(format!("culprit-mcp-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let ws = Workspace::open_in(&dir).with_provider(Engine::Mssql, Arc::new(|_| Ok(Box::new(Replay))));
    // No secret of any kind: `AuthKind::None` keeps every code path away from the OS keychain, and the names can't
    // collide with a real saved connection's keychain entry.
    for name in ["replay-demo", "replay-hidden"] {
        ws.add_connection(NewConnection {
            name: name.into(),
            input: "Server=localhost,14333;Database=culprit_demo".into(),
            engine: Some(Engine::Mssql),
            auth: Some(AuthKind::None),
            ..Default::default()
        })
        .unwrap();
    }
    reports::save(&ws.paths().reports_dir("replay-demo"), &report()).unwrap();
    ws.set_agent_access("replay-hidden", AgentAccess::Off).unwrap();
    Fixture { ws: Arc::new(ws), dir }
}

fn server(f: &Fixture, options: Options) -> CulpritMcp {
    CulpritMcp::new(f.ws.clone(), options)
}

/// Call a tool and return (is_error, parsed JSON body).
async fn call(server: &CulpritMcp, tool: &str, args: Value) -> (bool, Value) {
    let args = args.as_object().cloned().unwrap_or_default();
    let result = server.handle("test-agent", tool, args).await;
    let text = result.content[0].as_text().expect("text content").text.clone();
    (result.is_error == Some(true), serde_json::from_str(&text).expect("JSON body"))
}

#[tokio::test]
async fn read_tools_answer_from_the_saved_report() {
    let f = fixture("read");
    let s = server(&f, Options::default());

    let (err, body) = call(&s, "list_connections", json!({})).await;
    assert!(!err);
    let names: Vec<&str> =
        body["connections"].as_array().unwrap().iter().map(|c| c["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["replay-demo"], "connections with agent access off are hidden");
    assert_eq!(body["connections"][0]["agent_access"], "stats");
    assert!(body["connections"][0].get("host").is_none(), "no host or user goes to agents");

    let (err, body) = call(&s, "findings", json!({ "connection": "replay-demo", "min_severity": "high" })).await;
    assert!(!err, "{body}");
    let findings = body["findings"].as_array().unwrap();
    assert!(!findings.is_empty() && findings.iter().all(|f| f["severity"] == "high"));
    let id = findings[0]["id"].as_str().unwrap().to_string();

    let (err, body) = call(&s, "finding", json!({ "connection": "replay-demo", "id": id })).await;
    assert!(!err);
    assert!(body["brief"].as_str().unwrap().contains("## What's happening"));

    let (err, body) = call(&s, "top_queries", json!({ "connection": "replay-demo", "by": "cpu", "limit": 3 })).await;
    assert!(!err);
    let top = body["queries"].as_array().unwrap();
    assert_eq!(top.len(), 3);
    let query_id = top[0]["id"].as_str().unwrap().to_string();

    let (err, body) = call(&s, "query", json!({ "connection": "replay-demo", "query_id": query_id })).await;
    assert!(!err);
    assert!(body["query"]["text"].as_str().unwrap().len() > 10);

    let (err, body) = call(&s, "query_plan", json!({ "connection": "replay-demo", "query_id": query_id })).await;
    assert!(!err, "{body}");
    assert!(body["tree"].as_str().unwrap().contains("rows est"));

    let (err, body) = call(&s, "explain", json!({ "connection": "replay-demo", "sql": "SELECT 1;" })).await;
    assert!(!err, "{body}");
    assert!(body["operators"].as_u64().unwrap() > 0);

    let (err, body) = call(&s, "schema", json!({ "connection": "replay-demo" })).await;
    assert!(!err);
    let table = body["tables"][0]["table"].as_str().unwrap().to_string();
    let (err, body) = call(&s, "schema", json!({ "connection": "replay-demo", "table": table })).await;
    assert!(!err, "{body}");
    assert!(!body["columns"].as_array().unwrap().is_empty());
    assert!(!body["indexes"].as_array().unwrap().is_empty());

    let (err, body) = call(&s, "live", json!({ "connection": "replay-demo" })).await;
    assert!(!err);
    assert_eq!(body["blocked"], 0);

    let (err, body) = call(&s, "dashboard", json!({ "connection": "replay-demo" })).await;
    assert!(!err, "{body}");
    let tiles = body["tiles"].as_array().unwrap();
    assert_eq!(tiles.len(), 10, "every tile, on every engine");
    assert!(tiles.iter().all(|t| !t["takeaway"].as_str().unwrap().is_empty() && t["next"].is_string()));
    let now = tiles.iter().find(|t| t["key"] == "now").unwrap();
    assert_eq!(now["takeaway"], "Nothing is running right now.", "sessions were read once");
    assert_eq!(body["headline"], tiles.iter().find(|t| t["key"] == body["attention"][0]).unwrap()["takeaway"]);

    let (err, body) = call(&s, "finding", json!({ "connection": "replay-demo", "id": "nope" })).await;
    assert!(err);
    assert_eq!(body["error"]["code"], "not_found");
    assert!(body["error"]["hint"].as_str().unwrap().contains("findings"));
}

#[tokio::test]
async fn policies_hide_and_refuse() {
    let f = fixture("policy");
    let s = server(&f, Options::default());
    let (err, body) = call(&s, "findings", json!({ "connection": "replay-hidden" })).await;
    assert!(err);
    assert_eq!(body["error"]["code"], "permission_denied");
    assert!(body["error"]["hint"].as_str().unwrap().contains("culprit conn set replay-hidden --agent stats"));

    let only = server(&f, Options { connections: vec!["other".into()], ..Options::default() });
    let (_, body) = call(&only, "list_connections", json!({})).await;
    assert_eq!(body["connections"], json!([]));
    let (err, _) = call(&only, "findings", json!({ "connection": "replay-demo" })).await;
    assert!(err, "--connection limits what the server exposes");

    let (err, body) = call(&s, "run_query", json!({ "connection": "replay-demo", "sql": "SELECT 1" })).await;
    assert!(err);
    assert!(body["error"]["hint"].as_str().unwrap().contains("--agent query"), "stats access never runs SQL");

    let (err, body) = call(&s, "nonexistent", json!({})).await;
    assert!(err);
    assert_eq!(body["error"]["code"], "not_found");
}

#[tokio::test]
async fn run_query_needs_a_read_only_statement_and_a_person() {
    let f = fixture("run");
    f.ws.set_agent_access("replay-demo", AgentAccess::Query).unwrap();
    let s = server(&f, Options { approval_timeout: Duration::from_millis(400), ..Options::default() });

    let (err, body) =
        call(&s, "run_query", json!({ "connection": "replay-demo", "sql": "DELETE FROM shop.orders" })).await;
    assert!(err);
    assert!(body["error"]["message"].as_str().unwrap().contains("read data"), "writes are refused before any approval");

    let (err, body) = call(&s, "run_query", json!({ "connection": "replay-demo", "sql": "SELECT 1" })).await;
    assert!(err);
    assert!(body["error"]["message"].as_str().unwrap().contains("isn't running"));

    // The app is open but nobody answers.
    f.ws.pending_approvals().unwrap();
    let (err, body) = call(&s, "run_query", json!({ "connection": "replay-demo", "sql": "SELECT 1" })).await;
    assert!(err);
    assert!(body["error"]["message"].as_str().unwrap().contains("within 60 seconds"));

    // A person approves exactly what the agent sent.
    let app = {
        let ws = f.ws.clone();
        tokio::spawn(async move {
            loop {
                if let Some(r) = ws.pending_approvals().unwrap().pop() {
                    assert_eq!((r.tool.as_str(), r.client.as_str()), ("run_query", "test-agent"));
                    ws.decide_approval(&r.id, &r.hash, true).unwrap();
                    return r.sql;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
    };
    let (err, body) =
        call(&s, "run_query", json!({ "connection": "replay-demo", "sql": "SELECT TOP 5 email FROM shop.customers;" }))
            .await;
    assert_eq!(app.await.unwrap(), "SELECT TOP 5 email FROM shop.customers");
    assert!(!err, "{body}");
    let batch = body["rows"][0][0].as_str().unwrap();
    assert!(
        batch.contains("BEGIN TRANSACTION")
            && batch.contains("SELECT TOP 5 email")
            && batch.ends_with("ROLLBACK TRANSACTION;")
    );

    let log = f.ws.audit_log(20).unwrap();
    let runs: Vec<_> = log.iter().filter(|e| e.tool == "run_query").collect();
    assert_eq!(runs.len(), 4);
    assert_eq!(
        runs.iter().map(|e| e.outcome).collect::<Vec<_>>(),
        [AuditOutcome::Denied, AuditOutcome::Denied, AuditOutcome::Denied, AuditOutcome::Ok]
    );
    assert!(runs.iter().all(|e| e.sql.is_some() && e.client == "test-agent"));
}

#[tokio::test]
async fn audit_records_every_call_and_sql_only_where_sent() {
    let f = fixture("audit");
    let s = server(&f, Options::default());
    call(&s, "findings", json!({ "connection": "replay-demo" })).await;
    call(&s, "explain", json!({ "connection": "replay-demo", "sql": "SELECT 1" })).await;
    call(&s, "finding", json!({ "connection": "replay-demo", "id": "missing" })).await;
    let log = f.ws.audit_log(10).unwrap();
    let rows: Vec<_> = log.iter().map(|e| (e.tool.as_str(), e.outcome, e.sql.as_deref())).collect();
    assert_eq!(
        rows,
        [
            ("findings", AuditOutcome::Ok, None),
            ("explain", AuditOutcome::Ok, Some("SELECT 1")),
            ("finding", AuditOutcome::Error, None)
        ]
    );
    assert!(log.iter().all(|e| e.connection.as_deref() == Some("replay-demo")));
}

#[tokio::test]
async fn verify_and_export_close_the_loop() {
    let f = fixture("verify");
    let s = server(&f, Options::default());
    let (err, body) = call(&s, "findings", json!({ "connection": "replay-demo" })).await;
    assert!(!err, "{body}");
    let id = body["findings"][0]["id"].as_str().unwrap().to_string();

    let (err, body) = call(&s, "export_finding", json!({ "connection": "replay-demo", "finding_id": id })).await;
    assert!(!err, "{body}");
    let md = body["markdown"].as_str().unwrap();
    assert!(md.starts_with("## ") && md.contains("How to verify") || md.contains("**Fix.**"), "{md}");
    assert!(!md.contains("replay-demo") && !md.contains("localhost"), "no connection names or hosts in exports");
    assert_eq!(body["redacted"], true);

    // The replayed scan reports the same findings, so the one asked about is still present.
    let (err, body) = call(&s, "verify", json!({ "connection": "replay-demo", "finding_ids": [id] })).await;
    assert!(!err, "{body}");
    assert_eq!(body["findings"][0]["status"], "still-present");
    assert!(body["summary"].as_str().unwrap().starts_with("Still present"));

    let (err, body) = call(&s, "verify", json!({ "connection": "replay-hidden" })).await;
    assert!(err, "agent access off applies to verify too: {body}");
}
