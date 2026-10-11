pub mod agent;
pub mod aws;
pub mod catalog;
pub mod connstr;
pub mod discover;
pub mod entra;
mod http;
pub mod import;
pub mod plans;
mod private_fs;
pub mod registry;
pub mod reports;
pub mod saved;
pub mod secrets;
pub mod store;
pub mod trends;

use std::sync::Arc;
use std::time::Instant;

use culprit_core::{
    AuthKind, Check, CheckInfo, ConnectionSpec, Drift, Engine, Environment, Error, ErrorCode, ExplainMode, LiveState,
    LockedCheck, Plan, Provider, Report, Result, ResultSet, ServerInfo, SessionAction, SnapshotOptions,
    entra_host_allowed,
};
use serde::{Deserialize, Serialize};
use ts_rs::TS;

pub use agent::{AgentAccess, ApprovalOutcome, ApprovalRequest, AuditEntry, AuditOutcome};
pub use culprit_checks::agent_brief;
pub use discover::{Cloud, DiscoveredDatabase};
pub use import::{ImportCandidate, ImportSource};
pub use plans::{PlanFormat, parse_plan};
pub use reports::ReportSummary;
use secrets::SecretStore;
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
    /// A password is available (keychain or environment variable). Only meaningful for password authentication.
    pub has_secret: bool,
    pub display: String,
    pub last_report: Option<ReportSummary>,
    /// What coding agents may do with this connection through `culprit mcp`.
    pub agent_access: AgentAccess,
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
        let mut authorities = vec![entra::Authority::for_spec(&ConnectionSpec::default())?];
        for (_, stored) in self.store.list()? {
            if let Ok(authority) = entra::Authority::for_spec(&stored.spec())
                && !authorities.contains(&authority)
            {
                authorities.push(authority);
            }
        }
        self.entra.sign_out(&authorities, self.secrets.as_ref())
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

    fn open_provider(&self, spec: &ConnectionSpec) -> Result<Box<dyn Provider>> {
        match self.providers.iter().find(|(e, _)| Some(*e) == spec.engine) {
            Some((_, factory)) => factory(spec),
            None => registry::provider(spec),
        }
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
        let bytes = std::fs::read(path).map_err(|e| Error::not_found(format!("{}: {e}", path.display())))?;
        let value: serde_json::Value =
            serde_json::from_slice(&bytes).map_err(|e| Error::invalid(format!("{}: {e}", path.display())))?;
        let snapshot = value.get("snapshot").cloned().unwrap_or(value);
        let snapshot: culprit_core::Snapshot = serde_json::from_value(snapshot)
            .map_err(|e| Error::invalid(format!("{} is not a Culprit snapshot or report: {e}", path.display())))?;
        Ok(culprit_checks::analyze_with(snapshot, &self.extra_checks))
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
            has_secret: stored.auth == AuthKind::Password
                && (stored.password_env.is_some() || stored.password_saved.unwrap_or(true)),
            display: connstr::describe(&stored.spec()),
            last_report: reports::latest_summary(&self.paths.reports_dir(name), None),
            agent_access: stored.agent.unwrap_or_default(),
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
        let password = spec.password.as_deref().filter(|p| !p.is_empty() && password_env.is_none());
        if let Some(password) = password {
            self.secrets.set(&new.name, password)?;
        }
        let password_saved = Some(password.is_some());
        let stored = StoredConnection {
            engine,
            environment: new.environment,
            host: spec.host,
            port: spec.port,
            database: spec.database,
            user: spec.user,
            auth: spec.auth,
            password_env,
            password_saved,
            options: spec.options,
            agent: None,
        };
        self.store.put(&new.name, stored)?;
        self.connection(&new.name)
    }

    /// Store the password for a password-authenticated connection in the OS keychain.
    pub fn set_password(&self, name: &str, password: &str) -> Result<ConnectionInfo> {
        let mut stored = self.store.get(name)?;
        if stored.auth != AuthKind::Password {
            return Err(Error::invalid(format!("'{name}' does not use password authentication")));
        }
        if password.is_empty() {
            return Err(Error::invalid("The password is empty"));
        }
        self.secrets.set(name, password)?;
        stored.password_saved = Some(true);
        stored.password_env = None;
        self.store.put(name, stored)?;
        self.connection(name)
    }

    pub fn remove_connection(&self, name: &str) -> Result<()> {
        let stored = self.store.get(name)?;
        self.store.remove(name)?;
        if stored.auth == AuthKind::Password && stored.password_env.is_none() {
            self.secrets.delete(name)?;
        }
        Ok(())
    }

    /// Rename (re-alias) a connection, carrying its keychain secret and report history along.
    pub fn rename_connection(&self, old: &str, new: &str) -> Result<ConnectionInfo> {
        let stored = self.store.get(old)?;
        self.store.rename(old, new)?;
        if stored.auth == AuthKind::Password && stored.password_env.is_none() && stored.password_saved != Some(false) {
            if let Some(secret) = self.secrets.get(old)? {
                self.secrets.set(new, &secret)?;
            }
            self.secrets.delete(old)?;
        }
        let (from, to) = (self.paths.reports_dir(old), self.paths.reports_dir(new));
        if from.exists() && !to.exists() {
            std::fs::rename(&from, &to).map_err(|e| Error::internal(format!("{}: {e}", from.display())))?;
        }
        self.connection(new)
    }

    /// Azure SQL and Azure Database for PostgreSQL / MySQL flexible-server databases the signed-in Azure CLI account can see,
    /// marked with the saved connection that already points at each.
    pub async fn discover_azure(&self, terms: &[String]) -> Result<Vec<DiscoveredDatabase>> {
        let mut found = discover::azure(terms).await?;
        self.mark_saved(&mut found)?;
        Ok(found)
    }

    /// RDS instances and Aurora clusters the signed-in AWS CLI can see (all enabled regions when `regions` is empty).
    pub async fn discover_aws(&self, regions: &[String], terms: &[String]) -> Result<Vec<DiscoveredDatabase>> {
        let mut found = discover::aws_rds(regions, terms).await?;
        self.mark_saved(&mut found)?;
        Ok(found)
    }

    fn mark_saved(&self, found: &mut [DiscoveredDatabase]) -> Result<()> {
        let saved = self.store.list()?;
        for d in found {
            d.saved_as = saved
                .iter()
                .find(|(_, c)| {
                    c.host.eq_ignore_ascii_case(&d.host)
                        && c.database.as_deref().unwrap_or_default().eq_ignore_ascii_case(&d.database)
                })
                .map(|(name, _)| name.clone());
        }
        Ok(())
    }

    /// Save a discovered database. Azure SQL: Entra ID. Azure flexible servers: Entra ID when the server allows it, otherwise
    /// the admin login with its password set later. AWS: IAM when the instance allows it, otherwise password auth with the
    /// password set later (`set_password`). Nothing secret is stored here.
    pub fn add_discovered(&self, d: &DiscoveredDatabase) -> Result<ConnectionInfo> {
        match d.cloud {
            discover::Cloud::Azure if d.engine != Engine::Mssql => self.add_flexible(d),
            discover::Cloud::Azure => self.add_connection(NewConnection {
                name: d.alias.clone(),
                input: format!(
                    "Server=tcp:{},1433;Database={};Authentication=Active Directory Default;Encrypt=True",
                    d.host, d.database
                ),
                environment: d.environment,
                engine: Some(d.engine),
                auth: Some(AuthKind::Entra),
                ..Default::default()
            }),
            discover::Cloud::Aws => self.add_rds(d),
        }
    }

    fn add_flexible(&self, d: &DiscoveredDatabase) -> Result<ConnectionInfo> {
        store::validate_name(&d.alias)?;
        let auth = if d.auth == AuthKind::Entra && entra_host_allowed(d.engine, &d.host) {
            AuthKind::Entra
        } else {
            AuthKind::Password
        };
        let mut options = std::collections::BTreeMap::new();
        match d.engine {
            Engine::Mysql => options.insert("ssl-mode".into(), "VERIFY_IDENTITY".into()),
            _ => options.insert("sslmode".into(), "verify-full".into()),
        };
        let stored = StoredConnection {
            engine: d.engine,
            environment: d.environment,
            host: d.host.clone(),
            port: d.port,
            database: Some(d.database.clone()).filter(|db| !db.is_empty()),
            user: if auth == AuthKind::Entra { None } else { d.user.clone() },
            auth,
            password_env: None,
            password_saved: (auth == AuthKind::Password).then_some(false),
            options,
            agent: None,
        };
        self.store.put(&d.alias, stored)?;
        self.connection(&d.alias)
    }

    fn add_rds(&self, d: &DiscoveredDatabase) -> Result<ConnectionInfo> {
        store::validate_name(&d.alias)?;
        let auth =
            if d.auth == AuthKind::AwsIam && d.engine != Engine::Mssql { AuthKind::AwsIam } else { AuthKind::Password };
        let mut options = std::collections::BTreeMap::new();
        if let Some(region) = d.region.as_deref().filter(|r| aws::valid_region(r)) {
            options.insert("region".to_string(), region.to_string());
        }
        match d.engine {
            Engine::Postgres => {
                options.insert("sslmode".into(), "verify-full".into());
                options.insert("sslrootcert".into(), aws::RDS_CA.into());
            }
            Engine::Mysql => {
                options.insert("ssl-mode".into(), "VERIFY_IDENTITY".into());
                options.insert("ssl-ca".into(), aws::RDS_CA.into());
            }
            Engine::Mssql => {
                options.insert("encrypt".into(), "true".into());
                options.insert("trustServerCertificateCA".into(), aws::RDS_CA.into());
            }
        }
        let stored = StoredConnection {
            engine: d.engine,
            environment: d.environment,
            host: d.host.clone(),
            port: d.port,
            database: Some(d.database.clone()).filter(|db| !db.is_empty()),
            user: d.user.clone(),
            auth,
            password_env: None,
            password_saved: (auth == AuthKind::Password).then_some(false),
            options,
            agent: None,
        };
        self.store.put(&d.alias, stored)?;
        self.connection(&d.alias)
    }

    /// Connections other tools on this machine keep (all tools when `source` is `None`), marked with the saved connection that
    /// already points at the same database. Reads metadata only; never passwords.
    pub fn import_candidates(&self, source: Option<ImportSource>) -> Result<Vec<ImportCandidate>> {
        let mut found = import::scan(source);
        import::reconcile(&mut found, &self.store.list()?);
        Ok(found)
    }

    /// Connections in one file another tool wrote (`settings.json`, `data-sources.json`, `dataSources.xml`).
    pub fn import_file(&self, path: &std::path::Path, source: Option<ImportSource>) -> Result<Vec<ImportCandidate>> {
        let source = match source {
            Some(s) => s,
            None => import::guess_source(path)?,
        };
        let mut found = import::read_file(source, path)?;
        import::reconcile(&mut found, &self.store.list()?);
        Ok(found)
    }

    /// Save imported connections. All are checked before any is written; password connections ask for their password later.
    pub fn import_connections(&self, items: &[ImportCandidate]) -> Result<Vec<ConnectionInfo>> {
        let existing: std::collections::HashSet<String> = self.store.list()?.into_iter().map(|(n, _)| n).collect();
        let mut seen = std::collections::HashSet::new();
        let mut stored = Vec::with_capacity(items.len());
        for c in items {
            if existing.contains(&c.alias) || !seen.insert(c.alias.clone()) {
                return Err(Error::invalid(format!("A connection named '{}' already exists", c.alias))
                    .with_hint("Pick another alias for it"));
            }
            stored.push((c.alias.clone(), import::to_stored(c)?));
        }
        for (name, connection) in stored {
            self.store.put(&name, connection)?;
        }
        items.iter().map(|c| self.connection(&c.alias)).collect()
    }

    /// Replace the `rds` placeholder in certificate-authority options with the bundled RDS authorities on disk.
    fn materialize_certificates(&self, spec: &mut ConnectionSpec) -> Result<()> {
        let keys: Vec<String> = spec
            .options
            .iter()
            .filter(|(k, v)| {
                matches!(k.to_ascii_lowercase().as_str(), "sslrootcert" | "ssl-ca" | "trustservercertificateca")
                    && v.as_str() == aws::RDS_CA
            })
            .map(|(k, _)| k.clone())
            .collect();
        if keys.is_empty() {
            return Ok(());
        }
        let path = aws::rds_ca_file(&self.paths.data)?.to_string_lossy().into_owned();
        for key in keys {
            spec.options.insert(key, path.clone());
        }
        Ok(())
    }

    /// Open a provider for a saved connection, resolving the password from the keychain or a fresh Entra token.
    pub async fn provider(&self, name: &str) -> Result<Box<dyn Provider>> {
        let stored = self.store.get(name)?;
        let mut spec = stored.spec();
        self.resolve_secrets(name, stored.password_env.as_deref(), &mut spec).await?;
        self.materialize_certificates(&mut spec)?;
        self.open_provider(&spec)
    }

    /// Open a provider for an unsaved connection (used by "Test" before saving).
    pub async fn provider_for(&self, new: &NewConnection) -> Result<Box<dyn Provider>> {
        let mut spec = merge(new)?;
        spec.environment = new.environment;
        self.resolve_token(&mut spec).await?;
        self.materialize_certificates(&mut spec)?;
        self.open_provider(&spec)
    }

    pub async fn probe(&self, name: &str) -> Result<ServerInfo> {
        self.forget_rejected(name, self.provider(name).await?.probe().await)
    }

    pub async fn scan(&self, name: &str, options: &SnapshotOptions) -> Result<Report> {
        let started = Instant::now();
        let taken_at = chrono::Utc::now();
        let provider = self.provider(name).await?;
        let mut snapshot = self.forget_rejected(name, provider.snapshot(name, options).await)?;
        snapshot.id = taken_at.format("%Y%m%dT%H%M%S%.3fZ").to_string();
        snapshot.taken_at = taken_at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        snapshot.connection = name.to_string();
        snapshot.duration_ms = started.elapsed().as_secs_f64() * 1000.0;
        let dir = self.paths.reports_dir(name);
        if let Some(workload) = snapshot.workload.as_mut().filter(|w| !w.windowed && w.timeline.is_none()) {
            let history = reports::counters_history(&dir, &snapshot.taken_at);
            trends::from_scans(workload, &snapshot.server.database, &snapshot.taken_at, &history);
        }
        let report = culprit_checks::analyze_with(snapshot, &self.extra_checks);
        reports::save(&dir, &report)?;
        Ok(report)
    }

    /// The newest saved report, or the newest one scanned with `window_days` when given.
    pub fn latest_report(&self, name: &str, window_days: Option<u32>) -> Result<Option<Report>> {
        reports::latest(&self.paths.reports_dir(name), window_days)
    }

    pub fn report_history(&self, name: &str) -> Result<Vec<ReportSummary>> {
        reports::list(&self.paths.reports_dir(name))
    }

    /// What differs between the latest scans of two connections: tables, columns, indexes, settings, version.
    pub fn drift(&self, a: &str, b: &str) -> Result<Drift> {
        let load = |name: &str| -> Result<Report> {
            let mut report = self.latest_report(name, None)?.ok_or_else(|| {
                Error::not_found(format!("No saved report for '{name}'"))
                    .with_hint(format!("Run `culprit scan {name}` first"))
            })?;
            report.snapshot.connection = name.to_string();
            Ok(report)
        };
        drift_reports(&load(a)?, &load(b)?)
    }

    /// `confirmed` is the caller's explicit, per-action consent to execute on production (only matters for Analyze).
    pub async fn explain(&self, name: &str, sql: &str, mode: ExplainMode, confirmed: bool) -> Result<Plan> {
        let info = self.connection(name)?;
        guard_execution(&info, sql, mode == ExplainMode::Analyze, confirmed)?;
        self.forget_rejected(name, self.provider(name).await?.explain(sql, mode).await)
    }

    /// A plan for a query from the latest report: the stored plan when the engine keeps them (Query Store), otherwise an
    /// estimated plan of the query text. Never executes the statement.
    pub async fn query_plan(&self, name: &str, query_id: &str, plan_id: Option<&str>) -> Result<Plan> {
        self.forget_rejected(name, self.query_plan_from(name, query_id, plan_id).await)
    }

    async fn query_plan_from(&self, name: &str, query_id: &str, plan_id: Option<&str>) -> Result<Plan> {
        let provider = self.provider(name).await?;
        if let Some(plan_id) = plan_id
            && let Some(plan) = provider.stored_plan(query_id, plan_id).await?
        {
            return Ok(plan);
        }
        let report =
            self.latest_report(name, None)?.ok_or_else(|| Error::not_found(format!("No report for {name}")))?;
        let query = report
            .snapshot
            .workload
            .as_ref()
            .and_then(|w| w.query(query_id))
            .ok_or_else(|| Error::not_found(format!("No query {query_id} in the latest report for {name}")))?;
        if let Some(plan_id) = query.plans.first().map(|p| p.id.clone())
            && plan_id.chars().all(|c| c.is_ascii_digit())
            && let Some(plan) = provider.stored_plan(query_id, &plan_id).await?
        {
            return Ok(plan);
        }
        provider.explain(&query.text, ExplainMode::Estimated).await.map_err(|e| {
            let hint =
                "The statement text may need parameter values; open it in the editor to explain it with real values";
            Error { hint: Some(e.hint.clone().unwrap_or_else(|| hint.into())), ..e }
        })
    }

    /// Sessions, blocking and waits, plus what Live may do to other sessions here (nothing on production).
    pub async fn live(&self, name: &str) -> Result<LiveState> {
        let info = self.connection(name)?;
        let provider = self.provider(name).await?;
        let mut live = self.forget_rejected(name, provider.live().await)?;
        if info.environment.is_production() {
            live.session_actions = Vec::new();
            live.session_actions_note = Some(PRODUCTION_SESSIONS.to_string());
        } else {
            live.session_actions = provider.session_actions();
        }
        Ok(live)
    }

    /// Cancel another session's statement or end the session. Refused on production connections, whatever the caller says.
    pub async fn interrupt_session(&self, name: &str, session: &str, action: SessionAction) -> Result<()> {
        let info = self.connection(name)?;
        if info.environment.is_production() {
            return Err(Error::new(
                ErrorCode::PermissionDenied,
                format!("'{}' is a production connection; Culprit never cancels or ends sessions there", info.name),
            )
            .with_hint("Use the database's own tooling, or mark the connection as non-production if it isn't one."));
        }
        culprit_core::session_number(session)?;
        let provider = self.provider(name).await?;
        if !provider.session_actions().contains(&action) {
            return Err(Error::new(
                ErrorCode::Unsupported,
                match action {
                    SessionAction::Cancel => {
                        format!("{} can't stop another session's statement on its own", info.engine)
                    }
                    SessionAction::Terminate => format!("{} sessions can't be ended from Culprit", info.engine),
                },
            )
            .with_hint(match action {
                SessionAction::Cancel => "End the session instead; its open transaction rolls back.",
                SessionAction::Terminate => "Use the database's own tooling.",
            }));
        }
        provider.interrupt_session(session, action).await
    }

    /// `confirmed` is the caller's explicit, per-action consent to execute on production.
    /// Run a statement. Reads (as Culprit's classifier proves them) run in a read-only transaction and never ask.
    /// Anything else commits; on production only when the person confirmed it, otherwise this returns
    /// `ConfirmationRequired` with what the statement does, for the question to ask.
    pub async fn run(&self, name: &str, sql: &str, limit: usize, confirmed: bool) -> Result<ResultSet> {
        let info = self.connection(name)?;
        let check = culprit_core::sql::check(sql, info.engine);
        if check.reads_only {
            return self.run_read(name, sql, limit).await;
        }
        if info.environment.is_production() && !confirmed {
            let what = if check.verb.is_empty() { "statement".to_string() } else { check.verb.clone() };
            return Err(Error::new(
                ErrorCode::ConfirmationRequired,
                format!("This {what} changes data on the production connection '{}'", info.name),
            )
            .with_hint(
                check
                    .problem
                    .or(check.warning)
                    .unwrap_or_else(|| "Confirm to run it (--yes-execute-on-production in the CLI).".to_string()),
            ));
        }
        self.forget_rejected(name, self.provider(name).await?.execute(sql, limit).await)
    }

    /// Run a statement that must only read (agents, previews, the editor's reads). Refused unless Culprit's classifier
    /// proves it reads; Postgres and MySQL run it in a read-only transaction, and SQL Server, which has none, inside a
    /// transaction that is always rolled back.
    pub async fn run_read(&self, name: &str, sql: &str, limit: usize) -> Result<ResultSet> {
        let info = self.connection(name)?;
        let statement = culprit_core::sql::single_statement(sql, info.engine)?;
        if !culprit_core::sql::reads_only(statement, info.engine) {
            return Err(Error::new(ErrorCode::PermissionDenied, "Only statements that read data run here")
                .with_hint("Send a single SELECT: no writes, DDL, procedure calls or locking reads."));
        }
        let batch = read_only_batch(info.engine, statement);
        self.forget_rejected(name, self.provider(name).await?.run(&batch, limit).await)
    }

    /// Databases, tables, views and columns for editor completion, read live through the read-only run path.
    pub async fn catalog(&self, name: &str) -> Result<catalog::Catalog> {
        let info = self.connection(name)?;
        let provider = self.provider(name).await?;
        let columns = self.forget_rejected(name, provider.run(catalog::columns_sql(info.engine), 50_000).await)?;
        let databases = provider.run(catalog::databases_sql(info.engine), 500).await.ok();
        Ok(catalog::fold(columns, databases))
    }

    /// A rejected password may have been changed elsewhere since this process read it: forget the copy so the next
    /// attempt reads the keychain again.
    fn forget_rejected<T>(&self, name: &str, result: Result<T>) -> Result<T> {
        if matches!(&result, Err(e) if e.code == ErrorCode::AuthFailed) {
            self.secrets.forget(name);
        }
        result
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
    let (ea, eb) = (a.snapshot.server.engine, b.snapshot.server.engine);
    if ea != eb {
        return Err(Error::invalid(format!(
            "'{}' is {ea} and '{}' is {eb}; drift compares databases of the same engine",
            a.snapshot.connection, b.snapshot.connection
        )));
    }
    Ok(culprit_checks::drift(a, b))
}

