//! Tables, indexes, columns, foreign keys and configuration. No workload planning or scan orchestration.
use crate::{
    probe::Probe,
    rows::{iso, query, query_one},
};
use culprit_core::{ColumnInfo, ForeignKey, IndexStat, Result, Setting, TableStat};
use tokio_postgres::Client;

pub(crate) async fn tables(client: &Client, version_num: u32) -> Result<Vec<TableStat>> {
    let exact = "pg_total_relation_size(s.relid) AS total_bytes, pg_table_size(s.relid) AS data_bytes, pg_indexes_size(s.relid) AS index_bytes,
                 pg_relation_size(s.relid) AS in_row_bytes,
                 CASE WHEN c.reltoastrelid = 0 THEN 0 ELSE pg_total_relation_size(c.reltoastrelid) END AS lob_bytes";
    let estimated = "(c.relpages::bigint + coalesce((SELECT sum(i.relpages) FROM pg_index x JOIN pg_class i ON i.oid = x.indexrelid WHERE x.indrelid = s.relid), 0)) * current_setting('block_size')::bigint AS total_bytes,
                     (c.relpages::bigint + coalesce((SELECT t.relpages FROM pg_class t WHERE t.oid = c.reltoastrelid), 0)) * current_setting('block_size')::bigint AS data_bytes,
                     coalesce((SELECT sum(i.relpages) FROM pg_index x JOIN pg_class i ON i.oid = x.indexrelid WHERE x.indrelid = s.relid), 0) * current_setting('block_size')::bigint AS index_bytes,
                     c.relpages::bigint * current_setting('block_size')::bigint AS in_row_bytes,
                     coalesce((SELECT t.relpages FROM pg_class t WHERE t.oid = c.reltoastrelid), 0)::bigint * current_setting('block_size')::bigint AS lob_bytes";
    let sql = |sizes: &str| {
        format!(
            "SELECT s.schemaname AS schema_name, s.relname AS table_name,
                    {rows} AS rows,
                    {sizes},
                    EXISTS (SELECT 1 FROM pg_constraint k WHERE k.conrelid = s.relid AND k.contype = 'p') AS has_pk,
                    s.seq_scan, s.seq_tup_read, s.idx_scan, s.n_tup_ins, s.n_tup_upd, s.n_tup_del, s.n_dead_tup,
                    s.n_mod_since_analyze,
                    {vacuum} AS last_vacuum,
                    {analyze} AS last_analyze,
                    CASE WHEN c.relkind = 'p' THEN NULL ELSE age(c.relfrozenxid) END AS xid_age,
                    {partition_of} AS partition_of
             FROM pg_stat_user_tables s JOIN pg_class c ON c.oid = s.relid",
            rows = ROWS_SQL,
            vacuum = iso("greatest(s.last_vacuum, s.last_autovacuum)"),
            analyze = iso("greatest(s.last_analyze, s.last_autoanalyze)"),
            partition_of = if version_num >= 100_000 { partition_parent("c") } else { "NULL::text".to_string() },
        )
    };
    let rows = match query(client, &sql(exact)).await {
        Ok(rows) => rows,
        Err(e) if e.code == culprit_core::ErrorCode::Timeout => query(client, &sql(estimated)).await?,
        Err(e) => return Err(e),
    };
    Ok(rows
        .iter()
        .map(|r| TableStat {
            schema: r.string("schema_name"),
            name: r.string("table_name"),
            kind: None,
            definition: None,
            rows: r.number("rows").map(|v| v.max(0.0)),
            total_bytes: r.number("total_bytes"),
            data_bytes: r.number("data_bytes"),
            index_bytes: r.number("index_bytes"),
            has_primary_key: r.flag("has_pk"),
            is_heap: None,
            seq_scans: r.number("seq_scan"),
            seq_rows_read: r.number("seq_tup_read"),
            index_scans: r.number("idx_scan"),
            inserts: r.number("n_tup_ins"),
            updates: r.number("n_tup_upd"),
            deletes: r.number("n_tup_del"),
            dead_rows: r.number("n_dead_tup"),
            modifications_since_analyze: r.number("n_mod_since_analyze"),
            last_vacuum: r.text("last_vacuum"),
            last_analyze: r.text("last_analyze"),
            forwarded_records: None,
            forwarded_fetches: None,
            xid_age: r.number("xid_age"),
            storage: None,
            in_row_bytes: r.number("in_row_bytes"),
            lob_bytes: r.number("lob_bytes"),
            overflow_bytes: None,
            partition_of: r.text("partition_of"),
            counters_since: None,
        })
        .collect())
}

