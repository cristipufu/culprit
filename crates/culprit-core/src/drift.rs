//! Environment drift: what differs between two databases that should look alike (alpha vs prod, a replica vs its primary).

use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::model::{Engine, Environment};

#[derive(Serialize, Deserialize, TS, Clone, Debug)]
#[ts(export)]
pub struct Drift {
    pub a: DriftSide,
    pub b: DriftSide,
    /// "3 differences: 2 indexes and 1 setting" or "No differences in tables, columns, indexes or settings".
    pub summary: String,
    pub items: Vec<DriftItem>,
}

/// One of the compared databases, as of its scan.
#[derive(Serialize, Deserialize, TS, Clone, Debug)]
#[ts(export)]
pub struct DriftSide {
    pub connection: String,
    pub environment: Environment,
    pub engine: Engine,
    pub product: String,
    pub version: String,
    pub database: String,
    pub taken_at: String,
}

impl DriftSide {
    /// "SQL Server 2022 16.0.4295.3", "PostgreSQL 18.6 (Debian …)": product and version without saying the version twice.
    pub fn server(&self) -> String {
        server_label(&self.product, &self.version)
    }
}

pub fn server_label(product: &str, version: &str) -> String {
    let (first, rest) = version.split_once(' ').unwrap_or((version, ""));
    match (product.ends_with(first), rest.is_empty()) {
        (true, true) => product.to_string(),
        (true, false) => format!("{product} {rest}"),
        (false, _) => format!("{product} {version}"),
    }
}

#[derive(Serialize, Deserialize, TS, Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "lowercase")]
#[ts(export)]
pub enum DriftKind {
    Index,
    Table,
    Column,
    Setting,
    Version,
}

#[derive(Serialize, Deserialize, TS, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
#[ts(export)]
pub enum DriftPresence {
    OnlyA,
    OnlyB,
    Different,
    /// The same definition under a different name: harmless for performance, worth knowing for migrations.
    Renamed,
}

#[derive(Serialize, Deserialize, TS, Clone, Debug)]
#[ts(export)]
pub struct DriftItem {
    pub kind: DriftKind,
    pub presence: DriftPresence,
    /// Table, `table.column`, index or setting name.
    pub subject: String,
    /// One plain sentence: "orders has an index on (customer_id) in alpha that prod lacks".
    pub title: String,
    /// What each side has (a definition, a type, a value).
    pub a: Option<String>,
    pub b: Option<String>,
    /// SQL that makes the side missing something match the other, and which connection it is for.
    pub sql: Option<String>,
    pub sql_for: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::server_label;

    #[test]
    fn server_labels_never_repeat_the_version() {
        assert_eq!(server_label("SQL Server 2022", "16.0.4295.3"), "SQL Server 2022 16.0.4295.3");
        assert_eq!(server_label("PostgreSQL 18.6", "18.6 (Debian 18.6-1)"), "PostgreSQL 18.6 (Debian 18.6-1)");
        assert_eq!(server_label("MySQL 8.4.3", "8.4.3"), "MySQL 8.4.3");
    }
}
