use culprit_core::{
    ColumnInfo, ForeignKey, IndexStat, MissingIndex, ResourceSample, Setting, StatisticsInfo, TableStat, WaitStat,
};

use crate::conn::{MsClient, query};
use crate::value::Rec;
use crate::workload::utc_local_iso;

const TABLES_SQL: &str = r#"
SELECT s.name AS schema_name, t.name AS table_name,
  SUM(CASE WHEN ps.index_id IN (0, 1) THEN ps.row_count ELSE 0 END) AS row_count,
  SUM(ps.reserved_page_count) * 8192.0 AS total_bytes,
  SUM(CASE WHEN ps.index_id IN (0, 1) THEN ps.reserved_page_count ELSE 0 END) * 8192.0 AS data_bytes,
  SUM(CASE WHEN ps.index_id > 1 THEN ps.reserved_page_count ELSE 0 END) * 8192.0 AS index_bytes,
  SUM(CASE WHEN ps.index_id IN (0, 1) THEN ps.in_row_reserved_page_count ELSE 0 END) * 8192.0 AS in_row_bytes,
  SUM(CASE WHEN ps.index_id IN (0, 1) THEN ps.lob_reserved_page_count ELSE 0 END) * 8192.0 AS lob_bytes,
  SUM(CASE WHEN ps.index_id IN (0, 1) THEN ps.row_overflow_reserved_page_count ELSE 0 END) * 8192.0 AS overflow_bytes,
  MAX(CASE WHEN ps.index_id = 0 THEN 1 ELSE 0 END) AS is_heap,
  MAX(CAST(OBJECTPROPERTY(t.object_id, 'TableHasPrimaryKey') AS int)) AS has_primary_key
FROM sys.tables t
JOIN sys.schemas s ON s.schema_id = t.schema_id
LEFT JOIN sys.dm_db_partition_stats ps ON ps.object_id = t.object_id
WHERE t.is_ms_shipped = 0
GROUP BY s.name, t.name
"#;

/// Catalog-only variant for logins without VIEW DATABASE STATE: row counts and sizes from sys.partitions. A partition
/// has one allocation unit per kind of page (in-row, LOB, row-overflow), so its rows are counted on the in-row one only.
const TABLES_CATALOG_SQL: &str = r#"
SELECT s.name AS schema_name, t.name AS table_name,
  SUM(CASE WHEN p.index_id IN (0, 1) AND au.type = 1 THEN p.rows ELSE 0 END) AS row_count,
  SUM(au.total_pages) * 8192.0 AS total_bytes,
  SUM(CASE WHEN p.index_id IN (0, 1) THEN au.total_pages ELSE 0 END) * 8192.0 AS data_bytes,
  SUM(CASE WHEN p.index_id > 1 THEN au.total_pages ELSE 0 END) * 8192.0 AS index_bytes,
  SUM(CASE WHEN p.index_id IN (0, 1) AND au.type = 1 THEN au.total_pages ELSE 0 END) * 8192.0 AS in_row_bytes,
  SUM(CASE WHEN p.index_id IN (0, 1) AND au.type = 2 THEN au.total_pages ELSE 0 END) * 8192.0 AS lob_bytes,
  SUM(CASE WHEN p.index_id IN (0, 1) AND au.type = 3 THEN au.total_pages ELSE 0 END) * 8192.0 AS overflow_bytes,
  MAX(CASE WHEN p.index_id = 0 THEN 1 ELSE 0 END) AS is_heap,
  MAX(CAST(OBJECTPROPERTY(t.object_id, 'TableHasPrimaryKey') AS int)) AS has_primary_key
FROM sys.tables t
JOIN sys.schemas s ON s.schema_id = t.schema_id
LEFT JOIN sys.partitions p ON p.object_id = t.object_id
LEFT JOIN sys.allocation_units au ON au.container_id = p.partition_id
WHERE t.is_ms_shipped = 0
GROUP BY s.name, t.name
"#;

