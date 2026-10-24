//! Bring in connections saved by other database tools: Azure Data Studio, VS Code (mssql extension), DBeaver and DataGrip.
//! Only metadata is read (engine, host, port, database, user name, sign-in kind, name). Passwords, keychains and credential
//! stores such as DBeaver's `credentials-config.json` are never opened; imported password connections ask once.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use culprit_core::{AuthKind, ConnectionSpec, Engine, Environment, Error, Result, entra_host_allowed};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use ts_rs::TS;

use crate::{connstr, discover, store};

/// Settings files bigger than this are not connection lists.
const MAX_FILE_BYTES: u64 = 8 * 1024 * 1024;
/// How deep to look for DataGrip/DBeaver project folders.
const MAX_DEPTH: usize = 4;
/// Option keys an imported connection may carry: TLS behaviour only, never credentials.
const CARRIED_OPTIONS: &[&str] =
    &["encrypt", "trustServerCertificate", "hostNameInCertificate", "applicationIntent", "sslmode", "ssl-mode"];

#[derive(Serialize, Deserialize, TS, Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "lowercase")]
#[ts(export)]
pub enum ImportSource {
    Ads,
    Vscode,
    Dbeaver,
    Datagrip,
}

impl ImportSource {
    pub const ALL: [ImportSource; 4] =
        [ImportSource::Ads, ImportSource::Vscode, ImportSource::Dbeaver, ImportSource::Datagrip];

    pub fn label(self) -> &'static str {
        match self {
            ImportSource::Ads => "Azure Data Studio",
            ImportSource::Vscode => "VS Code",
            ImportSource::Dbeaver => "DBeaver",
            ImportSource::Datagrip => "DataGrip",
        }
    }
}

/// A connection found in another tool, ready to save under `alias`.
#[derive(Serialize, Deserialize, TS, Clone, Debug, PartialEq)]
#[ts(export)]
pub struct ImportCandidate {
    pub alias: String,
    pub source: ImportSource,
    /// The connection's name in the other tool.
    pub origin: String,
    /// The file it was read from.
    pub file: String,
    pub engine: Engine,
    pub environment: Environment,
    pub host: String,
    pub port: Option<u16>,
    pub database: Option<String>,
    /// The login name (not a secret). Empty for Entra ID sign-in, which uses the signed-in account.
    pub user: Option<String>,
    pub auth: AuthKind,
    /// TLS settings carried over (`encrypt`, `trustServerCertificate`, `sslmode`, `ssl-mode`, …).
    pub options: BTreeMap<String, String>,
    /// Something the person should know before importing it, in plain language.
    pub note: Option<String>,
    /// The saved connection that already points at the same database, if any.
    pub saved_as: Option<String>,
}

/// Where each tool keeps its connections on this machine. DataGrip also reads `.idea/` in the current directory (project data
/// sources).
pub fn default_files(source: ImportSource) -> Vec<PathBuf> {
    let home = dirs::home_dir();
    let config = dirs::config_dir();
    let mut out = Vec::new();
    match source {
        ImportSource::Ads => out.extend(config.map(|c| c.join("azuredatastudio/User/settings.json"))),
        ImportSource::Vscode => {
            if let Some(c) = config {
                out.push(c.join("Code/User/settings.json"));
                out.push(c.join("Code - Insiders/User/settings.json"));
            }
        }
        ImportSource::Dbeaver => {
            let workspace = if cfg!(target_os = "macos") {
                home.map(|h| h.join("Library/DBeaverData/workspace6"))
            } else {
                dirs::data_dir().map(|d| d.join("DBeaverData/workspace6"))
            };
            if let Some(workspace) = workspace {
                out.extend(find_files(&workspace, 2, |name| {
                    name.starts_with("data-sources") && name.ends_with(".json")
                }));
            }
        }
        ImportSource::Datagrip => {
            if let Some(jetbrains) = config.map(|c| c.join("JetBrains")) {
                for product in read_dir(&jetbrains).filter(|p| file_name(p).starts_with("DataGrip")) {
                    out.extend(find_files(&product, MAX_DEPTH, |name| name == "dataSources.xml"));
                }
            }
            if let Some(projects) = home.map(|h| h.join("DataGripProjects")) {
                out.extend(find_files(&projects, 3, |name| name == "dataSources.xml"));
            }
            if let Ok(cwd) = std::env::current_dir() {
                out.push(cwd.join(".idea/dataSources.xml"));
            }
        }
    }
    out.retain(|p| p.is_file());
    out.sort();
    out.dedup();
    out
}

/// Read every connection the given tools (all when `None`) keep on this machine.
pub fn scan(source: Option<ImportSource>) -> Vec<ImportCandidate> {
    let sources: Vec<ImportSource> = source.map(|s| vec![s]).unwrap_or(ImportSource::ALL.to_vec());
    let files: Vec<(ImportSource, PathBuf)> =
        sources.into_iter().flat_map(|s| default_files(s).into_iter().map(move |f| (s, f))).collect();
    let mut out: Vec<ImportCandidate> = files.iter().flat_map(|(s, f)| read_file(*s, f).unwrap_or_default()).collect();
    dedupe(&mut out);
    out
}

