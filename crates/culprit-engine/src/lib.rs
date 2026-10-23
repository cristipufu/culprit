pub mod agent;
mod analysis;
pub mod aws;
pub mod catalog;
pub mod changes;
pub mod cloud;
mod connection;
pub mod connstr;
pub mod deadlocks;
pub mod discover;
pub mod entra;
mod event_context;
mod execution;
pub mod export;
pub mod history;
mod http;
pub mod import;
pub mod marks;
mod operation;
pub mod plans;
mod private_fs;
mod query_log;
pub mod reading;
pub mod registry;
mod reporting;
pub mod reports;
pub mod saved;
mod scan;
pub mod secrets;
mod share;
pub mod store;
pub mod trends;

use std::sync::Arc;

use culprit_core::{
    AuthKind, Check, CheckInfo, ConnectionSpec, DeadlockReport, Drift, Engine, Environment, Error, ExplainMode,
    LiveState, LockedCheck, Plan, Provider, Report, Result, ResultSet, ServerInfo, SessionAction, SnapshotOptions,
};
use serde::{Deserialize, Serialize};
use ts_rs::TS;

pub use agent::{AgentAccess, ApprovalOutcome, ApprovalRequest, AuditEntry, AuditOutcome};
pub use culprit_checks::{agent_brief, index_advice};
pub use discover::{Cloud, DiscoveredDatabase};
pub use import::{ImportCandidate, ImportSource};
pub use marks::Mark;
pub use operation::run_blocking;
pub use plans::{PlanFormat, parse_plan};
pub use reports::ReportSummary;
use secrets::SecretStore;
pub use share::{export_diff, export_plan};
use store::{Paths, Store};

#[derive(Serialize, Deserialize, TS, Clone, Debug)]
#[ts(export)]
pub struct ConnectionInfo {
    pub name: String,
    pub engine: Engine,
    pub environment: Environment,
    pub host: String,
    pub port: Option<u16>,
    pub database: Option<String>,
    pub user: Option<String>,
    pub auth: AuthKind,
    /// A password is available (keychain or environment variable). Only meaningful for password authentication.
    pub has_secret: bool,
    pub display: String,
    pub last_report: Option<ReportSummary>,
    /// What coding agents may do with this connection through `culprit mcp`.
    pub agent_access: AgentAccess,
    /// The application's query log scans read the statements from when the database keeps none (SQLite).
    pub query_log: Option<String>,
}

/// A period on a connection's timeline. Everything is optional: by default the last 7 days.
#[derive(Serialize, Deserialize, TS, Clone, Debug, Default, PartialEq)]
#[ts(export)]
pub struct EventsRequest {
    #[serde(default)]
    pub start: Option<String>,
    #[serde(default)]
    pub end: Option<String>,
    /// The last N hours, instead of `start`.
    #[serde(default)]
    pub hours: Option<f64>,
    /// Read cloud metrics too (default: yes, for Amazon RDS and Azure databases).
    #[serde(default)]
    pub cloud: Option<bool>,
    /// Fetch cloud metrics again instead of using the last few minutes' answer.
    #[serde(default)]
    pub refresh: bool,
}

impl EventsRequest {
    /// `(start, end)` in epoch seconds.
    pub fn range(&self) -> Result<(f64, f64)> {
        let parse = |field: &str, value: &Option<String>| -> Result<Option<f64>> {
            value
                .as_deref()
                .map(|v| {
                    culprit_core::changes::epoch(v).ok_or_else(|| {
                        Error::invalid(format!("`{field}` isn't a timestamp: {v}"))
                            .with_hint("Use ISO 8601 with a time zone, e.g. 2026-10-11T14:05:00Z")
                    })
                })
                .transpose()
        };
        let now = chrono::Utc::now().timestamp() as f64;
        let end = parse("end", &self.end)?.unwrap_or(now).min(now);
        let start = match (parse("start", &self.start)?, self.hours.filter(|h| *h > 0.0)) {
            (Some(start), _) => start,
            (None, Some(hours)) => end - hours * 3600.0,
            (None, None) => end - 7.0 * 86_400.0,
        };
        if start >= end {
            return Err(Error::invalid("The period is empty: its start must come before its end"));
        }
        Ok((start, end))
    }
}

