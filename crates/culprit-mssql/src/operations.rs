//! Capacity and allocation limits from catalog/DMV metadata only. No DBCC, file reads or workload execution.
use crate::{
    conn::{MsClient, query},
    probe::Probe,
    sources,
};
use culprit_core::operations::{SequenceCapacity, SpacePart, SpaceUsage};
use culprit_core::{Coverage, FileKind, FileScope, Snapshot};

const LOG_SQL: &str = "SELECT total_log_size_in_bytes AS total, used_log_space_in_bytes AS used,
 (SELECT log_reuse_wait_desc FROM sys.databases WHERE database_id = DB_ID()) AS reuse_wait,
 (SELECT MAX(CASE WHEN growth > 0 AND (max_size = -1 OR size < max_size) THEN 1 ELSE 0 END) FROM sys.database_files WHERE type = 1) AS can_grow,
 (SELECT CASE WHEN MAX(CASE WHEN growth > 0 AND max_size = -1 THEN 1 ELSE 0 END) = 1 THEN NULL ELSE SUM(CAST(CASE WHEN growth = 0 OR max_size < size THEN size ELSE max_size END AS bigint)) * 8192.0 END FROM sys.database_files WHERE type = 1) AS max_bytes
 FROM sys.dm_db_log_space_usage";

const FILES_SQL: &str = "SELECT fg.name, SUM(CAST(df.size AS bigint)) * 8192.0 AS total,
 CASE WHEN COUNT(FILEPROPERTY(df.name, 'SpaceUsed')) = COUNT(*) THEN SUM(CAST(FILEPROPERTY(df.name, 'SpaceUsed') AS bigint)) * 8192.0 END AS used,
 MAX(CASE WHEN df.growth > 0 AND (df.max_size = -1 OR df.size < df.max_size) THEN 1 ELSE 0 END) AS can_grow,
 CASE WHEN MAX(CASE WHEN df.growth > 0 AND df.max_size = -1 THEN 1 ELSE 0 END) = 1 THEN NULL ELSE SUM(CAST(CASE WHEN df.growth = 0 OR df.max_size < df.size THEN df.size ELSE df.max_size END AS bigint)) * 8192.0 END AS max_bytes
 FROM sys.database_files df JOIN sys.filegroups fg ON fg.data_space_id = df.data_space_id
 WHERE df.type = 0 AND df.state = 0 AND fg.is_read_only = 0 GROUP BY fg.name";

const TEMP_SQL: &str = "SELECT SUM(total_page_count) * 8192.0 AS total,
 SUM(total_page_count - unallocated_extent_page_count) * 8192.0 AS used,
 SUM(user_object_reserved_page_count) * 8192.0 AS users,
 SUM(internal_object_reserved_page_count) * 8192.0 AS internals,
 SUM(version_store_reserved_page_count) * 8192.0 AS versions
 FROM tempdb.sys.dm_db_file_space_usage";

const SEQUENCES_SQL: &str = "SELECT s.name + '.' + q.name AS name, CAST(NULL AS nvarchar(512)) AS table_name,
 CONVERT(nvarchar(40), q.current_value) AS current_value, CONVERT(nvarchar(40), q.increment) AS step,
 CONVERT(nvarchar(40), q.minimum_value) AS min_value, CONVERT(nvarchar(40), q.maximum_value) AS max_value,
 CAST(q.is_cycling AS int) AS cycles, 1 AS allocated
 FROM sys.sequences q JOIN sys.schemas s ON s.schema_id = q.schema_id
 WHERE q.current_value IS NOT NULL
 UNION ALL
 SELECT s.name + '.' + t.name + '.' + c.name, s.name + '.' + t.name,
 CONVERT(nvarchar(40), c.last_value), CONVERT(nvarchar(40), c.increment_value),
 CASE TYPE_NAME(c.system_type_id) WHEN 'tinyint' THEN '0' WHEN 'smallint' THEN '-32768' WHEN 'int' THEN '-2147483648' WHEN 'bigint' THEN '-9223372036854775808' WHEN 'numeric' THEN '-' + REPLICATE('9', c.precision) WHEN 'decimal' THEN '-' + REPLICATE('9', c.precision) END,
 CASE TYPE_NAME(c.system_type_id) WHEN 'tinyint' THEN '255' WHEN 'smallint' THEN '32767' WHEN 'int' THEN '2147483647' WHEN 'bigint' THEN '9223372036854775807' WHEN 'numeric' THEN REPLICATE('9', c.precision) WHEN 'decimal' THEN REPLICATE('9', c.precision) END,
 0, 1 FROM sys.identity_columns c JOIN sys.tables t ON t.object_id = c.object_id
 JOIN sys.schemas s ON s.schema_id = t.schema_id WHERE t.is_ms_shipped = 0 AND c.last_value IS NOT NULL";

