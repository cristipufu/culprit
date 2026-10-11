//! Signals beyond time and reads: planning, JIT and WAL per statement (pg_stat_statements); I/O latency (pg_stat_io,
//! pg_stat_database, pg_stat_wal); sessions opened and abandoned; replicas, standby lag and replication slots; HOT
//! updates, fillfactor and estimated bloat per table; constraints not yet validated. PostgreSQL keeps no per-statement
//! error count. Each read is independent and cheap; a failure becomes a note, never a failed scan.

use std::collections::HashMap;

use culprit_core::{
    CollectorNote, ConstraintIssue, ConstraintKind, FileIo, FileKind, Pressure, QueryStat, Replica, ReplicaRole,
    Signal, SignalScope, SignalUnit, Snapshot, TableStorage,
};
use tokio_postgres::Client;

use crate::collect::{Probe, StatementsAccess};
use crate::rows::{Row, query, query_one};

/// Collect every signal into `snapshot`.
pub async fn collect(client: &Client, probe: &Probe, snapshot: &mut Snapshot) {
    let mut notes: Vec<(&str, String)> = Vec::new();
    if let StatementsAccess::Usable { relation, .. } = &probe.statements
        && let Some(workload) = snapshot.workload.as_mut()
    {
        if let Err(e) = attach_costs(client, relation, probe.version_num, &mut workload.queries).await {
            notes.push(("query costs", e.message));
        }
        culprit_core::signals::fill_row_bytes(&mut workload.queries);
    }
    match storage(client, probe.version_num).await {
        Ok(by_table) => {
            for t in &mut snapshot.tables {
                if let Some(s) = by_table.get(&(t.schema.clone(), t.name.clone())) {
                    t.storage = Some(s.clone());
                }
            }
        }
        Err(e) => notes.push(("table storage", e.message)),
    }
    match constraints(client).await {
        Ok(list) => snapshot.constraints = list,
        Err(e) => notes.push(("constraints", e.message)),
    }
    let pressure = snapshot.pressure.get_or_insert_with(Pressure::default);
    match files(client, probe.version_num).await {
        Ok(files) => pressure.files = files,
        Err(e) => notes.push(("file latency", e.message)),
    }
    if probe.version_num >= 140_000 {
        match sessions(client).await {
            Ok(signals) => pressure.signals.extend(signals),
            Err(e) => notes.push(("sessions", e.message)),
        }
    }
    if probe.version_num >= 100_000 {
        match replicas(client).await {
            Ok(replicas) => pressure.replicas = replicas,
            Err(e) => notes.push(("replication", e.message)),
        }
    }
    snapshot
        .notes
        .extend(notes.into_iter().map(|(collector, message)| CollectorNote { collector: collector.into(), message }));
}

// ── per statement ──────────────────────────────────────────────────────────────────────────────────

/// The pg_stat_statements columns each version has: planning (13+), WAL (13+), JIT (15+, deform 17+).
fn costs_columns(version_num: u32) -> Vec<&'static str> {
    let mut columns = Vec::new();
    if version_num >= 130_000 {
        columns.extend([
            "sum(total_plan_time) AS plan_ms",
            "sum(plans) AS plans",
            "sum(wal_bytes) AS wal_bytes",
            "sum(wal_fpi) AS wal_fpi",
            "sum(wal_records) AS wal_records",
        ]);
    }
    if version_num >= 170_000 {
        columns.push(
            "sum(jit_generation_time + jit_inlining_time + jit_optimization_time + jit_emission_time + jit_deform_time) AS jit_ms",
        );
        columns.push("sum(jit_functions) AS jit_functions");
    } else if version_num >= 150_000 {
        columns
            .push("sum(jit_generation_time + jit_inlining_time + jit_optimization_time + jit_emission_time) AS jit_ms");
        columns.push("sum(jit_functions) AS jit_functions");
    }
    columns
}