/// What changed around a connection over a period.
#[derive(Serialize, Deserialize, TS, Clone, Debug)]
#[ts(export)]
pub struct EventLog {
    pub connection: String,
    pub start: String,
    pub end: String,
    pub events: Vec<culprit_core::ChangeEvent>,
    /// Why something is missing (cloud metrics unavailable, not signed in).
    pub notes: Vec<String>,
}

/// What the UI and CLI send to save a connection. `input` is anything `connstr::parse` understands; explicit fields win over it.
#[derive(Serialize, Deserialize, TS, Clone, Debug, Default)]
#[ts(export)]
pub struct NewConnection {
    pub name: String,
    pub input: String,
    pub environment: Environment,
    pub engine: Option<Engine>,
    pub database: Option<String>,
    pub user: Option<String>,
    pub password: Option<String>,
    /// Read the password from this environment variable at connect time instead of storing it.
    pub password_env: Option<String>,
    pub auth: Option<AuthKind>,
}

/// What a chosen database file is as a connection's input: its path when the extension says SQLite (`.db`, `.sqlite`,
/// `.sqlite3`, `.db3`), else `sqlite:<path>`, so Culprit reads it as SQLite whatever the file is named. The app's file
/// picker hands this to the new-connection form.
pub fn database_file_input(path: &std::path::Path) -> String {
    connection::database_file_input(path)
}

/// The extensions a SQLite database file usually has (the app's file picker filters on them).
pub use connection::DATABASE_FILE_EXTENSIONS;

/// The non-secret result of parsing a pasted connection string, for live feedback while typing.
#[derive(Serialize, Deserialize, TS, Clone, Debug)]
#[ts(export)]
pub struct ParsedConnection {
    pub engine: Option<Engine>,
    pub host: String,
    pub port: Option<u16>,
    pub database: Option<String>,
    pub user: Option<String>,
    pub auth: AuthKind,
    pub has_password: bool,
    pub display: String,
    pub suggested_name: String,
}

/// Builds a provider for a connection; lets an embedding app supply its own engine implementation.
pub type ProviderFactory = Arc<dyn Fn(&ConnectionSpec) -> Result<Box<dyn Provider>> + Send + Sync>;

pub struct Workspace {
    paths: Paths,
    store: Store,
    saved: saved::SavedQueries,
    secrets: Arc<dyn SecretStore>,
    entra: entra::EntraAuth,
    extra_checks: Vec<Arc<dyn Check>>,
    providers: Vec<(Engine, ProviderFactory)>,
}

impl Workspace {
    /// The person's workspace: `CULPRIT_HOME` or the usual config/data directories, secrets in the OS keychain (unless
    /// `CULPRIT_KEYCHAIN=off`).
    pub fn open() -> Result<Workspace> {
        let paths = Paths::resolve()?;
        // Folders older versions created with the default permissions. Only Culprit's own: the reports folder, and the
        // data and config folders when they are the default `…/culprit` ones (never a CULPRIT_HOME someone pointed
        // at a folder of their own).
        private_fs::tighten(&paths.data.join("reports"));
        for dir in [&paths.data, &paths.config] {
            if dir.file_name().is_some_and(|n| n == "culprit") {
                private_fs::tighten(dir);
            }
        }
        Ok(Workspace::with_paths(paths, secrets::default_store()))
    }

    /// A workspace whose connections and reports live in `dir`, without reading `CULPRIT_HOME`. Secrets stay in memory
    /// (never the OS keychain) unless `with_secrets` says otherwise.
    pub fn open_in(dir: impl Into<std::path::PathBuf>) -> Workspace {
        let dir = dir.into();
        Workspace::with_paths(Paths { config: dir.clone(), data: dir }, Arc::new(secrets::Memory::default()))
    }

    fn with_paths(paths: Paths, secrets: Arc<dyn SecretStore>) -> Workspace {
        let store = Store::new(&paths);
        let saved = saved::SavedQueries::new(&paths.config);
        Workspace {
            paths,
            store,
            saved,
            secrets,
            entra: entra::EntraAuth::default(),
            extra_checks: Vec::new(),
            providers: Vec::new(),
        }
    }

