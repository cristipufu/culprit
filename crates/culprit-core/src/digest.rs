//! A scan in a few lines, for a scheduled job to post somewhere: what the database looks like now, what changed since
//! the previous digest, and the findings worth a look. Data only; the consumer decides how to show it.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::finding::Severity;

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
    /// Database time and its trend, as the dashboard says it.
    pub database_time: Option<String>,
    pub counts: DigestCounts,
    /// What changed since the previous digest; `None` when there was nothing to compare with.
    pub since: Option<DigestChanges>,
    /// The findings worth a look, most important first.
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
}

#[derive(Serialize, Deserialize, TS, Clone, Debug, PartialEq)]
#[ts(export)]
pub struct DigestFinding {
    /// The finding's stable id: `culprit show <connection> <id>` opens it.
    pub id: String,
    pub title: String,
    pub severity: Severity,
    /// Share of database time behind it, 0–1, when it is about queries.
    pub db_time_share: Option<f64>,
    /// How big it is in a few words: "11% of DB time", "2.0 MB unused".
    pub impact: String,
    /// The severity it had in the previous digest, for a finding that got worse.
    pub was: Option<Severity>,
}
