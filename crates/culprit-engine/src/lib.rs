pub mod connstr;
pub mod discover;
pub mod entra;
mod private_fs;
pub mod registry;
pub mod reports;
pub mod store;

use std::time::Instant;

use culprit_core::{
    AuthKind, ConnectionSpec, Engine, Environment, Error, ErrorCode, ExplainMode, LiveState, Plan, Provider, Report,
    Result, ResultSet, ServerInfo, SnapshotOptions,
};
use serde::{Deserialize, Serialize};
use ts_rs::TS;

pub use culprit_checks::{agent_brief, infos as checks, locked as locked_checks};
pub use discover::DiscoveredDatabase;
pub use reports::ReportSummary;
use store::{Paths, Store, StoredConnection};

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
    pub display: String,
    pub last_report: Option<ReportSummary>,
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

pub struct Workspace {
    paths: Paths,
    store: Store,
}

impl Workspace {
    pub fn open() -> Result<Workspace> {
        let paths = Paths::resolve()?;
        let store = Store::new(&paths);
        Ok(Workspace { paths, store })
    }

    pub fn paths(&self) -> &Paths {
        &self.paths
    }

    pub fn connections(&self) -> Result<Vec<ConnectionInfo>> {
        self.store.list()?.into_iter().map(|(name, stored)| Ok(self.info(&name, &stored))).collect()
    }

    pub fn connection(&self, name: &str) -> Result<ConnectionInfo> {
        let stored = self.store.get(name)?;
        Ok(self.info(name, &stored))
    }

    fn info(&self, name: &str, stored: &StoredConnection) -> ConnectionInfo {
        ConnectionInfo {
            name: name.to_string(),
            engine: stored.engine,
            environment: stored.environment,
            host: stored.host.clone(),
            port: stored.port,
            database: stored.database.clone(),
            user: stored.user.clone(),
            auth: stored.auth,
            display: connstr::describe(&stored.spec()),
            last_report: reports::latest_summary(&self.paths.reports_dir(name), None),
        }
    }

    pub fn add_connection(&self, new: NewConnection) -> Result<ConnectionInfo> {
        store::validate_name(&new.name)?;
        let spec = merge(&new)?;
        let engine = spec.engine.ok_or_else(|| {
            Error::invalid("Can't tell which database engine this is")
                .with_hint("Choose Postgres or SQL Server explicitly")
        })?;
        let password_env = new.password_env.clone().filter(|v| !v.is_empty());
        if let Some(password) = spec.password.as_deref().filter(|p| !p.is_empty() && password_env.is_none()) {
            store::set_secret(&new.name, password)?;
        }
        let stored = StoredConnection {
            engine,
            environment: new.environment,
            host: spec.host,
            port: spec.port,
            database: spec.database,
            user: spec.user,
            auth: spec.auth,
            password_env,
            options: spec.options,
        };
        self.store.put(&new.name, stored)?;
        self.connection(&new.name)
    }

    pub fn remove_connection(&self, name: &str) -> Result<()> {
        self.store.remove(name)
    }

    /// Rename (re-alias) a connection, carrying its keychain secret and report history along.
    pub fn rename_connection(&self, old: &str, new: &str) -> Result<ConnectionInfo> {
        self.store.rename(old, new)?;
        let (from, to) = (self.paths.reports_dir(old), self.paths.reports_dir(new));
        if from.exists() && !to.exists() {
            std::fs::rename(&from, &to).map_err(|e| Error::internal(format!("{}: {e}", from.display())))?;
        }
        self.connection(new)
    }

    /// Azure SQL databases the signed-in Azure CLI account can see, marked with the saved connection that already points at each.
    pub async fn discover_azure(&self, terms: &[String]) -> Result<Vec<DiscoveredDatabase>> {
        let mut found = discover::azure_sql(terms).await?;
        let saved = self.store.list()?;
        for d in &mut found {
            d.saved_as = saved
                .iter()
                .find(|(_, c)| {
                    c.host.eq_ignore_ascii_case(&d.host)
                        && c.database.as_deref().is_some_and(|db| db.eq_ignore_ascii_case(&d.database))
                })
                .map(|(name, _)| name.clone());
        }
        Ok(found)
    }