/// Read one tool's file. `source` picks the format; `None` guesses it from the file name and contents.
pub fn read_file(source: ImportSource, path: &Path) -> Result<Vec<ImportCandidate>> {
    let text = read_limited(path)?;
    let file = path.display().to_string();
    let mut out = match source {
        ImportSource::Ads | ImportSource::Vscode => parse_vscode_settings(source, &text, &file)?,
        ImportSource::Dbeaver => parse_dbeaver(&text, &file)?,
        ImportSource::Datagrip => {
            let local = path.with_file_name("dataSources.local.xml");
            let users = read_limited(&local).ok().map(|t| datagrip_users(&t)).unwrap_or_default();
            parse_datagrip(&text, &file, &users)?
        }
    };
    dedupe(&mut out);
    Ok(out)
}

/// Guess which tool wrote `path` (for `--file`) from its name and contents.
pub fn guess_source(path: &Path) -> Result<ImportSource> {
    let name = file_name(path).to_ascii_lowercase();
    if name.ends_with(".xml") {
        return Ok(ImportSource::Datagrip);
    }
    if name.starts_with("data-sources") {
        return Ok(ImportSource::Dbeaver);
    }
    if name.ends_with(".json") {
        let text = read_limited(path)?;
        if text.contains("\"datasource.connections\"") {
            return Ok(ImportSource::Ads);
        }
        if text.contains("\"mssql.connections\"") {
            return Ok(ImportSource::Vscode);
        }
        if text.contains("\"connections\"") && text.contains("\"configuration\"") {
            return Ok(ImportSource::Dbeaver);
        }
    }
    Err(Error::invalid(format!("Can't tell which tool wrote {}", path.display()))
        .with_hint("Pass --from ads, vscode, dbeaver or datagrip"))
}

fn read_limited(path: &Path) -> Result<String> {
    let meta = std::fs::metadata(path).map_err(|e| Error::not_found(format!("{}: {e}", path.display())))?;
    if meta.len() > MAX_FILE_BYTES {
        return Err(Error::invalid(format!("{} is too large to be a connection list", path.display())));
    }
    std::fs::read_to_string(path).map_err(|e| Error::invalid(format!("{}: {e}", path.display())))
}

fn file_name(path: &Path) -> String {
    path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

fn read_dir(dir: &Path) -> impl Iterator<Item = PathBuf> {
    std::fs::read_dir(dir).into_iter().flatten().flatten().map(|e| e.path())
}

/// Files under `root` (not following symlinks) whose name passes `keep`.
fn find_files(root: &Path, depth: usize, keep: fn(&str) -> bool) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![(root.to_path_buf(), 0)];
    while let Some((dir, level)) = stack.pop() {
        for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            let Ok(kind) = entry.file_type() else { continue };
            let path = entry.path();
            if kind.is_dir() && level < depth {
                stack.push((path, level + 1));
            } else if kind.is_file() && keep(&file_name(&path)) {
                out.push(path);
            }
        }
    }
    out
}

// ───────── Azure Data Studio and VS Code (mssql extension) ─────────

fn parse_vscode_settings(source: ImportSource, text: &str, file: &str) -> Result<Vec<ImportCandidate>> {
    let settings: Value = serde_json::from_str(&strip_jsonc(text))
        .map_err(|e| Error::invalid(format!("{file} is not valid JSON: {e}")))?;
    let mut out = Vec::new();
    // Azure Data Studio: provider + options. VS Code mssql: flat profiles.
    for entry in settings.get("datasource.connections").and_then(Value::as_array).into_iter().flatten() {
        let provider = str_at(entry, "providerName").unwrap_or("MSSQL");
        let options = entry.get("options").unwrap_or(entry);
        if let Some(c) = ads_profile(source, file, provider, options) {
            out.push(c);
        }
    }
    for profile in settings.get("mssql.connections").and_then(Value::as_array).into_iter().flatten() {
        if let Some(c) = ads_profile(source, file, "MSSQL", profile) {
            out.push(c);
        }
    }
    Ok(out)
}

