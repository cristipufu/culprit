//! Signals beyond time and reads. Per statement: failed and cancelled runs, planning and JIT time, log volume and the
//! size of what it returns. Per server: storage latency per file and replica lag. Per object: lock and latch waits on
//! an index, constraints the optimizer can't trust, column histograms and how updates land on a table's pages.
//!
//! Every engine keeps a different subset. A value the engine doesn't keep is `None` (or absent), never zero.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::{Digest, Plan, QueryStat, Snapshot};

/// Exact compile totals for one retained Query Store entry. Query Store does not timestamp every compilation, so
/// totals can be compared with windowed runs only when the entry's first compile falls inside that window.
#[derive(Serialize, Deserialize, TS, Clone, Debug, Default, PartialEq)]
#[ts(export)]
pub struct CompileHistory {
    pub count: f64,
    pub total_ms: f64,
    pub first_at: Option<String>,
    pub last_at: Option<String>,
    pub within_window: bool,
}

/// What a statement's runs cost besides elapsed time, as far as the engine keeps it. Totals cover all its runs in the
/// workload's window (or since its statistics began, for sources that keep running totals).
#[derive(Serialize, Deserialize, TS, Clone, Debug, Default, PartialEq)]
#[ts(export)]
pub struct QueryCosts {
    /// Runs that ended in an error (SQL Server Query Store "exception"; MySQL/MariaDB statements that raised errors).
    pub failed: Option<f64>,
    /// Runs the client cancelled, almost always a command timeout (SQL Server Query Store "aborted").
    pub aborted: Option<f64>,
    /// Average duration of the cancelled runs: close to 30 s is a client's default timeout.
    pub aborted_mean_ms: Option<f64>,
    /// Warnings raised (MySQL/MariaDB): truncations, conversions, deprecated syntax.
    pub warnings: Option<f64>,
    /// The error its recent failed runs raised most, as a code and a fixed name ("1205 lock wait timeout"); never the
    /// server's message, which can carry row values.
    pub top_error: Option<String>,
    /// Time spent planning (PostgreSQL with pg_stat_statements.track_planning) or compiling (SQL Server), all runs.
    pub plan_ms: Option<f64>,
    /// How many times it was planned or compiled.
    pub plans: Option<f64>,
    /// Time spent JIT-compiling expressions (PostgreSQL 15+), all runs. Part of execution time.
    pub jit_ms: Option<f64>,
    /// Functions JIT-compiled, all runs.
    pub jit_functions: Option<f64>,
    /// Write-ahead (PostgreSQL 13+) or transaction log (SQL Server 2017+) bytes it wrote, all runs.
    pub log_bytes: Option<f64>,
    /// Its part of all log bytes written by statements in this database.
    pub log_share: Option<f64>,
    /// Full-page images among its WAL records (PostgreSQL): high after every checkpoint.
    pub log_full_pages: Option<f64>,
    /// Estimated bytes per returned row, from the plan's output width (SQL Server AvgRowSize, PostgreSQL width).
    pub row_bytes: Option<f64>,
    #[serde(default)]
    pub compile_history: Option<CompileHistory>,
}

impl QueryCosts {
    pub fn is_empty(&self) -> bool {
        *self == QueryCosts::default()
    }

    /// Runs that didn't finish: errors plus cancellations, when the engine counts either.
    pub fn unfinished(&self) -> Option<f64> {
        match (self.failed, self.aborted) {
            (None, None) => None,
            (a, b) => Some(a.unwrap_or(0.0) + b.unwrap_or(0.0)),
        }
    }
}

/// Estimated bytes per row a plan returns: SQL Server's AvgRowSize or PostgreSQL's width on the top operator.
pub fn plan_row_bytes(plan: &Plan) -> Option<f64> {
    plan.root.facts().estimated_row_bytes.filter(|w| w.is_finite() && *w > 0.0)
}

/// Fill `row_bytes` from each statement's plan, creating its costs when needed.
pub fn fill_row_bytes(queries: &mut [QueryStat]) {
    for q in queries {
        if let Some(width) = q.plan.as_ref().and_then(plan_row_bytes) {
            q.costs.get_or_insert_with(QueryCosts::default).row_bytes = Some(width);
        }
    }
}

#[derive(Serialize, Deserialize, TS, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
#[ts(export)]
pub enum FileKind {
    /// Table and index data.
    Data,
    /// Transaction log, WAL, redo.
    Log,
    /// tempdb, temporary files and tablespaces.
    Temp,
    Other,
}