fn indexes_sql(with_usage: bool) -> String {
    let (usage_cols, usage_join) = if with_usage {
        (
            format!(
                ",
  us.user_seeks, us.user_scans, us.user_lookups, us.user_updates,
  {last_read} AS last_read,
  (SELECT SUM(ps.reserved_page_count) * 8192.0 FROM sys.dm_db_partition_stats ps WHERE ps.object_id = i.object_id AND ps.index_id = i.index_id) AS size_bytes",
                last_read = utc_local_iso(
                    "(SELECT MAX(v) FROM (VALUES (us.last_user_seek), (us.last_user_scan), (us.last_user_lookup)) AS x(v))"
                )
            ),
            "LEFT JOIN sys.dm_db_index_usage_stats us ON us.database_id = DB_ID() AND us.object_id = i.object_id AND us.index_id = i.index_id",
        )
    } else {
        (
            ",
  (SELECT SUM(au.total_pages) * 8192.0 FROM sys.partitions p JOIN sys.allocation_units au ON au.container_id = p.partition_id WHERE p.object_id = i.object_id AND p.index_id = i.index_id) AS size_bytes".to_string(),
            "",
        )
    };
    format!(
        r#"
SELECT s.name AS schema_name, t.name AS table_name, i.name AS index_name, i.index_id, i.type, i.type_desc,
  i.is_unique, i.is_primary_key, i.is_disabled, i.filter_definition,
  (SELECT STRING_AGG(CAST(c.name + CASE WHEN ic.is_descending_key = 1 THEN ' DESC' ELSE '' END AS nvarchar(max)), CHAR(31))
            WITHIN GROUP (ORDER BY ic.key_ordinal)
   FROM sys.index_columns ic JOIN sys.columns c ON c.object_id = ic.object_id AND c.column_id = ic.column_id
   WHERE ic.object_id = i.object_id AND ic.index_id = i.index_id AND ic.key_ordinal > 0) AS key_columns,
  (SELECT STRING_AGG(CAST(c.name AS nvarchar(max)), CHAR(31)) WITHIN GROUP (ORDER BY ic.index_column_id)
   FROM sys.index_columns ic JOIN sys.columns c ON c.object_id = ic.object_id AND c.column_id = ic.column_id
   WHERE ic.object_id = i.object_id AND ic.index_id = i.index_id AND ic.is_included_column = 1) AS include_columns{usage_cols}
FROM sys.indexes i
JOIN sys.tables t ON t.object_id = i.object_id
JOIN sys.schemas s ON s.schema_id = t.schema_id
{usage_join}
WHERE t.is_ms_shipped = 0 AND i.type > 0 AND i.is_hypothetical = 0
"#
    )
}

const COLUMNS_SQL: &str = r#"
SELECT s.name AS schema_name, t.name AS table_name, c.name AS column_name, c.column_id AS ordinal,
  ty.name AS type_name, c.max_length, c.precision, c.scale, c.is_nullable, c.is_identity, c.is_computed,
  dc.definition AS default_value
FROM sys.columns c
JOIN sys.tables t ON t.object_id = c.object_id
JOIN sys.schemas s ON s.schema_id = t.schema_id
JOIN sys.types ty ON ty.user_type_id = c.user_type_id
LEFT JOIN sys.default_constraints dc ON dc.object_id = c.default_object_id
WHERE t.is_ms_shipped = 0
ORDER BY s.name, t.name, c.column_id
"#;

const FOREIGN_KEYS_SQL: &str = r#"
SELECT fk.name AS fk_name, s.name AS schema_name, t.name AS table_name, rs.name AS ref_schema, rt.name AS ref_table,
  STRING_AGG(CAST(c.name AS nvarchar(max)), CHAR(31)) WITHIN GROUP (ORDER BY fkc.constraint_column_id) AS columns,
  STRING_AGG(CAST(rc.name AS nvarchar(max)), CHAR(31)) WITHIN GROUP (ORDER BY fkc.constraint_column_id) AS ref_columns
FROM sys.foreign_keys fk
JOIN sys.foreign_key_columns fkc ON fkc.constraint_object_id = fk.object_id
JOIN sys.tables t ON t.object_id = fk.parent_object_id
JOIN sys.schemas s ON s.schema_id = t.schema_id
JOIN sys.tables rt ON rt.object_id = fk.referenced_object_id
JOIN sys.schemas rs ON rs.schema_id = rt.schema_id
JOIN sys.columns c ON c.object_id = fkc.parent_object_id AND c.column_id = fkc.parent_column_id
JOIN sys.columns rc ON rc.object_id = fkc.referenced_object_id AND rc.column_id = fkc.referenced_column_id
GROUP BY fk.name, s.name, t.name, rs.name, rt.name
"#;

