//! Signals beyond time and reads: failed and cancelled runs, compile time and log volume per query (Query Store);
//! storage latency per file, logins, availability replicas; lock and latch waits per index; constraints SQL Server
//! doesn't trust. Each read is independent and cheap; a failure becomes a note, never a failed scan.

use std::collections::HashMap;

use culprit_core::{
    CollectorNote, ConstraintIssue, ConstraintKind, FileIo, FileKind, IndexContention, Pressure, Replica, ReplicaRole,
    Signal, SignalScope, SignalUnit, Snapshot, SnapshotOptions,
};

use crate::conn::{MsClient, map_error, query};
use crate::probe::Probe;
use crate::value::Rec;
use crate::workload::id_list;

/// Collect every signal into `snapshot`. Notes say what couldn't be read and why.
pub(crate) async fn collect(client: &mut MsClient, probe: &Probe, options: &SnapshotOptions, snapshot: &mut Snapshot) {
    let mut notes: Vec<(&str, String)> = Vec::new();
    let from_query_store = snapshot.workload.as_ref().is_some_and(|w| w.source == "Query Store");
    if from_query_store && let Some(workload) = snapshot.workload.as_mut() {
        if let Err(e) = attach_costs(client, options.window_in_minutes(), &mut workload.queries).await {
            notes.push(("query costs", map_error(e).message));
        }
        culprit_core::signals::fill_row_bytes(&mut workload.queries);
    }
    match contention(client, probe.major).await {
        Ok(by_index) => {
            for index in &mut snapshot.indexes {
                let key = (index.schema.to_lowercase(), index.table.to_lowercase(), index.name.to_lowercase());
                if let Some(c) = by_index.get(&key) {
                    index.contention = Some(c.clone());
                }
            }
        }
        Err(e) => notes.push(("index contention", map_error(e).message)),
    }
    match constraints(client).await {
        Ok(list) => snapshot.constraints = list,
        Err(e) => notes.push(("constraints", map_error(e).message)),
    }
    let pressure = snapshot.pressure.get_or_insert_with(Pressure::default);
    if probe.can_read_dmvs() {
        match files(client, probe.is_azure_sql_database()).await {
            Ok(files) => pressure.files = files,
            Err(e) => notes.push(("file latency", map_error(e).message)),
        }
        match logins(client).await {
            Ok(signals) => pressure.signals.extend(signals),
            Err(e) => notes.push(("logins", map_error(e).message)),
        }
    }
    pressure.replicas = replicas(client).await;
    snapshot
        .notes
        .extend(notes.into_iter().map(|(collector, message)| CollectorNote { collector: collector.into(), message }));
}

// ── per query ──────────────────────────────────────────────────────────────────────────────────────