/// A table's row count. `n_live_tup` is a statistics counter: after a reset (`pg_stat_reset()`, crash recovery, a
/// restore, a promoted standby) it counts only inserts minus deletes since, so a 10-million-row table reads as a few
/// thousand rows until a vacuum or analyze sets it again. Until one has (no vacuum or analyze time since the reset),
/// the planner's own count (`reltuples`, kept in the catalog) is the better one; a table never analyzed has only the
/// counter. Partitioned parents hold no rows of their own.
const ROWS_SQL: &str = "CASE WHEN c.relkind = 'p' THEN NULL
                             WHEN c.reltuples < 0 THEN s.n_live_tup
                             WHEN s.last_vacuum IS NULL AND s.last_autovacuum IS NULL AND s.last_analyze IS NULL
                                  AND s.last_autoanalyze IS NULL THEN greatest(c.reltuples, s.n_live_tup)
                             ELSE s.n_live_tup END";

/// The relation `alias` (a pg_class row) is a partition of, as `schema.name` (PostgreSQL 10+ for tables, 11+ for
/// indexes); NULL for one that isn't.
fn partition_parent(alias: &str) -> String {
    format!(
        "CASE WHEN {alias}.relispartition THEN (SELECT pn.nspname || '.' || pc.relname FROM pg_inherits ih
           JOIN pg_class pc ON pc.oid = ih.inhparent JOIN pg_namespace pn ON pn.oid = pc.relnamespace
          WHERE ih.inhrelid = {alias}.oid LIMIT 1) END"
    )
}