async fn attach_costs(
    client: &Client,
    relation: &str,
    version_num: u32,
    queries: &mut [QueryStat],
) -> culprit_core::Result<()> {
    let columns = costs_columns(version_num);
    let ids: Vec<&str> = queries
        .iter()
        .map(|q| q.id.as_str())
        // pg_stat_statements query ids are 64-bit integers; anything else never reaches the SQL text.
        .filter(|id| id.parse::<i64>().is_ok())
        .collect();
    if columns.is_empty() || ids.is_empty() {
        return Ok(());
    }
    let scope = format!(
        "dbid = (SELECT oid FROM pg_database WHERE datname = current_database()){}",
        if version_num >= 140_000 { " AND toplevel" } else { "" }
    );
    let sql = format!(
        "SELECT queryid::text AS id, {} FROM {relation} WHERE {scope} AND queryid IN ({}) GROUP BY queryid",
        columns.join(", "),
        ids.join(", ")
    );
    let rows: HashMap<String, Row> = query(client, &sql).await?.into_iter().map(|r| (r.string("id"), r)).collect();
    let total_wal = if version_num >= 130_000 {
        query_one(client, &format!("SELECT sum(wal_bytes) AS wal FROM {relation} WHERE {scope}"))
            .await
            .ok()
            .flatten()
            .and_then(|r| r.number("wal"))
            .filter(|w| *w > 0.0)
    } else {
        None
    };
    for q in queries.iter_mut() {
        let Some(r) = rows.get(&q.id) else { continue };
        let mut costs = q.costs.take().unwrap_or_default();
        // Zero planning time means track_planning is off, not free planning.
        let plans = r.number("plans").filter(|n| *n > 0.0);
        costs.plans = plans;
        costs.plan_ms = plans.and(r.number("plan_ms"));
        costs.log_bytes = r.number("wal_bytes").filter(|b| *b > 0.0);
        costs.log_share = match (costs.log_bytes, total_wal) {
            (Some(own), Some(all)) => Some((own / all).min(1.0)),
            _ => None,
        };
        costs.log_full_pages = costs.log_bytes.and(r.number("wal_fpi"));
        costs.jit_functions = r.number("jit_functions").filter(|n| *n > 0.0);
        costs.jit_ms = costs.jit_functions.and(r.number("jit_ms"));
        q.costs = (!costs.is_empty()).then_some(costs);
    }
    Ok(())
}

// ── per table ──────────────────────────────────────────────────────────────────────────────────────

/// HOT and new-page updates and fillfactor per table, plus a bloat estimate for tables over 10 MB.
async fn storage(client: &Client, version_num: u32) -> culprit_core::Result<HashMap<(String, String), TableStorage>> {
    let new_page = if version_num >= 160_000 { "s.n_tup_newpage_upd" } else { "NULL::float8" };
    let sql = format!(
        "SELECT s.schemaname AS schema_name, s.relname AS table_name, s.n_tup_hot_upd AS hot, {new_page} AS new_page,
                (SELECT substring(o FROM 'fillfactor=([0-9]+)')::float8 FROM unnest(c.reloptions) AS o
                  WHERE o LIKE 'fillfactor=%') AS fillfactor
           FROM pg_stat_user_tables s JOIN pg_class c ON c.oid = s.relid"
    );
    let mut out: HashMap<(String, String), TableStorage> = query(client, &sql)
        .await?
        .iter()
        .map(|r| {
            (
                (r.string("schema_name"), r.string("table_name")),
                TableStorage {
                    hot_updates: r.number("hot"),
                    new_page_updates: r.number("new_page"),
                    fillfactor: Some(r.number("fillfactor").unwrap_or(100.0)),
                    bloat_bytes: None,
                },
            )
        })
        .collect();
    // The estimate is a bonus: without it the update footprint still stands.
    if let Ok(rows) = query(client, BLOAT_SQL).await {
        for r in rows {
            if let Some(entry) = out.get_mut(&(r.string("schema_name"), r.string("table_name"))) {
                entry.bloat_bytes = r.number("bloat_bytes").map(|b| b.max(0.0));
            }
        }
    }
    Ok(out)
}

