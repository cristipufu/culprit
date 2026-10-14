//! Signals beyond time and reads: planning, JIT and WAL per statement (pg_stat_statements); I/O latency (pg_stat_io,
//! pg_stat_database, pg_stat_wal); sessions opened and abandoned; replicas, standby lag and replication slots; HOT
//! updates, fillfactor and estimated bloat per table; constraints not yet validated; the settings their advice depends
//! on (wal_compression, constraint_exclusion, max_slot_wal_keep_size). PostgreSQL keeps no per-statement error count.
//! Each read is independent and cheap; a failure becomes a note, never a failed scan.

use std::collections::HashMap;

use culprit_core::{
    CollectorNote, ConstraintIssue, ConstraintKind, FileIo, FileKind, Pressure, QueryStat, Replica, ReplicaRole,
    Setting, Signal, SignalScope, SignalUnit, Snapshot, TableStorage,
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
    match constraints(client, probe.version_num).await {
        Ok(list) => snapshot.constraints = list,
        Err(e) => notes.push(("constraints", e.message)),
    }
    match advice_settings(client).await {
        Ok(found) => {
            for setting in found {
                if snapshot.setting(&setting.name).is_none() {
                    snapshot.settings.push(setting);
                }
            }
        }
        Err(e) => notes.push(("settings", e.message)),
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
        match replicas(client, probe.version_num, probe.cloud.as_deref()).await {
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

/// HOT and new-page updates and fillfactor per table, how long its counters have counted and the planner's row count,
/// plus a bloat estimate for tables over 10 MB.
async fn storage(client: &Client, version_num: u32) -> culprit_core::Result<HashMap<(String, String), TableStorage>> {
    let new_page = if version_num >= 160_000 { "s.n_tup_newpage_upd" } else { "NULL::float8" };
    // The counters run from the database's last statistics reset. PostgreSQL 15+ leaves stats_reset empty until the
    // first one, and the counters outlive a clean restart: their age is then unknown, never the server's uptime.
    let sql = format!(
        "SELECT s.schemaname AS schema_name, s.relname AS table_name, s.n_tup_hot_upd AS hot, {new_page} AS new_page,
                (SELECT substring(o FROM 'fillfactor=([0-9]+)')::float8 FROM unnest(c.reloptions) AS o
                  WHERE o LIKE 'fillfactor=%') AS fillfactor,
                CASE WHEN c.reltuples >= 0 THEN c.reltuples::float8 END AS planner_rows,
                (SELECT extract(epoch FROM now() - d.stats_reset) / 3600 FROM pg_stat_database d
                  WHERE d.datname = current_database()) AS counter_hours
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
                    counter_hours: r.number("counter_hours").filter(|h| *h > 0.0),
                    planner_rows: r.number("planner_rows"),
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

/// CHECK constraints added NOT VALID, and whether their table is a legacy inheritance child: the one place the
/// planner reads CHECK constraints by default (constraint exclusion), and only validated ones.
async fn constraints(client: &Client, version_num: u32) -> culprit_core::Result<Vec<ConstraintIssue>> {
    // Declarative partitions (10+) are pruned by their bounds, not by CHECK constraints.
    let not_partition = if version_num >= 100_000 { " AND NOT t.relispartition" } else { "" };
    let sql = format!(
        "SELECT n.nspname AS schema_name, t.relname AS table_name, c.conname,
                EXISTS (SELECT 1 FROM pg_inherits i WHERE i.inhrelid = t.oid){not_partition} AS inheritance_child
           FROM pg_constraint c JOIN pg_class t ON t.oid = c.conrelid JOIN pg_namespace n ON n.oid = t.relnamespace
          WHERE c.contype = 'c' AND NOT c.convalidated
            AND n.nspname NOT IN ('pg_catalog', 'information_schema')"
    );
    Ok(query(client, &sql)
        .await?
        .iter()
        .map(|r| ConstraintIssue {
            schema: r.string("schema_name"),
            table: r.string("table_name"),
            name: r.string("conname"),
            kind: ConstraintKind::Check,
            disabled: false,
            inheritance_child: r.flag("inheritance_child"),
        })
        .collect())
}

/// Settings the signals' advice depends on, as pg_settings has them (absent on versions without them): whether WAL
/// full-page images are already compressed, where the planner reads CHECK constraints, and what caps a slot's WAL.
async fn advice_settings(client: &Client) -> culprit_core::Result<Vec<Setting>> {
    let rows = query(
        client,
        "SELECT name, setting, unit, boot_val FROM pg_settings
          WHERE name IN ('wal_compression', 'constraint_exclusion', 'max_slot_wal_keep_size')",
    )
    .await?;
    Ok(rows
        .iter()
        .map(|r| {
            let unit = r.text("unit");
            Setting {
                name: r.string("name"),
                value: crate::collect::pretty_setting(&r.string("setting"), unit.as_deref()),
                default_value: r.text("boot_val").map(|v| crate::collect::pretty_setting(&v, unit.as_deref())),
                scope: "server".into(),
            }
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
    // The counts run from the last statistics reset and outlive a clean restart. PostgreSQL 15+ leaves stats_reset
    // empty until the first reset: then how long they have counted is unknown, and the server's start time would turn
    // days of sessions into a rate many times too high. No rate then; the shares below need no time.
    let Some(r) = query_one(
        client,
        "SELECT sessions, session_time, sessions_abandoned, extract(epoch FROM now() - stats_reset) AS seconds
           FROM pg_stat_database WHERE datname = current_database()",
    )
    .await?
    else {
        return Ok(Vec::new());
    };
    let Some(sessions) = r.number("sessions").filter(|n| *n > 0.0) else {
        return Ok(Vec::new());
    };
    let seconds = r.number("seconds").filter(|s| *s > 0.0);
    Ok(session_signals(sessions, seconds, r.number("session_time"), r.number("sessions_abandoned")))
}

/// The session signals from pg_stat_database's counts; `seconds` is how long they have counted, when known.
fn session_signals(
    sessions: f64,
    seconds: Option<f64>,
    session_ms: Option<f64>,
    abandoned: Option<f64>,
) -> Vec<Signal> {
    let rate = seconds.map(|s| sessions / s);
    let mut out = Vec::new();
    if let Some(rate) = rate {
        out.push(
            Signal::new("new-connections", "New connections", rate, SignalUnit::PerSecond, SignalScope::SinceStartup)
                .graded("under 1 a second: pooled connections are opened once", 2.0, 10.0, true),
        );
    }
    if let Some(ms) = session_ms {
        let length = Signal::new(
            "session-length",
            "Average session length",
            ms / sessions / 1000.0,
            SignalUnit::Seconds,
            SignalScope::SinceStartup,
        );
        // Short sessions only matter when there are many of them.
        out.push(if rate.is_some_and(|r| r >= 1.0) {
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
    if let Some(abandoned) = abandoned {
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
    out
}

/// What a standby reads about itself.
struct Standby {
    /// WAL received and not yet replayed; `None` when nothing streams to it (archive recovery).
    behind: Option<f64>,
    /// Seconds since the last replayed commit was committed on the primary.
    since_replay: Option<f64>,
    /// pg_stat_wal_receiver's status, when a receiver runs.
    receiver: Option<String>,
    /// Whether primary_conninfo is set: `None` when this login can't see it (it can hold a password) or the version
    /// keeps it in recovery.conf.
    streaming_configured: Option<bool>,
    /// recovery_min_apply_delay, in seconds.
    delay: Option<f64>,
}

/// This server's own lag as a standby. Seconds since the last replayed commit are lag only while there is WAL it
/// received and hasn't replayed: on a quiet primary they grow with nothing to apply. A standby without a WAL receiver
/// is stopped only when it is meant to stream (primary_conninfo set); without it, it restores WAL from an archive.
fn standby_replica(s: Standby) -> Replica {
    let delay = s.delay.filter(|d| *d > 0.0);
    let state = match (s.receiver, s.streaming_configured) {
        (Some(status), _) => status,
        (None, Some(true)) => "not receiving".to_string(),
        (None, Some(false)) => "restoring from archive".to_string(),
        (None, None) => "not streaming".to_string(),
    };
    let lag_seconds = match s.behind {
        Some(b) if b > 0.0 => s.since_replay.map(|t| (t - delay.unwrap_or(0.0)).max(0.0)),
        Some(_) => Some(0.0),
        None => None,
    };
    Replica {
        state: Some(state),
        lag_seconds,
        lag_bytes: s.behind,
        delay_seconds: delay,
        ..Replica::new("this server", ReplicaRole::ThisServer)
    }
}

/// Replicas this server streams to and logical consumers it sends changes to, its own replay lag when it is a
/// standby, and replication slots that keep WAL.
async fn replicas(client: &Client, version_num: u32, cloud: Option<&str>) -> culprit_core::Result<Vec<Replica>> {
    let standby = query_one(client, "SELECT pg_is_in_recovery() AS standby").await?.is_some_and(|r| r.flag("standby"));
    let mut out = Vec::new();
    if standby {
        // Aurora readers replicate through the shared storage, not WAL streaming: no receiver, nothing to measure here
        // (Aurora reports their lag in aurora_replica_status() and CloudWatch).
        if cloud == Some("Amazon Aurora") {
            return Ok(out);
        }
        // primary_conninfo can hold a password: only whether it is set is read, and pg_settings hides it from logins
        // that may not see it.
        if let Some(r) = query_one(
            client,
            "SELECT pg_wal_lsn_diff(pg_last_wal_receive_lsn(), pg_last_wal_replay_lsn()) AS behind,
                    extract(epoch FROM now() - pg_last_xact_replay_timestamp()) AS since_replay,
                    (SELECT status FROM pg_stat_wal_receiver LIMIT 1) AS receiver,
                    (SELECT setting <> '' FROM pg_settings WHERE name = 'primary_conninfo') AS streaming_configured,
                    (SELECT setting::float8 / 1000 FROM pg_settings WHERE name = 'recovery_min_apply_delay') AS delay_s",
        )
        .await?
        {
            let configured = r.text("streaming_configured").map(|v| matches!(v.as_str(), "t" | "true"));
            out.push(standby_replica(Standby {
                behind: r.number("behind"),
                since_replay: r.number("since_replay"),
                receiver: r.text("receiver"),
                streaming_configured: configured,
                delay: r.number("delay_s"),
            }));
        }
        return Ok(out);
    }
    // A walsender with a logical slot sends decoded changes to a consumer, not WAL to a copy of the server.
    for r in query(
        client,
        "SELECT coalesce(nullif(r.application_name, ''), r.client_addr::text, r.pid::text) AS name, r.state, r.sync_state,
                extract(epoch FROM r.replay_lag) AS lag_s, pg_wal_lsn_diff(pg_current_wal_lsn(), r.replay_lsn) AS behind,
                coalesce(s.slot_type = 'logical', false) AS logical
           FROM pg_stat_replication r LEFT JOIN pg_replication_slots s ON s.active_pid = r.pid",
    )
    .await?
    {
        out.push(Replica {
            state: r.text("state"),
            sync: r.text("sync_state"),
            lag_seconds: r.number("lag_s"),
            lag_bytes: r.number("behind"),
            logical: Some(r.flag("logical")),
            ..Replica::new(r.string("name"), ReplicaRole::Replica)
        });
    }
    for r in query(client, &slots_sql(version_num)).await? {
        let slot_type = r.text("slot_type");
        out.push(Replica {
            state: Some(if r.flag("active") { "active" } else { "inactive" }.into()),
            lag_bytes: r.number("kept"),
            logical: slot_type.as_deref().map(|t| t == "logical"),
            sync: slot_type,
            inactive_seconds: r.number("inactive_s"),
            wal_status: r.text("wal_status"),
            safe_wal_bytes: r.number("safe_wal_size"),
            ..Replica::new(r.string("slot_name"), ReplicaRole::Slot)
        });
    }
    Ok(out)
}

/// Replication slots and the WAL each keeps; from 13 whether that WAL is still guaranteed and what max_slot_wal_keep_size
/// leaves before the slot is given up, from 17 since when it has had no consumer.
fn slots_sql(version_num: u32) -> String {
    let mut columns =
        vec!["slot_name", "slot_type", "active", "pg_wal_lsn_diff(pg_current_wal_lsn(), restart_lsn) AS kept"];
    if version_num >= 130_000 {
        columns.extend(["wal_status", "safe_wal_size::float8 AS safe_wal_size"]);
    }
    if version_num >= 170_000 {
        columns.push("extract(epoch FROM now() - inactive_since) AS inactive_s");
    }
    format!("SELECT {} FROM pg_replication_slots WHERE restart_lsn IS NOT NULL", columns.join(", "))
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

    fn standby(behind: Option<f64>, since_replay: f64) -> Standby {
        Standby {
            behind,
            since_replay: Some(since_replay),
            receiver: Some("streaming".into()),
            streaming_configured: Some(true),
            delay: None,
        }
    }

    // On a quiet primary the last replayed commit gets older by the minute while there is nothing to replay: that is
    // the primary's silence, not lag.
    #[test]
    fn a_standby_with_nothing_to_replay_is_not_behind() {
        let idle = standby_replica(standby(Some(0.0), 3_600.0));
        assert_eq!((idle.lag_seconds, idle.state.as_deref()), (Some(0.0), Some("streaming")));
        let behind = standby_replica(standby(Some(64.0 * 1024.0 * 1024.0), 95.0));
        assert_eq!(behind.lag_seconds, Some(95.0));
    }

    // recovery_min_apply_delay holds replay back on purpose: only what is beyond it is lag.
    #[test]
    fn a_delayed_standby_counts_only_the_lag_beyond_its_delay() {
        let delayed = standby_replica(Standby { delay: Some(3_600.0), ..standby(Some(1e9), 3_640.0) });
        assert_eq!((delayed.lag_seconds, delayed.delay_seconds), (Some(40.0), Some(3_600.0)));
    }

    // Without a WAL receiver a standby is stopped only when it is meant to stream; log shipping restores from an
    // archive and has no receiver at all.
    #[test]
    fn a_standby_without_a_receiver_is_stopped_only_when_it_should_stream() {
        let none = |configured| Standby {
            behind: None,
            receiver: None,
            streaming_configured: configured,
            ..standby(None, 600.0)
        };
        assert_eq!(standby_replica(none(Some(true))).state.as_deref(), Some("not receiving"));
        let shipping = standby_replica(none(Some(false)));
        assert_eq!((shipping.state.as_deref(), shipping.lag_seconds), (Some("restoring from archive"), None));
        assert_eq!(standby_replica(none(None)).state.as_deref(), Some("not streaming"));
    }

    // Without stats_reset (PostgreSQL 15+ until the first reset) how long the counts ran is unknown: no rate, but the
    // shares that need no time stay.
    #[test]
    fn sessions_without_a_known_age_have_no_rate() {
        let unknown = session_signals(400_000.0, None, Some(4.0e8), Some(4_000.0));
        assert!(unknown.iter().all(|s| s.key != "new-connections"));
        assert!(
            unknown.iter().any(|s| s.key == "session-length") && unknown.iter().any(|s| s.key == "sessions-abandoned")
        );
        let known = session_signals(400_000.0, Some(200_000.0), None, None);
        assert_eq!(known[0].value, 2.0);
    }

    #[test]
    fn slot_columns_follow_the_version() {
        assert!(!slots_sql(120_000).contains("wal_status"));
        assert!(slots_sql(130_000).contains("safe_wal_size") && !slots_sql(160_000).contains("inactive_since"));
        assert!(slots_sql(170_000).contains("inactive_since"));
    }
}