pub(crate) async fn indexes(client: &Client, probe: &Probe) -> Result<Vec<IndexStat>> {
    let last_read = if probe.version_num >= 160_000 { iso("s.last_idx_scan") } else { "NULL::text".to_string() };
    // An index being built or rebuilt by CREATE/REINDEX CONCURRENTLY has indisvalid = false until the build finishes;
    // pg_stat_progress_create_index (PostgreSQL 12+) distinguishes a live build from a failed leftover.
    let usable = if probe.version_num >= 120_000 {
        "(x.indisvalid AND x.indisready) OR EXISTS (SELECT 1 FROM pg_stat_progress_create_index p WHERE p.index_relid = x.indexrelid OR p.relid = x.indrelid)"
    } else {
        "x.indisvalid AND x.indisready"
    };
    let usable = if probe.timescale_version.is_some() && probe.version_num >= 120_000 {
        format!(
            "({usable}) OR EXISTS (SELECT 1 FROM pg_stat_progress_create_index p
           JOIN pg_class chunk ON chunk.oid = p.relid JOIN pg_namespace cn ON cn.oid = chunk.relnamespace
           JOIN timescaledb_information.chunks tc ON tc.chunk_schema = cn.nspname AND tc.chunk_name = chunk.relname
          WHERE tc.hypertable_schema = n.nspname AND tc.hypertable_name = t.relname)"
        )
    } else {
        usable.to_string()
    };
    let sql = format!(
        "SELECT n.nspname AS schema_name, t.relname AS table_name, i.relname AS index_name, am.amname AS kind,
                x.indisunique, x.indisprimary, x.indisready, {usable} AS usable,
                (SELECT string_agg(
                    CASE WHEN x.indkey[k - 1] <> 0
                         THEN (SELECT a.attname FROM pg_attribute a WHERE a.attrelid = x.indrelid AND a.attnum = x.indkey[k - 1])
                         ELSE pg_get_indexdef(x.indexrelid, k, true) END
                    || CASE WHEN (x.indoption[k - 1] & 1) = 1 THEN ' DESC' ELSE '' END,
                    chr(31) ORDER BY k)
                 FROM generate_series(1, x.indnkeyatts) AS k) AS key_columns,
                (SELECT string_agg(
                    CASE WHEN x.indkey[k - 1] <> 0
                         THEN (SELECT a.attname FROM pg_attribute a WHERE a.attrelid = x.indrelid AND a.attnum = x.indkey[k - 1])
                         ELSE pg_get_indexdef(x.indexrelid, k, true) END,
                    chr(31) ORDER BY k)
                 FROM generate_series(x.indnkeyatts + 1, x.indnatts) AS k) AS include_columns,
                (SELECT string_agg(concat_ws(' ',
                    CASE WHEN NOT opc.opcdefault THEN opc.opcname END,
                    CASE WHEN x.indcollation[k - 1] <> 0
                          AND x.indcollation[k - 1] IS DISTINCT FROM coalesce(
                              (SELECT a.attcollation FROM pg_attribute a WHERE a.attrelid = x.indrelid AND a.attnum = x.indkey[k - 1]),
                              100)
                         THEN 'COLLATE ' || quote_ident(coll.collname) END), chr(31) ORDER BY k)
                 FROM generate_series(1, x.indnkeyatts) AS k
                 JOIN pg_opclass opc ON opc.oid = x.indclass[k - 1]
                 LEFT JOIN pg_collation coll ON coll.oid = x.indcollation[k - 1]) AS key_opclasses,
                {partition_of} AS partition_of,
                pg_get_expr(x.indpred, x.indrelid, true) AS filter,
                i.relpages::bigint * current_setting('block_size')::bigint AS estimated_bytes,
                s.idx_scan, {last_read} AS last_read,
                pg_get_indexdef(x.indexrelid) AS definition
         FROM pg_index x
         JOIN pg_class i ON i.oid = x.indexrelid
         JOIN pg_class t ON t.oid = x.indrelid
         JOIN pg_namespace n ON n.oid = t.relnamespace
         JOIN pg_am am ON am.oid = i.relam
         LEFT JOIN pg_stat_user_indexes s ON s.indexrelid = x.indexrelid
         WHERE n.nspname NOT IN ('pg_catalog', 'information_schema')
           AND n.nspname NOT LIKE 'pg\\_toast%' AND n.nspname NOT LIKE 'pg\\_temp%'
           AND t.relkind IN ('r', 'm', 'p')",
        partition_of = partition_parent("i"),
    );
    let rows = query(client, &sql).await?;
    let mut indexes: Vec<IndexStat> = rows
        .iter()
        .map(|r| IndexStat {
            filter_facts: None,
            schema: r.string("schema_name"),
            table: r.string("table_name"),
            name: r.string("index_name"),
            kind: r.string("kind"),
            columns: r.list("key_columns"),
            include: r.list("include_columns"),
            filter: r.text("filter"),
            is_unique: r.flag("indisunique"),
            is_primary_key: r.flag("indisprimary"),
            is_clustered: false,
            is_disabled: !r.flag("usable"),
            state: None,
            maintained: Some(r.flag("indisready")),
            has_statistics: None,
            size_bytes: r.number("estimated_bytes"),
            reads: r.number("idx_scan"),
            writes: None,
            seeks: None,
            scans: r.number("idx_scan"),
            lookups: None,
            last_read: r.text("last_read"),
            definition: r.text("definition"),
            contention: None,
            created_at_latest: None,
            partition_of: r.text("partition_of"),
            opclasses: opclasses(r.text("key_opclasses").as_deref()),
            counters_since: None,
        })
        .collect();

    // Exact sizes take a lock per index; fall back to the page-count estimate if any is held up.
    let sizes = query(
        client,
        "SELECT n.nspname AS schema_name, i.relname AS index_name, pg_relation_size(x.indexrelid) AS bytes
         FROM pg_index x JOIN pg_class i ON i.oid = x.indexrelid JOIN pg_namespace n ON n.oid = i.relnamespace
         WHERE n.nspname NOT IN ('pg_catalog', 'information_schema') AND n.nspname NOT LIKE 'pg\\_toast%'",
    )
    .await;
    if let Ok(sizes) = sizes {
        for row in sizes {
            let (schema, name) = (row.string("schema_name"), row.string("index_name"));
            if let Some(index) = indexes.iter_mut().find(|i| i.schema == schema && i.name == name) {
                index.size_bytes = row.number("bytes").or(index.size_bytes);
            }
        }
    }
    Ok(indexes)
}

/// Per key, its operator class and collation when they aren't the defaults (`key_opclasses`, chr(31)-separated, empty
/// for a key on the defaults). `None` when every key uses the defaults.
fn opclasses(list: Option<&str>) -> Option<Vec<String>> {
    let keys: Vec<String> = list?.split(crate::rows::LIST_SEPARATOR).map(|k| k.trim().to_string()).collect();
    keys.iter().any(|k| !k.is_empty()).then_some(keys)
}

