//! Signals beyond time and reads, for MySQL and MariaDB: errors and warnings per statement digest (and the error code
//! recent failed runs raised), file I/O latency (performance_schema), connections opened and threads created, replica
//! status, and column histograms. Lock time per statement is already part of the digest's waits. Neither engine keeps
//! redo bytes per statement or row-lock waits per table. Each read is independent and cheap; a failure becomes a note.

use std::collections::HashMap;

use culprit_core::{
    CollectorNote, ColumnHistogram, FileIo, FileKind, Pressure, QueryStat, Replica, ReplicaRole, Signal, SignalScope,
    SignalUnit, Snapshot,
};
use mysql_async::{Conn, Value};

use crate::collect::Probe;
use crate::connect::{Connector, Scope};
use crate::rows::{Rec, query, query_with};

const PICOS_PER_MS: f64 = 1_000_000_000.0;

/// Collect every signal into `snapshot` on a connection of its own.
pub async fn collect(connector: &Connector, probe: &Probe, snapshot: &mut Snapshot) {
    let mut conn = match connector.connect(Scope::Server).await {
        Ok(conn) => conn,
        Err(e) => {
            snapshot.notes.push(CollectorNote { collector: "signals".into(), message: e.message });
            return;
        }
    };
    let mariadb = probe.version.to_ascii_lowercase().contains("mariadb");
    let mut notes: Vec<(&str, String)> = Vec::new();
    if probe.performance_schema
        && let Some(workload) = snapshot.workload.as_mut()
        && let Err(e) = attach_errors(&mut conn, &probe.database, &mut workload.queries).await
    {
        notes.push(("query errors", e.message));
    }
    match histograms(&mut conn, &probe.database, &probe.version, mariadb).await {
        Ok(found) => snapshot.histograms = found,
        Err(e) => notes.push(("histograms", e.message)),
    }
    let pressure = snapshot.pressure.get_or_insert_with(Pressure::default);
    if probe.performance_schema {
        match files(&mut conn).await {
            Ok(files) => pressure.files = files,
            Err(e) => notes.push(("file latency", e.message)),
        }
    }
    match connections(&mut conn).await {
        Ok(signals) => pressure.signals.extend(signals),
        Err(e) => notes.push(("connections", e.message)),
    }
    match replica_status(&mut conn).await {
        Ok(replicas) => pressure.replicas = replicas,
        Err(e) => notes.push((
            "replication",
            format!("{} Reading replica status needs the REPLICATION CLIENT privilege; it only matters if this server is a replica.", e.message),
        )),
    }
    let _ = conn.disconnect().await;
    snapshot
        .notes
        .extend(notes.into_iter().map(|(collector, message)| CollectorNote { collector: collector.into(), message }));
}

// ── per statement ──────────────────────────────────────────────────────────────────────────────────

/// A fixed name for error codes a statement commonly fails with. The server's message can carry row values, so it is
/// never kept.
pub fn error_name(code: u32) -> Option<&'static str> {
    Some(match code {
        1205 => "lock wait timeout",
        1213 => "deadlock",
        3024 => "statement timeout (max_execution_time)",
        1969 => "statement timeout (max_statement_time)",
        1317 => "query interrupted",
        3572 => "lock not available (NOWAIT)",
        1062 => "duplicate key",
        1451 => "row is referenced by a foreign key",
        1452 => "foreign key has no parent row",
        1048 => "column cannot be null",
        1406 => "data too long",
        1264 | 1690 => "value out of range",
        1292 | 1366 => "incorrect value",
        1064 => "syntax error",
        1054 => "unknown column",
        1146 => "table doesn't exist",
        1114 => "table is full",
        1206 => "lock table full",
        _ => return None,
    })
}