fn missing_indexes_sql() -> String {
    format!(
        r#"
SELECT s.name AS schema_name, t.name AS table_name,
  mid.equality_columns, mid.inequality_columns, mid.included_columns,
  migs.user_seeks, migs.user_scans, migs.avg_total_user_cost, migs.avg_user_impact,
  {last_used} AS last_used,
  migs.group_handle
FROM sys.dm_db_missing_index_details mid
JOIN sys.dm_db_missing_index_groups mig ON mig.index_handle = mid.index_handle
JOIN sys.dm_db_missing_index_group_stats migs ON migs.group_handle = mig.index_group_handle
JOIN sys.tables t ON t.object_id = mid.object_id
JOIN sys.schemas s ON s.schema_id = t.schema_id
WHERE mid.database_id = DB_ID() AND migs.user_seeks + migs.user_scans > 0
"#,
        last_used = utc_local_iso("(SELECT MAX(v) FROM (VALUES (migs.last_user_seek), (migs.last_user_scan)) AS x(v))")
    )
}

const MISSING_INDEX_QUERIES_SQL: &str = r#"
SELECT DISTINCT q.group_handle, CONVERT(varchar(20), q.query_hash, 1) AS query_hash
FROM sys.dm_db_missing_index_group_stats_query q
"#;

fn statistics_sql() -> String {
    format!(
        r#"
SELECT s.name AS schema_name, t.name AS table_name, st.name AS stats_name,
  {last_updated} AS last_updated, sp.rows, sp.rows_sampled, sp.modification_counter, st.auto_created
FROM sys.stats st
JOIN sys.tables t ON t.object_id = st.object_id
JOIN sys.schemas s ON s.schema_id = t.schema_id
CROSS APPLY sys.dm_db_stats_properties(st.object_id, st.stats_id) sp
WHERE t.is_ms_shipped = 0
"#,
        last_updated = utc_local_iso("sp.last_updated")
    )
}

const RESOURCES_SQL: &str = r#"
SELECT CONVERT(varchar(19), CAST(end_time AS datetime2(0)), 126) + 'Z' AS at,
  avg_cpu_percent, avg_data_io_percent, avg_log_write_percent, avg_memory_usage_percent, max_worker_percent
FROM sys.dm_db_resource_stats
ORDER BY end_time
"#;

const DATABASE_SETTINGS_SQL: &str = r#"
SELECT compatibility_level, is_read_committed_snapshot_on, snapshot_isolation_state_desc,
  is_auto_create_stats_on, is_auto_update_stats_on, is_auto_update_stats_async_on,
  is_parameterization_forced, is_query_store_on, page_verify_option_desc, recovery_model_desc,
  target_recovery_time_in_seconds
FROM sys.databases WHERE database_id = DB_ID()
"#;

const SCOPED_CONFIGURATIONS_SQL: &str = "SELECT * FROM sys.database_scoped_configurations";

/// This database's role where it is replicated (an availability group, Azure SQL's replicas): a readable secondary
/// counts index usage for the reads run on it, while Query Store's history is the primary's.
const REPLICA_ROLE_SQL: &str = "SELECT CAST(rs.is_primary_replica AS int) AS is_primary
FROM sys.dm_hadr_database_replica_states rs WHERE rs.database_id = DB_ID() AND rs.is_local = 1";

const SERVER_CONFIGURATIONS_SQL: &str = r#"
SELECT name, CAST(value_in_use AS nvarchar(64)) AS value
FROM sys.configurations
WHERE name IN ('cost threshold for parallelism', 'max degree of parallelism', 'optimize for ad hoc workloads', 'max server memory (MB)')
"#;

/// Values a fresh SQL Server install ships with, so checks can tell untouched settings from deliberate ones.
const SERVER_DEFAULTS: &[(&str, &str)] = &[
    ("cost threshold for parallelism", "5"),
    ("max degree of parallelism", "0"),
    ("optimize for ad hoc workloads", "0"),
    ("max server memory (MB)", "2147483647"),
];

