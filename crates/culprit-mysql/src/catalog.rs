//! Database catalog, usage counters, configuration and replication role. No plan capture or scan orchestration.
use crate::{
    connect::Flavor,
    probe::Probe,
    rows::{Rec, query, query_one, query_with},
    vars,
};
use culprit_core::{ColumnInfo, ForeignKey, IndexStat, Result, Setting, TableStat};
use mysql_async::{Conn, Value};
use std::collections::{BTreeMap, HashMap};

pub(crate) async fn tables(conn: &mut Conn, db: &str) -> Result<Vec<TableStat>> {
    let rows = query_with(
        conn,
        "SELECT t.TABLE_NAME AS name, t.TABLE_ROWS AS row_estimate, t.DATA_LENGTH AS data_bytes, t.INDEX_LENGTH AS index_bytes,
                EXISTS (SELECT 1 FROM information_schema.TABLE_CONSTRAINTS c
                         WHERE c.TABLE_SCHEMA = t.TABLE_SCHEMA AND c.TABLE_NAME = t.TABLE_NAME
                           AND c.CONSTRAINT_TYPE = 'PRIMARY KEY') AS has_pk
           FROM information_schema.TABLES t
          WHERE t.TABLE_SCHEMA = ? AND t.TABLE_TYPE = 'BASE TABLE'",
        vec![Value::from(db)],
    )
    .await?;
    let io: HashMap<String, Rec> = query_with(
        conn,
        "SELECT OBJECT_NAME AS name, COUNT_INSERT AS inserts, COUNT_UPDATE AS updates, COUNT_DELETE AS deletes
           FROM performance_schema.table_io_waits_summary_by_table WHERE OBJECT_SCHEMA = ?",
        vec![Value::from(db)],
    )
    .await
    .unwrap_or_default()
    .into_iter()
    .map(|r| (r.string("name").to_ascii_lowercase(), r))
    .collect();
    // Rows read without any index: performance_schema files full scans under a NULL index name.
    let scans: HashMap<String, f64> = query_with(
        conn,
        "SELECT OBJECT_NAME AS name, COUNT_READ AS read_rows FROM performance_schema.table_io_waits_summary_by_index_usage
          WHERE OBJECT_SCHEMA = ? AND INDEX_NAME IS NULL",
        vec![Value::from(db)],
    )
    .await
    .unwrap_or_default()
    .into_iter()
    .filter_map(|r| Some((r.string("name").to_ascii_lowercase(), r.number("read_rows")?)))
    .collect();
    let analyzed: HashMap<String, String> = query_with(
        conn,
        "SELECT table_name AS name, DATE_FORMAT(last_update, '%Y-%m-%dT%H:%i:%sZ') AS analyzed
           FROM mysql.innodb_table_stats WHERE database_name = ?",
        vec![Value::from(db)],
    )
    .await
    .unwrap_or_default()
    .into_iter()
    .filter_map(|r| Some((r.string("name").to_ascii_lowercase(), r.text("analyzed")?)))
    .collect();
    Ok(rows
        .iter()
        .map(|r| {
            let name = r.string("name");
            let key = name.to_ascii_lowercase();
            let io = io.get(&key);
            let data = r.number("data_bytes");
            let index = r.number("index_bytes");
            TableStat {
                schema: db.to_string(),
                kind: None,
                definition: None,
                rows: r.number("row_estimate"),
                total_bytes: match (data, index) {
                    (None, None) => None,
                    (d, i) => Some(d.unwrap_or(0.0) + i.unwrap_or(0.0)),
                },
                data_bytes: data,
                index_bytes: index,
                has_primary_key: r.flag("has_pk"),
                is_heap: None,
                seq_scans: None,
                seq_rows_read: scans.get(&key).copied(),
                index_scans: None,
                inserts: io.and_then(|r| r.number("inserts")),
                updates: io.and_then(|r| r.number("updates")),
                deletes: io.and_then(|r| r.number("deletes")),
                dead_rows: None,
                modifications_since_analyze: None,
                last_vacuum: None,
                last_analyze: analyzed.get(&key).cloned(),
                forwarded_records: None,
                forwarded_fetches: None,
                xid_age: None,
                name,
                storage: None,
                in_row_bytes: None,
                lob_bytes: None,
                overflow_bytes: None,
                partition_of: None,
                counters_since: None,
            }
        })
        .collect())
}

