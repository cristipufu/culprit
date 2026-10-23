//! Saved queries: named statements kept in `queries.toml` next to `connections.toml`, rerun from the editor, the command
//! palette or the CLI. They hold SQL only, never results or secrets.

use std::fs;
use std::path::{Path, PathBuf};

use culprit_core::{Error, ErrorCode, Result};
use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::private_fs;

#[derive(Serialize, Deserialize, TS, Clone, Debug, PartialEq)]
#[ts(export)]
pub struct SavedQuery {
    /// The alias it is found and run by, unique ignoring case.
    pub name: String,
    pub sql: String,
    /// The connection it was saved for. It can run on any connection; this one is the default.
    #[serde(default)]
    pub connection: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    #[serde(default)]
    pub last_run_at: Option<String>,
}

/// What the UI and CLI send to save a query. `previous_name` renames an existing one.
#[derive(Serialize, Deserialize, TS, Clone, Debug, Default)]
#[ts(export)]
pub struct SaveQuery {
    pub name: String,
    pub sql: String,
    #[serde(default)]
    pub connection: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub previous_name: Option<String>,
}

#[derive(Serialize, Deserialize, Default)]
struct QueriesFile {
    #[serde(default, rename = "query")]
    queries: Vec<SavedQuery>,
}

pub struct SavedQueries {
    file: PathBuf,
}

const MAX_NAME: usize = 80;

fn now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

fn io_error(path: &Path, error: std::io::Error) -> Error {
    Error::internal(format!("{}: {error}", path.display()))
}

fn same(a: &str, b: &str) -> bool {
    a.trim().eq_ignore_ascii_case(b.trim())
}

impl SavedQueries {
    pub fn new(config_dir: &Path) -> SavedQueries {
        SavedQueries { file: config_dir.join("queries.toml") }
    }

    fn load(&self) -> Result<QueriesFile> {
        if !self.file.exists() {
            return Ok(QueriesFile::default());
        }
        let text = fs::read_to_string(&self.file).map_err(|e| io_error(&self.file, e))?;
        toml::from_str(&text).map_err(|e| Error::invalid(format!("{} is not valid: {e}", self.file.display())))
    }

    fn save(&self, file: &QueriesFile) -> Result<()> {
        if let Some(dir) = self.file.parent() {
            private_fs::create_dir(dir).map_err(|e| io_error(dir, e))?;
        }
        let text = toml::to_string_pretty(file).map_err(|e| Error::internal(e.to_string()))?;
        private_fs::write_atomic(&self.file, text.as_bytes()).map_err(|e| io_error(&self.file, e))
    }

    /// Every saved query, most recently used first.
    pub fn list(&self) -> Result<Vec<SavedQuery>> {
        let mut queries = self.load()?.queries;
        queries.sort_by(|a, b| {
            let used = |q: &SavedQuery| q.last_run_at.clone().unwrap_or_else(|| q.updated_at.clone());
            used(b).cmp(&used(a)).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        });
        Ok(queries)
    }

    pub fn get(&self, name: &str) -> Result<SavedQuery> {
        self.load()?.queries.into_iter().find(|q| same(&q.name, name)).ok_or_else(|| {
            Error::not_found(format!("No saved query named '{name}'"))
                .with_hint("List them with `culprit saved list`, save one with `culprit saved add`")
        })
    }

    /// Create or update (by name, or by `previous_name` to rename) and return the stored query.
    pub fn put(&self, input: SaveQuery) -> Result<SavedQuery> {
        let _lock = private_fs::lock(&self.file).map_err(|e| io_error(&self.file, e))?;
        let name = input.name.trim().to_string();
        if name.is_empty() || name.chars().count() > MAX_NAME || name.chars().any(char::is_control) {
            return Err(Error::invalid(format!("A saved query needs a name of 1 to {MAX_NAME} characters")));
        }
        if input.sql.trim().is_empty() {
            return Err(Error::invalid("There is no SQL to save"));
        }
        let mut file = self.load()?;
        let target = input.previous_name.as_deref().unwrap_or(&name);
        let clash = file.queries.iter().any(|q| same(&q.name, &name) && !same(&q.name, target));
        if clash {
            return Err(Error::new(ErrorCode::InvalidInput, format!("A saved query named '{name}' already exists"))
                .with_hint("Pick another name, or open that query and save over it."));
        }
        let now = now();
        let stored = match file.queries.iter_mut().find(|q| same(&q.name, target)) {
            Some(existing) => {
                existing.name = name;
                existing.sql = input.sql;
                existing.connection = input.connection.filter(|c| !c.is_empty());
                existing.description = input.description.filter(|d| !d.trim().is_empty());
                existing.updated_at = now;
                existing.clone()
            }
            None => {
                let query = SavedQuery {
                    name,
                    sql: input.sql,
                    connection: input.connection.filter(|c| !c.is_empty()),
                    description: input.description.filter(|d| !d.trim().is_empty()),
                    created_at: now.clone(),
                    updated_at: now,
                    last_run_at: None,
                };
                file.queries.push(query.clone());
                query
            }
        };
        self.save(&file)?;
        Ok(stored)
    }

    pub fn delete(&self, name: &str) -> Result<()> {
        let _lock = private_fs::lock(&self.file).map_err(|e| io_error(&self.file, e))?;
        let mut file = self.load()?;
        let before = file.queries.len();
        file.queries.retain(|q| !same(&q.name, name));
        if file.queries.len() == before {
            return Err(Error::not_found(format!("No saved query named '{name}'")));
        }
        self.save(&file)
    }

    /// Record a run so the list puts the queries in use first.
    pub fn touch(&self, name: &str) -> Result<()> {
        let _lock = private_fs::lock(&self.file).map_err(|e| io_error(&self.file, e))?;
        let mut file = self.load()?;
        if let Some(query) = file.queries.iter_mut().find(|q| same(&q.name, name)) {
            query.last_run_at = Some(now());
            self.save(&file)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(name: &str, sql: &str) -> SaveQuery {
        SaveQuery { name: name.into(), sql: sql.into(), ..Default::default() }
    }

    #[test]
    fn save_rename_run_delete() {
        let dir = std::env::temp_dir().join(format!("culprit-saved-{}", std::process::id()));
        let saved = SavedQueries::new(&dir);
        saved.put(input("Open orders", "SELECT * FROM orders WHERE status = 'open'")).unwrap();
        saved.put(input("Late shipments", "SELECT 1")).unwrap();
        assert!(saved.put(input("open ORDERS", "SELECT 2")).is_ok(), "same name updates in place");
        assert_eq!(saved.get("OPEN orders").unwrap().sql, "SELECT 2");
        let clash = SaveQuery { previous_name: Some("Late shipments".into()), ..input("Open orders", "SELECT 3") };
        assert!(saved.put(clash).is_err(), "renaming onto another query is refused");
        let renamed = SaveQuery { previous_name: Some("Late shipments".into()), ..input("Late", "SELECT 3") };
        assert_eq!(saved.put(renamed).unwrap().name, "Late");
        saved.touch("Late").unwrap();
        assert_eq!(saved.list().unwrap()[0].name, "Late", "recently run first");
        assert!(saved.put(input("  ", "SELECT 1")).is_err());
        assert!(saved.put(input("Empty", "  ")).is_err());
        saved.delete("late").unwrap();
        assert!(saved.get("Late").is_err());
        assert!(saved.delete("Late").is_err());
        let _ = fs::remove_dir_all(dir);
    }
}