#[derive(Serialize, Deserialize, TS, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
#[ts(export)]
pub enum FileScope {
    Database,
    Server,
}

#[derive(Serialize, Deserialize, TS, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
#[ts(export)]
pub enum FileWriteOperation {
    Write,
    Flush,
}

/// Reads and writes against one database file (or, on PostgreSQL, one kind of I/O), since the server started or
/// its statistics were reset. Latency is time per operation as the engine measured it.
#[derive(Serialize, Deserialize, TS, Clone, Debug, PartialEq)]
#[ts(export)]
pub struct FileIo {
    /// The file's logical name or path; on PostgreSQL the kind of object read ("tables and indexes").
    pub name: String,
    pub kind: FileKind,
    /// The database the file belongs to, when the engine says (tempdb, the scanned database).
    pub database: Option<String>,
    pub reads: f64,
    /// Total time reads took; `None` when the engine doesn't time them (PostgreSQL without track_io_timing).
    pub read_ms: Option<f64>,
    pub read_bytes: Option<f64>,
    pub writes: f64,
    pub write_ms: Option<f64>,
    pub write_bytes: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub scope: Option<FileScope>,
    /// This source's own reset timestamp; another source's reset is not its measurement period.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub stats_reset: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub write_operation: Option<FileWriteOperation>,
}

/// Log files of the same database as one: MySQL 8.0.30+ spreads redo over many `#ib_redo*` files, each too quiet on
/// its own to average, and what commits wait on is the log as a whole. Other files pass through unchanged.
pub fn merge_logs(files: &[FileIo]) -> Vec<FileIo> {
    let mut out: Vec<FileIo> = Vec::new();
    for f in files {
        if f.kind != FileKind::Log {
            out.push(f.clone());
            continue;
        }
        match out.iter_mut().find(|o| {
            o.kind == FileKind::Log
                && o.database == f.database
                && o.scope == f.scope
                && o.stats_reset == f.stats_reset
                && o.write_operation == f.write_operation
        }) {
            Some(log) => {
                // Unknown timing or bytes for part of the log cannot become a zero and dilute its average.
                let sum = |a: Option<f64>, b: Option<f64>| a.zip(b).map(|(a, b)| a + b);
                log.reads += f.reads;
                log.writes += f.writes;
                log.read_ms = sum(log.read_ms, f.read_ms);
                log.write_ms = sum(log.write_ms, f.write_ms);
                log.read_bytes = sum(log.read_bytes, f.read_bytes);
                log.write_bytes = sum(log.write_bytes, f.write_bytes);
                if log.name.contains("ib_redo") || log.name.contains("ib_logfile") {
                    log.name = "redo log".into();
                }
            }
            None => out.push(f.clone()),
        }
    }
    out
}

impl FileIo {
    pub fn write_label(&self) -> &'static str {
        if self.write_operation == Some(FileWriteOperation::Flush) { "flush" } else { "write" }
    }

    pub fn coverage(&self, snapshot: &Snapshot) -> String {
        let scope = match self.scope {
            Some(FileScope::Server) => "whole server",
            Some(FileScope::Database) => {
                self.database.as_deref().filter(|db| *db != snapshot.server.database).unwrap_or("this database")
            }
            None => "recorded file statistics",
        };
        let period = self.stats_reset.as_deref().map(|at| format!("since {at}")).unwrap_or_else(|| {
            if snapshot.server.engine == crate::Engine::Postgres {
                "over an unknown statistics period".into()
            } else {
                "over the recorded history".into()
            }
        });
        format!("{scope}, {period}")
    }

    pub fn avg_read_ms(&self) -> Option<f64> {
        self.read_ms.filter(|_| self.reads > 0.0).map(|ms| ms / self.reads)
    }

    pub fn avg_write_ms(&self) -> Option<f64> {
        self.write_ms.filter(|_| self.writes > 0.0).map(|ms| ms / self.writes)
    }
}

#[derive(Serialize, Deserialize, TS, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
#[ts(export)]
pub enum ReplicaRole {
    /// A copy this server sends changes to (PostgreSQL pg_stat_replication, SQL Server availability replicas).
    Replica,
    /// This server is the copy; the lag is its own (PostgreSQL standby, MySQL/MariaDB replica).
    ThisServer,
    /// A replication slot that keeps WAL for a consumer (PostgreSQL).
    Slot,
}

