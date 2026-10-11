//! Signals beyond time and reads. Per statement: failed and cancelled runs, planning and JIT time, log volume and the
//! size of what it returns. Per server: storage latency per file and replica lag. Per object: lock and latch waits on
//! an index, constraints the optimizer can't trust, column histograms and how updates land on a table's pages.
//!
//! Every engine keeps a different subset. A value the engine doesn't keep is `None` (or absent), never zero.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::{Plan, QueryStat};

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
    ["AvgRowSize", "Plan Width"]
        .iter()
        .find_map(|key| plan.root.props.get(*key))
        .and_then(|v| v.trim().parse::<f64>().ok())
        .filter(|w| *w > 0.0)
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
        match out.iter_mut().find(|o| o.kind == FileKind::Log && o.database == f.database) {
            Some(log) => {
                let sum = |a: Option<f64>, b: Option<f64>| match (a, b) {
                    (None, None) => None,
                    (a, b) => Some(a.unwrap_or(0.0) + b.unwrap_or(0.0)),
                };
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
    pub lag_seconds: Option<f64>,
    /// Bytes not yet applied (PostgreSQL replay lag, SQL Server redo queue) or kept for a slot.
    pub lag_bytes: Option<f64>,
    /// Bytes the primary hasn't sent yet (SQL Server log send queue).
    pub unsent_bytes: Option<f64>,
    /// The last replication error, as the engine reports it (database text).
    pub error: Option<String>,
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
}

#[cfg(test)]
mod merge_tests {
    use super::*;

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
}