/// Postgres and MySQL providers already run reads inside a read-only transaction. SQL Server has no read-only
/// transactions, so a read runs inside one that is always rolled back.
pub fn read_only_batch(engine: Engine, statement: &str) -> String {
    match engine {
        Engine::Mssql => {
            format!("SET XACT_ABORT ON; BEGIN TRANSACTION;\n{statement}\n;IF @@TRANCOUNT > 0 ROLLBACK TRANSACTION;")
        }
        Engine::Postgres | Engine::Mysql => statement.to_string(),
    }
}

const PRODUCTION_SESSIONS: &str = "Production connection: Culprit doesn't cancel or end sessions here.";

/// Explain Analyze executes the statement. On production it only does that for a statement Culprit's classifier proves
/// reads (SQL Server has no read-only transactions, so the classifier is the whole guard there); a write is never
/// executed just to measure it.
fn guard_execution(info: &ConnectionInfo, sql: &str, executes: bool, _confirmed: bool) -> Result<()> {
    if !executes || !info.environment.is_production() || culprit_core::sql::reads_only(sql, info.engine) {
        return Ok(());
    }
    Err(Error::new(
        ErrorCode::PermissionDenied,
        format!(
            "'{}' is a production connection; Explain Analyze would execute a statement that changes data",
            info.name
        ),
    )
    .with_hint("Use the estimated plan (Explain), or measure it on a non-production copy."))
}

