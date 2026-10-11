pub mod aws;
pub mod connstr;
pub mod discover;
pub mod entra;
mod private_fs;
pub mod registry;
pub mod reports;
pub mod store;

use std::sync::Arc;
use std::time::Instant;

use culprit_core::{
    AuthKind, Check, CheckInfo, ConnectionSpec, Engine, Environment, Error, ErrorCode, ExplainMode, LiveState,
    LockedCheck, Plan, Provider, Report, Result, ResultSet, ServerInfo, SnapshotOptions,
};
use serde::{Deserialize, Serialize};
use ts_rs::TS;

pub use culprit_checks::agent_brief;
pub use discover::{Cloud, DiscoveredDatabase};
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
    /// A password is available (keychain or environment variable). Only meaningful for password authentication.
    pub has_secret: bool,
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

/// Builds a provider for a connection; lets an embedding app supply its own engine implementation.
pub type ProviderFactory = Arc<dyn Fn(&ConnectionSpec) -> Result<Box<dyn Provider>> + Send + Sync>;

pub struct Workspace {
    paths: Paths,
    store: Store,
    extra_checks: Vec<Arc<dyn Check>>,
    providers: Vec<(Engine, ProviderFactory)>,
}

impl Workspace {
    pub fn open() -> Result<Workspace> {
        let paths = Paths::resolve()?;
        Ok(Workspace::with_paths(paths))
    }

    /// A workspace whose connections and reports live in `dir`, without reading `CULPRIT_HOME`.
    pub fn open_in(dir: impl Into<std::path::PathBuf>) -> Workspace {
        let dir = dir.into();
        Workspace::with_paths(Paths { config: dir.clone(), data: dir })
    }

    fn with_paths(paths: Paths) -> Workspace {
        let store = Store::new(&paths);
        Workspace { paths, store, extra_checks: Vec::new(), providers: Vec::new() }
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
            store::set_secret(&new.name, password)?;
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
        store::set_secret(name, password)?;
        stored.password_saved = Some(true);
        stored.password_env = None;
        self.store.put(name, stored)?;
        self.connection(name)
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

    /// Save a discovered database. Azure: Entra ID. AWS: IAM when the instance allows it, otherwise password auth with the
    /// password set later (`set_password`). Nothing secret is stored here.
    pub fn add_discovered(&self, d: &DiscoveredDatabase) -> Result<ConnectionInfo> {
        match d.cloud {
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
        };
        self.store.put(&d.alias, stored)?;
        self.connection(&d.alias)
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
        resolve_secrets(name, stored.password_env.as_deref(), &mut spec).await?;
        self.materialize_certificates(&mut spec)?;
        self.open_provider(&spec)
    }

    /// Open a provider for an unsaved connection (used by "Test" before saving).
    pub async fn provider_for(&self, new: &NewConnection) -> Result<Box<dyn Provider>> {
        let mut spec = merge(new)?;
        spec.environment = new.environment;
        resolve_token(&mut spec).await?;
        self.materialize_certificates(&mut spec)?;
        self.open_provider(&spec)
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
        let report = culprit_checks::analyze_with(snapshot, &self.extra_checks);
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

    /// A plan for a query from the latest report: the stored plan when the engine keeps them (Query Store), otherwise an
    /// estimated plan of the query text. Never executes the statement.
    pub async fn query_plan(&self, name: &str, query_id: &str, plan_id: Option<&str>) -> Result<Plan> {
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

/// Fetch a fresh token for token-based auth (Entra ID, AWS IAM); a no-op for other kinds.
async fn resolve_token(spec: &mut ConnectionSpec) -> Result<()> {
    let engine = || spec.engine.ok_or_else(|| Error::invalid("Can't tell which database engine this is"));
    match spec.auth {
        AuthKind::Entra => {
            let engine = engine()?;
            spec.token = Some(entra::token_for(engine, &spec.host).await?);
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
            let region =
                spec.option("region").map(str::to_string).or_else(|| aws::region_from_host(&spec.host)).ok_or_else(
                    || {
                        Error::invalid("Can't tell which AWS region this RDS endpoint is in")
                            .with_hint("Add ?region=… to the connection")
                    },
                )?;
            spec.token = Some(aws::iam_token(engine, &spec.host, port, &user, &region).await?);
        }
        AuthKind::Password | AuthKind::None => {}
    }
    Ok(())
}

async fn resolve_secrets(name: &str, password_env: Option<&str>, spec: &mut ConnectionSpec) -> Result<()> {
    match (spec.auth, password_env) {
        (AuthKind::Entra | AuthKind::AwsIam, _) => resolve_token(spec).await?,
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
