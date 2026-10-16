//! Saved connections: metadata in `connections.toml`. Secrets live in a `secrets::SecretStore` (the OS keychain).

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use culprit_core::{AuthKind, ConnectionSpec, Engine, Environment, Error, Result};
use serde::{Deserialize, Serialize};

use crate::private_fs;

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct StoredConnection {
    pub engine: Engine,
    pub environment: Environment,
    pub host: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub database: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    #[serde(default)]
    pub auth: AuthKind,
    /// Read the password from this environment variable instead of the OS keychain (CI, agents).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password_env: Option<String>,
    /// Whether a password was stored in the keychain; `None` for connections saved before Culprit tracked it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password_saved: Option<bool>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub options: BTreeMap<String, String>,
    /// What coding agents may do with this connection (`culprit mcp`); `stats` when unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<crate::agent::AgentAccess>,
    /// A log the application's framework writes (Rails, Django, Entity Framework Core, Phoenix), which scans read the
    /// statements from when the database keeps no history of them (SQLite). An absolute path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query_log: Option<String>,
    /// The person removed the query log: scans don't look for one again.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub no_query_log: bool,
}

impl StoredConnection {
    pub fn spec(&self) -> ConnectionSpec {
        ConnectionSpec {
            engine: Some(self.engine),
            host: self.host.clone(),
            port: self.port,
            database: self.database.clone(),
            user: self.user.clone(),
            auth: self.auth,
            environment: self.environment,
            options: self.options.clone(),
            password: None,
            token: None,
        }
    }
}

#[derive(Serialize, Deserialize, Default)]
struct ConnectionsFile {
    #[serde(default)]
    connections: BTreeMap<String, StoredConnection>,
}

pub struct Paths {
    pub config: PathBuf,
    pub data: PathBuf,
}

impl Paths {
    /// `CULPRIT_HOME` puts everything in one directory; otherwise XDG-style config and data directories.
    pub fn resolve() -> Result<Paths> {
        if let Some(home) = std::env::var_os("CULPRIT_HOME") {
            let home = PathBuf::from(home);
            return Ok(Paths { config: home.clone(), data: home });
        }
        let config = xdg("XDG_CONFIG_HOME", ".config", dirs::config_dir)?;
        let data = xdg("XDG_DATA_HOME", ".local/share", dirs::data_dir)?;
        Ok(Paths { config: config.join("culprit"), data: data.join("culprit") })
    }

    pub fn connections_file(&self) -> PathBuf {
        self.config.join("connections.toml")
    }

    pub fn reports_dir(&self, connection: &str) -> PathBuf {
        self.data.join("reports").join(safe_file_name(connection))
    }
}

fn xdg(var: &str, unix_default: &str, fallback: fn() -> Option<PathBuf>) -> Result<PathBuf> {
    if let Some(v) = std::env::var_os(var).filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(v));
    }
    if cfg!(windows) {
        return fallback().ok_or_else(|| Error::internal("Cannot find the user's application data directory"));
    }
    dirs::home_dir().map(|h| h.join(unix_default)).ok_or_else(|| Error::internal("Cannot find the home directory"))
}

pub fn safe_file_name(name: &str) -> String {
    let mut safe: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' { c } else { '_' })
        .collect();
    // `.` and `..` are the directory itself and its parent; a leading dot also hides the folder. Never produce either.
    if safe.starts_with('.') || safe.is_empty() {
        safe.insert(0, '_');
    }
    if cfg!(windows) { windows_safe(safe) } else { safe }
}

/// Windows reserves device names (CON, NUL, COM1…) whatever the extension, and drops a trailing dot. Only applied on
/// Windows, so existing report folders elsewhere keep their names.
pub(crate) fn windows_safe(mut name: String) -> String {
    let stem = name.split('.').next().unwrap_or_default().to_ascii_uppercase();
    let numbered =
        stem.len() == 4 && (stem.starts_with("COM") || stem.starts_with("LPT")) && stem.as_bytes()[3].is_ascii_digit();
    if numbered || ["CON", "PRN", "AUX", "NUL"].contains(&stem.as_str()) {
        name.insert(0, '_');
    }
    if name.ends_with('.') {
        name.push('_');
    }
    name
}

