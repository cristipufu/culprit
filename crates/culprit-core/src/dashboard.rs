//! The per-database dashboard: one database at a glance. Each tile leads with one plain sentence, carries the few numbers
//! behind it and names where to look next. Built from a report (plus, optionally, one live read), so the app, the CLI
//! and the MCP server say the same thing.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::model::{Engine, Environment};

/// One database at a glance.
#[derive(Serialize, Deserialize, TS, Clone, Debug, PartialEq)]
#[ts(export)]
pub struct Dashboard {
    pub connection: String,
    pub engine: Engine,
    /// "PostgreSQL 15.17", "MariaDB 11.8.9".
    pub product: String,
    pub database: String,
    pub environment: Environment,
    /// The scan the tiles read.
    pub report_id: String,
    pub taken_at: String,
    /// What the workload numbers cover, e.g. "Query Store, the last 7 days".
    pub coverage: Option<String>,
    /// One sentence for the whole database: what to look at first, or that nothing needs a look.
    pub headline: String,
    /// Tiles that need a look, most urgent first.
    pub attention: Vec<TileKey>,
    pub tiles: Vec<Tile>,
}

impl Dashboard {
    pub fn tile(&self, key: TileKey) -> Option<&Tile> {
        self.tiles.iter().find(|t| t.key == key)
    }
}

#[derive(Serialize, Deserialize, TS, Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[serde(rename_all = "kebab-case")]
#[ts(export)]
pub enum TileKey {
    /// Database time across the window, its trend and the plan changes in it.
    DbTime,
    /// Findings by severity and the three that matter most.
    Findings,
    /// The heaviest queries and their share of database time.
    TopQueries,
    /// What the database waits on.
    Waits,
    /// Sessions right now: running, blocked, idle in a transaction.
    Now,
    /// Pressure signals: compiles, memory, cache, connections, the oldest open transaction.
    Health,
    /// Deadlocks in the last day.
    Deadlocks,
    /// The index advisor: indexes to add and to clean up.
    Indexes,
    /// What the captured plans have wrong with them.
    Plans,
    /// Where query statistics come from and how much they cover.
    Statistics,
    /// How fast storage answers: average read latency of data files, write latency of the log.
    Storage,
    /// New connections per second, and how many of the allowed are in use.
    Connections,
    /// How far replicas and replication slots are behind.
    Replication,
    /// Statements whose runs fail or are cancelled.
    Failures,
}

impl TileKey {
    pub fn label(self) -> &'static str {
        match self {
            TileKey::DbTime => "Database time",
            TileKey::Findings => "Findings",
            TileKey::TopQueries => "Top queries",
            TileKey::Waits => "Where time goes",
            TileKey::Now => "Right now",
            TileKey::Health => "Health",
            TileKey::Deadlocks => "Deadlocks",
            TileKey::Indexes => "Indexes",
            TileKey::Plans => "Plans",
            TileKey::Statistics => "Query statistics",
            TileKey::Storage => "Storage",
            TileKey::Connections => "Connections",
            TileKey::Replication => "Replication",
            TileKey::Failures => "Failing queries",
        }
    }
}

/// How a tile reads at a glance. `Unavailable` means this engine, login or scan can't show it; the tile says why.
#[derive(Serialize, Deserialize, TS, Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "kebab-case")]
#[ts(export)]
pub enum TileStatus {
    Good,
    Info,
    Unavailable,
    Watch,
    Bad,
}

#[derive(Serialize, Deserialize, TS, Clone, Debug, PartialEq)]
#[ts(export)]
pub struct Tile {
    pub key: TileKey,
    pub title: String,
    pub status: TileStatus,
    /// One plain sentence: the first thing to read.
    pub takeaway: String,
    /// A second line: what the numbers cover, or what would tell more.
    pub detail: Option<String>,
    pub metrics: Vec<TileMetric>,
    /// The rows behind the takeaway, biggest first.
    pub items: Vec<TileItem>,
    /// Copy-ready SQL that unlocks this tile. Culprit never runs it.
    pub fix_sql: Option<String>,
    /// Where an agent looks next, e.g. `culprit top <connection>`.
    pub next: Option<String>,
}

impl Tile {
    pub fn new(key: TileKey, status: TileStatus, takeaway: impl Into<String>) -> Tile {
        Tile {
            key,
            title: key.label().to_string(),
            status,
            takeaway: takeaway.into(),
            detail: None,
            metrics: Vec::new(),
            items: Vec::new(),
            fix_sql: None,
            next: None,
        }
    }

    pub fn metric(&self, key: &str) -> Option<&TileMetric> {
        self.metrics.iter().find(|m| m.key == key)
    }
}

#[derive(Serialize, Deserialize, TS, Clone, Debug, PartialEq)]
#[ts(export)]
pub struct TileMetric {
    /// Stable key, e.g. `per-day`, `blocked`, `high`.
    pub key: String,
    pub label: String,
    pub value: Option<f64>,
    /// The value as a person reads it: "4 h 12 min", "71%".
    pub text: String,
}

impl TileMetric {
    pub fn new(key: &str, label: &str, value: Option<f64>, text: impl Into<String>) -> TileMetric {
        TileMetric { key: key.into(), label: label.into(), value, text: text.into() }
    }
}

#[derive(Serialize, Deserialize, TS, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
#[ts(export)]
pub enum ItemKind {
    Query,
    Finding,
    Wait,
    Signal,
    Session,
    Deadlock,
    IndexAdd,
    IndexCleanup,
    Rule,
    PlanChange,
    Capability,
}

/// One row behind a tile. `id` is what the item is (a query id, a finding id, a wait category, a rule id…); `query_id`
/// and `finding_id` say where it leads.
#[derive(Serialize, Deserialize, TS, Clone, Debug, PartialEq)]
#[ts(export)]
pub struct TileItem {
    pub kind: ItemKind,
    pub id: String,
    pub label: String,
    /// "41%", "12 min", "4 plans".
    pub value: Option<String>,
    /// 0–1, for a bar.
    pub share: Option<f64>,
    pub status: Option<TileStatus>,
    pub query_id: Option<String>,
    pub finding_id: Option<String>,
    /// When it happened (RFC 3339), for plan changes and deadlocks.
    pub at: Option<String>,
}

impl TileItem {
    pub fn new(kind: ItemKind, id: impl Into<String>, label: impl Into<String>) -> TileItem {
        TileItem {
            kind,
            id: id.into(),
            label: label.into(),
            value: None,
            share: None,
            status: None,
            query_id: None,
            finding_id: None,
            at: None,
        }
    }
}
