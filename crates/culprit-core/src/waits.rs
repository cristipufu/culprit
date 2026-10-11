//! Where database time goes, explained: waits by category with the queries behind each, a "right now" sample, and the
//! pressure signals every engine reports in its own way. SQL Server, PostgreSQL and MySQL fill the same shapes; what an
//! engine can't measure is left out and said so, never guessed.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::model::{Engine, WaitStat};

/// How far back a set of numbers reaches. The UI states it next to every figure.
#[derive(Serialize, Deserialize, TS, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
#[ts(export)]
pub enum WaitScope {
    /// The scan's window (Query Store intervals).
    Window,
    /// Running totals since the server started (or its statistics were reset).
    SinceStartup,
    /// A short sample taken just now.
    Sampled,
    /// Statement timing totals (PostgreSQL's per-statement I/O timing), since statistics were reset.
    Statements,
}

/// One query's part in a wait category.
#[derive(Serialize, Deserialize, TS, Clone, Debug, PartialEq)]
#[ts(export)]
pub struct WaitQuery {
    pub id: String,
    /// The statement on one line, shortened.
    pub text: String,
    pub wait_ms: f64,
    /// Its share of the category.
    pub share_of_category: f64,
    /// How much of the query's own time this category takes.
    pub share_of_query: f64,
    pub executions: f64,
    pub rows_per_run: Option<f64>,
}

/// One category of waiting, explained in plain words.
#[derive(Serialize, Deserialize, TS, Clone, Debug, PartialEq)]
#[ts(export)]
pub struct WaitCategory {
    /// Stable key: cpu, io, rows, lock, latch, log, network, parallelism, memory, compilation, other.
    pub key: String,
    /// What a person calls it, e.g. "Sending results".
    pub label: String,
    pub wait_ms: f64,
    pub share: f64,
    /// The engine's own name for the biggest part, e.g. ASYNC_NETWORK_IO, ClientWrite.
    pub top_wait: Option<String>,
    /// What this kind of waiting means, in one or two sentences.
    pub meaning: String,
    /// What the numbers say about this database, e.g. "3 of these queries return 50K+ rows per call".
    pub detail: Option<String>,
    /// The queries behind it, biggest first. Empty when the engine doesn't attribute this category to statements.
    pub queries: Vec<WaitQuery>,
    /// Why `queries` is empty, when it is.
    pub attribution: Option<String>,
}

/// Where database time went over the scan's window (or since startup), by category.
#[derive(Serialize, Deserialize, TS, Clone, Debug, PartialEq)]
#[ts(export)]
pub struct WaitsReport {
    pub engine: Engine,
    pub scope: WaitScope,
    /// Where the numbers come from and how far back they reach, e.g. "Query Store, last 7 days".
    pub source: String,
    pub total_ms: f64,
    pub categories: Vec<WaitCategory>,
    /// What the engine can't tell, and what would unlock more.
    pub notes: Vec<String>,
}

/// A short "right now" sample: the difference between two readings, or sessions sampled every second.
#[derive(Serialize, Deserialize, TS, Clone, Debug)]
#[ts(export)]
pub struct WaitSample {
    pub engine: Engine,
    pub taken_at: String,
    pub seconds: f64,
    /// How it was measured, e.g. "difference of two readings of sys.dm_os_wait_stats 30 s apart".
    pub method: String,
    pub total_ms: f64,
    /// Category, top wait, time waited during the sample, and (when sampled) how many observations.
    pub categories: Vec<WaitStat>,
    /// Rates over the sample (batches/s, compiles/s, commits/s…).
    pub signals: Vec<Signal>,
    /// Sessions doing work on average during the sample, when the engine samples sessions.
    pub active_sessions: Option<f64>,
    pub notes: Vec<String>,
}

#[derive(Serialize, Deserialize, TS, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
#[ts(export)]
pub enum SignalUnit {
    Percent,
    PerSecond,
    PerHour,
    Count,
    Bytes,
    Milliseconds,
    Seconds,
}