pub fn validate_name(name: &str) -> Result<()> {
    // No leading dot: names become folder names, and `.`/`..` would point at the reports folder itself or its parent.
    let ok = !name.is_empty()
        && name.len() <= 64
        && !name.starts_with('.')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.');
    if ok {
        return Ok(());
    }
    Err(Error::invalid(format!("'{name}' is not a valid connection name"))
        .with_hint("Use letters, digits, '-', '_' or '.' (not first), up to 64 characters, e.g. shop-staging"))
}

pub struct Store {
    file: PathBuf,
}

impl Store {
    pub fn new(paths: &Paths) -> Store {
        Store { file: paths.connections_file() }
    }

    fn load(&self) -> Result<ConnectionsFile> {
        if !self.file.exists() {
            return Ok(ConnectionsFile::default());
        }
        let text = fs::read_to_string(&self.file).map_err(|e| io_error(&self.file, e))?;
        toml::from_str(&text).map_err(|e| Error::invalid(format!("{} is not valid: {e}", self.file.display())))
    }

    fn save(&self, file: &ConnectionsFile) -> Result<()> {
        if let Some(dir) = self.file.parent() {
            private_fs::create_dir(dir).map_err(|e| io_error(dir, e))?;
        }
        let text = toml::to_string_pretty(file).map_err(|e| Error::internal(e.to_string()))?;
        private_fs::write_atomic(&self.file, text.as_bytes()).map_err(|e| io_error(&self.file, e))
    }

    pub fn list(&self) -> Result<Vec<(String, StoredConnection)>> {
        Ok(self.load()?.connections.into_iter().collect())
    }

    pub fn get(&self, name: &str) -> Result<StoredConnection> {
        self.load()?.connections.remove(name).ok_or_else(|| {
            Error::not_found(format!("No connection named '{name}'"))
                .with_hint("List saved connections with `culprit conn list`, add one with `culprit conn add`")
        })
    }

    pub fn put(&self, name: &str, connection: StoredConnection) -> Result<()> {
        validate_name(name)?;
        let mut file = self.load()?;
        file.connections.insert(name.to_string(), connection);
        self.save(&file)
    }

    pub fn rename(&self, old: &str, new: &str) -> Result<()> {
        validate_name(new)?;
        let mut file = self.load()?;
        if file.connections.contains_key(new) {
            return Err(Error::invalid(format!("A connection named '{new}' already exists")));
        }
        let connection =
            file.connections.remove(old).ok_or_else(|| Error::not_found(format!("No connection named '{old}'")))?;
        file.connections.insert(new.to_string(), connection);
        self.save(&file)
    }

    pub fn remove(&self, name: &str) -> Result<()> {
        let mut file = self.load()?;
        if file.connections.remove(name).is_none() {
            return Err(Error::not_found(format!("No connection named '{name}'")));
        }
        self.save(&file)
    }
}

fn io_error(path: &Path, error: std::io::Error) -> Error {
    Error::internal(format!("{}: {error}", path.display()))
}

#[cfg(test)]
mod name_tests {
    use super::{safe_file_name, validate_name};

    #[test]
    fn names_never_leave_the_reports_folder() {
        for bad in [".", "..", ".hidden", "", "a/b", "a\\b"] {
            assert!(validate_name(bad).is_err(), "{bad:?}");
        }
        assert!(validate_name("shop-staging.eu").is_ok());
        // Whatever the platform's extra rules, a folder name is never empty and never starts with a dot.
        for name in ["..", ".", "", "../x", ".hidden"] {
            let safe = safe_file_name(name);
            assert!(!safe.is_empty() && !safe.starts_with('.') && !safe.contains('/'), "{name:?} -> {safe:?}");
        }
        assert_eq!(safe_file_name("shop-staging"), "shop-staging");
    }
}

#[cfg(test)]
mod windows_names {
    use super::windows_safe;

    #[test]
    fn device_names_and_trailing_dots_get_a_safe_spelling() {
        for (name, safe) in [
            ("con", "_con"),
            ("NUL.json", "_NUL.json"),
            ("com1", "_com1"),
            ("lpt9.x", "_lpt9.x"),
            ("console", "console"),
            ("com10", "com10"),
            ("shop.", "shop._"),
            ("shop-staging", "shop-staging"),
        ] {
            assert_eq!(windows_safe(name.to_string()), safe, "{name}");
        }
    }
}