/// Waits that accumulate while SQL Server is idle or doing background work; they say nothing about query performance.
pub(crate) const BENIGN_WAITS: &[&str] = &[
    "BROKER_EVENTHANDLER",
    "BROKER_RECEIVE_WAITFOR",
    "BROKER_TASK_STOP",
    "BROKER_TO_FLUSH",
    "BROKER_TRANSMITTER",
    "CHECKPOINT_QUEUE",
    "CHKPT",
    "CLR_AUTO_EVENT",
    "CLR_MANUAL_EVENT",
    "CLR_SEMAPHORE",
    "DBMIRROR_DBM_EVENT",
    "DBMIRROR_EVENTS_QUEUE",
    "DBMIRROR_WORKER_QUEUE",
    "DBMIRRORING_CMD",
    "DIRTY_PAGE_POLL",
    "DISPATCHER_QUEUE_SEMAPHORE",
    "EXECSYNC",
    "FSAGENT",
    "FT_IFTS_SCHEDULER_IDLE_WAIT",
    "FT_IFTSHC_MUTEX",
    "HADR_CLUSAPI_CALL",
    "HADR_FILESTREAM_IOMGR_IOCOMPLETION",
    "HADR_LOGCAPTURE_WAIT",
    "HADR_NOTIFICATION_DEQUEUE",
    "HADR_TIMER_TASK",
    "HADR_WORK_QUEUE",
    "KSOURCE_WAKEUP",
    "LAZYWRITER_SLEEP",
    "LOGMGR_QUEUE",
    "MEMORY_ALLOCATION_EXT",
    "ONDEMAND_TASK_QUEUE",
    "PARALLEL_REDO_DRAIN_WORKER",
    "PARALLEL_REDO_LOG_CACHE",
    "PARALLEL_REDO_TRAN_LIST",
    "PARALLEL_REDO_WORKER_SYNC",
    "PARALLEL_REDO_WORKER_WAIT_WORK",
    "PREEMPTIVE_OS_FLUSHFILEBUFFERS",
    "PREEMPTIVE_XE_GETTARGETSTATE",
    "PVS_PREALLOCATE",
    "PWAIT_ALL_COMPONENTS_INITIALIZED",
    "PWAIT_DIRECTLOGCONSUMER_GETNEXT",
    "PWAIT_EXTENSIBILITY_CLEANUP_TASK",
    "QDS_PERSIST_TASK_MAIN_LOOP_SLEEP",
    "QDS_ASYNC_QUEUE",
    "QDS_CLEANUP_STALE_QUERIES_TASK_MAIN_LOOP_SLEEP",
    "QDS_SHUTDOWN_QUEUE",
    "REDO_THREAD_PENDING_WORK",
    "REQUEST_FOR_DEADLOCK_SEARCH",
    "RESOURCE_QUEUE",
    "SERVER_IDLE_CHECK",
    "SLEEP_BPOOL_FLUSH",
    "SLEEP_DBSTARTUP",
    "SLEEP_DCOMSTARTUP",
    "SLEEP_MASTERDBREADY",
    "SLEEP_MASTERMDREADY",
    "SLEEP_MASTERUPGRADED",
    "SLEEP_MSDBSTARTUP",
    "SLEEP_SYSTEMTASK",
    "SLEEP_TASK",
    "SLEEP_TEMPDBSTARTUP",
    "SNI_HTTP_ACCEPT",
    "SOS_WORK_DISPATCHER",
    "SP_SERVER_DIAGNOSTICS_SLEEP",
    "SQLTRACE_BUFFER_FLUSH",
    "SQLTRACE_INCREMENTAL_FLUSH_SLEEP",
    "SQLTRACE_WAIT_ENTRIES",
    "UCS_SESSION_REGISTRATION",
    "VDI_CLIENT_OTHER",
    "WAIT_FOR_RESULTS",
    "WAITFOR",
    "WAITFOR_TASKSHUTDOWN",
    "WAIT_XTP_RECOVERY",
    "WAIT_XTP_HOST_WAIT",
    "WAIT_XTP_OFFLINE_CKPT_NEW_LOG",
    "WAIT_XTP_CKPT_CLOSE",
    "XE_DISPATCHER_JOIN",
    "XE_DISPATCHER_WAIT",
    "XE_TIMER_EVENT",
    "XE_LIVE_TARGET_TVF",
    "SOS_SCHEDULER_YIELD_IDLE",
    "BROKER_FORWARDER",
    "HADR_DATABASE_FLOW_CONTROL_IDLE",
];