/// How far a copy of the data is behind, as the engine reports it.
#[derive(Serialize, Deserialize, TS, Clone, Debug, PartialEq)]
#[ts(export)]
pub struct Replica {
    /// Replica, application or slot name (database text).
    pub name: String,
    pub role: ReplicaRole,
    /// What the engine says it's doing: streaming, catchup, SYNCHRONIZED, "SQL thread stopped", inactive.
    pub state: Option<String>,
    /// Synchronous or asynchronous, when known.
    pub sync: Option<String>,
    /// How far behind it is beyond any apply delay it is configured with (`delay_seconds`). On a PostgreSQL standby,
    /// counted only while it has received changes it hasn't replayed: with nothing to replay, the time since the last
    /// replayed commit is how long the primary has been quiet, not lag.
    pub lag_seconds: Option<f64>,
    /// Bytes not yet applied (PostgreSQL replay lag, SQL Server redo queue) or kept for a slot.
    pub lag_bytes: Option<f64>,
    /// Bytes the primary hasn't sent yet (SQL Server log send queue).
    pub unsent_bytes: Option<f64>,
    /// The last replication error, as the engine reports it (database text).
    pub error: Option<String>,
    /// A delay it is configured to apply changes with, on purpose (MySQL SQL_Delay, PostgreSQL
    /// recovery_min_apply_delay); already taken out of `lag_seconds`.
    #[serde(default)]
    pub delay_seconds: Option<f64>,
    /// A logical consumer (a subscription, a CDC tool such as Debezium or Fivetran) rather than a copy of the server.
    #[serde(default)]
    pub logical: Option<bool>,
    /// How long a replication slot has been without a consumer (PostgreSQL 17+ `inactive_since`).
    #[serde(default)]
    pub inactive_seconds: Option<f64>,
    /// Whether a slot's WAL is still kept for it: reserved, extended, unreserved (about to be given up) or lost
    /// (PostgreSQL 13+).
    #[serde(default)]
    pub wal_status: Option<String>,
    /// WAL that can still be written before the slot is given up under max_slot_wal_keep_size; `None` when nothing
    /// caps what slots keep (PostgreSQL 13+ `safe_wal_size`).
    #[serde(default)]
    pub safe_wal_bytes: Option<f64>,
    /// The database a logical walsender or slot decodes (PostgreSQL); `None` for a physical one, which carries every
    /// database. Another database's consumer is that database's lag, though its slot keeps WAL for the whole server.
    #[serde(default)]
    pub database: Option<String>,
}

/// WAL a PostgreSQL standby has received and not replayed that makes the time since its last replayed commit its lag:
/// a WAL segment. Below it replay is current, and that time is how long the primary went without a commit (one long
/// CREATE INDEX, COPY or VACUUM), not how far behind the standby is.
pub const STANDBY_BEHIND_BYTES: f64 = 16.0 * 1024.0 * 1024.0;

impl Replica {
    /// Older standby scans reported time since the last replayed commit as lag even when replay was current.
    pub fn effective_lag_seconds(&self, engine: crate::Engine) -> Option<f64> {
        if engine == crate::Engine::Postgres
            && self.role == ReplicaRole::ThisServer
            && self.lag_bytes.is_some_and(|b| b < STANDBY_BEHIND_BYTES)
        {
            return Some(0.0);
        }
        self.lag_seconds
    }

    /// A replica, slot or this server by name, with nothing measured yet.
    pub fn new(name: impl Into<String>, role: ReplicaRole) -> Replica {
        Replica {
            name: name.into(),
            role,
            state: None,
            sync: None,
            lag_seconds: None,
            lag_bytes: None,
            unsent_bytes: None,
            error: None,
            delay_seconds: None,
            logical: None,
            inactive_seconds: None,
            wal_status: None,
            safe_wal_bytes: None,
            database: None,
        }
    }

    /// A logical consumer: a walsender or slot that decodes changes for a subscription or a CDC tool. Slots said so
    /// in `sync` (their type) before `logical` existed.
    pub fn is_logical(&self) -> bool {
        self.logical.unwrap_or(self.role == ReplicaRole::Slot && self.sync.as_deref() == Some("logical"))
    }
}