pub(crate) async fn columns(conn: &mut Conn, db: &str) -> Result<Vec<ColumnInfo>> {
    let rows = query_with(
        conn,
        "SELECT TABLE_NAME AS table_name, COLUMN_NAME AS name, COLUMN_TYPE AS data_type, IS_NULLABLE AS nullable,
                COLUMN_DEFAULT AS default_value, EXTRA AS extra, ORDINAL_POSITION AS ordinal
           FROM information_schema.COLUMNS WHERE TABLE_SCHEMA = ? ORDER BY TABLE_NAME, ORDINAL_POSITION",
        vec![Value::from(db)],
    )
    .await?;
    Ok(rows
        .iter()
        .map(|r| ColumnInfo {
            schema: db.to_string(),
            table: r.string("table_name"),
            name: r.string("name"),
            data_type: r.string("data_type"),
            nullable: r.string("nullable").eq_ignore_ascii_case("YES"),
            default: r.text("default_value"),
            generated: r.string("extra").to_ascii_lowercase().contains("auto_increment"),
            ordinal: r.number("ordinal").unwrap_or(0.0) as u32,
        })
        .collect())
}

/// Indexes with their key columns, size and usage since the server started. The note explains missing usage counters.
pub(crate) async fn indexes(conn: &mut Conn, db: &str, probe: &Probe) -> Result<(Vec<IndexStat>, Option<String>)> {
    let with_visibility = "SELECT TABLE_NAME AS table_name, INDEX_NAME AS index_name, NON_UNIQUE AS non_unique,
                SEQ_IN_INDEX AS seq, COLUMN_NAME AS column_name, COLLATION AS collation, SUB_PART AS sub_part,
                INDEX_TYPE AS index_type, EXPRESSION AS expression, IS_VISIBLE AS visible
           FROM information_schema.STATISTICS WHERE TABLE_SCHEMA = ? ORDER BY TABLE_NAME, INDEX_NAME, SEQ_IN_INDEX";
    let without_visibility = "SELECT TABLE_NAME AS table_name, INDEX_NAME AS index_name, NON_UNIQUE AS non_unique,
                SEQ_IN_INDEX AS seq, COLUMN_NAME AS column_name, COLLATION AS collation, SUB_PART AS sub_part,
                INDEX_TYPE AS index_type
           FROM information_schema.STATISTICS WHERE TABLE_SCHEMA = ? ORDER BY TABLE_NAME, INDEX_NAME, SEQ_IN_INDEX";
    let rows = match query_with(conn, with_visibility, vec![Value::from(db)]).await {
        Ok(rows) => rows,
        Err(_) => query_with(conn, without_visibility, vec![Value::from(db)]).await?,
    };
    let engines: HashMap<String, String> = query_with(
        conn,
        "SELECT TABLE_NAME AS name, ENGINE AS engine FROM information_schema.TABLES WHERE TABLE_SCHEMA = ?",
        vec![Value::from(db)],
    )
    .await?
    .into_iter()
    .map(|r| (r.string("name").to_ascii_lowercase(), r.string("engine")))
    .collect();
    let page_size =
        query_one(conn, "SELECT @@innodb_page_size AS page").await.ok().flatten().and_then(|r| r.number("page"));
    let sizes: HashMap<(String, String), f64> = match page_size {
        Some(page) => query_with(
            conn,
            "SELECT table_name AS t, index_name AS i, stat_value AS pages FROM mysql.innodb_index_stats
              WHERE database_name = ? AND stat_name = 'size'",
            vec![Value::from(db)],
        )
        .await
        .unwrap_or_default()
        .into_iter()
        .filter_map(|r| {
            let table = r.string("t");
            let table = table.split("#p#").next().unwrap_or(&table).to_ascii_lowercase();
            Some(((table, r.string("i").to_ascii_lowercase()), r.number("pages")? * page))
        })
        .fold(HashMap::new(), |mut acc, (key, bytes)| {
            *acc.entry(key).or_insert(0.0) += bytes;
            acc
        }),
        None => HashMap::new(),
    };
    let (usage, usage_note) = if probe.performance_schema {
        match query_with(
            conn,
            "SELECT i.OBJECT_NAME AS t, i.INDEX_NAME AS i, i.COUNT_FETCH AS read_count,
                    t.COUNT_INSERT + t.COUNT_UPDATE + t.COUNT_DELETE AS write_count
               FROM performance_schema.table_io_waits_summary_by_index_usage i
               LEFT JOIN performance_schema.table_io_waits_summary_by_table t
                 ON t.OBJECT_TYPE = i.OBJECT_TYPE AND t.OBJECT_SCHEMA = i.OBJECT_SCHEMA AND t.OBJECT_NAME = i.OBJECT_NAME
              WHERE i.OBJECT_SCHEMA = ? AND i.INDEX_NAME IS NOT NULL",
            vec![Value::from(db)],
        )
        .await
        {
            Ok(rows) => (
                rows.into_iter()
                    .map(|r| {
                        let key = (r.string("t").to_ascii_lowercase(), r.string("i").to_ascii_lowercase());
                        (key, (r.number("read_count"), r.number("write_count")))
                    })
                    .collect::<HashMap<_, _>>(),
                None,
            ),
            Err(e) => (HashMap::new(), Some(format!("Reading index usage failed: {}", e.message))),
        }
    } else if probe.userstat == Some(true) {
        match userstat_usage(conn, db, &rows).await {
            Ok(usage) => (usage, None),
            Err(e) => (HashMap::new(), Some(format!("Reading userstat index statistics failed: {}", e.message))),
        }
    } else {
        (HashMap::new(), None)
    };

    let mut grouped: BTreeMap<(String, String), Vec<&Rec>> = BTreeMap::new();
    for r in &rows {
        grouped.entry((r.string("table_name"), r.string("index_name"))).or_default().push(r);
    }
    let mut out = Vec::new();
    for ((table, name), parts) in grouped {
        let first = parts[0];
        let columns: Vec<String> = parts
            .iter()
            .map(|p| {
                let base = match (p.text("column_name"), p.text("expression")) {
                    (Some(c), _) => c,
                    (None, Some(e)) => format!("({e})"),
                    (None, None) => "?".into(),
                };
                let base = match p.number("sub_part") {
                    Some(n) => format!("{base}({})", n as u64),
                    None => base,
                };
                if p.string("collation") == "D" { format!("{base} DESC") } else { base }
            })
            .collect();
        let primary = name.eq_ignore_ascii_case("PRIMARY");
        let unique = first.number("non_unique") == Some(0.0);
        let key = (table.to_ascii_lowercase(), name.to_ascii_lowercase());
        let used = usage.get(&key);
        let innodb = engines.get(&key.0).is_some_and(|e| e.eq_ignore_ascii_case("InnoDB"));
        let quoted = |s: &str| format!("`{}`", s.replace('`', "``"));
        let definition = if primary {
            format!("ALTER TABLE {}.{} ADD PRIMARY KEY ({})", quoted(db), quoted(&table), columns.join(", "))
        } else {
            format!(
                "CREATE {}INDEX {} ON {}.{} ({})",
                if unique { "UNIQUE " } else { "" },
                quoted(&name),
                quoted(db),
                quoted(&table),
                columns.join(", ")
            )
        };
        out.push(IndexStat {
            filter_facts: None,
            schema: db.to_string(),
            kind: first.string("index_type").to_ascii_lowercase(),
            include: Vec::new(),
            filter: None,
            is_unique: unique,
            is_primary_key: primary,
            is_clustered: primary && innodb,
            is_disabled: first.text("visible").is_some_and(|v| v.eq_ignore_ascii_case("NO")),
            state: None,
            maintained: Some(true),
            has_statistics: None,
            size_bytes: sizes.get(&key).copied(),
            // No row means the table hasn't been opened since the server started: usage is unknown, not zero.
            reads: used.and_then(|u| u.0),
            // Writes to the table the index is kept up for: performance_schema files every insert under no index, and
            // counts an update or delete under the index that found its rows, so per-index counts aren't its upkeep.
            writes: used.and_then(|u| u.1),
            seeks: None,
            scans: None,
            lookups: None,
            last_read: None,
            definition: Some(definition),
            table,
            name,
            columns,
            contention: None,
            created_at_latest: None,
            partition_of: None,
            opclasses: None,
            counters_since: None,
        });
    }
    Ok((out, usage_note))
}