pub(crate) fn wait_type_category(wait_type: &str) -> &'static str {
    let w = wait_type.to_ascii_uppercase();
    if w.starts_with("LCK_") {
        return "Lock";
    }
    if w.starts_with("PAGEIOLATCH") || w == "IO_COMPLETION" || w == "ASYNC_IO_COMPLETION" || w.starts_with("BACKUPIO") {
        return "IO";
    }
    if w.starts_with("PAGELATCH") || w.starts_with("LATCH_") {
        return "Latch";
    }
    if w == "WRITELOG"
        || w.starts_with("LOG_RATE_GOVERNOR")
        || w.contains("LOG_RATE_GOVERNOR")
        || w.starts_with("LOGBUFFER")
        || w.starts_with("HADR_SYNC_COMMIT")
    {
        return "Log";
    }
    if w == "SOS_SCHEDULER_YIELD" || w == "THREADPOOL" || w.starts_with("RESOURCE_GOVERNOR") {
        return "CPU";
    }
    // HT* and BMP* are batch-mode hash and bitmap work shared across a parallel query's threads.
    if w.starts_with("CXPACKET")
        || w.starts_with("CXCONSUMER")
        || w.starts_with("CXSYNC")
        || w.starts_with("EXCHANGE")
        || w.starts_with("HT")
        || w.starts_with("BMP")
    {
        return "Parallelism";
    }
    if w.starts_with("RESOURCE_SEMAPHORE") || w == "CMEMTHREAD" {
        return "Memory";
    }
    if w == "ASYNC_NETWORK_IO" || w.starts_with("NET_") {
        return "Network";
    }
    "Other"
}

fn waits_sql(azure_sql_database: bool) -> String {
    let view = if azure_sql_database { "sys.dm_db_wait_stats" } else { "sys.dm_os_wait_stats" };
    let excluded: Vec<String> = BENIGN_WAITS.iter().map(|w| format!("N'{w}'")).collect();
    format!(
        "SELECT wait_type, wait_time_ms, waiting_tasks_count FROM {view} WHERE wait_time_ms > 0 AND wait_type NOT IN ({}) AND wait_type NOT LIKE N'SLEEP[_]%'",
        excluded.join(", ")
    )
}

pub(crate) async fn tables(client: &mut MsClient, with_dmvs: bool) -> tiberius::Result<Vec<TableStat>> {
    let rows = match with_dmvs {
        true => query(client, TABLES_SQL).await?,
        false => query(client, TABLES_CATALOG_SQL).await?,
    };
    Ok(rows.iter().map(table_of).collect())
}

fn table_of(r: &Rec) -> TableStat {
    TableStat {
        schema: r.string("schema_name"),
        name: r.string("table_name"),
        rows: r.f64("row_count"),
        total_bytes: r.f64("total_bytes"),
        data_bytes: r.f64("data_bytes"),
        index_bytes: r.f64("index_bytes"),
        has_primary_key: r.bool("has_primary_key"),
        is_heap: Some(r.bool("is_heap")),
        seq_scans: None,
        seq_rows_read: None,
        index_scans: None,
        inserts: None,
        updates: None,
        deletes: None,
        dead_rows: None,
        modifications_since_analyze: None,
        last_vacuum: None,
        last_analyze: None,
        forwarded_records: None,
        forwarded_fetches: None,
        xid_age: None,
        storage: None,
        in_row_bytes: r.f64("in_row_bytes"),
        lob_bytes: r.f64("lob_bytes"),
        overflow_bytes: r.f64("overflow_bytes"),
        partition_of: None,
        counters_since: None,
    }
}