fn ads_profile(source: ImportSource, file: &str, provider: &str, o: &Value) -> Option<ImportCandidate> {
    let engine = match provider.to_ascii_uppercase().as_str() {
        "MSSQL" => Engine::Mssql,
        "PGSQL" | "POSTGRES" | "POSTGRESQL" => Engine::Postgres,
        "YSQL" | "YUGABYTE" | "YUGABYTEDB" => Engine::Yugabyte,
        "MYSQL" => Engine::Mysql,
        _ => return None,
    };
    let origin = first_str(o, &["connectionName", "profileName", "databaseDisplayName"]);
    let mut spec = match str_at(o, "connectionString") {
        Some(cs) => connstr::parse(cs).ok()?,
        None => {
            let server = first_str(o, &["server", "host"])?;
            let mut spec = match engine {
                Engine::Mssql => mssql_server(server)?,
                _ => ConnectionSpec { engine: Some(engine), host: server.trim().to_string(), ..Default::default() },
            };
            if let Some(port) = o.get("port").and_then(port_of) {
                spec.port = Some(port);
            }
            spec.database = first_str(o, &["database", "dbname"]).map(str::to_string);
            spec.user = first_str(o, &["user", "userName", "username"]).map(str::to_string);
            spec
        }
    };
    let engine =
        if engine == Engine::Postgres && spec.engine == Some(Engine::Yugabyte) { Engine::Yugabyte } else { engine };
    spec.engine = Some(engine);
    spec.password = None;
    let auth_type = first_str(o, &["authenticationType", "authentication"]).unwrap_or("SqlLogin").to_ascii_lowercase();
    let mut note = None;
    spec.auth = if auth_type.starts_with("azure") || auth_type.starts_with("activedirectory") {
        AuthKind::Entra
    } else if auth_type == "integrated" {
        note = Some("Uses Windows authentication there; Culprit needs a SQL login and asks for its password.".into());
        AuthKind::Password
    } else {
        AuthKind::Password
    };
    match o.get("encrypt") {
        Some(Value::Bool(b)) => set(&mut spec, "encrypt", if *b { "true" } else { "false" }),
        Some(Value::String(s)) => {
            let v = match s.to_ascii_lowercase().as_str() {
                "mandatory" | "true" | "yes" => "true",
                "strict" => "strict",
                "optional" | "false" | "no" => "false",
                _ => "",
            };
            if !v.is_empty() {
                set(&mut spec, "encrypt", v);
            }
        }
        _ => {}
    }
    if o.get("trustServerCertificate").and_then(Value::as_bool) == Some(true) {
        set(&mut spec, "trustServerCertificate", "true");
    }
    if let Some(h) = str_at(o, "hostNameInCertificate") {
        set(&mut spec, "hostNameInCertificate", h);
    }
    if let Some(mode) = first_str(o, &["sslmode", "sslMode"]) {
        set(&mut spec, if engine == Engine::Mysql { "ssl-mode" } else { "sslmode" }, mode);
    }
    candidate(source, file, origin, spec, None, note)
}

/// `tcp:host,1433`, `host,1433` or `host\instance` (named instances need their port).
fn mssql_server(server: &str) -> Option<ConnectionSpec> {
    let server = server.trim();
    let server = server.strip_prefix("tcp:").or_else(|| server.strip_prefix("TCP:")).unwrap_or(server);
    let (host, port) = match server.rsplit_once(',') {
        Some((h, p)) => (h.trim(), p.trim().parse::<u16>().ok()),
        None => (server, None),
    };
    let host = host.split('\\').next().unwrap_or(host).trim();
    (!host.is_empty()).then(|| ConnectionSpec {
        engine: Some(Engine::Mssql),
        host: host.to_string(),
        port,
        ..Default::default()
    })
}

/// VS Code settings allow comments and trailing commas; JSON does not.
fn strip_jsonc(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    let mut in_string = false;
    while i < chars.len() {
        let c = chars[i];
        if in_string {
            out.push(c);
            if c == '\\' && i + 1 < chars.len() {
                out.push(chars[i + 1]);
                i += 2;
                continue;
            }
            in_string = c != '"';
            i += 1;
            continue;
        }
        match (c, chars.get(i + 1)) {
            ('"', _) => {
                in_string = true;
                out.push(c);
                i += 1;
            }
            ('/', Some('/')) => {
                while i < chars.len() && chars[i] != '\n' {
                    i += 1;
                }
            }
            ('/', Some('*')) => {
                i += 2;
                while i + 1 < chars.len() && !(chars[i] == '*' && chars[i + 1] == '/') {
                    i += 1;
                }
                i += 2;
            }
            (',', _) => {
                let next = chars[i + 1..].iter().find(|c| !c.is_whitespace());
                if !matches!(next, Some('}') | Some(']')) {
                    out.push(c);
                }
                i += 1;
            }
            _ => {
                out.push(c);
                i += 1;
            }
        }
    }
    out
}

// ───────── DBeaver ─────────

fn yugabyte_driver(driver: &str) -> bool {
    driver.split(|c: char| !c.is_ascii_alphanumeric()).any(|part| part == "ysql" || part.starts_with("yugabyte"))
}

