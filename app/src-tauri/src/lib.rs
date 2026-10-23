use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use culprit_core::Verification;
use culprit_core::{
    Drift, Error, ErrorCode, ExplainMode, LiveState, Plan, PlanDiff, Report, Result, ResultSet, ServerInfo,
    SessionAction, SnapshotOptions,
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

mod screenshot;

type Ws<'a> = State<'a, Arc<Workspace>>;
static WORKSPACE_TASKS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(2);

/// Store updates can wait for file locks; report reads decode and replay large artifacts. Keep both off UI/runtime threads.
async fn blocking_workspace<T: Send + 'static>(
    ws: Arc<Workspace>,
    update: impl FnOnce(&Workspace) -> Result<T> + Send + 'static,
) -> Result<T> {
    let permit = tokio::time::timeout(culprit_core::limits::QUERY_TIMEOUT, WORKSPACE_TASKS.acquire())
        .await
        .map_err(|_| {
            Error::new(ErrorCode::Timeout, "The workspace is busy; try again after the current work finishes")
        })?
        .map_err(|_| Error::internal("Workspace workers are unavailable"))?;
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        update(&ws)
    })
    .await
    .map_err(|_| Error::internal("The workspace task stopped before completing"))?
}

/// The device code being shown, so "Open the sign-in page" opens Microsoft's page and nothing the webview asks for.
#[derive(Default)]
struct SignIn(Mutex<Option<DeviceCodePrompt>>);

#[tauri::command]
fn connections(ws: Ws) -> Result<Vec<ConnectionInfo>> {
    ws.connections()
}

#[tauri::command]
fn parse_connection(input: String, engine: Option<culprit_core::Engine>) -> Result<ParsedConnection> {
    culprit_engine::parse_connection(&input, engine)
}

#[tauri::command]
async fn test_connection(ws: Ws<'_>, input: NewConnection) -> Result<ServerInfo> {
    ws.provider_for(&input).await?.probe().await
}

#[tauri::command]
async fn add_connection(ws: Ws<'_>, input: NewConnection) -> Result<ConnectionInfo> {
    blocking_workspace(ws.inner().clone(), move |ws| ws.add_connection(input)).await
}

#[tauri::command]
async fn remove_connection(ws: Ws<'_>, name: String) -> Result<()> {
    blocking_workspace(ws.inner().clone(), move |ws| ws.remove_connection(&name)).await
}