/// Aborted (3) and exception (4) runs, their log bytes, and compile time from Query Store.
async fn attach_costs(
    client: &mut MsClient,
    window_minutes: u32,
    queries: &mut [culprit_core::QueryStat],
) -> tiberius::Result<()> {
    let ids: Vec<String> = queries.iter().map(|q| q.id.clone()).collect();
    if ids.is_empty() {
        return Ok(());
    }
    let list = id_list(&ids);
    let runs_sql = |log: &str| {
        format!(
            r#"
DECLARE @since datetimeoffset = DATEADD(minute, -{window_minutes}, SYSDATETIMEOFFSET());
SELECT p.query_id,
  SUM(CASE WHEN rs.execution_type = 3 THEN rs.count_executions ELSE 0 END) AS aborted,
  SUM(CASE WHEN rs.execution_type = 3 THEN rs.avg_duration * rs.count_executions ELSE 0 END) / 1000.0 AS aborted_ms,
  SUM(CASE WHEN rs.execution_type = 4 THEN rs.count_executions ELSE 0 END) AS failed,
  {log} AS log_bytes
FROM sys.query_store_runtime_stats rs
JOIN sys.query_store_runtime_stats_interval i ON i.runtime_stats_interval_id = rs.runtime_stats_interval_id
JOIN sys.query_store_plan p ON p.plan_id = rs.plan_id
WHERE i.end_time >= @since AND p.query_id IN ({list})
GROUP BY p.query_id
"#
        )
    };
    // avg_log_bytes_used arrived in SQL Server 2017.
    let (rows, with_log) = match query(client, &runs_sql("SUM(rs.avg_log_bytes_used * rs.count_executions)")).await {
        Ok(rows) => (rows, true),
        Err(_) => (query(client, &runs_sql("CAST(NULL AS float)")).await?, false),
    };
    let total_log = if with_log {
        let sql = format!(
            r#"
DECLARE @since datetimeoffset = DATEADD(minute, -{window_minutes}, SYSDATETIMEOFFSET());
SELECT SUM(rs.avg_log_bytes_used * rs.count_executions) AS log_bytes
FROM sys.query_store_runtime_stats rs
JOIN sys.query_store_runtime_stats_interval i ON i.runtime_stats_interval_id = rs.runtime_stats_interval_id
WHERE i.end_time >= @since
"#
        );
        query(client, &sql).await.ok().and_then(|r| r.into_iter().next()).and_then(|r| r.f64("log_bytes"))
    } else {
        None
    };
    let by_id: HashMap<String, Rec> = rows.into_iter().map(|r| (r.string("query_id"), r)).collect();

    // Compile statistics cover the query's whole life in Query Store; runs over the same life turn them into a
    // per-run cost that applies to the window's runs.
    let compile_sql = format!(
        r#"
SELECT q.query_id, q.count_compiles, q.avg_compile_duration / 1000.0 AS avg_compile_ms,
  (SELECT SUM(rs.count_executions) FROM sys.query_store_runtime_stats rs
     JOIN sys.query_store_plan p ON p.plan_id = rs.plan_id WHERE p.query_id = q.query_id) AS lifetime_runs
FROM sys.query_store_query q
WHERE q.query_id IN ({list})
"#
    );
    let compiles: HashMap<String, Rec> = match query(client, &compile_sql).await {
        Ok(rows) => rows.into_iter().map(|r| (r.string("query_id"), r)).collect(),
        Err(_) => HashMap::new(),
    };

    for q in queries.iter_mut() {
        let mut costs = q.costs.take().unwrap_or_default();
        if let Some(r) = by_id.get(&q.id) {
            let aborted = r.f64_or_zero("aborted");
            costs.aborted = Some(aborted);
            costs.failed = Some(r.f64_or_zero("failed"));
            costs.aborted_mean_ms = (aborted > 0.0).then(|| r.f64_or_zero("aborted_ms") / aborted);
            costs.log_bytes = r.f64("log_bytes").filter(|b| *b > 0.0);
            costs.log_share = match (costs.log_bytes, total_log) {
                (Some(own), Some(all)) if all > 0.0 => Some((own / all).min(1.0)),
                _ => None,
            };
        }
        if let Some(r) = compiles.get(&q.id) {
            let count = r.f64_or_zero("count_compiles");
            let avg = r.f64_or_zero("avg_compile_ms");
            let lifetime_runs = r.f64("lifetime_runs").filter(|n| *n > 0.0);
            if count > 0.0
                && let Some(lifetime_runs) = lifetime_runs
            {
                let per_run = count * avg / lifetime_runs;
                costs.plan_ms = Some(per_run * q.executions);
                costs.plans = Some((count / lifetime_runs * q.executions).round().min(q.executions.max(count)));
            }
        }
        q.costs = (!costs.is_empty()).then_some(costs);
    }
    Ok(())
}

// ── per index ──────────────────────────────────────────────────────────────────────────────────────

type IndexKey = (String, String, String);