#[derive(Serialize, Deserialize, TS, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
#[ts(export)]
pub enum SignalScope {
    /// Measured when the scan ran.
    Now,
    /// Averaged since the server started or its statistics were reset (see `Pressure::uptime_hours`).
    SinceStartup,
    /// Over a short sample.
    Sampled,
}

#[derive(Serialize, Deserialize, TS, Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "kebab-case")]
#[ts(export)]
pub enum SignalStatus {
    Good,
    Info,
    Watch,
    Bad,
}

/// One health number, with the threshold stated in words so nobody has to know it by heart.
#[derive(Serialize, Deserialize, TS, Clone, Debug, PartialEq)]
#[ts(export)]
pub struct Signal {
    /// Stable key, e.g. compiles-per-batch, cache-hit, checkpoints-requested.
    pub key: String,
    pub label: String,
    pub value: f64,
    pub unit: SignalUnit,
    pub scope: SignalScope,
    /// What healthy looks like, e.g. "under 10% is healthy".
    pub healthy: Option<String>,
    pub status: SignalStatus,
}

impl Signal {
    pub fn new(key: &str, label: &str, value: f64, unit: SignalUnit, scope: SignalScope) -> Signal {
        Signal { key: key.into(), label: label.into(), value, unit, scope, healthy: None, status: SignalStatus::Info }
    }

    /// State the healthy range and grade the value: `watch`/`bad` are the thresholds, `higher_is_worse` their direction.
    pub fn graded(mut self, healthy: &str, watch: f64, bad: f64, higher_is_worse: bool) -> Signal {
        self.healthy = Some(healthy.into());
        let worse = |limit: f64| if higher_is_worse { self.value >= limit } else { self.value <= limit };
        self.status = if worse(bad) {
            SignalStatus::Bad
        } else if worse(watch) {
            SignalStatus::Watch
        } else {
            SignalStatus::Good
        };
        self
    }
}

/// Memory a query reserves to sort and hash, against what it uses (SQL Server plan cache).
#[derive(Serialize, Deserialize, TS, Clone, Debug, PartialEq)]
#[ts(export)]
pub struct GrantStat {
    pub query_hash: String,
    pub executions: f64,
    pub avg_granted_kb: f64,
    pub avg_used_kb: f64,
    pub max_granted_kb: f64,
}

/// The oldest open transaction that keeps old row versions alive (version store, VACUUM horizon, InnoDB undo).
#[derive(Serialize, Deserialize, TS, Clone, Debug, PartialEq)]
#[ts(export)]
pub struct OpenTransaction {
    pub session: String,
    pub seconds: f64,
    pub program: Option<String>,
    /// What it's doing: "idle in transaction", "running", "snapshot".
    pub state: Option<String>,
    /// Bytes of old versions held (SQL Server version store).
    pub held_bytes: Option<f64>,
    /// Old row versions held (InnoDB history list length).
    pub held_versions: Option<f64>,
}

/// Pressure signals from cheap counters, collected with every scan.
#[derive(Serialize, Deserialize, TS, Clone, Debug, PartialEq, Default)]
#[ts(export)]
pub struct Pressure {
    /// How long the "since startup" signals cover.
    pub uptime_hours: Option<f64>,
    pub signals: Vec<Signal>,
    pub grants: Vec<GrantStat>,
    pub oldest_transaction: Option<OpenTransaction>,
    /// Read and write latency per database file (or kind of I/O), since startup.
    #[serde(default)]
    pub files: Vec<crate::signals::FileIo>,
    /// Replicas and replication slots, and how far behind they are.
    #[serde(default)]
    pub replicas: Vec<crate::signals::Replica>,
}

impl Pressure {
    pub fn signal(&self, key: &str) -> Option<&Signal> {
        self.signals.iter().find(|s| s.key == key)
    }
}