/// Index reads from MariaDB's userstat counters (since `userstat` was turned on or last flushed). An index missing from
/// INDEX_STATISTICS on a table that has been read was not used; on a table nobody has touched, usage is unknown.
async fn userstat_usage(
    conn: &mut Conn,
    db: &str,
    statistics: &[Rec],
) -> Result<HashMap<(String, String), (Option<f64>, Option<f64>)>> {
    let tables: HashMap<String, f64> = query_with(
        conn,
        "SELECT TABLE_NAME AS t, ROWS_CHANGED AS changed FROM information_schema.TABLE_STATISTICS WHERE TABLE_SCHEMA = ?",
        vec![Value::from(db)],
    )
    .await?
    .into_iter()
    .map(|r| (r.string("t").to_ascii_lowercase(), r.number("changed").unwrap_or(0.0)))
    .collect();
    let read: HashMap<(String, String), f64> = query_with(
        conn,
        "SELECT TABLE_NAME AS t, INDEX_NAME AS i, ROWS_READ AS rows_read FROM information_schema.INDEX_STATISTICS WHERE TABLE_SCHEMA = ?",
        vec![Value::from(db)],
    )
    .await?
    .into_iter()
    .map(|r| ((r.string("t").to_ascii_lowercase(), r.string("i").to_ascii_lowercase()), r.number("rows_read").unwrap_or(0.0)))
    .collect();
    let mut out = HashMap::new();
    for r in statistics {
        let key = (r.string("table_name").to_ascii_lowercase(), r.string("index_name").to_ascii_lowercase());
        let Some(changed) = tables.get(&key.0) else { continue };
        let reads = read.get(&key).copied().unwrap_or(0.0);
        out.insert(key, (Some(reads), Some(*changed)));
    }
    Ok(out)
}