/// Indexes with usage counters when the login can read them; `has_usage` reports which variant ran.
pub(crate) async fn indexes(client: &mut MsClient, with_dmvs: bool) -> tiberius::Result<(Vec<IndexStat>, bool)> {
    if with_dmvs && let Ok(rows) = query(client, &indexes_sql(true)).await {
        return Ok((rows.iter().map(|r| index_of(r, true)).collect(), true));
    }
    let rows = query(client, &indexes_sql(false)).await?;
    Ok((rows.iter().map(|r| index_of(r, false)).collect(), false))
}

fn index_of(r: &Rec, with_usage: bool) -> IndexStat {
    let index_type = r.f64("type").unwrap_or(0.0) as i64;
    // dm_db_index_usage_stats adds an index's row the first time anything uses it after the database comes online, so
    // no row is a count of zero since the counters started, not an unknown: the unused-index check rests on exactly
    // these indexes. Counters that started over without a restart (the database taken offline, restored, failed over)
    // are told apart by the scans (`counters_since`).
    let usage = |name: &str| with_usage.then(|| r.f64_or_zero(name));
    let (seeks, scans, lookups) = (usage("user_seeks"), usage("user_scans"), usage("user_lookups"));
    let reads = with_usage.then(|| seeks.unwrap_or(0.0) + scans.unwrap_or(0.0) + lookups.unwrap_or(0.0));
    IndexStat {
        schema: r.string("schema_name"),
        table: r.string("table_name"),
        name: r.string("index_name"),
        kind: r.string("type_desc").to_ascii_lowercase(),
        columns: r.list("key_columns"),
        include: r.list("include_columns"),
        filter: r.str("filter_definition"),
        is_unique: r.bool("is_unique"),
        is_primary_key: r.bool("is_primary_key"),
        // 1 = clustered rowstore, 5 = clustered columnstore
        is_clustered: index_type == 1 || index_type == 5,
        is_disabled: r.bool("is_disabled"),
        size_bytes: r.f64("size_bytes"),
        reads,
        writes: usage("user_updates"),
        seeks,
        scans,
        lookups,
        last_read: r.str("last_read"),
        definition: None,
        contention: None,
        created_at_latest: None,
        partition_of: None,
        opclasses: None,
        counters_since: None,
    }
}

/// `varchar(20)`, `nvarchar(max)`, `decimal(12,2)`, `datetime2(3)`: the type as it is declared.
fn declared_type(name: &str, max_length: f64, precision: f64, scale: f64) -> String {
    let lower = name.to_ascii_lowercase();
    match lower.as_str() {
        "varchar" | "char" | "varbinary" | "binary" if max_length < 0.0 => format!("{name}(max)"),
        "varchar" | "char" | "varbinary" | "binary" => format!("{name}({max_length:.0})"),
        "nvarchar" | "nchar" if max_length < 0.0 => format!("{name}(max)"),
        "nvarchar" | "nchar" => format!("{name}({:.0})", max_length / 2.0),
        "decimal" | "numeric" => format!("{name}({precision:.0},{scale:.0})"),
        "datetime2" | "datetimeoffset" | "time" => format!("{name}({scale:.0})"),
        _ => name.to_string(),
    }
}

pub(crate) async fn columns(client: &mut MsClient) -> tiberius::Result<Vec<ColumnInfo>> {
    let rows = query(client, COLUMNS_SQL).await?;
    Ok(rows
        .iter()
        .map(|r| ColumnInfo {
            schema: r.string("schema_name"),
            table: r.string("table_name"),
            name: r.string("column_name"),
            data_type: declared_type(
                &r.string("type_name"),
                r.f64_or_zero("max_length"),
                r.f64_or_zero("precision"),
                r.f64_or_zero("scale"),
            ),
            nullable: r.bool("is_nullable"),
            default: r.str("default_value"),
            generated: r.bool("is_identity") || r.bool("is_computed"),
            ordinal: r.f64_or_zero("ordinal") as u32,
        })
        .collect())
}

pub(crate) async fn foreign_keys(client: &mut MsClient) -> tiberius::Result<Vec<ForeignKey>> {
    let rows = query(client, FOREIGN_KEYS_SQL).await?;
    Ok(rows
        .iter()
        .map(|r| ForeignKey {
            name: r.string("fk_name"),
            schema: r.string("schema_name"),
            table: r.string("table_name"),
            columns: r.list("columns"),
            ref_schema: r.string("ref_schema"),
            ref_table: r.string("ref_table"),
            ref_columns: r.list("ref_columns"),
        })
        .collect())
}