    /// Save a discovered database as an Entra ID connection; nothing secret is stored.
    pub fn add_discovered(&self, d: &DiscoveredDatabase) -> Result<ConnectionInfo> {
        self.add_connection(NewConnection {
            name: d.alias.clone(),
            input: format!(
                "Server=tcp:{},1433;Database={};Authentication=Active Directory Default;Encrypt=True",
                d.host, d.database
            ),
            environment: d.environment,
            engine: Some(d.engine),
            auth: Some(AuthKind::Entra),
            ..Default::default()
        })
    }

    /// Open a provider for a saved connection, resolving the password from the keychain or a fresh Entra token.
    pub async fn provider(&self, name: &str) -> Result<Box<dyn Provider>> {
        let stored = self.store.get(name)?;
        let mut spec = stored.spec();
        resolve_secrets(name, stored.password_env.as_deref(), &mut spec).await?;
        registry::provider(&spec)
    }

    /// Open a provider for an unsaved connection (used by "Test" before saving).
    pub async fn provider_for(&self, new: &NewConnection) -> Result<Box<dyn Provider>> {
        let mut spec = merge(new)?;
        spec.environment = new.environment;
        if spec.auth == AuthKind::Entra {
            let engine = spec.engine.ok_or_else(|| Error::invalid("Can't tell which database engine this is"))?;
            spec.token = Some(entra::token_for(engine, &spec.host).await?);
        }
        registry::provider(&spec)
    }

    pub async fn probe(&self, name: &str) -> Result<ServerInfo> {
        self.provider(name).await?.probe().await
    }

    pub async fn scan(&self, name: &str, options: &SnapshotOptions) -> Result<Report> {
        let started = Instant::now();
        let taken_at = chrono::Utc::now();
        let provider = self.provider(name).await?;
        let mut snapshot = provider.snapshot(name, options).await?;
        snapshot.id = taken_at.format("%Y%m%dT%H%M%S%.3fZ").to_string();
        snapshot.taken_at = taken_at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        snapshot.connection = name.to_string();
        snapshot.duration_ms = started.elapsed().as_secs_f64() * 1000.0;
        let report = culprit_checks::analyze(snapshot);
        reports::save(&self.paths.reports_dir(name), &report)?;
        Ok(report)
    }

    /// The newest saved report, or the newest one scanned with `window_days` when given.
    pub fn latest_report(&self, name: &str, window_days: Option<u32>) -> Result<Option<Report>> {
        reports::latest(&self.paths.reports_dir(name), window_days)
    }

    pub fn report_history(&self, name: &str) -> Result<Vec<ReportSummary>> {
        reports::list(&self.paths.reports_dir(name))
    }

    /// `confirmed` is the caller's explicit, per-action consent to execute on production (only matters for Analyze).
    pub async fn explain(&self, name: &str, sql: &str, mode: ExplainMode, confirmed: bool) -> Result<Plan> {
        let info = self.connection(name)?;
        guard_execution(&info, mode == ExplainMode::Analyze, confirmed)?;
        self.provider(name).await?.explain(sql, mode).await
    }

    pub async fn live(&self, name: &str) -> Result<LiveState> {
        self.provider(name).await?.live().await
    }

    /// `confirmed` is the caller's explicit, per-action consent to execute on production.
    pub async fn run(&self, name: &str, sql: &str, limit: usize, confirmed: bool) -> Result<ResultSet> {
        let info = self.connection(name)?;
        guard_execution(&info, true, confirmed)?;
        self.provider(name).await?.run(sql, limit).await
    }
}