    fn connection_service(&self) -> connection::Connections<'_> {
        connection::Connections {
            paths: &self.paths,
            store: &self.store,
            secrets: self.secrets.as_ref(),
            entra: &self.entra,
            providers: &self.providers,
        }
    }

    fn report_service(&self) -> reporting::Reports<'_> {
        reporting::Reports { paths: &self.paths, checks: &self.extra_checks }
    }

    fn execution_service(&self) -> execution::Execution<'_> {
        execution::Execution { connections: self.connection_service(), reports: self.report_service() }
    }

    fn event_service(&self) -> event_context::Events<'_> {
        event_context::Events { connections: self.connection_service(), reports: self.report_service() }
    }

    /// Show Microsoft Entra ID device codes (the CLI prints them, the app opens a dialog). Without a handler, connections
    /// that need a sign-in fail with a hint instead of waiting for a person.
    pub fn with_sign_in_handler(self, handler: entra::SignInHandler) -> Workspace {
        self.entra.set_handler(Some(handler));
        self
    }

    /// Stop a device-code sign-in that is waiting for the person.
    pub fn cancel_sign_in(&self) {
        self.entra.cancel();
    }

    /// Forget device-code sign-ins: in-memory tokens and stored refresh tokens for every tenant the saved connections use.
    /// The Azure CLI's own login is untouched (`az logout`). Returns how many stored sign-ins were removed.
    pub fn sign_out(&self) -> Result<usize> {
        self.connection_service().sign_out()
    }

    /// Keep passwords and sign-in tokens somewhere else (a vault, a test double).
    pub fn with_secrets(mut self, secrets: Arc<dyn SecretStore>) -> Workspace {
        self.secrets = secrets;
        self
    }

    /// Use `factory` instead of the built-in provider for `engine` (a wrapped driver, a remote collector, a test double).
    pub fn with_provider(mut self, engine: Engine, factory: ProviderFactory) -> Workspace {
        self.providers.retain(|(e, _)| *e != engine);
        self.providers.push((engine, factory));
        self
    }

    /// Plug in a check from outside Culprit. It runs, ranks and locks exactly like a built-in check.
    pub fn with_check(mut self, check: Arc<dyn Check>) -> Workspace {
        self.extra_checks.push(check);
        self
    }

    pub fn checks(&self) -> Vec<CheckInfo> {
        culprit_checks::infos_with(&self.extra_checks)
    }

    pub fn locked_checks(&self, server: &ServerInfo) -> Vec<LockedCheck> {
        culprit_checks::locked_with(server, &self.extra_checks)
    }

    /// Re-run the checks on a snapshot or report file, so improved checks apply to old scans and shared snapshots.
    pub fn analyze_file(&self, path: &std::path::Path) -> Result<Report> {
        self.report_service().analyze_file(path)
    }

    pub fn paths(&self) -> &Paths {
        &self.paths
    }

    pub fn connections(&self) -> Result<Vec<ConnectionInfo>> {
        self.connection_service().connections()
    }

    pub fn connection(&self, name: &str) -> Result<ConnectionInfo> {
        self.connection_service().connection(name)
    }

    /// Read the statements from this log on every scan of `name` (`None` stops). The file must exist; it is kept as an
    /// absolute path, so a scan from any directory finds it.
    /// It must hold statements in a format Culprit reads, so a wrong file (or one the framework writes without SQL) is
    /// said now rather than at the next scan. `None` stops reading one, and stops looking for one too.
    pub fn set_query_log(&self, name: &str, path: Option<&std::path::Path>) -> Result<ConnectionInfo> {
        self.connection_service().set_query_log(name, path)
    }

    /// The log a file database's application writes beside it, found and saved on the connection when it has none and
    /// the person didn't remove one ([`query_log::discover`]).
    fn discover_query_log(&self, name: &str) -> Option<std::path::PathBuf> {
        self.connection_service().discover_query_log(name)
    }

    /// Where to look for a connection's query log: beside the log it reads, else beside its database file (the app's
    /// file picker starts there). `None` for a connection with neither.
    pub fn query_log_folder(info: &ConnectionInfo) -> Option<std::path::PathBuf> {
        connection::Connections::query_log_folder(info)
    }

    pub fn add_connection(&self, new: NewConnection) -> Result<ConnectionInfo> {
        self.connection_service().add_connection(new)
    }

    /// Store the password for a password-authenticated connection in the OS keychain.
    pub fn set_password(&self, name: &str, password: &str) -> Result<ConnectionInfo> {
        self.connection_service().set_password(name, password)
    }

    pub fn remove_connection(&self, name: &str) -> Result<()> {
        self.connection_service().remove_connection(name)
    }

    /// Mark a connection production, staging, development or local. Production refuses session kills and asks before
    /// anything that changes data.
    pub fn set_environment(&self, name: &str, environment: Environment) -> Result<ConnectionInfo> {
        self.connection_service().set_environment(name, environment)
    }

    /// Rename (re-alias) a connection, carrying its keychain secret and report history along.
    pub fn rename_connection(&self, old: &str, new: &str) -> Result<ConnectionInfo> {
        self.connection_service().rename_connection(old, new)
    }

    /// Azure SQL and Azure Database for PostgreSQL / MySQL flexible-server databases the signed-in Azure CLI account can see,
    /// marked with the saved connection that already points at each.
    pub async fn discover_azure(&self, terms: &[String]) -> Result<Vec<DiscoveredDatabase>> {
        self.connection_service().discover_azure(terms).await
    }

    /// RDS instances and Aurora clusters the signed-in AWS CLI can see (all enabled regions when `regions` is empty).
    pub async fn discover_aws(&self, regions: &[String], terms: &[String]) -> Result<Vec<DiscoveredDatabase>> {
        self.connection_service().discover_aws(regions, terms).await
    }

    /// Save a discovered database. Azure SQL: Entra ID. Azure flexible servers: Entra ID when the server allows it, otherwise
    /// the admin login with its password set later. AWS: IAM when the instance allows it, otherwise password auth with the
    /// password set later (`set_password`). Nothing secret is stored here.
    pub fn add_discovered(&self, d: &DiscoveredDatabase) -> Result<ConnectionInfo> {
        self.connection_service().add_discovered(d)
    }

    /// Connections other tools on this machine keep (all tools when `source` is `None`), marked with the saved connection that
    /// already points at the same database. Reads metadata only; never passwords.
    pub fn import_candidates(&self, source: Option<ImportSource>) -> Result<Vec<ImportCandidate>> {
        self.connection_service().import_candidates(source)
    }

    /// Connections in one file another tool wrote (`settings.json`, `data-sources.json`, `dataSources.xml`).
    pub fn import_file(&self, path: &std::path::Path, source: Option<ImportSource>) -> Result<Vec<ImportCandidate>> {
        self.connection_service().import_file(path, source)
    }

    /// Save imported connections. All are checked before any is written; password connections ask for their password later.
    pub fn import_connections(&self, items: &[ImportCandidate]) -> Result<Vec<ConnectionInfo>> {
        self.connection_service().import_connections(items)
    }

    /// Open a provider for a saved connection, resolving the password from the keychain or a fresh Entra token.
    pub async fn provider(&self, name: &str) -> Result<Box<dyn Provider>> {
        self.connection_service().provider(name).await
    }

    /// Open a provider for an unsaved connection (used by "Test" before saving).
    pub async fn provider_for(&self, new: &NewConnection) -> Result<Box<dyn Provider>> {
        self.connection_service().provider_for(new).await
    }

    pub async fn probe(&self, name: &str) -> Result<ServerInfo> {
        self.connection_service().probe(name).await
    }

    pub async fn scan(&self, name: &str, options: &SnapshotOptions) -> Result<Report> {
        self.scan_with_log(name, options, None).await
    }

    /// [`Workspace::scan`], reading the statements and their times from `query_log` (a log the application's framework
    /// already writes) instead of the connection's saved one, when the database keeps no history of its own (SQLite).
    pub async fn scan_with_log(
        &self,
        name: &str,
        options: &SnapshotOptions,
        query_log: Option<&std::path::Path>,
    ) -> Result<Report> {
        let saved = self.store.get(name)?.query_log.map(std::path::PathBuf::from);
        let found = if query_log.is_none() && saved.is_none() { self.discover_query_log(name) } else { None };
        let query_log = query_log.or(saved.as_deref()).or(found.as_deref());
        let scan = scan::Scan::begin(self.paths.reports_dir(name), &self.extra_checks);
        let provider = self.provider(name).await?;
        let snapshot = self.connection_service().forget_rejected(name, provider.snapshot(name, options).await)?;
        let log = query_log.map(|path| scan::QueryLog { path, discovered: found.is_some() });
        scan.finish(provider.as_ref(), snapshot, name, options, log).await
    }

    /// The newest saved report, or the newest one scanned with a window of `window_minutes` when given, read the way a
    /// scan taken now would read the same data ([`Workspace::analyze_file`]).
    pub fn latest_report(&self, name: &str, window_minutes: Option<u32>) -> Result<Option<Report>> {
        self.report_service().latest_report(name, window_minutes)
    }

    /// The newest saved snapshot for `name`, without re-parsing execution plans or re-running the check pipeline.
    pub fn latest_snapshot(&self, name: &str) -> Result<Option<culprit_core::Snapshot>> {
        self.report_service().latest_snapshot(name)
    }

    pub fn report_history(&self, name: &str) -> Result<Vec<ReportSummary>> {
        self.report_service().report_history(name)
    }

    /// The digest of a scan: `report` (a report or snapshot file) or the connection's latest saved scan, compared with
    /// `since` (a file) or the saved scan before it. Both are re-checked with the current checks, so a finding means
    /// the same thing on each side. `top` caps each list; `top_queries` is how many of the heaviest queries it lists.
    pub fn digest(
        &self,
        name: Option<&str>,
        report: Option<&std::path::Path>,
        since: Option<&std::path::Path>,
        top: usize,
        top_queries: usize,
    ) -> Result<culprit_core::Digest> {
        self.report_service().digest(name, report, since, top, top_queries)
    }

    /// The index advisor's plan for a connection, from its latest saved report re-checked with the current checks (so
    /// advice from an older report still links to today's findings).
    pub fn index_advice(&self, name: &str) -> Result<culprit_core::IndexAdvice> {
        self.report_service().index_advice(name)
    }

    /// The index advisor's plan for a report already in hand (a saved file, a fixture).
    pub fn index_advice_for(&self, report: Report) -> culprit_core::IndexAdvice {
        self.report_service().index_advice_for(report)
    }

    /// Advice for the exact scan displayed by a client, including its workload window. Never falls back to a
    /// different scan; ids must belong to this connection's report directory.
    pub fn index_advice_at(&self, name: &str, report_id: &str) -> Result<culprit_core::IndexAdvice> {
        self.report_service().index_advice_at(name, report_id)
    }

    /// The dashboard for a connection's latest report (the one scanned with a window of `window_minutes`, when given).
    /// With `live`, sessions are read once too; if that fails, the "right now" tile says why and the rest still comes
    /// back.
    pub async fn dashboard(
        &self,
        name: &str,
        window_minutes: Option<u32>,
        live: bool,
    ) -> Result<culprit_core::Dashboard> {
        self.execution_service().dashboard(name, window_minutes, live).await
    }

    /// The dashboard for the report already displayed by the caller, even if a newer scan was saved in the meantime.
    pub fn dashboard_for_report(&self, name: &str, report_id: &str) -> Result<culprit_core::Dashboard> {
        self.execution_service().dashboard_for_report(name, report_id)
    }

    /// Refresh sessions without reloading another report or selected window. One read, with Rust's shared assessment.
    pub async fn dashboard_live(
        &self,
        name: &str,
        dashboard: culprit_core::Dashboard,
    ) -> Result<culprit_core::DashboardLive> {
        self.execution_service().dashboard_live(name, dashboard).await
    }

    /// What differs between the latest scans of two connections: tables, columns, indexes, settings, version.
    pub fn drift(&self, a: &str, b: &str) -> Result<Drift> {
        self.report_service().drift(a, b)
    }

    /// Explain without running the statement, or measure it with Analyze. Production measurements only run proven reads.
    pub async fn explain(&self, name: &str, sql: &str, mode: ExplainMode, confirmed: bool) -> Result<Plan> {
        self.execution_service().explain(name, sql, mode, confirmed).await
    }

    /// Estimated plan of `sql` with one or more virtual indexes (`CREATE INDEX ...`) in planner memory (`hypopg`).
    pub async fn explain_hypothetical(&self, name: &str, sql: &str, indexes: &[String]) -> Result<Plan> {
        self.execution_service().explain_hypothetical(name, sql, indexes).await
    }

    /// The actual plan of a query's last run as the engine kept it (SQL Server 2019+ with LAST_QUERY_PLAN_STATS).
    /// Never runs the query; other engines say why they can't and how to measure it instead.
    pub async fn last_actual_plan(&self, name: &str, query_id: &str, plan_id: Option<&str>) -> Result<Plan> {
        self.execution_service().last_actual_plan(name, query_id, plan_id).await
    }

    pub async fn last_actual_plan_at(
        &self,
        name: &str,
        query_id: &str,
        plan_id: Option<&str>,
        report_id: Option<&str>,
    ) -> Result<Plan> {
        self.execution_service().last_actual_plan_at(name, query_id, plan_id, report_id).await
    }

    /// How far along a session's running statement is: per-operator progress on SQL Server, the estimated plan with
    /// elapsed time and command/stage progress on Postgres and MySQL. Reads only; works on production too.
    pub async fn query_progress(
        &self,
        name: &str,
        session: &str,
        with_plan: bool,
    ) -> Result<culprit_core::QueryProgress> {
        self.execution_service().query_progress(name, session, with_plan).await
    }

    /// A plan for a query from the latest report: the stored plan when the engine keeps them (Query Store), otherwise an
    /// estimated plan of the query text. Never executes the statement.
    pub async fn query_plan(&self, name: &str, query_id: &str, plan_id: Option<&str>) -> Result<Plan> {
        self.execution_service().query_plan(name, query_id, plan_id).await
    }

    /// Fetch a plan using the exact report/window the caller is displaying.
    pub async fn query_plan_at(
        &self,
        name: &str,
        query_id: &str,
        plan_id: Option<&str>,
        report_id: Option<&str>,
    ) -> Result<Plan> {
        self.execution_service().query_plan_at(name, query_id, plan_id, report_id).await
    }

    /// Where the latest scan's database time went, by wait category, with the queries behind each. `None` when the scan
    /// recorded no waits (PostgreSQL without track_io_timing, logins that can't read wait statistics).
    pub fn waits(&self, name: &str, window_minutes: Option<u32>) -> Result<Option<culprit_core::WaitsReport>> {
        self.report_service().waits(name, window_minutes)
    }

    /// What the server waits on right now: a sample `seconds` long (capped at 60). Reads statistics views only.
    pub async fn wait_sample(&self, name: &str, seconds: u32) -> Result<culprit_core::WaitSample> {
        self.execution_service().wait_sample(name, seconds).await
    }

    /// Sessions, blocking and waits, plus what Live may do to other sessions here (nothing on production).
    pub async fn live(&self, name: &str) -> Result<LiveState> {
        self.execution_service().live(name).await
    }

    /// Recent deadlocks on `name` (newest first, explained), with fixes linked to the latest report's findings.
    pub async fn deadlocks(&self, name: &str, since_hours: f64) -> Result<DeadlockReport> {
        self.execution_service().deadlocks(name, since_hours).await
    }

    /// What changed between two periods: each query's runs and time per run before and after, judged with the same
    /// rules everywhere. Query Store answers from its own intervals (any period it still keeps); sources that only keep
    /// running totals answer from Culprit's saved scans, so periods snap to the scans nearest to what was asked.
    pub async fn changes(&self, name: &str, request: &changes::ChangeRequest) -> Result<culprit_core::Changes> {
        self.event_service().changes(name, request).await
    }

    /// What changed around a connection over a period, oldest first: marks, schema, index and settings changes,
    /// restarts, upgrades and, for cloud databases, throttling. Reads saved scans and the cloud CLI; never the database.
    pub async fn events(&self, name: &str, request: &EventsRequest) -> Result<EventLog> {
        self.event_service().events(name, request).await
    }

    /// Put a moment on a connection's timeline (a deploy, a migration) so charts and What changed line queries up with it.
    pub fn mark(&self, name: &str, text: &str, at: Option<&str>, by: &str) -> Result<marks::Mark> {
        self.event_service().mark(name, text, at, by)
    }

    pub fn marks(&self, name: &str) -> Result<Vec<marks::Mark>> {
        self.event_service().marks(name)
    }

    pub fn unmark(&self, name: &str, id: &str) -> Result<marks::Mark> {
        self.event_service().unmark(name, id)
    }

    /// What the cloud measured for a connection's server (CloudWatch, Azure Monitor) and when it held the server back,
    /// through the person's own `aws` / `az` sign-in. Cached for a few minutes.
    pub async fn cloud(&self, name: &str, request: &EventsRequest) -> Result<culprit_core::CloudSignals> {
        self.event_service().cloud(name, request).await
    }

    /// Cancel another session's statement or end the session. Refused on production connections, whatever the caller says.
    pub async fn interrupt_session(&self, name: &str, session: &str, action: SessionAction) -> Result<()> {
        self.execution_service().interrupt_session(name, session, action).await
    }

    /// `confirmed` is the caller's explicit, per-action consent to execute on production.
    /// Run a statement. Reads (as Culprit's classifier proves them) run in a read-only transaction and never ask.
    /// Anything else commits; on production only when the person confirmed it, otherwise this returns
    /// `ConfirmationRequired` with what the statement does, for the question to ask.
    pub async fn run(&self, name: &str, sql: &str, limit: usize, confirmed: bool) -> Result<ResultSet> {
        self.execution_service().run(name, sql, limit, confirmed).await
    }

    /// Run a statement that must only read (agents, previews, the editor's reads). Refused unless Culprit's classifier
    /// proves it reads; Postgres and MySQL run it in a read-only transaction, and SQL Server, which has none, inside a
    /// transaction that is always rolled back.
    pub async fn run_read(&self, name: &str, sql: &str, limit: usize) -> Result<ResultSet> {
        self.execution_service().run_read(name, sql, limit).await
    }

    /// Databases, tables, views and columns for editor completion, read live through the read-only run path.
    pub async fn catalog(&self, name: &str) -> Result<catalog::Catalog> {
        self.execution_service().catalog(name).await
    }

    pub fn saved_queries(&self) -> Result<Vec<saved::SavedQuery>> {
        self.saved.list()
    }

    pub fn saved_query(&self, name: &str) -> Result<saved::SavedQuery> {
        self.saved.get(name)
    }

    pub fn save_query(&self, query: saved::SaveQuery) -> Result<saved::SavedQuery> {
        self.saved.put(query)
    }

    pub fn delete_saved_query(&self, name: &str) -> Result<()> {
        self.saved.delete(name)
    }

    /// Run a saved query on `connection` (default: the one it was saved for), with the same rules as `run`.
    pub async fn run_saved(
        &self,
        name: &str,
        connection: Option<&str>,
        limit: usize,
        confirmed: bool,
    ) -> Result<ResultSet> {
        let query = self.saved.get(name)?;
        let target = connection.map(str::to_string).or(query.connection.clone()).ok_or_else(|| {
            Error::invalid(format!("'{}' isn't tied to a connection", query.name))
                .with_hint("Pass the connection to run it on.")
        })?;
        let result = self.run(&target, &query.sql, limit, confirmed).await?;
        self.saved.touch(&query.name)?;
        Ok(result)
    }

    /// Record that a saved query ran (the editor runs the text itself).
    pub fn saved_query_ran(&self, name: &str) -> Result<()> {
        self.saved.touch(name)
    }
}