#[tauri::command]
async fn rename_connection(ws: Ws<'_>, old: String, new: String) -> Result<ConnectionInfo> {
    blocking_workspace(ws.inner().clone(), move |ws| ws.rename_connection(&old, &new)).await
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
async fn set_password(ws: Ws<'_>, name: String, password: String) -> Result<ConnectionInfo> {
    blocking_workspace(ws.inner().clone(), move |ws| ws.set_password(&name, &password)).await
}

#[tauri::command]
async fn import_candidates(ws: Ws<'_>, source: Option<ImportSource>) -> Result<Vec<ImportCandidate>> {
    ws.import_candidates(source)
}

#[tauri::command]
async fn import_connections(ws: Ws<'_>, items: Vec<ImportCandidate>) -> Result<Vec<ConnectionInfo>> {
    blocking_workspace(ws.inner().clone(), move |ws| ws.import_connections(&items)).await
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
async fn add_discovered(ws: Ws<'_>, items: Vec<DiscoveredDatabase>) -> Result<Vec<ConnectionInfo>> {
    blocking_workspace(ws.inner().clone(), move |ws| items.iter().map(|d| ws.add_discovered(d)).collect()).await
}

/// The UI never shows raw plan XML/JSON, and on busy databases it is most of a report's size; keep it on disk only.
fn for_ui(mut report: Report) -> Report {
    // Workspace scan/read paths have already run the full analysis and plan presentation pipeline.
    // Replay evidence stays with the saved report; the UI only needs the prepared view and its provenance.
    report.evidence = None;
    if let Some(workload) = report.snapshot.workload.as_mut() {
        for plan in workload.queries.iter_mut().filter_map(|q| q.plan.as_mut()) {
            plan.raw = None;
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
async fn latest_report(
    ws: Ws<'_>,
    name: String,
    window_minutes: Option<u32>,
    window_days: Option<u32>,
) -> Result<Option<Report>> {
    blocking_workspace(ws.inner().clone(), move |ws| {
        Ok(ws.latest_report(&name, requested_window(window_minutes, window_days))?.map(for_ui))
    })
    .await
}

#[tauri::command]
async fn scan(ws: Ws<'_>, name: String, window_minutes: Option<u32>, window_days: Option<u32>) -> Result<Report> {
    let defaults = SnapshotOptions::default();
    let window = requested_window(window_minutes, window_days).unwrap_or(defaults.window_in_minutes());
    let options = defaults.with_window_minutes(window);
    ws.scan(&name, &options).await.map(for_ui)
}

#[tauri::command]
async fn brief(
    ws: Ws<'_>,
    name: String,
    finding_id: String,
    window_minutes: Option<u32>,
    window_days: Option<u32>,
) -> Result<String> {
    blocking_workspace(ws.inner().clone(), move |ws| {
        let window = requested_window(window_minutes, window_days);
        let selected = ws.latest_report(&name, window)?;
        let report = if selected.is_none() && window.is_some() { ws.latest_report(&name, None)? } else { selected }
            .ok_or_else(|| Error::not_found(format!("No report for {name}")))?;
        let finding = report
            .findings
            .iter()
            .find(|f| f.id == finding_id)
            .ok_or_else(|| Error::not_found(format!("No finding {finding_id}")))?;
        Ok(culprit_engine::agent_brief(&report, finding))
    })
    .await
}

#[tauri::command]
async fn explain(ws: Ws<'_>, name: String, sql: String, mode: ExplainMode, confirmed: bool) -> Result<Plan> {
    ws.explain(&name, &sql, mode, confirmed).await
}

#[tauri::command]
async fn query_plan(
    ws: Ws<'_>,
    name: String,
    query_id: String,
    plan_id: Option<String>,
    report_id: Option<String>,
) -> Result<Plan> {
    let mut plan = ws.query_plan_at(&name, &query_id, plan_id.as_deref(), report_id.as_deref()).await?;
    plan.raw = None;
    Ok(plan)
}

/// The actual plan of a query's last run as the engine kept it; never runs the query.
#[tauri::command]
async fn last_actual_plan(
    ws: Ws<'_>,
    name: String,
    query_id: String,
    plan_id: Option<String>,
    report_id: Option<String>,
) -> Result<Plan> {
    let mut plan = ws.last_actual_plan_at(&name, &query_id, plan_id.as_deref(), report_id.as_deref()).await?;
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
async fn export_finding(
    ws: Ws<'_>,
    name: String,
    finding_id: String,
    format: ExportFormat,
    options: ExportOptions,
) -> Result<ExportFile> {
    blocking_workspace(ws.inner().clone(), move |ws| ws.export_finding(&name, &finding_id, format, &options)).await
}

#[tauri::command]
async fn verify_findings(ws: Ws<'_>, name: String, ids: Vec<String>) -> Result<Verification> {
    ws.verify(&name, &ids).await
}

#[tauri::command]
async fn save_plan_file(name: String, contents: String) -> Result<String> {
    culprit_engine::plans::save_to_downloads(&name, &contents).map(|path| path.display().to_string())
}

/// Copy a picture of part of the window to the clipboard as PNG; nothing is written to disk.
#[tauri::command]
async fn screenshot(webview: tauri::Webview, area: screenshot::Area) -> Result<()> {
    screenshot::copy(&webview, area).await
}

#[tauri::command]
async fn drift(ws: Ws<'_>, a: String, b: String) -> Result<Drift> {
    blocking_workspace(ws.inner().clone(), move |ws| ws.drift(&a, &b)).await
}

/// The index advisor's plan for the displayed scan (older clients may omit the id).
#[tauri::command]
async fn index_advice(ws: Ws<'_>, name: String, report_id: Option<String>) -> Result<culprit_core::IndexAdvice> {
    blocking_workspace(ws.inner().clone(), move |ws| match report_id {
        Some(id) => ws.index_advice_at(&name, &id),
        None => ws.index_advice(&name),
    })
    .await
}

/// The dashboard from the connection's latest report for the window; the app reads sessions itself, every few seconds.
#[tauri::command]
async fn dashboard(
    ws: Ws<'_>,
    name: String,
    window_minutes: Option<u32>,
    window_days: Option<u32>,
    report_id: Option<String>,
) -> Result<culprit_core::Dashboard> {
    if let Some(report_id) = report_id {
        return ws.dashboard_for_report(&name, &report_id);
    }
    ws.dashboard(&name, requested_window(window_minutes, window_days), false).await
}

#[tauri::command]
async fn live(ws: Ws<'_>, name: String) -> Result<LiveState> {
    ws.live(&name).await
}

#[tauri::command]
async fn dashboard_live(
    ws: Ws<'_>,
    name: String,
    dashboard: culprit_core::Dashboard,
) -> Result<culprit_core::DashboardLive> {
    ws.dashboard_live(&name, dashboard).await
}

/// Where the latest scan's database time went, by wait category with the queries behind each.
#[tauri::command]
async fn waits(
    ws: Ws<'_>,
    name: String,
    window_minutes: Option<u32>,
    window_days: Option<u32>,
) -> Result<Option<culprit_core::WaitsReport>> {
    blocking_workspace(ws.inner().clone(), move |ws| ws.waits(&name, requested_window(window_minutes, window_days)))
        .await
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
async fn mark(ws: Ws<'_>, name: String, text: String, at: Option<String>) -> Result<culprit_engine::Mark> {
    blocking_workspace(ws.inner().clone(), move |ws| ws.mark(&name, &text, at.as_deref(), "app")).await
}

#[tauri::command]
async fn unmark(ws: Ws<'_>, name: String, id: String) -> Result<culprit_engine::Mark> {
    blocking_workspace(ws.inner().clone(), move |ws| ws.unmark(&name, &id)).await
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

/// Lint migration or DDL SQL offline against the connection's latest saved snapshot (no database access).
#[tauri::command]
async fn lint_sql(ws: Ws<'_>, name: String, sql: String) -> Result<Vec<culprit_core::Finding>> {
    blocking_workspace(ws.inner().clone(), move |ws| {
        static CACHE: std::sync::Mutex<Option<(String, String, std::sync::Arc<culprit_core::Snapshot>)>> =
            std::sync::Mutex::new(None);
        let latest_id = culprit_engine::reports::latest_summary(&ws.paths().reports_dir(&name), None)
            .map(|summary| summary.id)
            .unwrap_or_default();
        let cached = CACHE.lock().ok().and_then(|guard| {
            guard
                .as_ref()
                .filter(|(cached_name, cached_id, _)| {
                    !latest_id.is_empty() && cached_name == &name && cached_id == &latest_id
                })
                .map(|(_, _, snapshot)| snapshot.clone())
        });
        if let Some(snapshot) = cached {
            return Ok(culprit_engine::lint_sql(&snapshot, &sql));
        }
        let Some(snapshot) = ws.latest_snapshot(&name)? else {
            return Ok(Vec::new());
        };
        let snap = std::sync::Arc::new(snapshot);
        if let Ok(mut guard) = CACHE.lock() {
            *guard = Some((name, snap.id.clone(), std::sync::Arc::clone(&snap)));
        }
        Ok(culprit_engine::lint_sql(&snap, &sql))
    })
    .await
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
async fn save_query(ws: Ws<'_>, query: culprit_engine::saved::SaveQuery) -> Result<culprit_engine::saved::SavedQuery> {
    blocking_workspace(ws.inner().clone(), move |ws| ws.save_query(query)).await
}

#[tauri::command]
async fn delete_saved_query(ws: Ws<'_>, name: String) -> Result<()> {
    blocking_workspace(ws.inner().clone(), move |ws| ws.delete_saved_query(&name)).await
}

#[tauri::command]
async fn saved_query_ran(ws: Ws<'_>, name: String) -> Result<()> {
    blocking_workspace(ws.inner().clone(), move |ws| ws.saved_query_ran(&name)).await
}

#[tauri::command]
async fn set_agent_access(ws: Ws<'_>, name: String, access: AgentAccess) -> Result<ConnectionInfo> {
    blocking_workspace(ws.inner().clone(), move |ws| ws.set_agent_access(&name, access)).await
}

#[tauri::command]
async fn set_query_log(ws: Ws<'_>, name: String, path: Option<String>) -> Result<ConnectionInfo> {
    blocking_workspace(ws.inner().clone(), move |ws| ws.set_query_log(&name, path.as_deref().map(std::path::Path::new)))
        .await
}

/// Choose a SQLite database file in a native dialog, for the new-connection form. Culprit opens the dialog itself, so the
/// window gets back only the path the person picked, as a connection string Culprit reads as SQLite whatever the file is
/// named. `None` when the dialog was cancelled.
#[tauri::command]
async fn choose_database_file(app: AppHandle) -> Result<Option<String>> {
    use tauri_plugin_dialog::DialogExt;
    let (sender, receiver) = tokio::sync::oneshot::channel();
    app.dialog()
        .file()
        .set_title("Choose a SQLite database file")
        .add_filter("SQLite databases", &culprit_engine::DATABASE_FILE_EXTENSIONS)
        .pick_file(move |picked| {
            let _ = sender.send(picked);
        });
    let Some(picked) = receiver.await.ok().flatten() else { return Ok(None) };
    let path = picked.into_path().map_err(|e| Error::invalid(format!("That file can't be read from here: {e}")))?;
    Ok(Some(culprit_engine::database_file_input(&path)))
}

/// Choose the connection's query log in a native file dialog. Culprit opens the dialog itself, so the window never gets
/// file system access: only the file the person picked reaches the workspace, which checks it holds statements before
/// saving it. `None` when the dialog was cancelled.
#[tauri::command]
async fn choose_query_log(app: AppHandle, ws: Ws<'_>, name: String) -> Result<Option<ConnectionInfo>> {
    use tauri_plugin_dialog::DialogExt;
    // Start where the log is, or beside the database file.
    let start = culprit_engine::Workspace::query_log_folder(&ws.connection(&name)?);
    let (sender, receiver) = tokio::sync::oneshot::channel();
    let mut dialog = app
        .dialog()
        .file()
        .set_title("Choose the log your framework writes")
        .add_filter("Logs", &["log", "jsonl", "ndjson", "txt"]);
    if let Some(dir) = start {
        dialog = dialog.set_directory(dir);
    }
    dialog.pick_file(move |picked| {
        let _ = sender.send(picked);
    });
    let Some(picked) = receiver.await.ok().flatten() else { return Ok(None) };
    let path = picked.into_path().map_err(|e| Error::invalid(format!("That file can't be read from here: {e}")))?;
    blocking_workspace(ws.inner().clone(), move |ws| ws.set_query_log(&name, Some(&path)).map(Some)).await
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
        .plugin(tauri_plugin_dialog::init())
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
            dashboard_live,
            parse_plan,
            diff_plans,
            export_plan,
            export_diff,
            export_query_plan,
            export_finding,
            verify_findings,
            save_plan_file,
            screenshot,
            live,
            waits,
            wait_sample,
            run_sql,
            deadlocks,
            parse_deadlock,
            check_statement,
            lint_sql,
            checks,
            catalog,
            saved_queries,
            save_query,
            delete_saved_query,
            saved_query_ran,
            set_agent_access,
            set_query_log,
            choose_query_log,
            choose_database_file,
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

#[cfg(test)]
mod workspace_write_tests {
    use super::*;

    #[tokio::test(flavor = "current_thread")]
    async fn a_contended_store_write_leaves_the_command_runtime_responsive() {
        let dir = std::env::temp_dir().join(format!("culprit-app-write-lock-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let held = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(dir.join("queries.toml.lock"))
            .unwrap();
        held.lock().unwrap();
        let ws = Arc::new(Workspace::open_in(&dir));
        let (started, waiting) = tokio::sync::oneshot::channel();
        let write = tokio::spawn(blocking_workspace(ws.clone(), move |ws| {
            let _ = started.send(());
            ws.save_query(culprit_engine::saved::SaveQuery {
                name: "health".into(),
                sql: "SELECT 1".into(),
                ..Default::default()
            })
        }));
        tokio::time::timeout(Duration::from_secs(2), waiting).await.unwrap().unwrap();
        let runtime_progressed_while_waiting = !write.is_finished();
        drop(held);
        let saved = tokio::time::timeout(Duration::from_secs(2), write).await.unwrap().unwrap().unwrap();
        assert!(runtime_progressed_while_waiting);
        assert_eq!(saved.name, "health");
        assert_eq!(ws.saved_queries().unwrap().len(), 1);
        std::fs::remove_dir_all(dir).unwrap();
    }
}

#[cfg(test)]
mod report_projection_tests {
    #[test]
    fn ui_omits_replay_copies_and_native_text_while_keeping_provenance_and_views() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/mssql-demo.json");
        let mut report = culprit_engine::reading::open(&path).unwrap();
        let collected = report.analysis_input().clone();
        report.evidence.as_mut().unwrap().collected = Some(Box::new(collected));
        let provenance = serde_json::to_value(&report.provenance).unwrap();
        for plan in report.snapshot.workload.as_mut().unwrap().queries.iter_mut().filter_map(|q| q.plan.as_mut()) {
            plan.raw = Some("PRIVATE_NATIVE_PLAN".into());
        }
        let started = std::time::Instant::now();
        let stored_bytes = serde_json::to_vec(&report).unwrap().len();
        let stored_serialization = started.elapsed();
        let ui = super::for_ui(report);
        assert!(ui.evidence.is_none());
        assert_eq!(serde_json::to_value(&ui.provenance).unwrap(), provenance);
        assert!(
            ui.snapshot.queries().iter().filter_map(|q| q.plan.as_ref()).all(|p| p.raw.is_none() && p.view.is_some())
        );
        let projection = serde_json::to_vec(&ui).unwrap();
        assert!(!String::from_utf8_lossy(&projection).contains("PRIVATE_NATIVE_PLAN"));
        assert!(projection.len() < stored_bytes);
        eprintln!(
            "report projection: {stored_bytes} stored bytes, {} UI bytes; stored serialization {stored_serialization:?}",
            projection.len()
        );
    }
}