fn guard_execution(info: &ConnectionInfo, executes: bool, confirmed: bool) -> Result<()> {
    if !executes || !info.environment.is_production() || confirmed {
        return Ok(());
    }
    Err(Error::new(
        ErrorCode::PermissionDenied,
        format!("'{}' is a production connection; this would execute the statement", info.name),
    )
    .with_hint("Confirm explicitly (--yes-execute-on-production in the CLI) or use an estimated plan instead"))
}

async fn resolve_secrets(name: &str, password_env: Option<&str>, spec: &mut ConnectionSpec) -> Result<()> {
    match (spec.auth, password_env) {
        (AuthKind::Entra, _) => {
            let engine = spec.engine.ok_or_else(|| Error::invalid("Can't tell which database engine this is"))?;
            spec.token = Some(entra::token_for(engine, &spec.host).await?);
        }
        (AuthKind::Password, Some(var)) => {
            let value = std::env::var(var).map_err(|_| {
                Error::new(ErrorCode::AuthFailed, format!("Environment variable {var} is not set"))
                    .with_hint(format!("Connection '{name}' reads its password from {var}"))
            })?;
            spec.password = Some(value);
        }
        (AuthKind::Password, None) => spec.password = store::get_secret(name)?,
        (AuthKind::None, _) => {}
    }
    Ok(())
}

fn merge(new: &NewConnection) -> Result<ConnectionSpec> {
    let mut spec = connstr::parse(&new.input)?;
    if let Some(var) = new.password_env.as_deref().filter(|v| !v.is_empty()) {
        spec.password = std::env::var(var).ok().or(spec.password);
    }
    if let Some(engine) = new.engine {
        spec.engine = Some(engine);
    }
    if let Some(db) = new.database.clone().filter(|d| !d.is_empty()) {
        spec.database = Some(db);
    }
    if let Some(user) = new.user.clone().filter(|u| !u.is_empty()) {
        spec.user = Some(user);
    }
    if let Some(password) = new.password.clone().filter(|p| !p.is_empty()) {
        spec.password = Some(password);
    }
    if let Some(auth) = new.auth {
        spec.auth = auth;
    }
    spec.environment = new.environment;
    Ok(spec)
}

/// Re-run the checks on a snapshot or report file, so improved checks apply to old scans and shared snapshots.
pub fn analyze_file(path: &std::path::Path) -> Result<Report> {
    let bytes = std::fs::read(path).map_err(|e| Error::not_found(format!("{}: {e}", path.display())))?;
    let value: serde_json::Value =
        serde_json::from_slice(&bytes).map_err(|e| Error::invalid(format!("{}: {e}", path.display())))?;
    let snapshot = value.get("snapshot").cloned().unwrap_or(value);
    let snapshot: culprit_core::Snapshot = serde_json::from_value(snapshot)
        .map_err(|e| Error::invalid(format!("{} is not a Culprit snapshot or report: {e}", path.display())))?;
    Ok(culprit_checks::analyze(snapshot))
}

pub fn parse_connection(input: &str) -> Result<ParsedConnection> {
    let spec = connstr::parse(input)?;
    Ok(ParsedConnection {
        engine: spec.engine,
        host: spec.host.clone(),
        port: spec.port,
        database: spec.database.clone(),
        user: spec.user.clone(),
        auth: spec.auth,
        has_password: spec.password.is_some(),
        display: connstr::describe(&spec),
        suggested_name: suggest_name(&spec),
    })
}

fn suggest_name(spec: &ConnectionSpec) -> String {
    let host = spec.host.split('.').next().unwrap_or(&spec.host);
    let base = match (&spec.database, host) {
        (Some(db), "localhost" | "127.0.0.1" | "::1") => format!("{db}-local"),
        (Some(db), host) if !host.to_ascii_lowercase().contains(&db.to_ascii_lowercase()) => {
            format!("{host}-{db}")
        }
        _ => host.to_string(),
    };
    store::safe_file_name(&base.to_ascii_lowercase())
}