/// Heap bytes beyond what the live rows need at the table's fillfactor, from row counts and per-column average widths
/// in pg_stats: the widely used statistics estimate, accurate to tens of percent. Tables with columns that have no
/// statistics are skipped, and so are tables under 10 MB.
const BLOAT_SQL: &str = "
WITH t AS (
  SELECT c.oid, n.nspname AS schema_name, c.relname AS table_name, c.relpages, c.reltuples,
         current_setting('block_size')::float8 AS bs,
         coalesce((SELECT substring(o FROM 'fillfactor=([0-9]+)')::float8 FROM unnest(c.reloptions) AS o
                    WHERE o LIKE 'fillfactor=%'), 100) AS fillfactor,
         23 + CASE WHEN max(coalesce(s.null_frac, 0)) > 0 THEN (7 + count(a.attname)) / 8 ELSE 0 END AS header,
         sum((1 - coalesce(s.null_frac, 0)) * coalesce(s.avg_width, 0)) AS data_width,
         count(a.attname) = count(s.attname) AS complete
    FROM pg_class c
    JOIN pg_namespace n ON n.oid = c.relnamespace
    JOIN pg_attribute a ON a.attrelid = c.oid AND a.attnum > 0 AND NOT a.attisdropped
    LEFT JOIN pg_stats s ON s.schemaname = n.nspname AND s.tablename = c.relname AND s.attname = a.attname AND NOT s.inherited
   WHERE c.relkind IN ('r', 'm') AND c.relpages >= 1280 AND c.reltuples > 0
     AND n.nspname NOT IN ('pg_catalog', 'information_schema') AND n.nspname NOT LIKE 'pg\\_toast%'
   GROUP BY c.oid, n.nspname, c.relname, c.relpages, c.reltuples, c.reloptions
)
SELECT schema_name, table_name,
       (relpages - ceil(reltuples / greatest(1, floor((bs - 24) * fillfactor / 100 / (4 + ceil((header + data_width) / 8) * 8))))) * bs
         AS bloat_bytes
  FROM t WHERE complete";

/// CHECK constraints added NOT VALID: the planner skips them for constraint exclusion until they're validated.
async fn constraints(client: &Client) -> culprit_core::Result<Vec<ConstraintIssue>> {
    let sql = "SELECT n.nspname AS schema_name, t.relname AS table_name, c.conname
                 FROM pg_constraint c JOIN pg_class t ON t.oid = c.conrelid JOIN pg_namespace n ON n.oid = t.relnamespace
                WHERE c.contype = 'c' AND NOT c.convalidated
                  AND n.nspname NOT IN ('pg_catalog', 'information_schema')";
    Ok(query(client, sql)
        .await?
        .iter()
        .map(|r| ConstraintIssue {
            schema: r.string("schema_name"),
            table: r.string("table_name"),
            name: r.string("conname"),
            kind: ConstraintKind::Check,
            disabled: false,
        })
        .collect())
}

// ── per server ─────────────────────────────────────────────────────────────────────────────────────

