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
    /// When the server last started (UTC, ISO 8601), as far as the scan could tell: the next digest tells from it
    /// whether counts since startup (lock escalations) started again in between.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_started_at: Option<String>,
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
    /// The queries that used the most database time over the window, most first (`--top-queries`, 5 by default).
    #[serde(default)]
    pub top_queries: Vec<DigestTopQuery>,
    /// What the index advisor suggests creating and dropping, Azure SQL's own recommendations included.
    #[serde(default)]
    pub index_advice: Option<DigestIndexAdvice>,
    /// The deadlocks the scan read, grouped by pattern; `count` is `None` when they couldn't be read (see `note`).
    #[serde(default)]
    pub deadlocks: Option<DigestDeadlocks>,
}

/// One of the queries that used the most database time. Its statement is redacted: literal values are placeholders.
#[derive(Serialize, Deserialize, TS, Clone, Debug, PartialEq)]
#[ts(export)]
pub struct DigestTopQuery {
    /// The query's id in the report (`culprit show <connection> <id>`).
    pub id: String,
    /// What it does in a few words: "a read from orders", "a statement in dbo.usp_refresh".
    pub label: String,
    /// The statement on one line, cut short.
    pub statement: String,
    /// The tables it names.
    pub tables: Vec<String>,
    /// The stored procedure or function it ran in.
    pub object: Option<String>,
    /// Share of database time, 0–1, as every surface states it: over Culprit's scans where they measured it (a source
    /// of running totals), otherwise over the window ([`crate::Workload::statement_share`]).
    pub db_time_share: Option<f64>,
    /// Its time and runs in the source's totals, over the window.
    pub total_ms: f64,
    pub runs: f64,
    /// Its average run, the same way as its share ([`crate::Workload::statement_mean_ms`]).
    pub mean_ms: f64,
    /// It stopped running before the window ended: a deploy may have changed it.
    pub stopped: bool,
    /// The ids of the findings about it.
    pub findings: Vec<String>,
}

#[derive(Serialize, Deserialize, TS, Clone, Debug, Default, PartialEq)]
#[ts(export)]
pub struct DigestIndexAdvice {
    /// Indexes to create, most valuable first, capped like the findings.
    pub create: Vec<DigestIndexCreate>,
    /// How many there are in all.
    pub create_total: u32,
    /// Indexes to drop or fold into another, capped the same way.
    pub drop: Vec<DigestIndexDrop>,
    pub drop_total: u32,
    /// What the usage counters behind the advice cover: "in the 6 days since the server last restarted".
    pub counters: String,
    /// What this engine or login can't tell the advisor.
    pub notes: Vec<String>,
}

#[derive(Serialize, Deserialize, TS, Clone, Debug, PartialEq)]
#[ts(export)]
pub struct DigestIndexCreate {
    /// `schema.table`.
    pub table: String,
    pub keys: Vec<String>,
    pub include: Vec<String>,
    /// Who asks for it: "optimizer", "plan-scan", "foreign-key" or "azure" (Azure SQL's automatic tuning).
    pub source: String,
    pub reason: String,
    /// Share of database time behind the queries that would use it, 0–1.
    pub db_time_share: Option<f64>,
    /// The CREATE statement, to copy: Culprit never runs it.
    pub sql: Option<String>,
    /// The finding about it, when there is one.
    pub finding: Option<String>,
}

#[derive(Serialize, Deserialize, TS, Clone, Debug, PartialEq)]
#[ts(export)]
pub struct DigestIndexDrop {
    /// "unused", "duplicate", "prefix", "overlap", or "azure" when Azure SQL's automatic tuning recommends it.
    pub kind: String,
    /// `schema.table`.
    pub table: String,
    pub index: String,
    pub keys: Vec<String>,
    /// The index that makes this one unnecessary (duplicate, prefix, overlap).
    pub other: Option<String>,
    pub size_bytes: Option<f64>,
    pub reason: String,
    /// The statement that drops or disables it, to copy.
    pub sql: Option<String>,
    pub finding: Option<String>,
}

#[derive(Serialize, Deserialize, TS, Clone, Debug, Default, PartialEq)]
#[ts(export)]
pub struct DigestDeadlocks {
    /// Deadlocks the scan read; `None` when it couldn't tell (no graphs to read, see `note`).
    pub count: Option<u32>,
    /// Deadlocks the server counted without keeping their graphs (Postgres), since `counted_since`.
    pub counted: Option<f64>,
    pub counted_since: Option<String>,
    /// The hours the scan read back.
    pub since_hours: Option<f64>,
    /// Where they were read from, in words.
    pub source: Option<String>,
    /// The most recent one (UTC, ISO 8601).
    pub latest_at: Option<String>,
    /// How many happened after the previous digest's scan; `None` without one to compare with.
    pub new_since: Option<u32>,
    /// One per pattern on the same tables, the most frequent first, capped like the findings.
    pub patterns: Vec<DigestDeadlockPattern>,
    /// Why there are no graphs, or what unlocks more.
    pub note: Option<String>,
}

#[derive(Serialize, Deserialize, TS, Clone, Debug, PartialEq)]
#[ts(export)]
pub struct DigestDeadlockPattern {
    /// "key-lookup", "lock-conversion", "opposite-order", "lock-escalation", "parallelism" or "cycle".
    pub pattern: String,
    /// "key-lookup deadlock", "opposite access order".
    pub label: String,
    pub count: u32,
    pub latest_at: Option<String>,
    /// The tables involved.
    pub tables: Vec<String>,
    /// The statements in the most recent one, redacted, on one line each.
    pub statements: Vec<String>,
    /// The lock modes the processes waited for.
    pub lock_modes: Vec<String>,
    /// The explainer's one-line story of the most recent one.
    pub headline: String,
    /// What makes it go away, in a few words.
    pub fix: Option<String>,
}

#[derive(Serialize, Deserialize, TS, Clone, Debug, Default, PartialEq)]
#[ts(export)]
pub struct DigestCounts {
    /// Findings about the database as it runs now.
    pub active: u32,
    pub high: u32,
    pub medium: u32,
    pub low: u32,
    /// Findings about queries that stopped running: a deploy may have changed them.
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
    /// Active both times, more severe now. Findings that count events since startup (lock escalations) are in `again`
    /// instead: they turn High when the count grew.
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
    #[serde(default)]
    pub actionability: crate::Actionability,
    #[serde(default)]
    pub verification_basis: crate::VerificationBasis,
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