pub(crate) async fn columns(client: &Client) -> Result<Vec<ColumnInfo>> {
    let sql = "SELECT n.nspname AS schema_name, c.relname AS table_name, a.attname AS column_name, a.attnum AS ordinal,
                      format_type(a.atttypid, a.atttypmod) AS data_type, NOT a.attnotnull AS nullable,
                      pg_get_expr(d.adbin, d.adrelid) AS default_value,
                      (a.attidentity <> '' OR a.attgenerated <> '' OR coalesce(pg_get_expr(d.adbin, d.adrelid), '') LIKE 'nextval(%') AS generated
               FROM pg_attribute a
               JOIN pg_class c ON c.oid = a.attrelid JOIN pg_namespace n ON n.oid = c.relnamespace
               LEFT JOIN pg_attrdef d ON d.adrelid = a.attrelid AND d.adnum = a.attnum
               WHERE c.relkind IN ('r', 'p') AND a.attnum > 0 AND NOT a.attisdropped
                 AND n.nspname NOT IN ('pg_catalog', 'information_schema') AND n.nspname NOT LIKE 'pg_toast%'
               ORDER BY 1, 2, 4";
    let rows = query(client, sql).await?;
    Ok(rows
        .iter()
        .map(|r| ColumnInfo {
            schema: r.string("schema_name"),
            table: r.string("table_name"),
            name: r.string("column_name"),
            data_type: r.string("data_type"),
            nullable: r.flag("nullable"),
            default: r.text("default_value"),
            generated: r.flag("generated"),
            ordinal: r.number("ordinal").unwrap_or(0.0) as u32,
        })
        .collect())
}

pub(crate) async fn foreign_keys(client: &Client) -> Result<Vec<ForeignKey>> {
    let sql = "SELECT c.conname, n.nspname AS schema_name, t.relname AS table_name,
                      rn.nspname AS ref_schema, rt.relname AS ref_table,
                      c.confdeltype::text AS delete_rule,
                      (SELECT string_agg(a.attname, chr(31) ORDER BY u.ord)
                         FROM unnest(c.conkey) WITH ORDINALITY AS u(attnum, ord)
                         JOIN pg_attribute a ON a.attrelid = c.conrelid AND a.attnum = u.attnum) AS columns,
                      (SELECT string_agg(a.attname, chr(31) ORDER BY u.ord)
                         FROM unnest(c.confkey) WITH ORDINALITY AS u(attnum, ord)
                         JOIN pg_attribute a ON a.attrelid = c.confrelid AND a.attnum = u.attnum) AS ref_columns
               FROM pg_constraint c
               JOIN pg_class t ON t.oid = c.conrelid JOIN pg_namespace n ON n.oid = t.relnamespace
               JOIN pg_class rt ON rt.oid = c.confrelid JOIN pg_namespace rn ON rn.oid = rt.relnamespace
               WHERE c.contype = 'f' AND n.nspname NOT IN ('pg_catalog', 'information_schema')";
    // A key on a partitioned table is repeated for each partition (conparentid points at the original): keep the original.
    let rows = match query(client, &format!("{sql} AND c.conparentid = 0")).await {
        Ok(rows) => rows,
        Err(_) => query(client, sql).await?,
    };
    Ok(rows
        .iter()
        .map(|r| ForeignKey {
            name: r.string("conname"),
            schema: r.string("schema_name"),
            table: r.string("table_name"),
            columns: r.list("columns"),
            ref_schema: r.string("ref_schema"),
            ref_table: r.string("ref_table"),
            ref_columns: r.list("ref_columns"),
            delete_rule: match r.string("delete_rule").as_str() {
                "c" => Some("CASCADE".into()),
                "n" => Some("SET NULL".into()),
                "d" => Some("SET DEFAULT".into()),
                "r" => Some("RESTRICT".into()),
                _ => None,
            },
        })
        .collect())
}

const SETTINGS: &[&str] = &[
    "pg_stat_statements.max",
    "pg_stat_statements.track",
    "pg_stat_statements.track_planning",
    "shared_buffers",
    "wal_buffers",
    "work_mem",
    "maintenance_work_mem",
    "effective_cache_size",
    "random_page_cost",
    "seq_page_cost",
    "effective_io_concurrency",
    "max_connections",
    "autovacuum",
    "autovacuum_vacuum_scale_factor",
    "autovacuum_analyze_scale_factor",
    "autovacuum_vacuum_threshold",
    "autovacuum_vacuum_max_threshold",
    "autovacuum_analyze_threshold",
    "autovacuum_multixact_freeze_max_age",
    "autovacuum_freeze_max_age",
    "jit",
    "jit_above_cost",
    "track_io_timing",
    "track_wal_io_timing",
    "log_lock_waits",
    "default_statistics_target",
];

