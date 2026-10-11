use std::sync::Arc;
use std::time::Instant;

use culprit_core::{Error, ExplainMode, LiveState, Plan, Report, Result, ResultSet, ServerInfo, SnapshotOptions};
use culprit_engine::{ConnectionInfo, DiscoveredDatabase, NewConnection, ParsedConnection, Workspace};
use tauri::State;
use tauri::webview::PageLoadEvent;

type Ws<'a> = State<'a, Arc<Workspace>>;

#[tauri::command]
fn connections(ws: Ws) -> Result<Vec<ConnectionInfo>> {
    ws.connections()
}

#[tauri::command]
fn parse_connection(input: String) -> Result<ParsedConnection> {
    culprit_engine::parse_connection(&input)
}

#[tauri::command]
async fn test_connection(ws: Ws<'_>, input: NewConnection) -> Result<ServerInfo> {
    ws.provider_for(&input).await?.probe().await
}

#[tauri::command]
fn add_connection(ws: Ws, input: NewConnection) -> Result<ConnectionInfo> {
    ws.add_connection(input)
}

#[tauri::command]
fn remove_connection(ws: Ws, name: String) -> Result<()> {
    ws.remove_connection(&name)
}

#[tauri::command]
fn rename_connection(ws: Ws, old: String, new: String) -> Result<ConnectionInfo> {
    ws.rename_connection(&old, &new)
}

#[tauri::command]
async fn discover_azure(ws: Ws<'_>, terms: Vec<String>) -> Result<Vec<DiscoveredDatabase>> {
    ws.discover_azure(&terms).await
}

#[tauri::command]
async fn discover_aws(ws: Ws<'_>, terms: Vec<String>, regions: Vec<String>) -> Result<Vec<DiscoveredDatabase>> {
    ws.discover_aws(&regions, &terms).await
}

#[tauri::command]
fn set_password(ws: Ws, name: String, password: String) -> Result<ConnectionInfo> {
    ws.set_password(&name, &password)
}

#[tauri::command]
fn add_discovered(ws: Ws, items: Vec<DiscoveredDatabase>) -> Result<Vec<ConnectionInfo>> {
    items.iter().map(|d| ws.add_discovered(d)).collect()
}

/// The UI never shows raw plan XML/JSON, and on busy databases it is most of a report's size; keep it on disk only.
fn for_ui(mut report: Report) -> Report {
    if let Some(workload) = report.snapshot.workload.as_mut() {
        for plan in workload.queries.iter_mut().filter_map(|q| q.plan.as_mut()) {
            plan.raw = None;
        }
    }
    report
}

#[tauri::command]
fn latest_report(ws: Ws, name: String, window_days: Option<u32>) -> Result<Option<Report>> {
    Ok(ws.latest_report(&name, window_days)?.map(for_ui))
}

#[tauri::command]
async fn scan(ws: Ws<'_>, name: String, window_days: Option<u32>) -> Result<Report> {
    let defaults = SnapshotOptions::default();
    let options = SnapshotOptions { window_days: window_days.unwrap_or(defaults.window_days).clamp(1, 90), ..defaults };
    ws.scan(&name, &options).await.map(for_ui)
}

#[tauri::command]
fn brief(ws: Ws, name: String, finding_id: String, window_days: Option<u32>) -> Result<String> {
    let report = ws
        .latest_report(&name, window_days)?
        .or(ws.latest_report(&name, None)?)
        .ok_or_else(|| Error::not_found(format!("No report for {name}")))?;
    let finding = report
        .findings
        .iter()
        .find(|f| f.id == finding_id)
        .ok_or_else(|| Error::not_found(format!("No finding {finding_id}")))?;
    Ok(culprit_engine::agent_brief(&report, finding))
}

#[tauri::command]
async fn explain(ws: Ws<'_>, name: String, sql: String, mode: ExplainMode, confirmed: bool) -> Result<Plan> {
    ws.explain(&name, &sql, mode, confirmed).await
}

#[tauri::command]
async fn query_plan(ws: Ws<'_>, name: String, query_id: String, plan_id: Option<String>) -> Result<Plan> {
    let mut plan = ws.query_plan(&name, &query_id, plan_id.as_deref()).await?;
    plan.raw = None;
    Ok(plan)
}

#[tauri::command]
async fn live(ws: Ws<'_>, name: String) -> Result<LiveState> {
    ws.live(&name).await
}

#[tauri::command]
async fn run_sql(ws: Ws<'_>, name: String, sql: String, limit: usize, confirmed: bool) -> Result<ResultSet> {
    ws.run(&name, &sql, limit.min(10_000), confirmed).await
}

pub fn run_app() -> std::result::Result<(), Box<dyn std::error::Error>> {
    let started = Instant::now();
    let trace = std::env::var_os("CULPRIT_TRACE_STARTUP").is_some();
    let workspace = Arc::new(Workspace::open()?);
    tauri::Builder::default()
        .manage(workspace)
        .on_page_load(move |webview, payload| {
            if payload.event() != PageLoadEvent::Finished {
                return;
            }
            // The window starts hidden so it never flashes white; show it once the page (and its CSS) is in.
            let window = webview.window();
            let _ = window.show();
            let _ = window.set_focus();
            if trace {
                eprintln!("culprit: window shown {} ms after start", started.elapsed().as_millis());
            }
        })
        .invoke_handler(tauri::generate_handler![
            connections,
            parse_connection,
            test_connection,
            add_connection,
            remove_connection,
            rename_connection,
            discover_azure,
            discover_aws,
            set_password,
            add_discovered,
            latest_report,
            scan,
            brief,
            explain,
            query_plan,
            live,
            run_sql
        ])
        .run(tauri::generate_context!())?;
    Ok(())
}

pub fn run() {
    if let Err(e) = run_app() {
        eprintln!("culprit: {e}");
        std::process::exit(1);
    }
}