async fn attach_errors(conn: &mut Conn, database: &str, queries: &mut [QueryStat]) -> culprit_core::Result<()> {
    let digests: Vec<String> = queries.iter().filter_map(|q| q.query_hash.clone()).collect();
    if digests.is_empty() {
        return Ok(());
    }
    let marks = vec!["?"; digests.len()].join(", ");
    let params = || {
        let mut p = vec![Value::from(database.to_string())];
        p.extend(digests.iter().cloned().map(Value::from));
        p
    };
    let rows = query_with(
        conn,
        &format!(
            "SELECT DIGEST AS digest, SUM_ERRORS AS errors, SUM_WARNINGS AS warnings
               FROM performance_schema.events_statements_summary_by_digest
              WHERE SCHEMA_NAME = ? AND DIGEST IN ({marks})"
        ),
        params(),
    )
    .await?;
    let counts: HashMap<String, Rec> = rows.into_iter().map(|r| (r.string("digest"), r)).collect();

    // The recent-statement tables keep each failed run's error code: enough to name what failed.
    let mut codes: HashMap<String, HashMap<u32, f64>> = HashMap::new();
    for table in ["events_statements_history", "events_statements_history_long"] {
        let sql = format!(
            "SELECT DIGEST AS digest, MYSQL_ERRNO AS code, COUNT(*) AS n FROM performance_schema.{table}
              WHERE CURRENT_SCHEMA = ? AND MYSQL_ERRNO <> 0 AND DIGEST IN ({marks}) GROUP BY DIGEST, MYSQL_ERRNO"
        );
        if let Ok(rows) = query_with(conn, &sql, params()).await {
            for r in rows {
                if let Some(code) = r.number("code") {
                    *codes.entry(r.string("digest")).or_default().entry(code as u32).or_default() +=
                        r.number("n").unwrap_or(0.0);
                }
            }
        }
    }
    for q in queries.iter_mut() {
        let Some(digest) = q.query_hash.clone() else { continue };
        let Some(r) = counts.get(&digest) else { continue };
        let mut costs = q.costs.take().unwrap_or_default();
        costs.failed = r.number("errors");
        costs.warnings = r.number("warnings");
        costs.top_error = codes.get(&digest).and_then(|by_code| {
            let (code, _) = by_code.iter().max_by(|a, b| a.1.total_cmp(b.1))?;
            Some(match error_name(*code) {
                Some(name) => format!("{code} {name}"),
                None => format!("error {code}"),
            })
        });
        q.costs = (!costs.is_empty()).then_some(costs);
    }
    Ok(())
}

// ── per server ─────────────────────────────────────────────────────────────────────────────────────

/// The kind of file an instrument reads and writes.
pub fn file_kind(event: &str, file: &str) -> FileKind {
    let event = event.to_ascii_lowercase();
    let file = file.to_ascii_lowercase();
    if event.contains("log_file")
        || event.contains("redo")
        || event.ends_with("/binlog")
        || event.ends_with("/relaylog")
        || file.contains("#ib_redo")
        || file.contains("ib_logfile")
    {
        FileKind::Log
    } else if event.contains("temp") || file.contains("ibtmp") || file.contains("#innodb_temp") {
        FileKind::Temp
    } else if event.contains("innodb_data_file") {
        FileKind::Data
    } else {
        FileKind::Other
    }
}

/// The last two path components: enough to recognize `shop/orders.ibd` without the server's directory layout.
pub fn short_path(path: &str) -> String {
    let parts: Vec<&str> = path.split(['/', '\\']).filter(|p| !p.is_empty() && *p != ".").collect();
    parts[parts.len().saturating_sub(2)..].join("/")
}

/// Read and write time per file since startup, for the 24 files that took the most time.
async fn files(conn: &mut Conn) -> culprit_core::Result<Vec<FileIo>> {
    let rows = query(
        conn,
        "SELECT FILE_NAME AS file_name, EVENT_NAME AS event_name, COUNT_READ AS read_count, SUM_TIMER_READ AS read_ps,
                SUM_NUMBER_OF_BYTES_READ AS read_bytes, COUNT_WRITE AS write_count, SUM_TIMER_WRITE AS write_ps,
                SUM_NUMBER_OF_BYTES_WRITE AS write_bytes
           FROM performance_schema.file_summary_by_instance
          WHERE COUNT_READ + COUNT_WRITE > 0
          ORDER BY SUM_TIMER_READ + SUM_TIMER_WRITE DESC LIMIT 24",
    )
    .await?;
    Ok(rows
        .iter()
        .map(|r| {
            let path = r.string("file_name");
            let reads = r.number("read_count").unwrap_or(0.0);
            let writes = r.number("write_count").unwrap_or(0.0);
            // A zero timer with operations means the instrument isn't timed, not that the disk is instant.
            let timed = |ps: Option<f64>, n: f64| ps.filter(|p| *p > 0.0 || n == 0.0).map(|p| p / PICOS_PER_MS);
            FileIo {
                name: short_path(&path),
                kind: file_kind(&r.string("event_name"), &path),
                database: None,
                reads,
                read_ms: timed(r.number("read_ps"), reads),
                read_bytes: r.number("read_bytes"),
                writes,
                write_ms: timed(r.number("write_ps"), writes),
                write_bytes: r.number("write_bytes"),
            }
        })
        .collect())
}