/// Splits the DMV's "[A], [B]" column lists.
pub(crate) fn bracket_list(value: Option<String>) -> Vec<String> {
    let Some(value) = value else { return Vec::new() };
    let mut out = Vec::new();
    let mut current = String::new();
    let mut depth = 0;
    for ch in value.chars() {
        match ch {
            '[' if depth == 0 => depth = 1,
            ']' if depth == 1 => {
                out.push(std::mem::take(&mut current));
                depth = 0;
            }
            _ if depth == 1 => current.push(ch),
            _ => {}
        }
    }
    out
}

pub(crate) async fn missing_indexes(client: &mut MsClient) -> tiberius::Result<Vec<MissingIndex>> {
    let rows = query(client, &missing_indexes_sql()).await?;
    let links = query(client, MISSING_INDEX_QUERIES_SQL).await.unwrap_or_default();
    Ok(rows
        .iter()
        .map(|r| {
            let group = r.string("group_handle");
            let mut query_hashes: Vec<String> = links
                .iter()
                .filter(|l| l.string("group_handle") == group)
                .filter_map(|l| l.str("query_hash"))
                .collect();
            query_hashes.sort();
            query_hashes.dedup();
            MissingIndex {
                schema: r.string("schema_name"),
                table: r.string("table_name"),
                equality: bracket_list(r.str("equality_columns")),
                inequality: bracket_list(r.str("inequality_columns")),
                include: bracket_list(r.str("included_columns")),
                seeks: r.f64_or_zero("user_seeks"),
                scans: r.f64_or_zero("user_scans"),
                avg_query_cost: r.f64_or_zero("avg_total_user_cost"),
                avg_impact_pct: r.f64_or_zero("avg_user_impact"),
                last_used: r.str("last_used"),
                query_hashes,
            }
        })
        .collect())
}

pub(crate) async fn statistics(client: &mut MsClient) -> tiberius::Result<Vec<StatisticsInfo>> {
    let rows = query(client, &statistics_sql()).await?;
    Ok(rows
        .iter()
        .map(|r| StatisticsInfo {
            schema: r.string("schema_name"),
            table: r.string("table_name"),
            name: r.string("stats_name"),
            last_updated: r.str("last_updated"),
            rows: r.f64("rows"),
            rows_sampled: r.f64("rows_sampled"),
            modifications: r.f64("modification_counter"),
            auto_created: r.bool("auto_created"),
        })
        .collect())
}

pub(crate) async fn waits(client: &mut MsClient, azure_sql_database: bool) -> tiberius::Result<Vec<WaitStat>> {
    let rows = query(client, &waits_sql(azure_sql_database)).await?;
    let mut by_category: std::collections::BTreeMap<&'static str, (f64, f64, String, f64)> = Default::default();
    for r in &rows {
        let wait_type = r.string("wait_type");
        let ms = r.f64_or_zero("wait_time_ms");
        let entry = by_category.entry(wait_type_category(&wait_type)).or_default();
        entry.0 += ms;
        entry.1 += r.f64_or_zero("waiting_tasks_count");
        if ms > entry.3 {
            entry.2 = wait_type;
            entry.3 = ms;
        }
    }
    let mut out: Vec<WaitStat> = by_category
        .into_iter()
        .map(|(category, (ms, tasks, top, _))| WaitStat {
            category: category.to_string(),
            wait_type: Some(top),
            wait_ms: ms,
            tasks: Some(tasks),
        })
        .collect();
    out.sort_by(|a, b| b.wait_ms.total_cmp(&a.wait_ms));
    Ok(out)
}

pub(crate) async fn resources(client: &mut MsClient) -> tiberius::Result<Vec<ResourceSample>> {
    let rows = query(client, RESOURCES_SQL).await?;
    Ok(rows
        .iter()
        .map(|r| ResourceSample {
            at: r.string("at"),
            cpu_pct: r.f64("avg_cpu_percent"),
            data_io_pct: r.f64("avg_data_io_percent"),
            log_write_pct: r.f64("avg_log_write_percent"),
            memory_pct: r.f64("avg_memory_usage_percent"),
            workers_pct: r.f64("max_worker_percent"),
        })
        .collect())
}