fn parse_dbeaver(text: &str, file: &str) -> Result<Vec<ImportCandidate>> {
    let doc: Value =
        serde_json::from_str(text).map_err(|e| Error::invalid(format!("{file} is not valid JSON: {e}")))?;
    let mut out = Vec::new();
    for c in doc.get("connections").and_then(Value::as_object).into_iter().flat_map(|m| m.values()) {
        let provider = format!("{} {}", str_at(c, "provider").unwrap_or(""), str_at(c, "driver").unwrap_or(""))
            .to_ascii_lowercase();
        let engine = if yugabyte_driver(&provider) {
            Engine::Yugabyte
        } else if provider.contains("postgres") {
            Engine::Postgres
        } else if provider.contains("mysql") || provider.contains("mariadb") {
            Engine::Mysql
        } else if provider.contains("sqlserver") || provider.contains("mssql") || provider.contains("azure") {
            Engine::Mssql
        } else {
            continue;
        };
        let Some(cfg) = c.get("configuration") else { continue };
        let manual = str_at(cfg, "configurationType").is_none_or(|t| !t.eq_ignore_ascii_case("url"));
        let mut spec = match (manual, str_at(cfg, "url")) {
            (false, Some(url)) => match connstr::parse(url) {
                Ok(spec) => spec,
                Err(_) => continue,
            },
            _ => {
                let Some(host) = str_at(cfg, "host") else { continue };
                ConnectionSpec {
                    engine: Some(engine),
                    host: host.trim().to_string(),
                    port: cfg.get("port").and_then(port_of),
                    database: str_at(cfg, "database").map(str::to_string),
                    ..Default::default()
                }
            }
        };
        let engine =
            if engine == Engine::Postgres && spec.engine == Some(Engine::Yugabyte) { Engine::Yugabyte } else { engine };
        spec.engine = Some(engine);
        spec.password = None;
        if let Some(user) = str_at(cfg, "user") {
            spec.user = Some(user.to_string());
        }
        let auth_model = str_at(cfg, "auth-model").unwrap_or("native").to_ascii_lowercase();
        spec.auth = if ["azure", "entra", "_ad", "activedirectory"].iter().any(|k| auth_model.contains(k)) {
            AuthKind::Entra
        } else {
            AuthKind::Password
        };
        for (key, value) in cfg.get("properties").and_then(Value::as_object).into_iter().flatten() {
            if let (Some(name), Some(v)) =
                (CARRIED_OPTIONS.iter().find(|k| k.eq_ignore_ascii_case(key)), value.as_str())
            {
                set(&mut spec, name, v);
            }
        }
        let ssl = cfg
            .get("handlers")
            .and_then(Value::as_object)
            .into_iter()
            .flatten()
            .filter(|(name, h)| name.contains("ssl") && h.get("enabled").and_then(Value::as_bool) == Some(true))
            .find_map(|(_, h)| {
                h.get("properties").and_then(|p| str_at(p, "sslMode").or_else(|| str_at(p, "ssl.mode")))
            });
        if let Some(mode) = ssl {
            set(&mut spec, if engine == Engine::Mysql { "ssl-mode" } else { "sslmode" }, mode);
        }
        let environment = match str_at(cfg, "type").map(str::to_ascii_lowercase).as_deref() {
            Some("prod") => Some(Environment::Production),
            Some("test") => Some(Environment::Staging),
            Some("dev") => Some(Environment::Development),
            _ => None,
        };
        if let Some(found) = candidate(ImportSource::Dbeaver, file, str_at(c, "name"), spec, environment, None) {
            out.push(found);
        }
    }
    Ok(out)
}

// ───────── DataGrip ─────────

fn parse_datagrip(text: &str, file: &str, users: &BTreeMap<String, String>) -> Result<Vec<ImportCandidate>> {
    let doc = roxmltree::Document::parse(text).map_err(|e| Error::invalid(format!("{file} is not valid XML: {e}")))?;
    let mut out = Vec::new();
    for ds in doc.descendants().filter(|n| n.has_tag_name("data-source")) {
        let child = |tag: &str| ds.children().find(|n| n.has_tag_name(tag)).and_then(|n| n.text()).map(str::trim);
        let Some(url) = child("jdbc-url") else { continue };
        let driver = child("driver-ref").unwrap_or("").to_ascii_lowercase();
        let Some(mut spec) = datagrip_url(url) else { continue };
        let engine = if spec.engine == Some(Engine::Postgres) && yugabyte_driver(&driver) {
            Engine::Yugabyte
        } else {
            spec.engine.unwrap_or(Engine::Postgres)
        };
        if !driver.is_empty()
            && !["postgres", "sqlserver", "azure", "mysql", "mariadb", "yugabyte", "ysql"]
                .iter()
                .any(|k| driver.contains(k))
        {
            continue;
        }
        spec.password = None;
        let uuid = ds.attribute("uuid").unwrap_or("");
        if let Some(user) = users.get(uuid).or_else(|| ds.attribute("name").and_then(|n| users.get(n))) {
            spec.user = Some(user.clone());
        }
        let auth = child("auth-provider").unwrap_or("").to_ascii_lowercase();
        spec.auth = if auth.contains("azure") || auth.contains("entra") { AuthKind::Entra } else { AuthKind::Password };
        spec.engine = Some(engine);
        if let Some(found) = candidate(ImportSource::Datagrip, file, ds.attribute("name"), spec, None, None) {
            out.push(found);
        }
    }
    Ok(out)
}

/// User names from `dataSources.local.xml`, by data source uuid (and name).
fn datagrip_users(text: &str) -> BTreeMap<String, String> {
    let Ok(doc) = roxmltree::Document::parse(text) else { return BTreeMap::new() };
    let mut out = BTreeMap::new();
    for ds in doc.descendants().filter(|n| n.has_tag_name("data-source")) {
        let user = ds.children().find(|n| n.has_tag_name("user-name")).and_then(|n| n.text()).map(str::trim);
        if let Some(user) = user.filter(|u| !u.is_empty()) {
            for key in [ds.attribute("uuid"), ds.attribute("name")].into_iter().flatten() {
                out.insert(key.to_string(), user.to_string());
            }
        }
    }
    out
}