/// Connections opened per second since startup, how many needed a new thread, and failed connection attempts.
async fn connections(conn: &mut Conn) -> culprit_core::Result<Vec<Signal>> {
    let rows = query(
        conn,
        "SHOW GLOBAL STATUS WHERE Variable_name IN
           ('Connections', 'Threads_created', 'Aborted_connects', 'Aborted_clients', 'Uptime')",
    )
    .await?;
    let status: HashMap<String, f64> = rows
        .iter()
        .filter_map(|r| Some((r.string("variable_name"), r.text("value")?.trim().parse::<f64>().ok()?)))
        .collect();
    let get = |k: &str| status.get(k).copied();
    let Some(uptime) = get("Uptime").filter(|u| *u > 0.0) else { return Ok(Vec::new()) };
    let mut out = Vec::new();
    if let Some(connections) = get("Connections") {
        let rate = connections / uptime;
        out.push(
            Signal::new("new-connections", "New connections", rate, SignalUnit::PerSecond, SignalScope::SinceStartup)
                .graded("under 5 a second: pooled connections are opened once", 5.0, 25.0, true),
        );
        if let Some(created) = get("Threads_created")
            && connections >= 1_000.0
        {
            out.push(
                Signal::new(
                    "thread-cache-misses",
                    "Connections that needed a new thread",
                    created / connections * 100.0,
                    SignalUnit::Percent,
                    SignalScope::SinceStartup,
                )
                .graded("under 10%: thread_cache_size covers new connections", 10.0, 50.0, true),
            );
        }
    }
    let hours = uptime / 3600.0;
    for (key, name, label) in [
        ("aborted-connects", "Aborted_connects", "Failed connection attempts"),
        ("aborted-clients", "Aborted_clients", "Connections dropped without closing"),
    ] {
        if let Some(n) = get(name) {
            // Under an hour of uptime, a rate per hour would be an extrapolation: report the count.
            let signal = if hours >= 1.0 {
                Signal::new(key, label, n / hours, SignalUnit::PerHour, SignalScope::SinceStartup)
            } else {
                Signal::new(key, &format!("{label} since start"), n, SignalUnit::Count, SignalScope::SinceStartup)
            };
            out.push(signal);
        }
    }
    Ok(out)
}

/// This server's replication channels when it is a replica: lag, thread state and the last error code.
async fn replica_status(conn: &mut Conn) -> culprit_core::Result<Vec<Replica>> {
    let rows = match query(conn, "SHOW REPLICA STATUS").await {
        Ok(rows) => rows,
        // Before MySQL 8.0.22 and MariaDB 10.5.1.
        Err(_) => query(conn, "SHOW SLAVE STATUS").await?,
    };
    Ok(rows.iter().map(replica).collect())
}

fn replica(r: &Rec) -> Replica {
    let either = |a: &str, b: &str| r.text(a).or_else(|| r.text(b));
    let io = either("replica_io_running", "slave_io_running").unwrap_or_default();
    let sql = either("replica_sql_running", "slave_sql_running").unwrap_or_default();
    let io_errno = r.number("last_io_errno").filter(|n| *n > 0.0);
    let sql_errno = r.number("last_sql_errno").filter(|n| *n > 0.0);
    let state = match (io.eq_ignore_ascii_case("yes"), sql.eq_ignore_ascii_case("yes")) {
        (true, true) => "replicating".to_string(),
        (false, true) => format!("receiving stopped (I/O thread: {io})"),
        (true, false) => "applying stopped (SQL thread)".to_string(),
        (false, false) => "stopped".to_string(),
    };
    let channel = either("channel_name", "connection_name").filter(|c| !c.is_empty());
    let source = either("source_host", "master_host").unwrap_or_default();
    Replica {
        name: match channel {
            Some(c) => format!("{c} (from {source})"),
            None => format!("from {source}"),
        },
        role: ReplicaRole::ThisServer,
        state: Some(state),
        sync: None,
        lag_seconds: either("seconds_behind_source", "seconds_behind_master").and_then(|s| s.trim().parse().ok()),
        lag_bytes: None,
        unsent_bytes: None,
        error: sql_errno.or(io_errno).map(|n| {
            let code = n as u32;
            match error_name(code) {
                Some(name) => format!("{code} {name}"),
                None => format!("error {code}"),
            }
        }),
    }
}