pub(crate) async fn foreign_keys(conn: &mut Conn, db: &str) -> Result<Vec<ForeignKey>> {
    let rows = query_with(
        conn,
        "SELECT CONSTRAINT_NAME AS name, TABLE_NAME AS table_name, COLUMN_NAME AS column_name,
                REFERENCED_TABLE_SCHEMA AS ref_schema, REFERENCED_TABLE_NAME AS ref_table, REFERENCED_COLUMN_NAME AS ref_column
           FROM information_schema.KEY_COLUMN_USAGE
          WHERE TABLE_SCHEMA = ? AND REFERENCED_TABLE_NAME IS NOT NULL
          ORDER BY TABLE_NAME, CONSTRAINT_NAME, ORDINAL_POSITION",
        vec![Value::from(db)],
    )
    .await?;
    let mut out: Vec<ForeignKey> = Vec::new();
    for r in &rows {
        let (name, table) = (r.string("name"), r.string("table_name"));
        match out.iter_mut().find(|f| f.name == name && f.table == table) {
            Some(fk) => {
                fk.columns.push(r.string("column_name"));
                fk.ref_columns.push(r.string("ref_column"));
            }
            None => out.push(ForeignKey {
                name,
                schema: db.to_string(),
                table,
                columns: vec![r.string("column_name")],
                ref_schema: r.string("ref_schema"),
                ref_table: r.string("ref_table"),
                ref_columns: vec![r.string("ref_column")],
            }),
        }
    }
    Ok(out)
}

/// Server variables the checks and the UI read, with their compiled-in defaults: MySQL 8.0/8.4, then MariaDB 10.11/11.x
/// (`None` where that server has no such variable, empty where its default isn't one value). MySQL 5.7 has no
/// internal_tmp_mem_storage_engine or temptable_max_ram: a variable the server doesn't have isn't read.
const SETTINGS: [(&str, Option<&str>, Option<&str>); 23] = [
    ("performance_schema_digests_size", Some("10000"), Some("-1")),
    ("performance_schema_max_digest_length", Some("1024"), Some("1024")),
    ("performance_schema_max_sql_text_length", Some("1024"), Some("1024")),
    ("max_digest_length", Some("1024"), Some("1024")),
    ("innodb_buffer_pool_size", Some("134217728"), Some("134217728")),
    ("innodb_page_size", Some("16384"), Some("16384")),
    ("tmp_table_size", Some("16777216"), Some("16777216")),
    ("max_heap_table_size", Some("16777216"), Some("16777216")),
    // Which engine keeps internal temporary tables in memory, and TempTable's memory for all of them together (1 GB
    // on 8.0; 8.4 takes 3% of the host's memory, between 1 and 4 GB).
    ("internal_tmp_mem_storage_engine", Some("TempTable"), None),
    ("temptable_max_ram", Some(""), None),
    ("sort_buffer_size", Some("262144"), Some("2097152")),
    ("join_buffer_size", Some("262144"), Some("262144")),
    ("long_query_time", Some("10.000000"), Some("10.000000")),
    ("slow_query_log", Some("OFF"), Some("OFF")),
    ("log_output", Some("FILE"), Some("FILE")),
    ("performance_schema", Some("ON"), Some("OFF")),
    ("max_connections", Some("151"), Some("151")),
    ("innodb_flush_log_at_trx_commit", Some("1"), Some("1")),
    ("transaction_isolation", Some("REPEATABLE-READ"), Some("REPEATABLE-READ")),
    ("tx_isolation", None, Some("REPEATABLE-READ")),
    ("max_execution_time", Some("0"), None),
    ("max_statement_time", None, Some("0.000000")),
    ("userstat", None, Some("OFF")),
];