/// [`Workspace::drift`] for two reports already in hand (saved files, fixtures).
pub fn drift_reports(a: &Report, b: &Report) -> Result<Drift> {
    reporting::drift_reports(a, b)
}

/// The dashboard for a report already in hand (a saved file, a fixture), without a live read.
pub fn dashboard_report(report: &Report) -> culprit_core::Dashboard {
    reporting::dashboard_report(report)
}

/// Where a snapshot's database time went, by wait category with the queries behind each (saved files, fixtures).
pub fn explain_waits(snapshot: &culprit_core::Snapshot) -> Option<culprit_core::WaitsReport> {
    reporting::explain_waits(snapshot)
}

/// Lint migration or DDL SQL offline against a saved snapshot's catalog, table sizes and indexes.
pub fn lint_sql(snapshot: &culprit_core::Snapshot, sql: &str) -> Vec<culprit_core::Finding> {
    culprit_checks::lint_sql(snapshot, sql)
}

/// Apply all safe DDL auto-fixes recommended by [`lint_sql`] to `sql`, returning `(rewritten_sql, fixes_applied)`.
pub fn apply_sql_fixes(snapshot: &culprit_core::Snapshot, sql: &str) -> (String, usize) {
    culprit_checks::apply_sql_fixes(snapshot, sql)
}