/// Column histograms in this database: MySQL 8's COLUMN_STATISTICS, MariaDB's mysql.column_stats. `None` on MySQL 5.7,
/// which has none.
async fn histograms(
    conn: &mut Conn,
    database: &str,
    version: &str,
    mariadb: bool,
) -> culprit_core::Result<Option<Vec<ColumnHistogram>>> {
    let params = vec![Value::from(database.to_string())];
    if mariadb {
        let rows = query_with(
            conn,
            "SELECT db_name AS schema_name, table_name, column_name, hist_type AS kind
               FROM mysql.column_stats WHERE db_name = ? AND hist_type IS NOT NULL",
            params,
        )
        .await?;
        return Ok(Some(
            rows.iter()
                .map(|r| ColumnHistogram {
                    schema: r.string("schema_name"),
                    table: r.string("table_name"),
                    column: r.string("column_name"),
                    kind: r.text("kind"),
                    buckets: None,
                })
                .collect(),
        ));
    }
    let major: u32 = version.split('.').next().and_then(|m| m.parse().ok()).unwrap_or(0);
    if major < 8 {
        return Ok(None);
    }
    let rows = query_with(
        conn,
        "SELECT SCHEMA_NAME AS schema_name, TABLE_NAME AS table_name, COLUMN_NAME AS column_name,
                JSON_UNQUOTE(JSON_EXTRACT(HISTOGRAM, '$.\"histogram-type\"')) AS kind,
                JSON_LENGTH(JSON_EXTRACT(HISTOGRAM, '$.buckets')) AS buckets
           FROM information_schema.COLUMN_STATISTICS WHERE SCHEMA_NAME = ?",
        params,
    )
    .await?;
    Ok(Some(
        rows.iter()
            .map(|r| ColumnHistogram {
                schema: r.string("schema_name"),
                table: r.string("table_name"),
                column: r.string("column_name"),
                kind: r.text("kind"),
                buckets: r.number("buckets"),
            })
            .collect(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn files_are_sorted_by_kind() {
        assert_eq!(file_kind("wait/io/file/innodb/innodb_data_file", "/var/lib/mysql/shop/orders.ibd"), FileKind::Data);
        assert_eq!(
            file_kind("wait/io/file/innodb/innodb_log_file", "/var/lib/mysql/#innodb_redo/#ib_redo9"),
            FileKind::Log
        );
        assert_eq!(
            file_kind("wait/io/file/innodb/innodb_temp_file", "/var/lib/mysql/#innodb_temp/temp_1.ibt"),
            FileKind::Temp
        );
        assert_eq!(file_kind("wait/io/file/innodb/innodb_data_file", "/var/lib/mysql/ibtmp1"), FileKind::Temp);
        assert_eq!(file_kind("wait/io/file/sql/binlog", "/var/lib/mysql/binlog.000002"), FileKind::Log);
        assert_eq!(file_kind("wait/io/file/aria/MAD", "/var/lib/mysql/mysql/db.MAD"), FileKind::Other);
    }

    #[test]
    fn paths_keep_the_last_two_parts() {
        assert_eq!(short_path("/var/lib/mysql/shop/orders.ibd"), "shop/orders.ibd");
        assert_eq!(short_path("./ibdata1"), "ibdata1");
        assert_eq!(short_path("C:\\data\\shop\\orders.ibd"), "shop/orders.ibd");
    }

    #[test]
    fn error_codes_have_fixed_names() {
        assert_eq!(error_name(1205), Some("lock wait timeout"));
        assert_eq!(error_name(1969), Some("statement timeout (max_statement_time)"));
        assert_eq!(error_name(4242), None);
    }
}
