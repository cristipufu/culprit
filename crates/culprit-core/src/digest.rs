//! A scan in a few lines, for a scheduled job to post somewhere: what the database looks like now, what changed since
//! the previous digest, and the findings worth a look. Data only; the consumer decides how to show it.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::finding::{Fix, Severity};

#[derive(Serialize, Deserialize, TS, Clone, Debug, Default, PartialEq)]
#[ts(export)]
pub struct Digest {
    pub connection: String,
    /// "Azure SQL Database", "PostgreSQL 17.2".
    pub server: String,
    pub database: String,
    pub environment: Option<String>,
    /// When the scan was taken (UTC, ISO 8601).
    pub taken_at: String,
    /// What the query history covers: "the last 25 hours".
    pub window: Option<String>,
    /// The report's one-line summary.
    pub headline: String,
    /// Database time in the last 24 hours against the 24 hours before, in one sentence: "2h 10m of database time in
    /// the last 24 hours, 1.8× the 24 hours before (1h 12m)." Without the day before (a scan shorter than two days),
    /// the last day only; `None` without query history or under a day of it.
    pub database_time: Option<String>,
    /// The same as numbers.
    #[serde(default)]
    pub day: Option<DigestDay>,
    pub counts: DigestCounts,
    /// What changed since the previous digest; `None` when there was nothing to compare with.
    pub since: Option<DigestChanges>,
    /// The findings worth a look, most important first: one per query, table or index, the others about the same one
    /// in its `also`.
    pub top: Vec<DigestFinding>,
    /// Every finding about the database as it runs now: what the next digest compares with, so a scheduled job can
    /// keep this digest instead of the report (which holds statement text and parameter values).
    #[serde(default)]
    pub findings: Vec<DigestFinding>,
}

#[derive(Serialize, Deserialize, TS, Clone, Debug, Default, PartialEq)]
#[ts(export)]
pub struct DigestCounts {
    /// Findings about the database as it runs now.
    pub active: u32,
    pub high: u32,
    pub medium: u32,
    pub low: u32,
    /// Findings about queries that stopped running, most likely changed by a deploy.
    pub stopped: u32,
}

/// Database time per day, from the query history's timeline.
#[derive(Serialize, Deserialize, TS, Clone, Debug, Default, PartialEq)]
#[ts(export)]
pub struct DigestDay {
    /// Database time in the last 24 hours, in milliseconds.
    pub last_ms: f64,
    /// In the 24 hours before that; `None` when the history doesn't reach back that far.
    pub before_ms: Option<f64>,
}

#[derive(Serialize, Deserialize, TS, Clone, Debug, Default, PartialEq)]
#[ts(export)]
pub struct DigestChanges {
    /// When the previous digest's scan was taken (UTC, ISO 8601).
    pub taken_at: String,
    /// Active now, not active then.
    pub new: Vec<DigestFinding>,
    /// Active then, gone or stopped now.
    pub fixed: Vec<DigestFinding>,
    /// Active both times, more severe now.
    pub worse: Vec<DigestFinding>,
    /// Active both times, and what it counts since startup (lock escalations) happened again in between: each item's
    /// `count_since` says how many times.
    #[serde(default)]
    pub again: Vec<DigestFinding>,
}

#[derive(Serialize, Deserialize, TS, Clone, Debug, PartialEq)]
#[ts(export)]
pub struct DigestFinding {
    /// The finding's stable id (`culprit show <connection> <id>` opens it): what digests are compared by.
    pub id: String,
    pub title: String,
    pub severity: Severity,
    /// Share of database time behind it, 0–1, when it is about queries.
    pub db_time_share: Option<f64>,
    /// How big it is in a few words: "11% of DB time", "2.0 MB unused".
    pub impact: String,
    /// The severity it had in the previous digest, for a finding that got worse.
    pub was: Option<Severity>,
    /// In `top` only: the titles of the other active findings about the same query, table or index.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub also: Vec<String>,
    /// In `top` only: the finding in full, as the app shows it, with literal values redacted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<DigestDetail>,
    /// For a finding about events counted since startup (lock escalations): the count at this scan, which the next
    /// digest compares with.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub count: Option<f64>,
    /// In `since.again` only: how many happened since the previous digest's scan.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub count_since: Option<f64>,
}

/// A finding in full, in the app's order: what's happening, the numbers, the queries and where their plans spend,
/// the fix, how to verify it and what to check first. Literal values in SQL, predicates and code spans are
/// placeholders, so it can be posted where the database's data must not go.
#[derive(Serialize, Deserialize, TS, Clone, Debug, PartialEq)]
#[ts(export)]
pub struct DigestDetail {
    /// "high", "medium" or "low".
    pub confidence: String,
    pub summary: String,
    pub metrics: Vec<DigestMetric>,
    pub queries: Vec<DigestQuery>,
    pub fix: Option<Fix>,
    pub verify: Vec<String>,
    pub caveats: Vec<String>,
}

#[derive(Serialize, Deserialize, TS, Clone, Debug, PartialEq)]
#[ts(export)]
pub struct DigestMetric {
    pub label: String,
    /// Formatted: "18%", "87K", "0.12 ms".
    pub value: String,
}

#[derive(Serialize, Deserialize, TS, Clone, Debug, PartialEq)]
#[ts(export)]
pub struct DigestQuery {
    /// "10% of database time · 87K runs a day · 0.12 ms on average".
    pub stats: String,
    /// The statement, literal values replaced with placeholders.
    pub sql: String,
    /// The plan step to look at: "Clustered Index Scan (workflow_executions): 92% of the plan's cost, 280K rows, on
    /// `…`".
    pub plan: Option<String>,
    /// What the finding says about this query.
    pub note: Option<String>,
}