/// JDBC URLs, including jTDS (`jdbc:jtds:sqlserver://host:port/db`) which `connstr` doesn't speak.
fn datagrip_url(url: &str) -> Option<ConnectionSpec> {
    let lower = url.to_ascii_lowercase();
    if let Some(rest) = lower.strip_prefix("jdbc:jtds:sqlserver://").map(|_| &url["jdbc:jtds:sqlserver://".len()..]) {
        let rest = rest.split(';').next().unwrap_or(rest);
        let (hostport, db) = rest.split_once('/').unwrap_or((rest, ""));
        let (host, port) = match hostport.rsplit_once(':') {
            Some((h, p)) => (h, p.parse().ok()),
            None => (hostport, None),
        };
        return Some(ConnectionSpec {
            engine: Some(Engine::Mssql),
            host: host.to_string(),
            port,
            database: Some(db.to_string()).filter(|d| !d.is_empty()),
            ..Default::default()
        });
    }
    connstr::parse(url).ok().filter(|s| s.engine.is_some() && !s.host.is_empty())
}

// ───────── shared ─────────

fn str_at<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty())
}

fn first_str<'a>(v: &'a Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter().find_map(|k| str_at(v, k))
}

fn port_of(v: &Value) -> Option<u16> {
    match v {
        Value::Number(n) => n.as_u64().and_then(|p| u16::try_from(p).ok()),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

fn set(spec: &mut ConnectionSpec, key: &str, value: &str) {
    spec.options.insert(key.to_string(), value.to_string());
}

fn is_local(host: &str) -> bool {
    matches!(host.to_ascii_lowercase().as_str(), "localhost" | "127.0.0.1" | "::1" | "[::1]" | "." | "(local)")
}

/// Turn a parsed spec into a candidate: keep only TLS options, refuse Entra outside Azure, infer the environment and alias.
fn candidate(
    source: ImportSource,
    file: &str,
    origin: Option<&str>,
    spec: ConnectionSpec,
    environment: Option<Environment>,
    mut note: Option<String>,
) -> Option<ImportCandidate> {
    let engine = spec.engine?;
    let host = spec.host.trim().to_string();
    if host.is_empty() || host.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return None;
    }
    let mut auth = spec.auth;
    if auth == AuthKind::Entra && !entra_host_allowed(engine, &host) {
        auth = AuthKind::Password;
        note = Some(
            "Signs in with Microsoft Entra ID there, but this host isn't an Azure database; Culprit uses a password."
                .into(),
        );
    }
    let options: BTreeMap<String, String> = spec
        .options
        .iter()
        .filter_map(|(k, v)| {
            CARRIED_OPTIONS.iter().find(|c| c.eq_ignore_ascii_case(k)).map(|c| (c.to_string(), v.clone()))
        })
        .collect();
    let origin = origin
        .map(str::to_string)
        .unwrap_or_else(|| format!("{host}{}", spec.database.as_deref().map(|d| format!("/{d}")).unwrap_or_default()));
    let environment = environment.unwrap_or_else(|| {
        if is_local(&host) {
            Environment::Local
        } else {
            discover::infer_environment(&format!("{origin} {host} {}", spec.database.as_deref().unwrap_or("")))
        }
    });
    let user = if auth == AuthKind::Entra && engine == Engine::Mssql { None } else { spec.user.clone() };
    Some(ImportCandidate {
        alias: alias_from(&origin),
        source,
        origin,
        file: file.to_string(),
        engine,
        environment,
        host,
        port: spec.port,
        database: spec.database.clone().filter(|d| !d.is_empty()),
        user,
        auth,
        options,
        note,
        saved_as: None,
    })
}

/// `Shop (staging)` becomes `shop-staging`.
pub fn alias_from(text: &str) -> String {
    let mut alias = String::new();
    for c in text.trim().to_ascii_lowercase().chars() {
        let c = if c.is_ascii_alphanumeric() || c == '.' || c == '_' { c } else { '-' };
        if c == '-' && (alias.is_empty() || alias.ends_with('-')) {
            continue;
        }
        alias.push(c);
    }
    let alias: String = alias.trim_matches(|c| c == '-' || c == '.').chars().take(64).collect();
    let alias = alias.trim_end_matches(['-', '.']).to_string();
    if alias.is_empty() { "imported".to_string() } else { alias }
}

/// The same database listed twice (two tools, a project and a global list) shows once. Listings merge: the most cautious
/// environment wins (marked production anywhere stays production) and a missing user or TLS setting is filled in.
fn dedupe(list: &mut Vec<ImportCandidate>) {
    let caution = |e: Environment| match e {
        Environment::Production => 3,
        Environment::Staging => 2,
        Environment::Development => 1,
        Environment::Local => 0,
    };
    let mut kept: Vec<ImportCandidate> = Vec::with_capacity(list.len());
    for c in list.drain(..) {
        let key = target_key(c.engine, &c.host, c.port, c.database.as_deref(), None);
        let twin = kept.iter_mut().find(|k| {
            target_key(k.engine, &k.host, k.port, k.database.as_deref(), None) == key
                && (k.user.is_none() || c.user.is_none() || k.user == c.user)
        });
        match twin {
            None => kept.push(c),
            Some(k) => {
                if caution(c.environment) > caution(k.environment) {
                    k.environment = c.environment;
                }
                k.user = k.user.take().or(c.user);
                k.note = k.note.take().or(c.note);
                for (key, value) in c.options {
                    k.options.entry(key).or_insert(value);
                }
            }
        }
    }
    *list = kept;
}

fn target_key(engine: Engine, host: &str, port: Option<u16>, db: Option<&str>, user: Option<&str>) -> String {
    let port = port.unwrap_or(match engine {
        Engine::Mssql => 1433,
        Engine::Postgres => 5432,
        Engine::Yugabyte => 5433,
        Engine::Mysql => 3306,
        Engine::Sqlite => 0,
    });
    format!(
        "{engine:?}|{}|{port}|{}|{}",
        host.to_ascii_lowercase(),
        db.unwrap_or("").to_ascii_lowercase(),
        user.unwrap_or("").to_ascii_lowercase()
    )
}

/// Mark candidates that match a saved connection and give the rest aliases that are unique and not taken.
pub(crate) fn reconcile(list: &mut [ImportCandidate], saved: &[(String, store::StoredConnection)]) {
    let key = |e: Engine, h: &str, p: Option<u16>, d: Option<&str>| target_key(e, h, p, d, None);
    let mut taken: std::collections::HashSet<String> = saved.iter().map(|(n, _)| n.clone()).collect();
    for c in list.iter_mut() {
        let this = key(c.engine, &c.host, c.port, c.database.as_deref());
        c.saved_as = saved
            .iter()
            .find(|(_, s)| key(s.engine, &s.host, s.port, s.database.as_deref()) == this)
            .map(|(n, _)| n.clone());
        if c.saved_as.is_some() {
            continue;
        }
        let base = c.alias.clone();
        let mut n = 2;
        while taken.contains(&c.alias) {
            c.alias = format!("{}-{n}", base.chars().take(60).collect::<String>());
            n += 1;
        }
        taken.insert(c.alias.clone());
    }
}

/// The stored form of a candidate. Everything comes from the caller (the UI can edit aliases), so it is validated again.
pub(crate) fn to_stored(c: &ImportCandidate) -> Result<store::StoredConnection> {
    store::validate_name(&c.alias)?;
    let host = c.host.trim();
    if host.is_empty() || host.chars().any(|ch| ch.is_whitespace() || ch.is_control()) {
        return Err(Error::invalid(format!("'{}' has no usable host", c.alias)));
    }
    let auth = match c.auth {
        AuthKind::Entra if !entra_host_allowed(c.engine, host) => {
            return Err(Error::invalid(format!(
                "'{}' would send a Microsoft Entra ID token to {host}, which is not an Azure database host",
                c.alias
            )));
        }
        AuthKind::AwsIam => AuthKind::Password,
        other => other,
    };
    let options = c
        .options
        .iter()
        .filter_map(|(k, v)| {
            CARRIED_OPTIONS.iter().find(|o| o.eq_ignore_ascii_case(k)).map(|o| (o.to_string(), v.clone()))
        })
        .collect();
    Ok(store::StoredConnection {
        engine: c.engine,
        environment: c.environment,
        host: host.to_string(),
        port: c.port,
        database: c.database.clone().filter(|d| !d.is_empty()),
        user: c.user.clone().filter(|u| !u.is_empty()),
        auth,
        password_env: None,
        password_saved: (auth == AuthKind::Password).then_some(false),
        options,
        agent: None,
        query_log: None,
        no_query_log: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const ADS: &str = r#"{
        // Azure Data Studio keeps comments and trailing commas
        "workbench.colorTheme": "Default Dark",
        "datasource.connections": [
            {
                "options": {
                    "server": "tcp:shop-sql-stg.database.windows.net,1433",
                    "database": "shop",
                    "authenticationType": "AzureMFA",
                    "user": "dev@example.com",
                    "password": "never-read-this",
                    "connectionName": "Shop (staging)",
                    "encrypt": "Mandatory",
                    "trustServerCertificate": false,
                },
                "groupId": "C777F06B-202E-4480-B475-FA416154D458",
                "providerName": "MSSQL",
                "savePassword": true,
            },
            {
                "options": { "host": "localhost", "port": "5433", "dbname": "orders", "user": "app", "sslmode": "disable",
                             "authenticationType": "SqlLogin", "connectionName": "orders local" },
                "providerName": "PGSQL"
            },
            {
                "options": { "server": "10.0.0.5\\REPORTING", "database": "reports", "authenticationType": "Integrated" },
                "providerName": "MSSQL"
            },
            { "options": { "server": "cosmos" }, "providerName": "COSMOS" }
        ],
        "mssql.connections": [
            { "server": "tcp:shop-sql-prd.database.windows.net,1433", "database": "shop", "authenticationType": "AzureMFA",
              "profileName": "shop prod" },
            { "server": "build-box,1533", "database": "ci", "authenticationType": "SqlLogin", "user": "ci",
              "password": "never-read-this", "encrypt": false, "trustServerCertificate": true, "profileName": "CI box" },
            { "server": "", "profileName": "broken" }
        ]
    }"#;

    #[test]
    fn azure_data_studio_settings() {
        let list = parse_vscode_settings(ImportSource::Ads, ADS, "settings.json").unwrap();
        let names: Vec<&str> = list.iter().map(|c| c.alias.as_str()).collect();
        assert_eq!(names, vec!["shop-staging", "orders-local", "10.0.0.5-reports", "shop-prod", "ci-box"]);

        let shop = &list[0];
        assert_eq!((shop.engine, shop.auth, shop.port), (Engine::Mssql, AuthKind::Entra, Some(1433)));
        assert_eq!(shop.host, "shop-sql-stg.database.windows.net");
        assert_eq!(shop.user, None, "Entra ID uses the signed-in account");
        assert_eq!(shop.environment, Environment::Staging);
        assert_eq!(shop.options.get("encrypt").map(String::as_str), Some("true"));

        let orders = &list[1];
        assert_eq!(
            (orders.engine, orders.port, orders.database.as_deref()),
            (Engine::Postgres, Some(5433), Some("orders"))
        );
        assert_eq!((orders.environment, orders.user.as_deref()), (Environment::Local, Some("app")));
        assert_eq!(orders.options.get("sslmode").map(String::as_str), Some("disable"));

        let reporting = &list[2];
        assert_eq!((reporting.host.as_str(), reporting.auth), ("10.0.0.5", AuthKind::Password));
        assert!(reporting.note.as_deref().unwrap().contains("Windows authentication"));

        assert_eq!(list[3].environment, Environment::Production);
        let ci = &list[4];
        assert_eq!((ci.host.as_str(), ci.port, ci.user.as_deref()), ("build-box", Some(1533), Some("ci")));
        assert_eq!(ci.options.get("trustServerCertificate").map(String::as_str), Some("true"));
        assert_eq!(ci.options.get("encrypt").map(String::as_str), Some("false"));

        let json = serde_json::to_string(&list).unwrap();
        assert!(!json.contains("never-read-this"), "passwords in settings files are never carried");
    }

    const DBEAVER: &str = r#"{
        "folders": {},
        "connections": {
            "postgres-jdbc-1": {
                "provider": "postgresql", "driver": "postgres-jdbc", "name": "Shop Prod", "save-password": true,
                "configuration": { "host": "shop-db.internal.example", "port": "5432", "database": "shop",
                    "url": "jdbc:postgresql://shop-db.internal.example:5432/shop", "configurationType": "MANUAL",
                    "type": "prod", "auth-model": "native",
                    "handlers": { "postgre_ssl": { "enabled": true, "properties": { "sslMode": "verify-full" } } } }
            },
            "mysql8-2": {
                "provider": "mysql", "driver": "mysql8", "name": "events",
                "configuration": { "url": "jdbc:mysql://events.example:3306/events?user=x&password=never-read-this",
                    "configurationType": "URL", "type": "dev", "user": "events_ro" }
            },
            "mssql-3": {
                "provider": "sqlserver", "driver": "microsoft", "name": "Billing (Azure)",
                "configuration": { "host": "billing.database.windows.net", "port": "1433", "database": "billing",
                    "configurationType": "MANUAL", "type": "test", "auth-model": "sqlserver_ad_mfa",
                    "properties": { "encrypt": "true", "password": "never-read-this" } }
            },
            "oracle-4": { "provider": "oracle", "name": "ledger", "configuration": { "host": "ledger" } },
            "postgres-5": { "provider": "postgresql", "name": "dup", "configuration": { "host": "shop-db.internal.example",
                "port": "5432", "database": "shop" } }
        }
    }"#;

    #[test]
    fn dbeaver_data_sources() {
        let mut list = parse_dbeaver(DBEAVER, "data-sources.json").unwrap();
        dedupe(&mut list);
        let names: Vec<&str> = list.iter().map(|c| c.alias.as_str()).collect();
        assert_eq!(names.len(), 3, "{names:?}");
        let shop = list.iter().find(|c| c.engine == Engine::Postgres).unwrap();
        assert_eq!(shop.environment, Environment::Production, "listed twice; the production marking wins");
        assert_eq!(shop.options.get("sslmode").map(String::as_str), Some("verify-full"));
        let events = list.iter().find(|c| c.alias == "events").unwrap();
        assert_eq!(
            (events.engine, events.host.as_str(), events.user.as_deref()),
            (Engine::Mysql, "events.example", Some("events_ro"))
        );
        assert_eq!(events.environment, Environment::Development);
        let billing = list.iter().find(|c| c.alias == "billing-azure").unwrap();
        assert_eq!(
            (billing.engine, billing.auth, billing.environment),
            (Engine::Mssql, AuthKind::Entra, Environment::Staging)
        );
        assert_eq!(billing.options.get("encrypt").map(String::as_str), Some("true"));
        assert!(!serde_json::to_string(&list).unwrap().contains("never-read-this"));
    }

    const DATAGRIP: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<project version="4">
  <component name="DataSourceManagerImpl" format="xml" multifile-model="true">
    <data-source source="LOCAL" name="shop@localhost" uuid="a1">
      <driver-ref>postgresql</driver-ref>
      <jdbc-driver>org.postgresql.Driver</jdbc-driver>
      <jdbc-url>jdbc:postgresql://localhost:55432/shop</jdbc-url>
    </data-source>
    <data-source source="LOCAL" name="reports" uuid="b2">
      <driver-ref>sqlserver.jb</driver-ref>
      <jdbc-url>jdbc:jtds:sqlserver://reports.example:1433/reports</jdbc-url>
    </data-source>
    <data-source source="LOCAL" name="catalog" uuid="c3">
      <driver-ref>sqlserver.ms</driver-ref>
      <jdbc-url>jdbc:sqlserver://catalog.example:1433;database=catalog;encrypt=true;trustServerCertificate=true</jdbc-url>
    </data-source>
    <data-source source="LOCAL" name="cache" uuid="d4">
      <driver-ref>redis</driver-ref>
      <jdbc-url>jdbc:redis://localhost:6379/0</jdbc-url>
    </data-source>
  </component>
</project>"#;

    const DATAGRIP_LOCAL: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<project version="4">
  <component name="dataSourceStorageLocal">
    <data-source name="shop@localhost" uuid="a1">
      <secret-storage>master_key</secret-storage>
      <user-name>postgres</user-name>
    </data-source>
  </component>
</project>"#;

    #[test]
    fn datagrip_data_sources() {
        let users = datagrip_users(DATAGRIP_LOCAL);
        let list = parse_datagrip(DATAGRIP, "dataSources.xml", &users).unwrap();
        let names: Vec<&str> = list.iter().map(|c| c.alias.as_str()).collect();
        assert_eq!(names, vec!["shop-localhost", "reports", "catalog"]);
        assert_eq!(
            (list[0].port, list[0].user.as_deref(), list[0].environment),
            (Some(55432), Some("postgres"), Environment::Local)
        );
        assert_eq!(
            (list[1].engine, list[1].host.as_str(), list[1].database.as_deref()),
            (Engine::Mssql, "reports.example", Some("reports"))
        );
        assert_eq!(list[2].options.get("trustServerCertificate").map(String::as_str), Some("true"));
    }

    #[test]
    fn dtds_are_refused() {
        let xxe = r#"<?xml version="1.0"?><!DOCTYPE x [<!ENTITY e SYSTEM "file:///etc/passwd">]>
            <project><component><data-source name="&e;"><jdbc-url>jdbc:postgresql://h/db</jdbc-url></data-source></component></project>"#;
        assert!(parse_datagrip(xxe, "x.xml", &BTreeMap::new()).is_err());
    }

    #[test]
    fn aliases_are_safe_and_unique() {
        assert_eq!(alias_from("Shop (staging)"), "shop-staging");
        assert_eq!(alias_from("  ../../etc/passwd "), "etc-passwd");
        assert_eq!(alias_from("***"), "imported");
        let mut list = parse_vscode_settings(ImportSource::Vscode, ADS, "settings.json").unwrap();
        let mut existing = to_stored(&list[1]).unwrap();
        existing.port = Some(5433);
        let saved =
            vec![("orders-local".to_string(), existing), ("shop-staging".to_string(), to_stored(&list[4]).unwrap())];
        reconcile(&mut list, &saved);
        assert_eq!(list[1].saved_as.as_deref(), Some("orders-local"), "same database, already saved");
        assert_eq!(list[0].alias, "shop-staging-2", "the alias is taken by another connection");
        assert_eq!(list[4].saved_as.as_deref(), Some("shop-staging"));
    }

    #[test]
    fn stored_form_is_validated_again() {
        let list = parse_vscode_settings(ImportSource::Ads, ADS, "settings.json").unwrap();
        let mut shop = list[0].clone();
        shop.host = "attacker.example".into();
        assert!(to_stored(&shop).is_err(), "Entra tokens only go to Azure hosts");
        let mut ci = list[4].clone();
        ci.options.insert("password".into(), "x".into());
        ci.options.insert("sslrootcert".into(), "/tmp/x".into());
        let stored = to_stored(&ci).unwrap();
        assert!(!stored.options.contains_key("password") && !stored.options.contains_key("sslrootcert"));
        assert_eq!((stored.auth, stored.password_saved), (AuthKind::Password, Some(false)));
        ci.alias = "bad name".into();
        assert!(to_stored(&ci).is_err());
    }

    #[test]
    fn jsonc_keeps_strings_intact() {
        let text = r#"{ "a": "http://x // not a comment", /* gone */ "b": [1, 2,], }"#;
        let v: Value = serde_json::from_str(&strip_jsonc(text)).unwrap();
        assert_eq!(v["a"], "http://x // not a comment");
        assert_eq!(v["b"], serde_json::json!([1, 2]));
    }
    #[test]
    fn native_ysql_client_profiles_keep_the_dedicated_provider_and_default_port() {
        assert!(!yugabyte_driver("mysql mysql8"));
        assert!(yugabyte_driver("postgresql ysql-jdbc"));
        let dbeaver = r#"{"connections":{"yb":{"provider":"postgresql","driver":"yugabytedb","name":"YB",
            "configuration":{"url":"jdbc:postgresql://db.example/shop","configurationType":"URL","user":"app"}}}}"#;
        let imported = parse_dbeaver(dbeaver, "dbeaver.json").unwrap();
        assert_eq!(imported[0].engine, Engine::Yugabyte);
        let xml = r#"<project><component><data-source name="YB"><driver-ref>yugabytedb</driver-ref>
            <jdbc-url>jdbc:postgresql://db.example/shop</jdbc-url></data-source></component></project>"#;
        let imported = parse_datagrip(xml, "datasources.xml", &std::collections::BTreeMap::new()).unwrap();
        assert_eq!(imported[0].engine, Engine::Yugabyte);
        assert_eq!(
            target_key(Engine::Yugabyte, "db.example", None, Some("shop"), Some("app")),
            target_key(Engine::Yugabyte, "db.example", Some(5433), Some("shop"), Some("app"))
        );
    }
}