fn contention_sql(sequential_option: bool) -> String {
    let option =
        if sequential_option { "MAX(CAST(i.optimize_for_sequential_key AS int))" } else { "CAST(NULL AS int)" };
    format!(
        r#"
SELECT OBJECT_SCHEMA_NAME(s.object_id) AS schema_name, OBJECT_NAME(s.object_id) AS table_name, i.name AS index_name,
  SUM(s.row_lock_wait_count) AS row_lock_waits, SUM(s.row_lock_wait_in_ms) AS row_lock_ms,
  SUM(s.page_lock_wait_count) AS page_lock_waits, SUM(s.page_lock_wait_in_ms) AS page_lock_ms,
  SUM(s.page_latch_wait_count) AS latch_waits, SUM(s.page_latch_wait_in_ms) AS latch_ms,
  SUM(s.index_lock_promotion_count) AS escalations, SUM(s.index_lock_promotion_attempt_count) AS escalation_attempts,
  SUM(s.leaf_insert_count) AS leaf_inserts,
  MAX(CASE WHEN c.is_identity = 1 OR d.definition LIKE '%NEXT VALUE FOR%' OR d.definition LIKE '%newsequentialid%'
            OR d.definition LIKE '%getdate()%' OR d.definition LIKE '%getutcdate()%' OR d.definition LIKE '%sysdatetime%'
            OR d.definition LIKE '%sysutcdatetime%' THEN 1 ELSE 0 END) AS sequential,
  {option} AS optimize_sequential
FROM sys.dm_db_index_operational_stats(DB_ID(), NULL, NULL, NULL) s
JOIN sys.indexes i ON i.object_id = s.object_id AND i.index_id = s.index_id
JOIN sys.objects o ON o.object_id = s.object_id AND o.is_ms_shipped = 0
LEFT JOIN sys.index_columns ic ON ic.object_id = i.object_id AND ic.index_id = i.index_id AND ic.key_ordinal = 1
LEFT JOIN sys.columns c ON c.object_id = ic.object_id AND c.column_id = ic.column_id
LEFT JOIN sys.default_constraints d ON d.parent_object_id = c.object_id AND d.parent_column_id = c.column_id
WHERE i.name IS NOT NULL
GROUP BY s.object_id, i.name
HAVING SUM(s.row_lock_wait_count + s.page_lock_wait_count + s.page_latch_wait_count + s.index_lock_promotion_count
  + s.index_lock_promotion_attempt_count) > 0
"#
    )
}

/// Row and page lock waits, page latch waits and escalations (and attempts) per index since its metadata was loaded.
async fn contention(client: &mut MsClient, major: u32) -> tiberius::Result<HashMap<IndexKey, IndexContention>> {
    // OPTIMIZE_FOR_SEQUENTIAL_KEY is a SQL Server 2019 (15) option.
    let rows = match query(client, &contention_sql(major >= 15)).await {
        Ok(rows) => rows,
        Err(_) if major >= 15 => query(client, &contention_sql(false)).await?,
        Err(e) => return Err(e),
    };
    Ok(rows
        .iter()
        .map(|r| {
            let key = (
                r.string("schema_name").to_lowercase(),
                r.string("table_name").to_lowercase(),
                r.string("index_name").to_lowercase(),
            );
            let contention = IndexContention {
                row_lock_waits: r.f64_or_zero("row_lock_waits"),
                row_lock_wait_ms: r.f64_or_zero("row_lock_ms"),
                page_lock_waits: r.f64_or_zero("page_lock_waits"),
                page_lock_wait_ms: r.f64_or_zero("page_lock_ms"),
                page_latch_waits: r.f64_or_zero("latch_waits"),
                page_latch_wait_ms: r.f64_or_zero("latch_ms"),
                lock_escalations: r.f64_or_zero("escalations"),
                lock_escalation_attempts: r.f64_or_zero("escalation_attempts"),
                leaf_inserts: r.f64("leaf_inserts"),
                sequential_key: r.f64_or_zero("sequential") > 0.0,
                optimize_for_sequential_key: r.f64("optimize_sequential").map(|v| v > 0.0),
            };
            (key, contention)
        })
        .collect())
}