/// Read and write latency since statistics were reset. PostgreSQL 16+ splits I/O by object (pg_stat_io); older
/// servers have database-wide read time. Times exist only with track_io_timing (and track_wal_io_timing for WAL).
async fn files(client: &Client, version_num: u32) -> culprit_core::Result<Vec<FileIo>> {
    let timing = query_one(client, "SELECT current_setting('track_io_timing') = 'on' AS io")
        .await?
        .is_some_and(|r| r.flag("io"));
    let mut out = Vec::new();
    if version_num >= 160_000 {
        let bytes = if version_num >= 180_000 {
            ("sum(read_bytes)", "sum(write_bytes + extend_bytes)")
        } else {
            ("sum(reads * op_bytes)", "sum((writes + extends) * op_bytes)")
        };
        let sql = format!(
            "SELECT object, sum(reads) AS reads, sum(read_time) AS read_ms, {} AS read_bytes,
                    sum(writes + extends) AS writes, sum(write_time + extend_time) AS write_ms, {} AS write_bytes,
                    sum(fsyncs) AS fsyncs, sum(fsync_time) AS fsync_ms
               FROM pg_stat_io GROUP BY object",
            bytes.0, bytes.1
        );
        for r in query(client, &sql).await? {
            let object = r.string("object");
            let (name, kind) = match object.as_str() {
                "relation" => ("Tables and indexes", FileKind::Data),
                "temp relation" => ("Temporary tables", FileKind::Temp),
                "wal" => ("WAL", FileKind::Log),
                _ => continue,
            };
            // WAL commits wait on fsync; data writes on the write itself.
            let (writes, write_ms) = if kind == FileKind::Log {
                (r.number("fsyncs").unwrap_or(0.0), r.number("fsync_ms"))
            } else {
                (r.number("writes").unwrap_or(0.0), r.number("write_ms"))
            };
            let reads = r.number("reads").unwrap_or(0.0);
            if reads + writes <= 0.0 {
                continue;
            }
            let timed =
                |ms: Option<f64>| if kind == FileKind::Log { ms.filter(|m| *m > 0.0) } else { ms.filter(|_| timing) };
            out.push(FileIo {
                name: name.into(),
                kind,
                database: None,
                reads,
                read_ms: timed(r.number("read_ms")),
                read_bytes: r.number("read_bytes"),
                writes,
                write_ms: timed(write_ms),
                write_bytes: r.number("write_bytes"),
            });
        }
    } else {
        let sql = "SELECT blks_read, blk_read_time, current_setting('block_size')::float8 AS bs
                     FROM pg_stat_database WHERE datname = current_database()";
        if let Some(r) = query_one(client, sql).await?
            && let Some(reads) = r.number("blks_read").filter(|n| *n > 0.0)
        {
            out.push(FileIo {
                name: "Tables and indexes".into(),
                kind: FileKind::Data,
                database: None,
                reads,
                read_ms: r.number("blk_read_time").filter(|_| timing),
                read_bytes: r.number("bs").map(|bs| bs * reads),
                writes: 0.0,
                write_ms: None,
                write_bytes: None,
            });
        }
    }
    // WAL flushes before 18 live in pg_stat_wal (14+), timed with track_wal_io_timing.
    if (140_000..180_000).contains(&version_num)
        && let Some(r) = query_one(client, "SELECT wal_sync, wal_sync_time, wal_bytes FROM pg_stat_wal").await?
        && let Some(syncs) = r.number("wal_sync").filter(|n| *n > 0.0)
    {
        out.push(FileIo {
            name: "WAL".into(),
            kind: FileKind::Log,
            database: None,
            reads: 0.0,
            read_ms: None,
            read_bytes: None,
            writes: syncs,
            write_ms: r.number("wal_sync_time").filter(|ms| *ms > 0.0),
            write_bytes: r.number("wal_bytes"),
        });
    }
    Ok(out)
}