/// Lock and latch waits on one index since its metadata was last loaded (SQL Server sys.dm_db_index_operational_stats).
#[derive(Serialize, Deserialize, TS, Clone, Debug, Default, PartialEq)]
#[ts(export)]
pub struct IndexContention {
    pub row_lock_waits: f64,
    pub row_lock_wait_ms: f64,
    pub page_lock_waits: f64,
    pub page_lock_wait_ms: f64,
    /// Waits for an in-memory page another session is changing (PAGELATCH): last-page inserts, hot pages.
    pub page_latch_waits: f64,
    pub page_latch_wait_ms: f64,
    /// Row or page locks escalated to a lock on the whole table.
    pub lock_escalations: f64,
    /// Escalations SQL Server tried, the ones that happened included: an attempt fails when another session holds a
    /// conflicting lock on the table, and the statement keeps its row locks.
    #[serde(default)]
    pub lock_escalation_attempts: f64,
    pub leaf_inserts: Option<f64>,
    /// The leading key column only grows (identity or sequence), so every insert lands on the last page.
    pub sequential_key: bool,
    /// SQL Server 2019+ OPTIMIZE_FOR_SEQUENTIAL_KEY; `None` on older versions.
    pub optimize_for_sequential_key: Option<bool>,
}

impl IndexContention {
    pub fn lock_wait_ms(&self) -> f64 {
        self.row_lock_wait_ms + self.page_lock_wait_ms
    }

    pub fn lock_waits(&self) -> f64 {
        self.row_lock_waits + self.page_lock_waits
    }
}

/// Timed row and page lock waits kept by one index.
#[derive(Serialize, Deserialize, TS, Clone, Debug, Default, PartialEq)]
#[ts(export)]
pub struct IndexLockCounts {
    pub waits: f64,
    pub wait_ms: f64,
}

/// Per-index lock counters from an earlier scan. Missing indexes, a restart or counters that shrink do not establish
/// zero activity. A provider without per-index lock counters supplies no baseline.
#[derive(Serialize, Deserialize, TS, Clone, Debug, Default, PartialEq)]
#[ts(export)]
pub struct ContentionBaseline {
    pub taken_at: String,
    pub indexes: BTreeMap<String, IndexLockCounts>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub reference: Option<crate::ScanReference>,
}

impl ContentionBaseline {
    fn key(index: &crate::IndexStat) -> String {
        serde_json::to_string(&(&index.schema, &index.table, &index.name)).expect("three index name strings")
    }

    pub fn from_scan(previous: &Snapshot, current: &Snapshot) -> Option<Self> {
        if crate::format::epoch_seconds(&previous.taken_at)? > crate::format::epoch_seconds(&current.taken_at)?
            || !previous.same_database(current)
            || restarted(previous.server.started_at.as_deref()?, current.server.started_at.as_deref()?)?
        {
            return None;
        }
        let indexes: BTreeMap<_, _> = previous
            .indexes
            .iter()
            .filter_map(|i| {
                let c = i.contention.as_ref()?;
                Some((Self::key(i), IndexLockCounts { waits: c.lock_waits(), wait_ms: c.lock_wait_ms() }))
            })
            .collect();
        (!indexes.is_empty()).then(|| Self {
            taken_at: previous.taken_at.clone(),
            indexes,
            reference: Some(crate::ScanReference::of(previous)),
        })
    }

    pub fn since(&self, index: &crate::IndexStat) -> Option<IndexLockCounts> {
        let then = self.indexes.get(&Self::key(index))?;
        let now = index.contention.as_ref()?;
        (now.lock_waits() >= then.waits && now.lock_wait_ms() >= then.wait_ms).then(|| IndexLockCounts {
            waits: now.lock_waits() - then.waits,
            wait_ms: now.lock_wait_ms() - then.wait_ms,
        })
    }
}

/// The lock escalations an earlier scan counted, separately from timed lock waits.
#[derive(Serialize, Deserialize, TS, Clone, Debug, Default, PartialEq)]
#[ts(export)]
pub struct EscalationBaseline {
    /// When the earlier scan was taken (UTC, ISO 8601).
    pub taken_at: String,
    /// Escalations it counted per table (`schema.table`); a table missing here had none. Empty when the server
    /// restarted after it: every escalation counted now came later.
    pub tables: BTreeMap<String, f64>,
    /// The server restarted between the earlier scan and this one, which started the counts again from zero.
    pub restarted: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub reference: Option<crate::ScanReference>,
}