/// The stable key, label and plain meaning of a category, worded for this engine.
pub fn category_info(key: &str, engine: Engine) -> (&'static str, String) {
    let name = engine.label();
    match key {
        "cpu" => (
            "CPU",
            "Running on a processor, or queued for one. When CPU dominates, queries usually do more work than they need to: scans, conversions, functions evaluated on every row.".into(),
        ),
        "io" => (
            "Disk reads",
            "Waiting for data to come from disk because it wasn't in memory. Usually scans of large tables, or memory too small for the data the workload keeps touching.".into(),
        ),
        "rows" => (
            "Row access",
            "Time inside InnoDB fetching and changing rows. MySQL's table I/O instrument counts rows served from memory too, so this is storage-engine work, not only disk.".into(),
        ),
        "lock" => (
            "Locks",
            "Waiting for another transaction to release a row or table lock. Long transactions, and missing indexes on the columns updates filter by, make it worse.".into(),
        ),
        "latch" => (
            "In-memory contention",
            "Short waits on the server's own memory structures (page latches, mutexes). Hot spots such as an ever-increasing key or busy temporary space cause most of it.".into(),
        ),
        "log" => (
            "Log writes",
            match engine {
                Engine::Postgres => "Commits waiting for the write-ahead log (WAL) to reach disk. Many tiny transactions, or slow log storage.".into(),
                Engine::Mysql => "Commits waiting for InnoDB's redo log to reach disk. Many tiny transactions, or slow log storage.".into(),
                Engine::Mssql => "Commits waiting for the transaction log to reach disk (WRITELOG), or a tier's log-rate limit. Many tiny transactions, or slow log storage.".into(),
            },
        ),
        "network" => (
            "Sending results",
            format!("{name} has results ready but the application reads them slowly. Usually large result sets, or an application that processes each row before asking for the next."),
        ),
        "parallelism" => (
            "Parallel queries",
            "Threads of a parallel query waiting for each other. Often a big scan that an index would avoid, or work split unevenly across threads.".into(),
        ),
        "memory" => (
            "Memory grants",
            "Queries waiting for memory to sort or hash. A few queries that reserve far more than they use can make everyone else queue.".into(),
        ),
        "compilation" => (
            "Compiling plans",
            "Building query plans. A lot of it means the application sends one-off statements instead of parameterized ones.".into(),
        ),
        _ => (
            "Everything else",
            match engine {
                Engine::Postgres => "Statement time PostgreSQL doesn't break down: running on CPU, or waiting on locks and events it doesn't time per statement.".into(),
                _ => format!("Everything {name} doesn't break down further."),
            },
        ),
    }
}

/// Fold an engine category name into the shared keys.
pub fn category_key(category: &str) -> &'static str {
    match category.to_ascii_lowercase().as_str() {
        "cpu" | "worker thread" | "running" => "cpu",
        "io" | "buffer io" | "other disk io" | "disk" => "io",
        "rows" | "table io" => "rows",
        "lock" => "lock",
        "latch" | "buffer latch" | "lwlock" | "bufferpin" => "latch",
        "log" | "tran log io" | "log rate governor" | "wal" => "log",
        "network" | "network io" | "client" => "network",
        "parallelism" => "parallelism",
        "memory" => "memory",
        "compilation" => "compilation",
        _ => "other",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signals_grade_both_directions() {
        let high = Signal::new("compiles-per-batch", "Compiles", 31.0, SignalUnit::Percent, SignalScope::SinceStartup)
            .graded("under 10% is healthy", 10.0, 25.0, true);
        assert_eq!(high.status, SignalStatus::Bad);
        let hit = Signal::new("cache-hit", "Cache hits", 99.5, SignalUnit::Percent, SignalScope::SinceStartup).graded(
            "above 99% is healthy",
            99.0,
            95.0,
            false,
        );
        assert_eq!(hit.status, SignalStatus::Good);
        let low = Signal { value: 97.0, ..hit.clone() }.graded("above 99% is healthy", 99.0, 95.0, false);
        assert_eq!(low.status, SignalStatus::Watch);
    }

    #[test]
    fn engine_categories_share_keys() {
        assert_eq!(category_key("Network"), "network");
        assert_eq!(category_key("Tran Log IO"), "log");
        assert_eq!(category_key("LWLock"), "latch");
        assert_eq!(category_key("Table IO"), "rows");
        assert_eq!(category_key("Idle"), "other");
        assert_eq!(category_info("network", Engine::Postgres).0, "Sending results");
    }
}