/// Sessions opened per second, how long they last and how many the client abandoned (PostgreSQL 14+).
async fn sessions(client: &Client) -> culprit_core::Result<Vec<Signal>> {
    let Some(r) = query_one(
        client,
        "SELECT sessions, session_time, sessions_abandoned,
                extract(epoch FROM now() - coalesce(stats_reset, pg_postmaster_start_time())) AS seconds
           FROM pg_stat_database WHERE datname = current_database()",
    )
    .await?
    else {
        return Ok(Vec::new());
    };
    let (Some(sessions), Some(seconds)) =
        (r.number("sessions").filter(|n| *n > 0.0), r.number("seconds").filter(|s| *s > 0.0))
    else {
        return Ok(Vec::new());
    };
    let rate = sessions / seconds;
    let mut out = vec![
        Signal::new("new-connections", "New connections", rate, SignalUnit::PerSecond, SignalScope::SinceStartup)
            .graded("under 1 a second: pooled connections are opened once", 2.0, 10.0, true),
    ];
    if let Some(ms) = r.number("session_time") {
        let length = Signal::new(
            "session-length",
            "Average session length",
            ms / sessions / 1000.0,
            SignalUnit::Seconds,
            SignalScope::SinceStartup,
        );
        // Short sessions only matter when there are many of them.
        out.push(if rate >= 1.0 {
            length.graded(
                "minutes: a session that lasts under a second pays more to connect than to work",
                60.0,
                1.0,
                false,
            )
        } else {
            length
        });
    }
    if let Some(abandoned) = r.number("sessions_abandoned") {
        out.push(
            Signal::new(
                "sessions-abandoned",
                "Sessions the client abandoned",
                abandoned / sessions * 100.0,
                SignalUnit::Percent,
                SignalScope::SinceStartup,
            )
            .graded("under 1%: clients close their connections", 1.0, 10.0, true),
        );
    }
    Ok(out)
}

/// Replicas this server streams to, its own replay lag when it is a standby, and replication slots that keep WAL.
async fn replicas(client: &Client) -> culprit_core::Result<Vec<Replica>> {
    let standby = query_one(client, "SELECT pg_is_in_recovery() AS standby").await?.is_some_and(|r| r.flag("standby"));
    let mut out = Vec::new();
    if standby {
        if let Some(r) = query_one(
            client,
            "SELECT extract(epoch FROM now() - pg_last_xact_replay_timestamp()) AS lag_s,
                    pg_wal_lsn_diff(pg_last_wal_receive_lsn(), pg_last_wal_replay_lsn()) AS behind,
                    (SELECT status FROM pg_stat_wal_receiver LIMIT 1) AS state",
        )
        .await?
        {
            out.push(Replica {
                name: "this server".into(),
                role: ReplicaRole::ThisServer,
                state: r.text("state").or_else(|| Some("not receiving".into())),
                sync: None,
                lag_seconds: r.number("lag_s"),
                lag_bytes: r.number("behind"),
                unsent_bytes: None,
                error: None,
            });
        }
        return Ok(out);
    }
    for r in query(
        client,
        "SELECT coalesce(nullif(application_name, ''), client_addr::text, pid::text) AS name, state, sync_state,
                extract(epoch FROM replay_lag) AS lag_s, pg_wal_lsn_diff(pg_current_wal_lsn(), replay_lsn) AS behind
           FROM pg_stat_replication",
    )
    .await?
    {
        out.push(Replica {
            name: r.string("name"),
            role: ReplicaRole::Replica,
            state: r.text("state"),
            sync: r.text("sync_state"),
            lag_seconds: r.number("lag_s"),
            lag_bytes: r.number("behind"),
            unsent_bytes: None,
            error: None,
        });
    }
    for r in query(
        client,
        "SELECT slot_name, slot_type, active, pg_wal_lsn_diff(pg_current_wal_lsn(), restart_lsn) AS kept
           FROM pg_replication_slots WHERE restart_lsn IS NOT NULL",
    )
    .await?
    {
        out.push(Replica {
            name: r.string("slot_name"),
            role: ReplicaRole::Slot,
            state: Some(if r.flag("active") { "active" } else { "inactive" }.into()),
            sync: r.text("slot_type"),
            lag_seconds: None,
            lag_bytes: r.number("kept"),
            unsent_bytes: None,
            error: None,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cost_columns_follow_the_version() {
        assert!(costs_columns(120_000).is_empty());
        assert!(costs_columns(130_000).iter().any(|c| c.contains("total_plan_time")));
        assert!(!costs_columns(140_000).iter().any(|c| c.contains("jit")));
        assert!(costs_columns(150_000).iter().any(|c| c.contains("jit_emission_time")));
        assert!(costs_columns(170_000).iter().any(|c| c.contains("jit_deform_time")));
    }
}