/// How big each table's data was at an earlier scan, to tell how fast it grows.
#[derive(Serialize, Deserialize, TS, Clone, Debug, Default, PartialEq)]
#[ts(export)]
pub struct SizeBaseline {
    /// When the earlier scan was taken (UTC, ISO 8601).
    pub taken_at: String,
    /// Bytes of each table's own rows (`schema.table`) then: its heap or clustered index, large values included.
    pub tables: BTreeMap<String, f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub reference: Option<crate::ScanReference>,
}

/// A table's own rows in bytes, large values included, measured the one way growth compares: the in-row part plus the
/// large values kept apart, where the scan split them, else its data size. (PostgreSQL's size from the catalog when the
/// exact sizes time out once counted the main fork alone, so a data size against a split one was all TOAST "growth".)
pub fn rows_bytes(t: &crate::TableStat) -> Option<f64> {
    match (t.in_row_bytes, t.lob_bytes) {
        (Some(in_row), Some(lob)) => Some(in_row + lob + t.overflow_bytes.unwrap_or(0.0)),
        _ => t.data_bytes,
    }
}

impl SizeBaseline {
    /// The sizes `previous`, an earlier scan of the same database, read. `None` when it read none.
    pub fn from_scan(previous: &Snapshot) -> Option<SizeBaseline> {
        let tables: BTreeMap<String, f64> =
            previous.tables.iter().filter_map(|t| Some((t.qualified().to_ascii_lowercase(), rows_bytes(t)?))).collect();
        (!tables.is_empty()).then(|| SizeBaseline {
            taken_at: previous.taken_at.clone(),
            tables,
            reference: Some(crate::ScanReference::of(previous)),
        })
    }

    /// Bytes of `table` (`schema.table`) at the earlier scan.
    pub fn bytes(&self, table: &str) -> Option<f64> {
        self.tables.get(&table.to_ascii_lowercase()).copied()
    }
}

/// A count kept since its source last started over, as an earlier scan read it: what tells what was counted since then
/// from what was counted before.
#[derive(Serialize, Deserialize, TS, Clone, Debug, Default, PartialEq)]
#[ts(export)]
pub struct CountBaseline {
    /// When the earlier scan was taken (UTC, ISO 8601).
    pub taken_at: String,
    /// What it counted then; 0 when the count started over after it, so all of this scan's count came later.
    pub count: f64,
    /// The count started over between the earlier scan and this one.
    pub restarted: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub reference: Option<crate::ScanReference>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub measurement: Option<crate::MeasurementContext>,
}

impl CountBaseline {
    /// The count a provider reports as setting `name` (`pg_stat_statements.dealloc`) that `previous`, an earlier scan
    /// of the same database, read. `None` when either scan didn't read it. It started over when it shrank, or when the
    /// workload's counting started again after `previous` (the statistics that keep it were reset).
    pub fn from_setting(previous: &Snapshot, current: &Snapshot, name: &str) -> Option<CountBaseline> {
        if !previous.same_database(current)
            || crate::format::epoch_seconds(&previous.taken_at)? > crate::format::epoch_seconds(&current.taken_at)?
        {
            return None;
        }
        let read = |s: &Snapshot| s.setting(name).and_then(|s| s.value.trim().parse::<f64>().ok());
        let (then, now) = (read(previous)?, read(current)?);
        let referenced = |mut baseline: Self| {
            baseline.reference = Some(crate::ScanReference::of(previous));
            baseline
        };
        match (previous.captured_measurement(), current.captured_measurement()) {
            (Some(a), Some(b)) => return Self::in_context(&previous.taken_at, then, now, &a, &b).map(referenced),
            (None, None) => {}
            _ => return None,
        }
        let start = |s: &Snapshot| s.workload.as_ref().and_then(|w| w.window_start.clone());
        Some(referenced(CountBaseline::between(
            &previous.taken_at,
            then,
            now,
            start(previous).as_deref(),
            start(current).as_deref(),
        )))
    }

    /// Compare a source counter using its recorded clock. Shrinking totals without a reset remain unknown.
    pub fn in_context(
        taken_at: &str,
        then: f64,
        now: f64,
        before: &crate::MeasurementContext,
        after: &crate::MeasurementContext,
    ) -> Option<Self> {
        if !then.is_finite() || !now.is_finite() || then < 0.0 || now < 0.0 {
            return None;
        }
        let restarted = match before.counter_transition(after) {
            crate::CounterTransition::Reset => true,
            crate::CounterTransition::Continued | crate::CounterTransition::Unmarked if now >= then => false,
            _ => return None,
        };
        Some(Self {
            taken_at: taken_at.into(),
            count: if restarted { 0.0 } else { then },
            restarted,
            reference: None,
            measurement: Some(before.clone()),
        })
    }