/// Foreign keys and check constraints loaded or re-enabled WITH NOCHECK, or switched off. NOT FOR REPLICATION foreign
/// keys are always untrusted and checking them can't change that, so they're left out.
async fn constraints(client: &mut MsClient) -> tiberius::Result<Vec<ConstraintIssue>> {
    let sql = r#"
SELECT OBJECT_SCHEMA_NAME(parent_object_id) AS schema_name, OBJECT_NAME(parent_object_id) AS table_name, name,
  'foreign-key' AS kind, CAST(is_disabled AS int) AS disabled
FROM sys.foreign_keys
WHERE (is_not_trusted = 1 OR is_disabled = 1) AND is_not_for_replication = 0 AND is_ms_shipped = 0
UNION ALL
SELECT OBJECT_SCHEMA_NAME(parent_object_id), OBJECT_NAME(parent_object_id), name, 'check', CAST(is_disabled AS int)
FROM sys.check_constraints
WHERE (is_not_trusted = 1 OR is_disabled = 1) AND is_not_for_replication = 0 AND is_ms_shipped = 0
"#;
    Ok(query(client, sql)
        .await?
        .iter()
        .map(|r| ConstraintIssue {
            schema: r.string("schema_name"),
            table: r.string("table_name"),
            name: r.string("name"),
            kind: if r.string("kind") == "check" { ConstraintKind::Check } else { ConstraintKind::ForeignKey },
            disabled: r.f64_or_zero("disabled") > 0.0,
        })
        .collect())
}

// ── per server ─────────────────────────────────────────────────────────────────────────────────────

fn file_io(r: &Rec, database: Option<String>, kind: FileKind) -> FileIo {
    FileIo {
        name: r.string("file_name"),
        kind,
        database,
        reads: r.f64_or_zero("reads"),
        read_ms: r.f64("read_ms"),
        read_bytes: r.f64("read_bytes"),
        writes: r.f64_or_zero("writes"),
        write_ms: r.f64("write_ms"),
        write_bytes: r.f64("write_bytes"),
    }
}

/// Read and write stalls per file of this database and tempdb, since SQL Server started.
async fn files(client: &mut MsClient, azure: bool) -> tiberius::Result<Vec<FileIo>> {
    const COLUMNS: &str =
        "vfs.num_of_reads AS reads, vfs.io_stall_read_ms AS read_ms, vfs.num_of_bytes_read AS read_bytes,
  vfs.num_of_writes AS writes, vfs.io_stall_write_ms AS write_ms, vfs.num_of_bytes_written AS write_bytes";
    let server = format!(
        "SELECT DB_NAME(vfs.database_id) AS database_name, vfs.database_id, mf.name AS file_name, mf.type_desc, {COLUMNS}
FROM sys.dm_io_virtual_file_stats(NULL, NULL) vfs
JOIN sys.master_files mf ON mf.database_id = vfs.database_id AND mf.file_id = vfs.file_id
WHERE vfs.database_id IN (DB_ID(), 2)"
    );
    let database = format!(
        "SELECT DB_NAME() AS database_name, DB_ID() AS database_id, df.name AS file_name, df.type_desc, {COLUMNS}
FROM sys.dm_io_virtual_file_stats(DB_ID(), NULL) vfs
JOIN sys.database_files df ON df.file_id = vfs.file_id
WHERE vfs.database_id = DB_ID()"
    );
    // Azure SQL Database has no sys.master_files: its own files only. Hyperscale also returns its page servers' files
    // under database_id 0 with the same file ids, and those would pass for this database's data and log files.
    let rows = if azure {
        query(client, &database).await?
    } else {
        match query(client, &server).await {
            Ok(rows) => rows,
            Err(_) => query(client, &database).await?,
        }
    };
    let mut out: Vec<FileIo> = rows
        .iter()
        .map(|r| {
            let tempdb = r.f64("database_id") == Some(2.0);
            let kind = match (r.string("type_desc").as_str(), tempdb) {
                ("LOG", _) => FileKind::Log,
                (_, true) => FileKind::Temp,
                ("ROWS", false) => FileKind::Data,
                _ => FileKind::Other,
            };
            file_io(r, r.str("database_name"), kind)
        })
        .filter(|f| f.reads + f.writes > 0.0)
        .collect();
    out.sort_by(|a, b| {
        let time = |f: &FileIo| f.read_ms.unwrap_or(0.0) + f.write_ms.unwrap_or(0.0);
        time(b).total_cmp(&time(a))
    });
    out.truncate(24);
    Ok(out)
}