impl Workspace {
    /// Fetch a fresh token for token-based auth (Entra ID, AWS IAM); a no-op for other kinds.
    async fn resolve_token(&self, spec: &mut ConnectionSpec) -> Result<()> {
        let engine = || spec.engine.ok_or_else(|| Error::invalid("Can't tell which database engine this is"));
        match spec.auth {
            AuthKind::Entra => {
                let engine = engine()?;
                let token = self.entra.token(engine, spec, self.secrets.as_ref()).await?;
                // PostgreSQL and MySQL sign in as the person the token names.
                if engine != Engine::Mssql && spec.user.as_deref().is_none_or(str::is_empty) {
                    spec.user = entra::token_user(&token);
                }
                spec.token = Some(token);
            }
            AuthKind::AwsIam => {
                let engine = engine()?;
                let user = spec
                    .user
                    .clone()
                    .filter(|u| !u.is_empty())
                    .ok_or_else(|| Error::invalid("AWS IAM authentication needs a database user name"))?;
                let port = spec.port.unwrap_or(match engine {
                    Engine::Postgres => 5432,
                    Engine::Mysql => 3306,
                    Engine::Mssql => 1433,
                });
                let region = spec
                    .option("region")
                    .map(str::to_string)
                    .or_else(|| aws::region_from_host(&spec.host))
                    .ok_or_else(|| {
                        Error::invalid("Can't tell which AWS region this RDS endpoint is in")
                            .with_hint("Add ?region=… to the connection")
                    })?;
                spec.token = Some(aws::iam_token(engine, &spec.host, port, &user, &region).await?);
            }
            AuthKind::Password | AuthKind::None => {}
        }
        Ok(())
    }