    /// The count `then` an earlier scan taken at `taken_at` read, against `now`: it started over when it shrank, or
    /// when the statistics that keep it were reset after that scan (`start_then` and `start_now`, when each scan says
    /// they last started).
    pub fn between(
        taken_at: &str,
        then: f64,
        now: f64,
        start_then: Option<&str>,
        start_now: Option<&str>,
    ) -> CountBaseline {
        let reset = start_then.zip(start_now).and_then(|(a, b)| restarted(a, b)).unwrap_or(false);
        let restarted = reset || now < then;
        CountBaseline {
            taken_at: taken_at.to_string(),
            count: if restarted { 0.0 } else { then },
            restarted,
            reference: None,
            measurement: None,
        }
    }
}

/// Start times read a second or two apart are the same start.
const SAME_START_SECONDS: f64 = 120.0;

impl EscalationBaseline {
    /// What `previous`, an earlier scan of the same database, counted, to compare `current` with. `None` when it can't
    /// tell: either scan doesn't know when the server started, or `previous` has no lock counters.
    pub fn from_scan(previous: &Snapshot, current: &Snapshot) -> Option<EscalationBaseline> {
        if !previous.same_database(current)
            || crate::format::epoch_seconds(&previous.taken_at)? > crate::format::epoch_seconds(&current.taken_at)?
        {
            return None;
        }
        if !previous.indexes.iter().any(|i| i.contention.is_some()) {
            return None;
        }
        let restarted = restarted(previous.server.started_at.as_deref()?, current.server.started_at.as_deref()?)?;
        Some(EscalationBaseline {
            reference: Some(crate::ScanReference::of(previous)),
            taken_at: previous.taken_at.clone(),
            tables: if restarted { BTreeMap::new() } else { escalations_by_table(previous) },
            restarted,
        })
    }

    /// The same from a previous digest, which keeps each lock-escalations finding's count: what a scheduled job keeps
    /// between runs instead of the scans. `None` for a digest that doesn't say when the server started.
    pub fn from_digest(previous: &Digest, current: &Snapshot) -> Option<EscalationBaseline> {
        if !previous.comparison(current).allows_comparison()
            || crate::format::epoch_seconds(&previous.taken_at)? > crate::format::epoch_seconds(&current.taken_at)?
        {
            return None;
        }
        let restarted = restarted(previous.server_started_at.as_deref()?, current.server.started_at.as_deref()?)?;
        let tables = if restarted {
            BTreeMap::new()
        } else {
            previous
                .findings
                .iter()
                .filter_map(|f| Some((f.id.strip_prefix("lock-escalations:")?.to_string(), f.count?)))
                .collect()
        };
        Some(EscalationBaseline {
            taken_at: previous.taken_at.clone(),
            tables,
            restarted,
            reference: Some(crate::ScanReference {
                id: None,
                taken_at: previous.taken_at.clone(),
                database: previous.identity.clone(),
            }),
        })
    }

    /// Escalations on `table` (`schema.table`) since the earlier scan, out of `now` counted since the server started.
    /// `None` for a count lower than then without a restart: the table's indexes were reloaded into memory, or one of
    /// them (or a partition) was rebuilt or dropped and took its own count with it. The rest may all be old, so it
    /// can't say how many are new.
    pub fn since(&self, table: &str, now: f64) -> Option<f64> {
        let then = self.tables.get(table).copied().unwrap_or(0.0);
        (now >= then).then_some(now - then)
    }
}

/// Whether the server started again between two readings of its start time; `None` when either doesn't parse.
fn restarted(before: &str, after: &str) -> Option<bool> {
    let (before, after) = (crate::format::epoch_seconds(before)?, crate::format::epoch_seconds(after)?);
    Some((after - before).abs() > SAME_START_SECONDS)
}