/// New logins per second since startup, and how many connection uses a pool served (Connection Reset).
async fn logins(client: &mut MsClient) -> tiberius::Result<Vec<Signal>> {
    let sql = r#"
SELECT RTRIM(pc.counter_name) AS counter, pc.cntr_value AS value,
  DATEDIFF(second, si.sqlserver_start_time, SYSDATETIME()) AS uptime_s
FROM sys.dm_os_performance_counters pc CROSS JOIN sys.dm_os_sys_info si
WHERE pc.object_name LIKE '%General Statistics%' AND pc.counter_name IN ('Logins/sec', 'Connection Reset/sec')
"#;
    let rows = query(client, sql).await?;
    let get = |name: &str| rows.iter().find(|r| r.string("counter") == name).and_then(|r| r.f64("value"));
    let uptime = rows.first().and_then(|r| r.f64("uptime_s")).filter(|s| *s > 0.0);
    let mut out = Vec::new();
    if let (Some(logins), Some(seconds)) = (get("Logins/sec"), uptime) {
        out.push(
            Signal::new(
                "new-connections",
                "New connections",
                logins / seconds,
                SignalUnit::PerSecond,
                SignalScope::SinceStartup,
            )
            .graded("under 5 a second: pooled connections log in once and are reused", 5.0, 25.0, true),
        );
        if let Some(resets) = get("Connection Reset/sec")
            && logins + resets >= 1_000.0
        {
            out.push(
                Signal::new(
                    "pooled-reuse",
                    "Connections served from a pool",
                    resets / (logins + resets) * 100.0,
                    SignalUnit::Percent,
                    SignalScope::SinceStartup,
                )
                .graded("above 90% when the application pools connections", 90.0, 50.0, false),
            );
        }
    }
    Ok(out)
}

/// Availability group replicas of this database, and Azure SQL geo-replication links. Neither exists on most servers.
async fn replicas(client: &mut MsClient) -> Vec<Replica> {
    let ag = |lag: &str| {
        format!(
            r#"
SELECT ar.replica_server_name AS name, CAST(drs.is_local AS int) AS is_local, CAST(drs.is_primary_replica AS int) AS is_primary,
  drs.synchronization_state_desc AS state, ar.availability_mode_desc AS sync,
  drs.log_send_queue_size * 1024.0 AS unsent_bytes, drs.redo_queue_size * 1024.0 AS redo_bytes, {lag} AS lag_s
FROM sys.dm_hadr_database_replica_states drs
JOIN sys.availability_replicas ar ON ar.replica_id = drs.replica_id
WHERE drs.database_id = DB_ID()
"#
        )
    };
    let rows = match query(client, &ag("drs.secondary_lag_seconds")).await {
        Ok(rows) => rows,
        Err(_) => query(client, &ag("CAST(NULL AS float)")).await.unwrap_or_default(),
    };
    let mut out: Vec<Replica> = rows
        .iter()
        .filter(|r| !(r.f64_or_zero("is_local") > 0.0 && r.f64_or_zero("is_primary") > 0.0))
        .map(|r| Replica {
            name: r.string("name"),
            role: if r.f64_or_zero("is_local") > 0.0 { ReplicaRole::ThisServer } else { ReplicaRole::Replica },
            state: r.str("state"),
            sync: r.str("sync").map(|s| s.replace('_', " ").to_lowercase()),
            lag_seconds: r.f64("lag_s"),
            lag_bytes: r.f64("redo_bytes"),
            unsent_bytes: r.f64("unsent_bytes"),
            error: None,
        })
        .collect();
    let geo = "SELECT partner_server + '/' + partner_database AS name, replication_state_desc AS state, role_desc,
  replication_lag_sec AS lag_s FROM sys.dm_geo_replication_link_status";
    if let Ok(rows) = query(client, geo).await {
        out.extend(rows.iter().map(|r| Replica {
            name: r.string("name"),
            role: if r.string("role_desc").eq_ignore_ascii_case("SECONDARY") {
                ReplicaRole::ThisServer
            } else {
                ReplicaRole::Replica
            },
            state: r.str("state"),
            sync: Some("asynchronous".into()),
            lag_seconds: r.f64("lag_s"),
            lag_bytes: None,
            unsent_bytes: None,
            error: None,
        }));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contention_sql_only_names_the_2019_option_when_asked() {
        assert!(contention_sql(true).contains("optimize_for_sequential_key"));
        assert!(!contention_sql(false).contains("optimize_for_sequential_key"));
    }
}