    async fn resolve_secrets(&self, name: &str, password_env: Option<&str>, spec: &mut ConnectionSpec) -> Result<()> {
        match (spec.auth, password_env) {
            (AuthKind::Entra | AuthKind::AwsIam, _) => self.resolve_token(spec).await?,
            (AuthKind::Password, Some(var)) => {
                let value = std::env::var(var).map_err(|_| {
                    Error::new(ErrorCode::AuthFailed, format!("Environment variable {var} is not set"))
                        .with_hint(format!("Connection '{name}' reads its password from {var}"))
                })?;
                spec.password = Some(value);
            }
            (AuthKind::Password, None) => spec.password = self.secrets.get(name)?,
            (AuthKind::None, _) => {}
        }
        Ok(())
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn info(engine: Engine, environment: Environment) -> ConnectionInfo {
        ConnectionInfo {
            name: "shop".into(),
            engine,
            environment,
            host: "db.example.com".into(),
            port: None,
            database: None,
            user: None,
            auth: AuthKind::Password,
            has_secret: true,
            display: String::new(),
            last_report: None,
            agent_access: Default::default(),
        }
    }

    #[test]
    fn production_analyze_only_measures_reads() {
        let prod = info(Engine::Mssql, Environment::Production);
        assert!(guard_execution(&prod, "SELECT * FROM t", true, true).is_ok());
        assert!(guard_execution(&prod, "SELECT * FROM t", true, false).is_ok());
        assert!(guard_execution(&prod, "DELETE FROM t", true, true).is_err());
        assert!(guard_execution(&prod, "SELECT 1; DELETE FROM t", true, true).is_err());
        assert!(guard_execution(&prod, "SELECT * INTO copy FROM t", true, true).is_err());
        // Estimated plans don't execute, so they are never gated.
        assert!(guard_execution(&prod, "DELETE FROM t", false, false).is_ok());
        let dev = info(Engine::Postgres, Environment::Development);
        assert!(guard_execution(&dev, "DELETE FROM t", true, false).is_ok());
    }

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
                taken_at: String::new(),
                sessions: Vec::new(),
                waits: Vec::new(),
                session_actions: Vec::new(),
                session_actions_note: None,
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

    fn rds_row(alias: &str, engine: Engine, auth: AuthKind) -> DiscoveredDatabase {
        DiscoveredDatabase {
            alias: alias.into(),
            cloud: discover::Cloud::Aws,
            engine,
            environment: Environment::Staging,
            server: alias.into(),
            host: format!("{alias}.abc123.eu-central-1.rds.amazonaws.com"),
            database: if engine == Engine::Mysql { String::new() } else { "shop".into() },
            location: "eu-central-1".into(),
            subscription: "111122223333".into(),
            resource_group: String::new(),
            port: Some(if engine == Engine::Mysql { 3306 } else { 5432 }),
            user: Some("app".into()),
            region: Some("eu-central-1".into()),
            auth,
            saved_as: None,
        }
    }

    #[test]
    fn discovered_rds_databases_save_with_verified_tls_and_no_secret() {
        let home = std::env::temp_dir().join(format!("culprit-aws-test-{}", std::process::id()));
        let ws = Workspace::open_in(&home);

        let iam = ws.add_discovered(&rds_row("shop-api", Engine::Postgres, AuthKind::AwsIam)).unwrap();
        assert_eq!((iam.auth, iam.has_secret, iam.port), (AuthKind::AwsIam, false, Some(5432)));
        let stored = ws.store.get("shop-api").unwrap();
        assert_eq!(stored.options.get("region").map(String::as_str), Some("eu-central-1"));
        assert_eq!(stored.options.get("sslmode").map(String::as_str), Some("verify-full"));

        let mut spec = stored.spec();
        ws.materialize_certificates(&mut spec).unwrap();
        let ca = std::path::PathBuf::from(spec.option("sslrootcert").unwrap());
        assert!(std::fs::read_to_string(&ca).unwrap().contains("BEGIN CERTIFICATE"));

        let password = ws.add_discovered(&rds_row("events", Engine::Mysql, AuthKind::Password)).unwrap();
        assert_eq!(
            (password.auth, password.has_secret, password.database.as_deref()),
            (AuthKind::Password, false, None)
        );
        assert_eq!(
            ws.store.get("events").unwrap().options.get("ssl-mode").map(String::as_str),
            Some("VERIFY_IDENTITY")
        );

        let sqlserver = ws.add_discovered(&rds_row("reports", Engine::Mssql, AuthKind::AwsIam)).unwrap();
        assert_eq!(sqlserver.auth, AuthKind::Password, "RDS for SQL Server has no IAM database auth");

        assert_eq!(ws.set_password("shop-api", "x").unwrap_err().code, ErrorCode::InvalidInput);
        let _ = std::fs::remove_dir_all(home);
    }
}