/// Lock escalations per table (`schema.table`) a scan counted, its tables without any left out.
pub fn escalations_by_table(snapshot: &Snapshot) -> BTreeMap<String, f64> {
    let mut out = BTreeMap::new();
    for i in &snapshot.indexes {
        if let Some(c) = i.contention.as_ref().filter(|c| c.lock_escalations > 0.0) {
            *out.entry(format!("{}.{}", i.schema, i.table)).or_insert(0.0) += c.lock_escalations;
        }
    }
    out
}

#[derive(Serialize, Deserialize, TS, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
#[ts(export)]
pub enum ConstraintKind {
    ForeignKey,
    Check,
}

/// A constraint the engine doesn't rely on: never checked against existing rows (SQL Server WITH NOCHECK, PostgreSQL
/// NOT VALID) or switched off.
#[derive(Serialize, Deserialize, TS, Clone, Debug, PartialEq)]
#[ts(export)]
pub struct ConstraintIssue {
    pub schema: String,
    pub table: String,
    pub name: String,
    pub kind: ConstraintKind,
    /// Switched off entirely (SQL Server NOCHECK CONSTRAINT): new rows aren't checked either.
    pub disabled: bool,
    /// The table inherits from another (PostgreSQL legacy inheritance, not a declarative partition): the planner reads
    /// CHECK constraints there to skip it (constraint exclusion), and only validated ones.
    #[serde(default)]
    pub inheritance_child: bool,
}

/// A column histogram the optimizer can use (MySQL 8 COLUMN_STATISTICS, MariaDB mysql.column_stats).
#[derive(Serialize, Deserialize, TS, Clone, Debug, PartialEq)]
#[ts(export)]
pub struct ColumnHistogram {
    pub schema: String,
    pub table: String,
    pub column: String,
    /// singleton, equi-height, JSON_HB, DOUBLE_PREC_HB…
    pub kind: Option<String>,
    pub buckets: Option<f64>,
}

/// How updates land on a table's pages and how much of it is empty space (PostgreSQL).
#[derive(Serialize, Deserialize, TS, Clone, Debug, Default, PartialEq)]
#[ts(export)]
pub struct TableStorage {
    /// Updates that stayed on the same page and wrote no index entries (n_tup_hot_upd).
    pub hot_updates: Option<f64>,
    /// Updates whose new row version had to move to another page (n_tup_newpage_upd, PostgreSQL 16+).
    pub new_page_updates: Option<f64>,
    /// Percent of each page filled by inserts; 100 unless set.
    pub fillfactor: Option<f64>,
    /// Bytes beyond what the live rows need, estimated from column statistics (no pgstattuple needed).
    pub bloat_bytes: Option<f64>,
    /// Hours the update counters have been counting: since the database's statistics were last reset. `None` when
    /// that is unknown: PostgreSQL 15+ leaves `stats_reset` empty until the first reset, and the counters outlive a
    /// clean restart, so they may be far older than the server's start.
    #[serde(default)]
    pub counter_hours: Option<f64>,
    /// The planner's row count (pg_class.reltuples), which a statistics reset, crash or failover doesn't zero the way
    /// it does the live-row counter.
    #[serde(default)]
    pub planner_rows: Option<f64>,
    /// The table's own storage parameters as set (`autovacuum_vacuum_scale_factor=0.01`, `fillfactor=70`): what
    /// overrides the server's autovacuum settings for it. Empty when it has none, or the scan predates them.
    #[serde(default)]
    pub options: Vec<String>,
}

impl TableStorage {
    /// The value of storage parameter `name` set on the table, when it is set and a number.
    pub fn option(&self, name: &str) -> Option<f64> {
        self.options.iter().find_map(|o| {
            let (key, value) = o.split_once('=')?;
            key.trim().eq_ignore_ascii_case(name).then(|| value.trim().parse().ok()).flatten()
        })
    }

    /// Whether the table turns autovacuum off for itself (`autovacuum_enabled=false`).
    pub fn autovacuum_off(&self) -> bool {
        self.options.iter().any(|o| {
            o.split_once('=').is_some_and(|(k, v)| {
                k.trim().eq_ignore_ascii_case("autovacuum_enabled")
                    && matches!(v.trim().to_ascii_lowercase().as_str(), "false" | "off" | "no" | "0")
            })
        })
    }

    /// User-defined triggers recorded on the table (`trigger=<name>:<events>`), as `(name, events)` pairs where
    /// `events` holds `'I'`, `'U'`, and/or `'D'`.
    pub fn triggers(&self) -> Vec<(&str, &str)> {
        self.options
            .iter()
            .filter_map(|o| {
                let rest = o.strip_prefix("trigger=")?;
                Some(rest.split_once(':').unwrap_or((rest, "")))
            })
            .collect()
    }