pub async fn collect(client: &mut MsClient, probe: &Probe, snapshot: &mut Snapshot) {
    let mut coverage = Coverage::default();
    if let Some(rows) =
        sources::read(&mut coverage, &sources::LOG_CAPACITY, query(client, LOG_SQL).await, || probe.grant_hint())
    {
        for r in rows {
            if let (Some(total), Some(used)) = (r.f64("total"), r.f64("used")) {
                snapshot.operations.spaces.push(SpaceUsage {
                    parts: vec![],
                    name: format!("{} transaction log", snapshot.server.database),
                    kind: FileKind::Log,
                    scope: FileScope::Database,
                    total_bytes: total,
                    used_bytes: used,
                    growth_enabled: r.f64("can_grow").map(|v| v > 0.0),
                    limit_bytes: r.f64("max_bytes"),
                    reuse_wait: r.str("reuse_wait"),
                });
            }
        }
    }
    if let Some(rows) =
        sources::read(&mut coverage, &sources::FILE_CAPACITY, query(client, FILES_SQL).await, || probe.grant_hint())
    {
        for r in rows {
            if let (Some(total), Some(used)) = (r.f64("total"), r.f64("used")) {
                snapshot.operations.spaces.push(SpaceUsage {
                    parts: vec![],
                    name: format!("{} data filegroup {}", snapshot.server.database, r.string("name")),
                    kind: FileKind::Data,
                    scope: FileScope::Database,
                    total_bytes: total,
                    used_bytes: used,
                    growth_enabled: r.f64("can_grow").map(|v| v > 0.0),
                    limit_bytes: r.f64("max_bytes"),
                    reuse_wait: None,
                });
            }
        }
    }
    if let Some(rows) =
        sources::read(&mut coverage, &sources::TEMP_CAPACITY, query(client, TEMP_SQL).await, || probe.grant_hint())
    {
        for r in rows {
            if let (Some(total), Some(used)) = (r.f64("total"), r.f64("used")) {
                let parts =
                    [("User objects", "users"), ("Internal objects", "internals"), ("Version store", "versions")]
                        .into_iter()
                        .filter_map(|(name, col)| r.f64(col).map(|bytes| SpacePart { name: name.into(), bytes }))
                        .collect();
                snapshot.operations.spaces.push(SpaceUsage {
                    parts,
                    name: "tempdb data files (whole instance)".into(),
                    kind: FileKind::Temp,
                    scope: FileScope::Server,
                    total_bytes: total,
                    used_bytes: used,
                    growth_enabled: None,
                    limit_bytes: None,
                    reuse_wait: None,
                });
            }
        }
    }
    if let Some(rows) =
        sources::read(&mut coverage, &sources::SEQUENCE_CAPACITY, query(client, SEQUENCES_SQL).await, || {
            probe.grant_hint()
        })
    {
        for r in rows {
            let (Some(current), Some(increment), Some(minimum), Some(maximum)) =
                (r.str("current_value"), r.str("step"), r.str("min_value"), r.str("max_value"))
            else {
                continue;
            };
            snapshot.operations.sequences.push(SequenceCapacity {
                name: r.string("name"),
                table: r.str("table_name"),
                current,
                increment,
                minimum,
                maximum,
                cycles: r.f64("cycles").is_some_and(|v| v > 0.0),
                allocated: true,
            });
        }
    }
    snapshot.notes.extend(coverage.finish().2);
}
