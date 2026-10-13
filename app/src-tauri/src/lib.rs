use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use culprit_core::Verification;
use culprit_core::{
    Drift, Error, ErrorCode, ExplainMode, LiveState, Plan, PlanDiff, Report, Result, ResultSet, ServerInfo, SessionAction,
    SnapshotOptions,
};
use culprit_engine::entra::{DeviceCodePrompt, SignInEvent};
use culprit_engine::export::{ExportFile, ExportFormat, ExportOptions};
use culprit_engine::{
    AgentAccess, ApprovalRequest, ConnectionInfo, DiscoveredDatabase, ImportCandidate, ImportSource, NewConnection,
    ParsedConnection, Workspace,
};
use tauri::webview::PageLoadEvent;
use tauri::{AppHandle, Emitter, Manager, State};
use tauri_plugin_updater::{Update, UpdaterExt};

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
            // Saved reports don't carry insights; the UI shows them on every plan.
            culprit_core::insights::annotate(plan);
        }
    }
    report
}

/// The workload window the app asked for, in minutes; a build from before minutes sent whole days.
fn requested_window(window_minutes: Option<u32>, window_days: Option<u32>) -> Option<u32> {
    window_minutes
        .map(|minutes| minutes.clamp(1, culprit_core::MAX_WINDOW_MINUTES))
        .or_else(|| window_days.map(|days| days.clamp(1, 90) * culprit_core::MINUTES_PER_DAY))
}

#[tauri::command]
fn latest_report(
    ws: Ws,
    name: String,
    window_minutes: Option<u32>,
    window_days: Option<u32>,
) -> Result<Option<Report>> {
    Ok(ws.latest_report(&name, requested_window(window_minutes, window_days))?.map(for_ui))
}

#[tauri::command]
async fn scan(ws: Ws<'_>, name: String, window_minutes: Option<u32>, window_days: Option<u32>) -> Result<Report> {
    let defaults = SnapshotOptions::default();
    let window = requested_window(window_minutes, window_days).unwrap_or(defaults.window_in_minutes());
    let options = defaults.with_window_minutes(window);
    ws.scan(&name, &options).await.map(for_ui)
}