pub(crate) async fn settings(
    client: &mut MsClient,
    azure_sql_database: bool,
    query_store_options: Option<&Rec>,
) -> tiberius::Result<Vec<Setting>> {
    let mut out = Vec::new();
    if let Some(db) = query(client, DATABASE_SETTINGS_SQL).await?.into_iter().next() {
        for (name, value) in db.columns() {
            out.push(Setting {
                name: name.to_string(),
                value: render(value),
                default_value: None,
                scope: "database".into(),
            });
        }
    }
    if let Ok(rows) = query(client, REPLICA_ROLE_SQL).await
        && let Some(role) = replica_role(rows.first().and_then(|r| r.f64("is_primary")))
    {
        out.push(Setting {
            name: culprit_core::REPLICA_SETTING.into(),
            value: role.into(),
            default_value: None,
            scope: "database".into(),
        });
    }
    if let Ok(rows) = query(client, SCOPED_CONFIGURATIONS_SQL).await {
        for r in rows {
            let value = r.string("value");
            let is_default = r.str("is_value_default").map(|_| r.bool("is_value_default"));
            out.push(Setting {
                name: r.string("name"),
                default_value: is_default.filter(|d| *d).map(|_| value.clone()),
                value,
                scope: "database".into(),
            });
        }
    }
    if !azure_sql_database && let Ok(rows) = query(client, SERVER_CONFIGURATIONS_SQL).await {
        for r in rows {
            let name = r.string("name");
            let default_value = SERVER_DEFAULTS.iter().find(|(n, _)| *n == name).map(|(_, v)| v.to_string());
            out.push(Setting { name, value: r.string("value"), default_value, scope: "server".into() });
        }
    }
    if let Some(options) = query_store_options {
        for (name, value) in options.columns() {
            if matches!(value, crate::value::Val::Null) {
                continue;
            }
            out.push(Setting {
                name: format!("query_store_options.{name}"),
                value: render(value),
                default_value: None,
                scope: "database".into(),
            });
        }
    }
    Ok(out)
}

/// "readable secondary" when this connection reached a replica that isn't the primary (`is_primary` 0); nothing for a
/// primary or a database that isn't replicated (no row).
fn replica_role(is_primary: Option<f64>) -> Option<&'static str> {
    (is_primary == Some(0.0)).then_some("readable secondary")
}

fn render(value: &crate::value::Val) -> String {
    use crate::value::Val;
    match value {
        Val::Null => String::new(),
        Val::Int(v) => v.to_string(),
        Val::Float(v) if v.fract() == 0.0 => format!("{v:.0}"),
        Val::Float(v) => v.to_string(),
        Val::Text(v) => v.clone(),
        Val::Bool(v) => {
            if *v {
                "1".into()
            } else {
                "0".into()
            }
        }
        Val::Bytes(v) => crate::value::hex(v),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_readable_secondary_says_so() {
        assert_eq!(replica_role(Some(0.0)), Some("readable secondary"));
        assert_eq!(replica_role(Some(1.0)), None);
        assert_eq!(replica_role(None), None);
        assert!(REPLICA_ROLE_SQL.contains("rs.is_local = 1") && REPLICA_ROLE_SQL.contains("DB_ID()"));
    }

    #[test]
    fn splits_bracketed_lists() {
        assert_eq!(bracket_list(Some("[TenantId], [Created At]".into())), vec!["TenantId", "Created At"]);
        assert!(bracket_list(None).is_empty());
    }

    #[test]
    fn categorises_wait_types() {
        assert_eq!(wait_type_category("LCK_M_X"), "Lock");
        assert_eq!(wait_type_category("PAGEIOLATCH_SH"), "IO");
        assert_eq!(wait_type_category("WRITELOG"), "Log");
        assert_eq!(wait_type_category("HTREPARTITION"), "Parallelism");
        assert_eq!(wait_type_category("BMPBUILD"), "Parallelism");
        assert_eq!(wait_type_category("SOS_SCHEDULER_YIELD"), "CPU");
        assert_eq!(wait_type_category("CXPACKET"), "Parallelism");
        assert_eq!(wait_type_category("ASYNC_NETWORK_IO"), "Network");
    }
}