pub(crate) async fn settings(client: &Client) -> Result<Vec<Setting>> {
    let names = SETTINGS.iter().map(|s| format!("'{s}'")).collect::<Vec<_>>().join(", ");
    let rows = query(
        client,
        &format!("SELECT name, setting, unit, boot_val FROM pg_settings WHERE name IN ({names}) ORDER BY name"),
    )
    .await?;
    let mut out: Vec<Setting> = rows
        .iter()
        .filter(|r| r.text("setting").is_some())
        .map(|r| {
            let unit = r.text("unit");
            Setting {
                name: r.string("name"),
                value: pretty_setting(&r.string("setting"), unit.as_deref()),
                default_value: r.text("boot_val").map(|v| pretty_setting(&v, unit.as_deref())),
                scope: "server".into(),
            }
        })
        .collect();
    if let Some(row) =
        query_one(client, "SELECT age(datfrozenxid) AS age FROM pg_database WHERE datname = current_database()").await?
    {
        out.push(Setting {
            name: "datfrozenxid_age".into(),
            value: row.string("age"),
            default_value: None,
            scope: "database".into(),
        });
    }
    Ok(out)
}

/// Render a pg_settings value with its unit the way Postgres displays it (`4MB`, `200ms`).
pub fn pretty_setting(value: &str, unit: Option<&str>) -> String {
    let Some(unit) = unit else { return value.to_string() };
    let Ok(number) = value.trim().parse::<i64>() else { return value.to_string() };
    if number < 0 {
        return number.to_string();
    }
    const MEMORY: &[(&str, i64)] = &[("TB", 1 << 40), ("GB", 1 << 30), ("MB", 1 << 20), ("kB", 1 << 10), ("B", 1)];
    const TIME: &[(&str, i64)] = &[("d", 86_400_000), ("h", 3_600_000), ("min", 60_000), ("s", 1_000), ("ms", 1)];
    let (base, scale) = match unit {
        "B" => (MEMORY, 1),
        "kB" => (MEMORY, 1 << 10),
        "8kB" => (MEMORY, 8 << 10),
        "16kB" => (MEMORY, 16 << 10),
        "32kB" => (MEMORY, 32 << 10),
        "MB" => (MEMORY, 1 << 20),
        "ms" => (TIME, 1),
        "s" => (TIME, 1_000),
        "min" => (TIME, 60_000),
        _ => return format!("{value} {unit}"),
    };
    let amount = number * scale;
    if amount == 0 {
        return format!("0{}", base.last().map(|(u, _)| *u).unwrap_or(""));
    }
    for (label, size) in base {
        if amount % size == 0 {
            return format!("{}{label}", amount / size);
        }
    }
    amount.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_render_like_postgres() {
        assert_eq!(pretty_setting("16384", Some("8kB")), "128MB");
        assert_eq!(pretty_setting("4096", Some("kB")), "4MB");
        assert_eq!(pretty_setting("200", Some("ms")), "200ms");
        assert_eq!(pretty_setting("60", Some("s")), "1min");
        assert_eq!(pretty_setting("4", None), "4");
        assert_eq!(pretty_setting("-1", Some("kB")), "-1");
    }

    #[test]
    fn rows_after_a_statistics_reset_come_from_the_planner() {
        // No vacuum or analyze since the counters started: n_live_tup counts only inserts minus deletes since.
        assert!(ROWS_SQL.contains("s.last_autoanalyze IS NULL THEN greatest(c.reltuples, s.n_live_tup)"));
        assert!(ROWS_SQL.contains("WHEN c.relkind = 'p' THEN NULL"));
        let parent = partition_parent("c");
        assert!(parent.contains("c.relispartition") && parent.contains("ih.inhrelid = c.oid"), "{parent}");
    }

    #[test]
    fn operator_classes_and_collations_per_key() {
        assert_eq!(opclasses(None), None);
        assert_eq!(opclasses(Some("")), None);
        assert_eq!(opclasses(Some("\u{1f}")), None, "every key on the defaults");
        assert_eq!(
            opclasses(Some("text_pattern_ops\u{1f}")),
            Some(vec!["text_pattern_ops".to_string(), String::new()])
        );
        assert_eq!(opclasses(Some("COLLATE \"C\"")), Some(vec!["COLLATE \"C\"".to_string()]));
    }
}
