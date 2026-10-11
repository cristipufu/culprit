use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use culprit_core::{
    Drift, Error, ExplainMode, LiveState, Plan, PlanDiff, Report, Result, ResultSet, ServerInfo, SessionAction,
    SnapshotOptions,
};
use culprit_engine::entra::{DeviceCodePrompt, SignInEvent};
use culprit_engine::{
    AgentAccess, ApprovalRequest, ConnectionInfo, DiscoveredDatabase, ImportCandidate, ImportSource, NewConnection,
    ParsedConnection, Workspace,
};
use tauri::webview::PageLoadEvent;
use tauri::{Emitter, Manager, State};

type Ws<'a> = State<'a, Arc<Workspace>>;

/// The device code being shown, so "Open the sign-in page" opens Microsoft's page and nothing the webview asks for.
#[derive(Default)]
struct SignIn(Mutex<Option<DeviceCodePrompt>>);

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
async fn import_candidates(ws: Ws<'_>, source: Option<ImportSource>) -> Result<Vec<ImportCandidate>> {
    ws.import_candidates(source)
}

#[tauri::command]
fn import_connections(ws: Ws, items: Vec<ImportCandidate>) -> Result<Vec<ConnectionInfo>> {
    ws.import_connections(&items)
}

#[tauri::command]
fn cancel_sign_in(ws: Ws) {
    ws.cancel_sign_in();
}

/// Microsoft's device-login pages only.
fn sign_in_page_allowed(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("https://") else { return false };
    let host = rest.split(['/', '?', '#']).next().unwrap_or("").to_ascii_lowercase();
    matches!(
        host.as_str(),
        "microsoft.com" | "www.microsoft.com" | "login.microsoftonline.com" | "login.microsoft.com" | "aka.ms"
    )
}

#[tauri::command]
fn open_sign_in_page(sign_in: State<SignIn>) -> Result<()> {
    let url = sign_in
        .0
        .lock()
        .ok()
        .and_then(|p| p.as_ref().map(|p| p.verification_uri.clone()))
        .filter(|u| sign_in_page_allowed(u))
        .ok_or_else(|| Error::not_found("No sign-in is waiting"))?;
    let opened = if cfg!(target_os = "macos") {
        std::process::Command::new("open").arg(&url).status()
    } else if cfg!(windows) {
        std::process::Command::new("explorer").arg(&url).status()
    } else {
        std::process::Command::new("xdg-open").arg(&url).status()
    };
    opened.map(|_| ()).map_err(|e| Error::internal(format!("Could not open the browser: {e}")))
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

/// A plan pasted or opened in the Plans view (.sqlplan, EXPLAIN JSON or text, MySQL JSON or tree).
#[tauri::command]
async fn parse_plan(text: String) -> Result<Plan> {
    culprit_engine::plans::parse_plan(&text)
}

#[tauri::command]
async fn diff_plans(a: Plan, b: Plan, name_a: String, name_b: String) -> Result<PlanDiff> {
    Ok(culprit_core::plan_diff_named(&a, &name_a, &b, &name_b))
}

#[tauri::command]
async fn save_plan_file(name: String, contents: String) -> Result<String> {
    culprit_engine::plans::save_to_downloads(&name, &contents).map(|path| path.display().to_string())
}

#[tauri::command]
fn drift(ws: Ws, a: String, b: String) -> Result<Drift> {
    ws.drift(&a, &b)
}

#[tauri::command]
async fn live(ws: Ws<'_>, name: String) -> Result<LiveState> {
    ws.live(&name).await
}

#[tauri::command]
async fn interrupt_session(ws: Ws<'_>, name: String, session: String, action: SessionAction) -> Result<()> {
    ws.interrupt_session(&name, &session, action).await
}

#[tauri::command]
async fn run_sql(ws: Ws<'_>, name: String, sql: String, limit: usize, confirmed: bool) -> Result<ResultSet> {
    ws.run(&name, &sql, limit.min(10_000), confirmed).await
}

/// What a statement does, for the question asked before it changes data on production. Pure; no connection.
#[tauri::command]
fn check_statement(engine: culprit_core::Engine, sql: String) -> culprit_core::sql::StatementCheck {
    culprit_core::sql::check(&sql, engine)
}

#[tauri::command]
async fn catalog(ws: Ws<'_>, name: String) -> Result<culprit_engine::catalog::Catalog> {
    ws.catalog(&name).await
}

#[tauri::command]
fn saved_queries(ws: Ws<'_>) -> Result<Vec<culprit_engine::saved::SavedQuery>> {
    ws.saved_queries()
}

#[tauri::command]
fn save_query(ws: Ws<'_>, query: culprit_engine::saved::SaveQuery) -> Result<culprit_engine::saved::SavedQuery> {
    ws.save_query(query)
}

#[tauri::command]
fn delete_saved_query(ws: Ws<'_>, name: String) -> Result<()> {
    ws.delete_saved_query(&name)
}

#[tauri::command]
fn saved_query_ran(ws: Ws<'_>, name: String) -> Result<()> {
    ws.saved_query_ran(&name)
}

#[tauri::command]
fn set_agent_access(ws: Ws, name: String, access: AgentAccess) -> Result<ConnectionInfo> {
    ws.set_agent_access(&name, access)
}

/// Approval requests already announced to the person, so the dock/taskbar only asks for attention once per request.
#[derive(Default)]
struct Announced(Mutex<HashSet<String>>);

/// Statements coding agents are waiting to run. Polled by the window while it is open; that also tells `culprit mcp`
/// someone is here to answer.
#[tauri::command]
fn pending_approvals(window: tauri::Window, ws: Ws, announced: State<Announced>) -> Result<Vec<ApprovalRequest>> {
    let pending = ws.pending_approvals()?;
    let mut seen = announced.0.lock().unwrap_or_else(|e| e.into_inner());
    if pending.iter().any(|r| !seen.contains(&r.id)) {
        let _ = window.request_user_attention(Some(tauri::UserAttentionType::Informational));
    }
    *seen = pending.iter().map(|r| r.id.clone()).collect();
    Ok(pending)
}

#[tauri::command]
fn decide_approval(ws: Ws, id: String, hash: String, approved: bool) -> Result<()> {
    ws.decide_approval(&id, &hash, approved)
}

pub fn run_app() -> std::result::Result<(), Box<dyn std::error::Error>> {
    let started = Instant::now();
    let trace = std::env::var_os("CULPRIT_TRACE_STARTUP").is_some();
    let workspace = Workspace::open()?;
    tauri::Builder::default()
        .manage(Announced::default())
        .manage(SignIn::default())
        .setup(move |app| {
            let handle = app.handle().clone();
            let workspace = workspace.with_sign_in_handler(Arc::new(move |event: SignInEvent| {
                if let Some(state) = handle.try_state::<SignIn>()
                    && let Ok(mut current) = state.0.lock()
                {
                    *current = match &event {
                        SignInEvent::Code { prompt } => Some(prompt.clone()),
                        _ => None,
                    };
                }
                let _ = handle.emit("entra-sign-in", &event);
            }));
            app.manage(Arc::new(workspace));
            Ok(())
        })
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
            import_candidates,
            import_connections,
            cancel_sign_in,
            open_sign_in_page,
            latest_report,
            scan,
            brief,
            explain,
            query_plan,
            drift,
            parse_plan,
            diff_plans,
            save_plan_file,
            live,
            run_sql,
            check_statement,
            catalog,
            saved_queries,
            save_query,
            delete_saved_query,
            saved_query_ran,
            set_agent_access,
            pending_approvals,
            decide_approval,
            interrupt_session
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