#[tauri::command]
fn brief(
    ws: Ws,
    name: String,
    finding_id: String,
    window_minutes: Option<u32>,
    window_days: Option<u32>,
) -> Result<String> {
    let report = ws
        .latest_report(&name, requested_window(window_minutes, window_days))?
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

/// The actual plan of a query's last run as the engine kept it; never runs the query.
#[tauri::command]
async fn last_actual_plan(ws: Ws<'_>, name: String, query_id: String, plan_id: Option<String>) -> Result<Plan> {
    let mut plan = ws.last_actual_plan(&name, &query_id, plan_id.as_deref()).await?;
    plan.raw = None;
    Ok(plan)
}

/// How far along a session's running statement is (polled about once a second while visible).
#[tauri::command]
async fn query_progress(
    ws: Ws<'_>,
    name: String,
    session: String,
    with_plan: bool,
) -> Result<culprit_core::QueryProgress> {
    ws.query_progress(&name, &session, with_plan).await
}

/// A plan pasted or opened in the Plans view (.sqlplan, EXPLAIN JSON or text, MySQL JSON or tree).
#[tauri::command]
async fn parse_plan(text: String) -> Result<Plan> {
    culprit_engine::plans::parse_plan(&text)
}

/// `connection`, when the plans came from one, lets scenarios name stale statistics from its latest scan.
#[tauri::command]
async fn diff_plans(
    ws: Ws<'_>,
    a: Plan,
    b: Plan,
    name_a: String,
    name_b: String,
    connection: Option<String>,
) -> Result<PlanDiff> {
    Ok(ws.diff_plans(&a, &name_a, &b, &name_b, connection.as_deref()))
}

#[tauri::command]
async fn export_plan(plan: Plan, title: String, format: ExportFormat, options: ExportOptions) -> Result<ExportFile> {
    culprit_engine::export_plan(&plan, &title, format, &options)
}

#[tauri::command]
#[allow(clippy::too_many_arguments)]
async fn export_diff(
    ws: Ws<'_>,
    a: Plan,
    b: Plan,
    name_a: String,
    name_b: String,
    format: ExportFormat,
    options: ExportOptions,
    connection: Option<String>,
) -> Result<ExportFile> {
    let diff = ws.diff_plans(&a, &name_a, &b, &name_b, connection.as_deref());
    culprit_engine::export_diff(&a, &b, &diff, &name_a, &name_b, format, &options)
}

#[tauri::command]
async fn export_query_plan(
    ws: Ws<'_>,
    name: String,
    query_id: String,
    plan_id: Option<String>,
    format: ExportFormat,
    options: ExportOptions,
) -> Result<ExportFile> {
    ws.export_query_plan(&name, &query_id, plan_id.as_deref(), format, &options).await
}

#[tauri::command]
fn export_finding(
    ws: Ws,
    name: String,
    finding_id: String,
    format: ExportFormat,
    options: ExportOptions,
) -> Result<ExportFile> {
    ws.export_finding(&name, &finding_id, format, &options)
}

#[tauri::command]
async fn verify_findings(ws: Ws<'_>, name: String, ids: Vec<String>) -> Result<Verification> {
    ws.verify(&name, &ids).await
}

#[tauri::command]
async fn save_plan_file(name: String, contents: String) -> Result<String> {
    culprit_engine::plans::save_to_downloads(&name, &contents).map(|path| path.display().to_string())
}

#[tauri::command]
fn drift(ws: Ws, a: String, b: String) -> Result<Drift> {
    ws.drift(&a, &b)
}

/// The index advisor's plan from the connection's latest report.
#[tauri::command]
fn index_advice(ws: Ws, name: String) -> Result<culprit_core::IndexAdvice> {
    ws.index_advice(&name)
}

/// The dashboard from the connection's latest report for the window; the app reads sessions itself, every few seconds.
#[tauri::command]
async fn dashboard(
    ws: Ws<'_>,
    name: String,
    window_minutes: Option<u32>,
    window_days: Option<u32>,
) -> Result<culprit_core::Dashboard> {
    ws.dashboard(&name, requested_window(window_minutes, window_days), false).await
}

#[tauri::command]
async fn live(ws: Ws<'_>, name: String) -> Result<LiveState> {
    ws.live(&name).await
}

/// Where the latest scan's database time went, by wait category with the queries behind each.
#[tauri::command]
fn waits(
    ws: Ws<'_>,
    name: String,
    window_minutes: Option<u32>,
    window_days: Option<u32>,
) -> Result<Option<culprit_core::WaitsReport>> {
    ws.waits(&name, requested_window(window_minutes, window_days))
}

/// What the server waits on right now, sampled for `seconds`.
#[tauri::command]
async fn wait_sample(ws: Ws<'_>, name: String, seconds: u32) -> Result<culprit_core::WaitSample> {
    ws.wait_sample(&name, seconds).await
}

#[tauri::command]
async fn interrupt_session(ws: Ws<'_>, name: String, session: String, action: SessionAction) -> Result<()> {
    ws.interrupt_session(&name, &session, action).await
}

/// What changed between two periods on a connection (Query Store intervals, or Culprit's own scans).
#[tauri::command]
async fn changes(
    ws: Ws<'_>,
    name: String,
    request: culprit_engine::changes::ChangeRequest,
) -> Result<culprit_core::Changes> {
    ws.changes(&name, &request).await
}

/// What changed around a connection over a period: marks, schema, settings, restarts and cloud throttling.
#[tauri::command]
async fn events(ws: Ws<'_>, name: String, request: culprit_engine::EventsRequest) -> Result<culprit_engine::EventLog> {
    ws.events(&name, &request).await
}

/// What the cloud measured for a connection's server and when it held the server back (through `aws` / `az`).
#[tauri::command]
async fn cloud(ws: Ws<'_>, name: String, request: culprit_engine::EventsRequest) -> Result<culprit_core::CloudSignals> {
    ws.cloud(&name, &request).await
}

/// Put a moment (a deploy, a migration) on a connection's timeline.
#[tauri::command]
fn mark(ws: Ws<'_>, name: String, text: String, at: Option<String>) -> Result<culprit_engine::Mark> {
    ws.mark(&name, &text, at.as_deref(), "app")
}

#[tauri::command]
fn unmark(ws: Ws<'_>, name: String, id: String) -> Result<culprit_engine::Mark> {
    ws.unmark(&name, &id)
}

/// Recent deadlocks on a connection, explained (pattern, statements, fixes).
#[tauri::command]
async fn deadlocks(ws: Ws<'_>, name: String, since_hours: f64) -> Result<culprit_core::DeadlockReport> {
    ws.deadlocks(&name, since_hours).await
}

/// Deadlocks in pasted or dropped text: SQL Server .xdl XML, InnoDB status, Postgres log lines. No connection needed.
#[tauri::command]
fn parse_deadlock(text: String) -> Result<Vec<culprit_core::Deadlock>> {
    culprit_engine::deadlocks::parse_deadlock(&text)
}

#[tauri::command]
async fn run_sql(ws: Ws<'_>, name: String, sql: String, limit: usize, confirmed: bool) -> Result<ResultSet> {
    ws.run(&name, &sql, limit.min(10_000), confirmed).await
}

/// Every check Culprit can run, so the app can say what it looked at and found fine.
#[tauri::command]
fn checks(ws: Ws<'_>) -> Vec<culprit_core::CheckInfo> {
    ws.checks()
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

/// A newer release the last check found, kept for the install.
#[derive(Default)]
struct PendingUpdate(Mutex<Option<Update>>);

/// A newer release than this build: its version, and its notes when it has some.
#[derive(serde::Serialize)]
struct UpdateInfo {
    version: String,
    current: String,
    notes: Option<String>,
}

#[derive(Clone, serde::Serialize)]
struct UpdateProgress {
    downloaded: u64,
    total: Option<u64>,
}

/// Updates come from the latest published release's `latest.json` (tauri.conf.json), or from
/// CULPRIT_UPDATE_ENDPOINT for a mirror or a test. Whatever serves it, an update installs only when its signature
/// matches the public key built into the app.
fn updater(app: &AppHandle) -> Result<tauri_plugin_updater::Updater> {
    let failed = |e: tauri_plugin_updater::Error| {
        Error::new(ErrorCode::ConnectionFailed, format!("Couldn't check for updates: {e}"))
    };
    let mut builder = app.updater_builder().timeout(Duration::from_secs(30));
    if let Ok(endpoint) = std::env::var("CULPRIT_UPDATE_ENDPOINT") {
        let url = endpoint.parse().map_err(|_| Error::invalid("CULPRIT_UPDATE_ENDPOINT isn't a URL"))?;
        builder = builder.endpoints(vec![url]).map_err(failed)?;
    }
    builder.build().map_err(failed)
}

/// Whether a newer release is out. Only ever asked by the app itself, at most once a day, or when the person checks.
#[tauri::command]
async fn check_update(app: AppHandle, pending: State<'_, PendingUpdate>) -> Result<Option<UpdateInfo>> {
    let update = updater(&app)?
        .check()
        .await
        .map_err(|e| Error::new(ErrorCode::ConnectionFailed, format!("Couldn't check for updates: {e}")))?;
    let info = update.as_ref().map(|u| UpdateInfo {
        version: u.version.clone(),
        current: u.current_version.clone(),
        notes: u.body.clone().filter(|b| !b.trim().is_empty()),
    });
    *pending.0.lock().unwrap_or_else(|e| e.into_inner()) = update;
    Ok(info)
}

/// Downloads the update the last check found, verifies its signature and installs it, reporting progress as
/// `update-progress`. The app keeps running the old version until `restart_app`, when the person chooses.
#[tauri::command]
async fn install_update(app: AppHandle, pending: State<'_, PendingUpdate>) -> Result<()> {
    let update = pending
        .0
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
        .ok_or_else(|| Error::not_found("No update is waiting. Check for updates again."))?;
    let mut downloaded = 0u64;
    let mut reported = 0u64;
    update
        .download_and_install(
            |chunk, total| {
                downloaded += chunk as u64;
                // About a hundred events for the whole download, not one per chunk.
                let step = total.map_or(256 * 1024, |t| (t / 100).max(1));
                if downloaded - reported >= step || total == Some(downloaded) {
                    reported = downloaded;
                    let _ = app.emit("update-progress", UpdateProgress { downloaded, total });
                }
            },
            || {},
        )
        .await
        .map_err(|e| Error::new(ErrorCode::Internal, format!("Couldn't install the update: {e}")))?;
    Ok(())
}

#[tauri::command]
fn restart_app(app: AppHandle) {
    app.restart();
}

pub fn run_app() -> std::result::Result<(), Box<dyn std::error::Error>> {
    let started = Instant::now();
    let trace = std::env::var_os("CULPRIT_TRACE_STARTUP").is_some();
    let workspace = Workspace::open()?;
    tauri::Builder::default()
        .plugin(tauri_plugin_updater::Builder::new().build())
        .manage(Announced::default())
        .manage(SignIn::default())
        .manage(PendingUpdate::default())
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
            last_actual_plan,
            query_progress,
            drift,
            changes,
            events,
            cloud,
            mark,
            unmark,
            index_advice,
            dashboard,
            parse_plan,
            diff_plans,
            export_plan,
            export_diff,
            export_query_plan,
            export_finding,
            verify_findings,
            save_plan_file,
            live,
            waits,
            wait_sample,
            run_sql,
            deadlocks,
            parse_deadlock,
            check_statement,
            checks,
            catalog,
            saved_queries,
            save_query,
            delete_saved_query,
            saved_query_ran,
            set_agent_access,
            pending_approvals,
            decide_approval,
            interrupt_session,
            check_update,
            install_update,
            restart_app
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