/// "replica" when this server replicates from another: its replication status has a row, or, where this login can't
/// read that status, it is read-only (how replicas and Aurora's readers run). Its usage counters count the reads run on
/// it (and the replication applier's), not the primary's workload.
pub(crate) async fn replica_role(conn: &mut Conn, probe: &Probe) -> Option<&'static str> {
    let status = match query(
        conn,
        if probe.flavor == Flavor::Mariadb { "SHOW ALL REPLICAS STATUS" } else { "SHOW REPLICA STATUS" },
    )
    .await
    {
        Ok(rows) => Ok(rows),
        Err(_) => {
            query(conn, if probe.flavor == Flavor::Mariadb { "SHOW ALL SLAVES STATUS" } else { "SHOW SLAVE STATUS" })
                .await
        }
    };
    match status {
        Ok(rows) if !rows.is_empty() => Some("replica"),
        Ok(_) => None,
        Err(_) => probe.read_only.then_some("replica"),
    }
}

pub(crate) async fn settings(conn: &mut Conn, flavor: Flavor) -> Result<Vec<Setting>> {
    let default_of = |name: &str| {
        SETTINGS.iter().find(|(n, _, _)| *n == name).and_then(|(_, mysql, maria)| match flavor {
            Flavor::Mysql => *mysql,
            Flavor::Mariadb => *maria,
        })
    };
    let names: Vec<&str> = SETTINGS.iter().filter(|(n, _, _)| default_of(n).is_some()).map(|(n, _, _)| *n).collect();
    let rows = vars::variables(conn, &names).await?;
    Ok(rows
        .into_iter()
        .map(|(name, value)| {
            let default = default_of(&name).filter(|d| !d.is_empty()).map(str::to_string);
            Setting { name, value, default_value: default, scope: "server".into() }
        })
        .chain(statement_status(conn).await)
        .collect())
}

/// Whether performance_schema keeps up with the workload: digests it had no room for, statements lumped into the
/// overflow row, and the consumer that has to be on for digests at all. Best effort; each part may be unreadable.
async fn statement_status(conn: &mut Conn) -> Vec<Setting> {
    let mut out = Vec::new();
    let status =
        |name: &str, value: String| Setting { name: name.into(), value, default_value: None, scope: "status".into() };
    if let Ok(found) = vars::status(conn, &["Performance_schema_digest_lost"]).await
        && let Some(lost) = found.get("Performance_schema_digest_lost")
    {
        out.push(status("performance_schema_digest_lost", lost.to_string()));
    }
    if let Ok(Some(r)) = query_one(
        conn,
        "SELECT COUNT_STAR AS calls, SUM_TIMER_WAIT / 1000000000 AS total_ms
           FROM performance_schema.events_statements_summary_by_digest WHERE DIGEST IS NULL",
    )
    .await
    {
        out.push(status("performance_schema.untracked_statements", r.string("calls")));
        out.push(status("performance_schema.untracked_ms", r.string("total_ms")));
    }
    if let Ok(rows) = query(
        conn,
        "SELECT NAME AS name, ENABLED AS enabled FROM performance_schema.setup_consumers
          WHERE NAME IN ('statements_digest', 'events_statements_current', 'global_instrumentation')",
    )
    .await
    {
        for r in rows {
            out.push(status(&format!("performance_schema.consumer.{}", r.string("name")), r.string("enabled")));
        }
    }
    out
}
