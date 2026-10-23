//! Connection metadata, provider resolution and authentication. Dependencies are explicit; scan analysis and
//! query execution do not belong to this service.

use culprit_core::{
    AuthKind, ConnectionSpec, Engine, Environment, Error, ErrorCode, Provider, Result, ServerInfo, entra_host_allowed,
};

use crate::{
    ConnectionInfo, DiscoveredDatabase, ImportCandidate, ImportSource, NewConnection, ParsedConnection,
    ProviderFactory, aws, connstr, discover, entra, import, query_log, registry, reports,
    secrets::SecretStore,
    store::{self, Paths, Store, StoredConnection},
};

/// Conventional SQLite file suffixes, also exposed by the public workspace API.
pub const DATABASE_FILE_EXTENSIONS: [&str; 4] = ["db", "sqlite", "sqlite3", "db3"];

pub(crate) struct Connections<'a> {
    pub paths: &'a Paths,
    pub store: &'a Store,
    pub secrets: &'a dyn SecretStore,
    pub entra: &'a entra::EntraAuth,
    pub providers: &'a [(Engine, ProviderFactory)],
}

impl Connections<'_> {
    /// Forget device-code sign-ins: in-memory tokens and stored refresh tokens for every tenant the saved connections use.
    /// The Azure CLI's own login is untouched (`az logout`). Returns how many stored sign-ins were removed.
    pub(crate) fn sign_out(&self) -> Result<usize> {
        let mut authorities = vec![entra::Authority::for_spec(&ConnectionSpec::default())?];
        for (_, stored) in self.store.list()? {
            if let Ok(authority) = entra::Authority::for_spec(&stored.spec())
                && !authorities.contains(&authority)
            {
                authorities.push(authority);
            }
        }
        self.entra.sign_out(&authorities, self.secrets)
    }

    fn open_provider(&self, spec: &ConnectionSpec) -> Result<Box<dyn Provider>> {
        match self.providers.iter().find(|(e, _)| Some(*e) == spec.engine) {
            Some((_, factory)) => factory(spec),
            None => registry::provider(spec),
        }
    }

    pub(crate) fn connections(&self) -> Result<Vec<ConnectionInfo>> {
        self.store.list()?.into_iter().map(|(name, stored)| Ok(self.info(&name, &stored))).collect()
    }

    pub(crate) fn connection(&self, name: &str) -> Result<ConnectionInfo> {
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
            query_log: stored.query_log.clone(),
        }
    }

    /// Read the statements from this log on every scan of `name` (`None` stops). The file must exist; it is kept as an
    /// absolute path, so a scan from any directory finds it.
    /// It must hold statements in a format Culprit reads, so a wrong file (or one the framework writes without SQL) is
    /// said now rather than at the next scan. `None` stops reading one, and stops looking for one too.
    pub(crate) fn set_query_log(&self, name: &str, path: Option<&std::path::Path>) -> Result<ConnectionInfo> {
        let query_log = match path {
            None => None,
            Some(path) => {
                let absolute = std::fs::canonicalize(path).map_err(|e| {
                    Error::new(ErrorCode::NotFound, format!("Can't read {}: {e}", path.display()))
                        .with_hint("Give the path of the log file the application writes, e.g. log/production.log.")
                })?;
                if !absolute.is_file() {
                    return Err(Error::invalid(format!("{} isn't a file", absolute.display())));
                }
                query_log::recognize(&absolute)?;
                Some(absolute.to_string_lossy().into_owned())
            }
        };
        self.store.update(name, |stored| {
            stored.query_log = query_log;
            stored.no_query_log = path.is_none();
        })?;
        self.connection(name)
    }

    /// The log a file database's application writes beside it, found and saved on the connection when it has none and
    /// the person didn't remove one ([`query_log::discover`]).
    pub(crate) fn discover_query_log(&self, name: &str) -> Option<std::path::PathBuf> {
        let stored = self.store.get(name).ok()?;
        if stored.engine.has_server() || stored.query_log.is_some() || stored.no_query_log {
            return None;
        }
        let found = query_log::discover(&culprit_core::expand_home(stored.database.as_deref()?))?;
        let mut saved = false;
        self.store
            .update(name, |stored| {
                if stored.query_log.is_none() && !stored.no_query_log {
                    stored.query_log = Some(found.to_string_lossy().into_owned());
                    saved = true;
                }
            })
            .ok()?;
        saved.then_some(found)
    }

    /// Where to look for a connection's query log: beside the log it reads, else beside its database file (the app's
    /// file picker starts there). `None` for a connection with neither.
    pub(crate) fn query_log_folder(info: &ConnectionInfo) -> Option<std::path::PathBuf> {
        info.query_log
            .as_deref()
            .or(info.database.as_deref())
            .map(culprit_core::expand_home)
            .and_then(|p| p.parent().map(std::path::Path::to_path_buf))
    }

    pub(crate) fn add_connection(&self, new: NewConnection) -> Result<ConnectionInfo> {
        store::validate_name(&new.name)?;
        let spec = merge(&new)?;
        let engine = spec.engine.ok_or_else(|| {
            Error::invalid("Can't tell which database engine this is").with_hint(
                "Choose the engine explicitly: PostgreSQL, SQL Server, MySQL or SQLite (--engine in the CLI)",
            )
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
            query_log: None,
            no_query_log: false,
        };
        self.store.put(&new.name, stored)?;
        self.discover_query_log(&new.name);
        self.connection(&new.name)
    }

    /// Store the password for a password-authenticated connection in the OS keychain.
    pub(crate) fn set_password(&self, name: &str, password: &str) -> Result<ConnectionInfo> {
        let stored = self.store.get(name)?;
        if stored.auth != AuthKind::Password {
            return Err(Error::invalid(format!("'{name}' does not use password authentication")));
        }
        if password.is_empty() {
            return Err(Error::invalid("The password is empty"));
        }
        self.secrets.set(name, password)?;
        self.store.update(name, |stored| {
            stored.password_saved = Some(true);
            stored.password_env = None;
        })?;
        self.connection(name)
    }

    pub(crate) fn remove_connection(&self, name: &str) -> Result<()> {
        let stored = self.store.get(name)?;
        self.store.remove(name)?;
        if stored.auth == AuthKind::Password && stored.password_env.is_none() {
            self.secrets.delete(name)?;
        }
        Ok(())
    }

    /// Mark a connection production, staging, development or local. Production refuses session kills and asks before
    /// anything that changes data.
    pub(crate) fn set_environment(&self, name: &str, environment: Environment) -> Result<ConnectionInfo> {
        self.store.update(name, |stored| stored.environment = environment)?;
        self.connection(name)
    }

    /// Rename (re-alias) a connection, carrying its keychain secret and report history along.
    pub(crate) fn rename_connection(&self, old: &str, new: &str) -> Result<ConnectionInfo> {
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
    pub(crate) async fn discover_azure(&self, terms: &[String]) -> Result<Vec<DiscoveredDatabase>> {
        let mut found = discover::azure(terms).await?;
        self.mark_saved(&mut found)?;
        Ok(found)
    }

    /// RDS instances and Aurora clusters the signed-in AWS CLI can see (all enabled regions when `regions` is empty).
    pub(crate) async fn discover_aws(&self, regions: &[String], terms: &[String]) -> Result<Vec<DiscoveredDatabase>> {
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
    pub(crate) fn add_discovered(&self, d: &DiscoveredDatabase) -> Result<ConnectionInfo> {
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
            query_log: None,
            no_query_log: false,
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
            Engine::Sqlite => {}
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
            query_log: None,
            no_query_log: false,
        };
        self.store.put(&d.alias, stored)?;
        self.connection(&d.alias)
    }

    /// Connections other tools on this machine keep (all tools when `source` is `None`), marked with the saved connection that
    /// already points at the same database. Reads metadata only; never passwords.
    pub(crate) fn import_candidates(&self, source: Option<ImportSource>) -> Result<Vec<ImportCandidate>> {
        let mut found = import::scan(source);
        import::reconcile(&mut found, &self.store.list()?);
        Ok(found)
    }

    /// Connections in one file another tool wrote (`settings.json`, `data-sources.json`, `dataSources.xml`).
    pub(crate) fn import_file(
        &self,
        path: &std::path::Path,
        source: Option<ImportSource>,
    ) -> Result<Vec<ImportCandidate>> {
        let source = match source {
            Some(s) => s,
            None => import::guess_source(path)?,
        };
        let mut found = import::read_file(source, path)?;
        import::reconcile(&mut found, &self.store.list()?);
        Ok(found)
    }

    /// Save imported connections. All are checked before any is written; password connections ask for their password later.
    pub(crate) fn import_connections(&self, items: &[ImportCandidate]) -> Result<Vec<ConnectionInfo>> {
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
    pub(crate) async fn provider(&self, name: &str) -> Result<Box<dyn Provider>> {
        let stored = self.store.get(name)?;
        let mut spec = stored.spec();
        self.resolve_secrets(name, stored.password_env.as_deref(), &mut spec).await?;
        self.materialize_certificates(&mut spec)?;
        self.open_provider(&spec)
    }

    /// Open a provider for an unsaved connection (used by "Test" before saving).
    pub(crate) async fn provider_for(&self, new: &NewConnection) -> Result<Box<dyn Provider>> {
        let mut spec = merge(new)?;
        spec.environment = new.environment;
        self.resolve_token(&mut spec).await?;
        self.materialize_certificates(&mut spec)?;
        self.open_provider(&spec)
    }

    pub(crate) async fn probe(&self, name: &str) -> Result<ServerInfo> {
        let provider = self.provider(name).await?;
        self.forget_rejected(
            name,
            crate::operation::within(culprit_core::limits::OBSERVE_TIMEOUT, provider.probe()).await,
        )
    }

    pub(crate) async fn test_connection(&self, new: &NewConnection) -> Result<ServerInfo> {
        let provider = self.provider_for(new).await?;
        crate::operation::within(culprit_core::limits::OBSERVE_TIMEOUT, provider.probe()).await
    }

    /// A rejected password may have been changed elsewhere since this process read it: forget the copy so the next
    /// attempt reads the keychain again.
    pub(crate) fn forget_rejected<T>(&self, name: &str, result: Result<T>) -> Result<T> {
        if matches!(&result, Err(e) if e.code == ErrorCode::AuthFailed) {
            self.secrets.forget(name);
        }
        result
    }

    /// Fetch a fresh token for token-based auth (Entra ID, AWS IAM); a no-op for other kinds.
    async fn resolve_token(&self, spec: &mut ConnectionSpec) -> Result<()> {
        let engine = || spec.engine.ok_or_else(|| Error::invalid("Can't tell which database engine this is"));
        match spec.auth {
            AuthKind::Entra => {
                let engine = engine()?;
                let token = self.entra.token(engine, spec, self.secrets).await?;
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
                    Engine::Sqlite => 0,
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

/// What a chosen database file is as a connection's input: its path when the extension says SQLite (`.db`, `.sqlite`,
/// `.sqlite3`, `.db3`), else `sqlite:<path>`, so Culprit reads it as SQLite whatever the file is named. The app's file
/// picker hands this to the new-connection form.
pub(crate) fn database_file_input(path: &std::path::Path) -> String {
    let known = path.extension().is_some_and(|x| DATABASE_FILE_EXTENSIONS.iter().any(|k| x.eq_ignore_ascii_case(k)));
    let text = path.to_string_lossy().into_owned();
    if known { text } else { format!("sqlite:{text}") }
}

fn merge(new: &NewConnection) -> Result<ConnectionSpec> {
    let mut spec = match new.engine {
        Some(Engine::Sqlite) => connstr::parse_sqlite(&new.input)?,
        _ => connstr::parse(&new.input)?,
    };
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
    // A file path is kept absolute, so the app and `culprit mcp`, which run from elsewhere, open the same file.
    if spec.engine == Some(Engine::Sqlite)
        && let Some(path) = spec.database.as_deref().filter(|p| !p.starts_with("~/"))
        && let Ok(absolute) = std::path::absolute(path)
    {
        spec.database = Some(absolute.to_string_lossy().into_owned());
    }
    spec.environment = new.environment;
    Ok(spec)
}

/// What `input` connects to, read as `engine` wherever the text leaves the engine open (see `connstr::parse_as`).
pub(crate) fn parse_connection(input: &str, engine: Option<Engine>) -> Result<ParsedConnection> {
    let spec = connstr::parse_as(input, engine)?;
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
    if spec.engine == Some(Engine::Sqlite) {
        return store::safe_file_name(
            &file_connection_name(spec.database.as_deref().unwrap_or_default()).to_ascii_lowercase(),
        );
    }
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

/// A SQLite file's connection name: the file's own, after its project's when that alone says little
/// (`shop/db/production.sqlite3` → `shop-production`, `chinook.db` → `chinook`).
fn file_connection_name(path: &str) -> String {
    const SAYS_LITTLE: [&str; 13] = [
        "app",
        "data",
        "database",
        "db",
        "dev",
        "development",
        "local",
        "main",
        "prod",
        "production",
        "sqlite",
        "staging",
        "test",
    ];
    const DATA_FOLDERS: [&str; 7] = ["data", "database", "db", "instance", "prisma", "storage", "var"];
    let path = std::path::Path::new(path);
    let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    if !SAYS_LITTLE.contains(&stem.to_ascii_lowercase().as_str()) {
        return stem;
    }
    let project = path
        .ancestors()
        .skip(1)
        .filter_map(|folder| folder.file_name())
        .map(|name| name.to_string_lossy().into_owned())
        .find(|name| name != "~" && !DATA_FOLDERS.contains(&name.to_ascii_lowercase().as_str()));
    match project {
        Some(project) => format!("{project}-{stem}"),
        None => stem,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Workspace;

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
        ws.connection_service().materialize_certificates(&mut spec).unwrap();
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

    #[test]
    fn a_sqlite_file_is_named_after_itself_or_its_project() {
        let name = |path: &str| parse_connection(path, Some(Engine::Sqlite)).unwrap().suggested_name;
        assert_eq!(name("/Users/me/code/shop/db/production.sqlite3"), "shop-production");
        assert_eq!(name("/Users/me/Downloads/Chinook.db"), "chinook");
        assert_eq!(name("~/code/blog/instance/app.db"), "blog-app");
        assert_eq!(name("~/app.db"), "app");
        assert_eq!(name("app.db"), "app");
    }
}