/// PostgreSQL and MySQL providers already run reads inside a read-only transaction. SQL Server has no read-only
/// transactions, so a read runs inside one that is always rolled back.
pub fn read_only_batch(engine: Engine, statement: &str) -> String {
    execution::read_only_batch(engine, statement)
}

/// What `input` connects to, read as `engine` wherever the text leaves the engine open (see `connstr::parse_as`).
pub fn parse_connection(input: &str, engine: Option<Engine>) -> Result<ParsedConnection> {
    connection::parse_connection(input, engine)
}

#[cfg(test)]
mod tests {
    use super::*;
    use culprit_core::ErrorCode;

    fn temp_home(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("culprit-{tag}-{}-{:?}", std::process::id(), std::thread::current().id()))
    }

    #[test]
    fn passwords_live_in_the_workspace_secret_store_not_the_keychain_or_the_file() {
        let home = temp_home("secrets");
        let ws = Workspace::open_in(&home);
        ws.add_connection(NewConnection {
            name: "shop".into(),
            input: "postgres://app:s3cret@db.example:5432/shop".into(),
            ..Default::default()
        })
        .unwrap();
        assert!(ws.connection("shop").unwrap().has_secret);
        assert_eq!(ws.secrets.get("shop").unwrap().as_deref(), Some("s3cret"));
        let file = std::fs::read_to_string(home.join("connections.toml")).unwrap();
        assert!(!file.contains("s3cret"));

        ws.rename_connection("shop", "shop-main").unwrap();
        assert_eq!(ws.secrets.get("shop").unwrap(), None);
        assert_eq!(ws.secrets.get("shop-main").unwrap().as_deref(), Some("s3cret"));
        ws.remove_connection("shop-main").unwrap();
        assert_eq!(ws.secrets.get("shop-main").unwrap(), None);
        let _ = std::fs::remove_dir_all(home);
    }

    /// Records interrupts; supports cancelling statements only.
    struct Sessions(Arc<std::sync::Mutex<Vec<(String, SessionAction)>>>);

    #[async_trait::async_trait]
    impl Provider for Sessions {
        fn engine(&self) -> Engine {
            Engine::Postgres
        }
        async fn probe(&self) -> Result<ServerInfo> {
            Err(Error::new(ErrorCode::Unsupported, "test double"))
        }
        async fn snapshot(&self, _: &str, _: &SnapshotOptions) -> Result<culprit_core::Snapshot> {
            Err(Error::new(ErrorCode::Unsupported, "test double"))
        }
        async fn explain(&self, _: &str, _: ExplainMode) -> Result<Plan> {
            Err(Error::new(ErrorCode::Unsupported, "test double"))
        }
        async fn live(&self) -> Result<LiveState> {
            Ok(LiveState {
                notes: Vec::new(),
                taken_at: String::new(),
                sessions: Vec::new(),
                waits: Vec::new(),
                session_actions: Vec::new(),
                session_actions_note: None,
                blocking: None,
            })
        }
        async fn run(&self, _: &str, _: usize) -> Result<ResultSet> {
            Err(Error::new(ErrorCode::Unsupported, "test double"))
        }
        fn session_actions(&self) -> Vec<SessionAction> {
            vec![SessionAction::Cancel]
        }
        async fn interrupt_session(&self, session: &str, action: SessionAction) -> Result<()> {
            self.0.lock().unwrap().push((session.to_string(), action));
            Ok(())
        }
    }

    #[tokio::test]
    async fn sessions_are_never_interrupted_on_production() {
        let home = temp_home("sessions");
        let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
        let recorded = calls.clone();
        let ws = Workspace::open_in(&home).with_provider(
            Engine::Postgres,
            Arc::new(move |_| Ok(Box::new(Sessions(recorded.clone())) as Box<dyn Provider>)),
        );
        for (name, environment) in [("shop-dev", Environment::Development), ("shop-prod", Environment::Production)] {
            ws.add_connection(NewConnection {
                name: name.into(),
                input: "postgres://app@db.example:5432/shop".into(),
                environment,
                auth: Some(AuthKind::None),
                ..Default::default()
            })
            .unwrap();
        }

        let prod = ws.live("shop-prod").await.unwrap();
        assert!(prod.session_actions.is_empty() && prod.session_actions_note.is_some());
        let e = ws.interrupt_session("shop-prod", "42", SessionAction::Cancel).await.unwrap_err();
        assert_eq!(e.code, ErrorCode::PermissionDenied);

        assert_eq!(ws.live("shop-dev").await.unwrap().session_actions, vec![SessionAction::Cancel]);
        assert_eq!(
            ws.interrupt_session("shop-dev", "42", SessionAction::Terminate).await.unwrap_err().code,
            ErrorCode::Unsupported
        );
        assert_eq!(
            ws.interrupt_session("shop-dev", "4 2", SessionAction::Cancel).await.unwrap_err().code,
            ErrorCode::InvalidInput
        );
        ws.interrupt_session("shop-dev", "42", SessionAction::Cancel).await.unwrap();
        assert_eq!(*calls.lock().unwrap(), vec![("42".to_string(), SessionAction::Cancel)]);
        let _ = std::fs::remove_dir_all(home);
    }
}