    /// Partition bound expressions recorded on the table or partition (`partition_bound=<expr>`).
    pub fn partition_bounds(&self) -> Vec<&str> {
        self.options.iter().filter_map(|o| o.strip_prefix("partition_bound=")).collect()
    }

    /// Whether this partition is a declarative `DEFAULT` partition (`partition_bound=DEFAULT`).
    pub fn is_default_partition(&self) -> bool {
        self.partition_bounds().iter().any(|b| b.eq_ignore_ascii_case("default"))
    }
}

#[cfg(test)]
mod merge_tests {
    use super::*;

    #[test]
    fn a_count_that_shrank_or_whose_statistics_were_reset_started_over() {
        let demo: Snapshot = serde_json::from_str(include_str!("../../../fixtures/postgres-demo.json")).unwrap();
        let scan = |taken_at: &str, dealloc: &str, reset: &str| {
            let mut s = demo.clone();
            s.taken_at = taken_at.into();
            s.settings.retain(|x| x.name != "pg_stat_statements.dealloc");
            s.settings.push(crate::Setting {
                name: "pg_stat_statements.dealloc".into(),
                value: dealloc.into(),
                default_value: None,
                scope: "server".into(),
            });
            s.workload.as_mut().unwrap().window_start = Some(reset.into());
            s
        };
        let name = "pg_stat_statements.dealloc";
        let before = scan("2026-10-15T10:00:00Z", "40", "2023-04-17T10:00:21Z");
        let grew =
            CountBaseline::from_setting(&before, &scan("2026-10-17T10:00:00Z", "52", "2023-04-17T10:00:21Z"), name);
        assert_eq!(
            grew,
            Some(CountBaseline {
                taken_at: before.taken_at.clone(),
                count: 40.0,
                restarted: false,
                reference: Some(crate::ScanReference::of(&before)),
                measurement: None,
            })
        );
        let shrank =
            CountBaseline::from_setting(&before, &scan("2026-10-17T10:00:00Z", "3", "2023-04-17T10:00:21Z"), name);
        assert!(shrank.is_some_and(|b| b.restarted && b.count == 0.0));
        let reset =
            CountBaseline::from_setting(&before, &scan("2026-10-17T10:00:00Z", "52", "2026-10-16T08:00:00Z"), name);
        assert!(reset.is_some_and(|b| b.restarted), "reset after the earlier scan, then more rounds than before");
        let mut unread = scan("2026-10-17T10:00:00Z", "52", "2023-04-17T10:00:21Z");
        unread.settings.clear();
        assert_eq!(CountBaseline::from_setting(&before, &unread, name), None);
    }

    fn file(name: &str, kind: FileKind, writes: f64, write_ms: f64) -> FileIo {
        FileIo {
            name: name.into(),
            kind,
            database: None,
            reads: 0.0,
            read_ms: None,
            read_bytes: None,
            writes,
            write_ms: Some(write_ms),
            write_bytes: None,
            scope: None,
            stats_reset: None,
            write_operation: None,
        }
    }

    #[test]
    fn redo_files_add_up_to_one_log() {
        let files = vec![
            file("#ib_redo10", FileKind::Log, 600.0, 3.0),
            file("#ib_redo11", FileKind::Log, 900.0, 6.0),
            file("shop/orders.ibd", FileKind::Data, 10.0, 1.0),
        ];
        let merged = merge_logs(&files);
        let log = merged.iter().find(|f| f.kind == FileKind::Log).unwrap();
        assert_eq!((log.name.as_str(), log.writes, log.write_ms), ("redo log", 1_500.0, Some(9.0)));
        assert_eq!(merged.len(), 2);
    }

    #[test]
    fn merging_an_untimed_log_file_cannot_dilute_the_known_latency() {
        let timed = file("#ib_redo10", FileKind::Log, 1000.0, 50_000.0);
        let untimed = FileIo { write_ms: None, ..file("#ib_redo11", FileKind::Log, 9000.0, 0.0) };
        let merged = merge_logs(&[timed, untimed]);
        assert_eq!(merged[0].writes, 10_000.0);
        assert_eq!(merged[0].avg_write_ms(), None, "the missing timings are unknown, not zero");
    }
}
