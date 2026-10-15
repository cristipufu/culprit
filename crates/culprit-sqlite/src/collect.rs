//! What a SQLite file says about itself: the library's version, the settings that live in the file, the schema
//! (tables, columns, indexes, foreign keys), the planner's statistics (`sqlite_stat1`) and, when the library has the
//! `dbstat` table, each table's and index's size. Every read is a SELECT or a read-only pragma; rows are never counted
//! (that reads the whole file): without statistics they are unknown.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::time::Instant;

use culprit_core::{
    Capability, CollectorNote, ColumnInfo, Coverage, Engine, Environment, ForeignKey, IndexStat, MissingCapability,
    Result, ResultSet, SNAPSHOT_FORMAT, ServerInfo, Setting, Snapshot, TableStat,
};
use rusqlite::Connection;
use rusqlite::types::ValueRef;

use crate::map_error;

/// The settings that live in the file (or describe it), recorded for the checks and the settings view.
const FILE_PRAGMAS: &[&str] = &["journal_mode", "auto_vacuum", "page_size", "page_count", "freelist_count", "encoding"];

/// What a SQLite snapshot can hold, and what SQLite doesn't keep (`have`, `missing`).
pub fn capabilities(conn: &Connection) -> (Vec<Capability>, Vec<MissingCapability>) {
    let mut coverage = Coverage::with(vec![Capability::Showplan], Vec::new());
    if has_stat1(conn) {
        coverage.delivered(Capability::Statistics);
        coverage.delivered(Capability::TableStats);
    } else {
        coverage.lacking(
            Capability::Statistics,
            "The file has no planner statistics (sqlite_stat1): row counts are unknown and the planner guesses.",
            Some("ANALYZE;".into()),
        );
    }
    coverage.lacking(Capability::IndexUsage, "SQLite keeps no record of which indexes queries use.", None);
    coverage.lacking(Capability::WaitStats, "SQLite has no server, so nothing records waits.", None);
    coverage.lacking(Capability::Sessions, "SQLite has no server, so no sessions to list.", None);
    let (have, missing, _) = coverage.finish();
    (have, missing)
}

fn has_stat1(conn: &Connection) -> bool {
    conn.query_row("SELECT 1 FROM sqlite_schema WHERE type = 'table' AND name = 'sqlite_stat1'", [], |_| Ok(())).is_ok()
}

fn pragma(conn: &Connection, name: &str) -> Option<String> {
    conn.query_row(&format!("PRAGMA {name}"), [], |r| {
        Ok(match r.get_ref(0)? {
            ValueRef::Integer(i) => i.to_string(),
            ValueRef::Real(f) => f.to_string(),
            ValueRef::Text(t) => String::from_utf8_lossy(t).into_owned(),
            _ => String::new(),
        })
    })
    .ok()
}

fn number(conn: &Connection, name: &str) -> Option<f64> {
    pragma(conn, name).and_then(|v| v.parse::<f64>().ok())
}

pub fn server_info(
    conn: &Connection,
    path: &Path,
    have: Vec<Capability>,
    missing: Vec<MissingCapability>,
) -> ServerInfo {
    let version = conn.query_row("SELECT sqlite_version()", [], |r| r.get::<_, String>(0)).unwrap_or_default();
    let size = match (number(conn, "page_count"), number(conn, "page_size")) {
        (Some(pages), Some(size)) => Some(pages * size),
        _ => None,
    };
    ServerInfo {
        engine: Engine::Sqlite,
        product: "SQLite".into(),
        version,
        edition: None,
        cloud: None,
        platform: None,
        database: path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string()),
        database_size_bytes: size,
        started_at: None,
        read_only_session: true,
        capabilities: have,
        missing,
    }
}

pub fn snapshot(
    conn: &Connection,
    path: &Path,
    connection: &str,
    environment: Environment,
    started: Instant,
) -> Result<Snapshot> {
    let taken_at = conn
        .query_row("SELECT strftime('%Y-%m-%dT%H:%M:%fZ', 'now')", [], |r| r.get::<_, String>(0))
        .map_err(|e| map_error(&e))?;
    let (have, missing) = capabilities(conn);
    let mut notes = Vec::new();
    let mut note =
        |collector: &str, message: String| notes.push(CollectorNote { collector: collector.into(), message });

    let schema = read_schema(conn).map_err(|e| map_error(&e))?;
    let rows = table_rows(conn);
    let sizes = match object_sizes(conn) {
        Ok(sizes) => sizes,
        Err(_) => {
            note(
                "sizes",
                "This SQLite library has no dbstat table, so each table's and index's size is unknown.".into(),
            );
            HashMap::new()
        }
    };

    let mut tables = Vec::new();
    let mut indexes = Vec::new();
    let mut columns = Vec::new();
    let mut foreign_keys = Vec::new();
    for t in &schema.tables {
        let table_columns = read_columns(conn, &t.name).map_err(|e| map_error(&e))?;
        let primary_key: Vec<String> = {
            let mut pk: Vec<(i64, String)> =
                table_columns.iter().filter(|c| c.pk > 0).map(|c| (c.pk, c.name.clone())).collect();
            pk.sort();
            pk.into_iter().map(|(_, n)| n).collect()
        };
        let table_indexes =
            read_indexes(conn, &t.name, t.without_rowid, &schema.index_sql, &sizes).map_err(|e| map_error(&e))?;
        let index_bytes = table_indexes.iter().filter_map(|i| i.size_bytes).fold(0.0, |a, b| a + b);
        let data_bytes = sizes.get(&t.name).copied();
        tables.push(table(t, rows.get(&t.name).copied(), data_bytes, index_bytes, !primary_key.is_empty()));
        for c in &table_columns {
            columns.push(ColumnInfo {
                schema: "main".into(),
                table: t.name.clone(),
                name: c.name.clone(),
                data_type: c.data_type.clone(),
                nullable: !c.not_null && c.pk == 0,
                default: c.default.clone(),
                generated: c.hidden >= 2,
                ordinal: c.ordinal,
            });
        }
        indexes.extend(table_indexes);
        foreign_keys.extend(read_foreign_keys(conn, &t.name).map_err(|e| map_error(&e))?);
    }

    let settings = FILE_PRAGMAS
        .iter()
        .filter_map(|name| {
            pragma(conn, name).map(|value| Setting {
                name: (*name).into(),
                value,
                default_value: None,
                scope: "file".into(),
            })
        })
        .collect();

    let server = server_info(conn, path, have, missing);
    Ok(Snapshot {
        format: SNAPSHOT_FORMAT,
        id: taken_at.replace(['-', ':'], ""),
        connection: connection.into(),
        environment,
        taken_at,
        duration_ms: started.elapsed().as_secs_f64() * 1000.0,
        server,
        workload: None,
        tables,
        indexes,
        columns,
        foreign_keys,
        missing_indexes: Vec::new(),
        statistics: Vec::new(),
        waits: Vec::new(),
        resources: Vec::new(),
        settings,
        notes,
        pressure: None,
        options: None,
        query_families: Vec::new(),
        plan_cache: None,
        tuning: None,
        deadlocks: None,
        constraints: Vec::new(),
        histograms: None,
        escalations_before: None,
        sizes_before: None,
    })
}

struct SchemaTable {
    name: String,
    without_rowid: bool,
}

struct Schema {
    tables: Vec<SchemaTable>,
    /// Each index's CREATE statement, for a partial index's WHERE (autoindexes have none).
    index_sql: HashMap<String, String>,
}

fn read_schema(conn: &Connection) -> rusqlite::Result<Schema> {
    let mut stmt = conn.prepare(
        "SELECT type, name, coalesce(sql, '') FROM sqlite_schema
          WHERE type IN ('table', 'index') AND name NOT LIKE 'sqlite\\_%' ESCAPE '\\'
          ORDER BY name",
    )?;
    let mut tables = Vec::new();
    let mut index_sql = HashMap::new();
    let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?)))?;
    for row in rows {
        let (kind, name, sql) = row?;
        if kind == "table" {
            // A virtual table (FTS5, R*Tree) is a module's storage, not rows the planner reads as a table.
            if sql.to_ascii_uppercase().starts_with("CREATE VIRTUAL TABLE") {
                continue;
            }
            let upper = sql.to_ascii_uppercase();
            tables.push(SchemaTable { name, without_rowid: upper.contains("WITHOUT ROWID") });
        } else {
            index_sql.insert(name, sql);
        }
    }
    Ok(Schema { tables, index_sql })
}

/// Rows per table from the planner's statistics: the first number of `sqlite_stat1.stat` is the table's row count.
fn table_rows(conn: &Connection) -> HashMap<String, f64> {
    let mut out = HashMap::new();
    if !has_stat1(conn) {
        return out;
    }
    let Ok(mut stmt) = conn.prepare("SELECT tbl, stat FROM sqlite_stat1") else { return out };
    let Ok(rows) = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))) else { return out };
    for (table, stat) in rows.flatten() {
        if let Some(n) = stat.split_whitespace().next().and_then(|n| n.parse::<f64>().ok()) {
            let entry = out.entry(table).or_insert(n);
            *entry = entry.max(n);
        }
    }
    out
}

/// Bytes per table and index, from `dbstat` (when the library has it).
fn object_sizes(conn: &Connection) -> rusqlite::Result<HashMap<String, f64>> {
    let mut stmt = conn.prepare("SELECT name, sum(pgsize) FROM dbstat GROUP BY name")?;
    let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, f64>(1)?)))?;
    rows.collect()
}

fn table(t: &SchemaTable, rows: Option<f64>, data: Option<f64>, index_bytes: f64, has_pk: bool) -> TableStat {
    TableStat {
        schema: "main".into(),
        name: t.name.clone(),
        rows,
        total_bytes: data.map(|d| d + index_bytes),
        data_bytes: data,
        index_bytes: data.map(|_| index_bytes),
        has_primary_key: has_pk,
        is_heap: None,
        // SQLite keeps no activity counters for a table: unknown, never zero.
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
        xid_age: None,
        forwarded_fetches: None,
        storage: None,
        in_row_bytes: None,
        lob_bytes: None,
        overflow_bytes: None,
        partition_of: None,
        counters_since: None,
    }
}

struct Column {
    name: String,
    data_type: String,
    not_null: bool,
    default: Option<String>,
    pk: i64,
    hidden: i64,
    ordinal: u32,
}

fn read_columns(conn: &Connection, table: &str) -> rusqlite::Result<Vec<Column>> {
    let mut stmt =
        conn.prepare("SELECT cid, name, type, \"notnull\", dflt_value, pk, hidden FROM pragma_table_xinfo(?1)")?;
    let rows = stmt.query_map([table], |r| {
        Ok(Column {
            ordinal: r.get::<_, i64>(0)? as u32 + 1,
            name: r.get(1)?,
            data_type: r.get::<_, String>(2)?,
            not_null: r.get::<_, i64>(3)? != 0,
            default: r.get(4)?,
            pk: r.get(5)?,
            hidden: r.get(6)?,
        })
    })?;
    rows.collect()
}

fn read_indexes(
    conn: &Connection,
    table: &str,
    without_rowid: bool,
    index_sql: &HashMap<String, String>,
    sizes: &HashMap<String, f64>,
) -> rusqlite::Result<Vec<IndexStat>> {
    let mut list = conn.prepare("SELECT name, \"unique\", origin, partial FROM pragma_index_list(?1)")?;
    let indexes: Vec<(String, bool, String, bool)> = list
        .query_map([table], |r| {
            Ok((r.get(0)?, r.get::<_, i64>(1)? != 0, r.get::<_, String>(2)?, r.get::<_, i64>(3)? != 0))
        })?
        .collect::<rusqlite::Result<_>>()?;
    let mut out = Vec::new();
    for (name, unique, origin, partial) in indexes {
        let mut info = conn.prepare(
            "SELECT coalesce(name, '(expression)'), \"desc\" FROM pragma_index_xinfo(?1) WHERE key = 1 ORDER BY seqno",
        )?;
        let columns: Vec<String> = info
            .query_map([&name], |r| {
                let column: String = r.get(0)?;
                Ok(if r.get::<_, i64>(1)? != 0 { format!("{column} DESC") } else { column })
            })?
            .collect::<rusqlite::Result<_>>()?;
        let sql = index_sql.get(&name).cloned().filter(|s| !s.is_empty());
        let filter = if partial { sql.as_deref().and_then(partial_filter) } else { None };
        let primary = origin == "pk";
        out.push(IndexStat {
            schema: "main".into(),
            table: table.into(),
            name: name.clone(),
            kind: "BTREE".into(),
            columns,
            include: Vec::new(),
            filter,
            is_unique: unique,
            is_primary_key: primary,
            // A WITHOUT ROWID table is stored in its primary key's b-tree.
            is_clustered: primary && without_rowid,
            is_disabled: false,
            size_bytes: sizes.get(&name).copied(),
            // SQLite keeps no usage counters: unknown, never zero.
            reads: None,
            writes: None,
            seeks: None,
            scans: None,
            lookups: None,
            last_read: None,
            definition: sql,
            contention: None,
            created_at_latest: None,
            partition_of: None,
            opclasses: None,
            counters_since: None,
        });
    }
    Ok(out)
}

/// The WHERE of a partial index's CREATE statement.
fn partial_filter(sql: &str) -> Option<String> {
    let upper = sql.to_ascii_uppercase();
    let at = upper.rfind(" WHERE ")?;
    Some(sql[at + 7..].trim().trim_end_matches(';').trim().to_string())
}

/// One foreign key's parent table, its columns, and the parent columns each refers to (`None`: the primary key's).
type KeyParts = (String, Vec<String>, Vec<Option<String>>);

fn read_foreign_keys(conn: &Connection, table: &str) -> rusqlite::Result<Vec<ForeignKey>> {
    let mut stmt =
        conn.prepare("SELECT id, seq, \"table\", \"from\", \"to\" FROM pragma_foreign_key_list(?1) ORDER BY id, seq")?;
    let mut grouped: BTreeMap<i64, KeyParts> = BTreeMap::new();
    let rows = stmt.query_map([table], |r| {
        Ok((r.get::<_, i64>(0)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?, r.get::<_, Option<String>>(4)?))
    })?;
    for row in rows {
        let (id, parent, from, to) = row?;
        let entry = grouped.entry(id).or_insert_with(|| (parent, Vec::new(), Vec::new()));
        entry.1.push(from);
        entry.2.push(to);
    }
    let mut out = Vec::new();
    for (id, (parent, from, to)) in grouped {
        // A key that names no parent columns refers to the parent's primary key.
        let ref_columns = if to.iter().all(Option::is_some) {
            to.into_iter().flatten().collect()
        } else {
            read_columns(conn, &parent)?.into_iter().filter(|c| c.pk > 0).map(|c| c.name).collect()
        };
        out.push(ForeignKey {
            name: format!("{table}_fk_{id}"),
            schema: "main".into(),
            table: table.into(),
            columns: from,
            ref_schema: "main".into(),
            ref_table: parent,
            ref_columns,
        });
    }
    Ok(out)
}

/// A statement's rows, up to `limit`, on the read-only connection.
pub fn run(conn: &Connection, sql: &str, limit: usize) -> Result<ResultSet> {
    let started = Instant::now();
    let mut stmt = conn.prepare(sql).map_err(|e| map_error(&e))?;
    let columns: Vec<String> = stmt.column_names().iter().map(|c| c.to_string()).collect();
    let mut rows = stmt.query([]).map_err(|e| map_error(&e))?;
    let mut out = Vec::new();
    let mut truncated = false;
    while let Some(row) = rows.next().map_err(|e| map_error(&e))? {
        if out.len() >= limit {
            truncated = true;
            break;
        }
        let mut values = Vec::with_capacity(columns.len());
        for i in 0..columns.len() {
            values.push(match row.get_ref(i).map_err(|e| map_error(&e))? {
                ValueRef::Null => serde_json::Value::Null,
                ValueRef::Integer(n) => serde_json::Value::from(n),
                ValueRef::Real(f) => serde_json::Value::from(f),
                ValueRef::Text(t) => serde_json::Value::from(String::from_utf8_lossy(t).into_owned()),
                ValueRef::Blob(b) => serde_json::Value::from(format!("<{} bytes>", b.len())),
            });
        }
        out.push(values);
    }
    Ok(ResultSet {
        columns,
        rows: out,
        truncated,
        elapsed_ms: started.elapsed().as_secs_f64() * 1000.0,
        messages: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_partial_index_keeps_its_filter() {
        assert_eq!(
            partial_filter("CREATE INDEX ix ON t (a) WHERE deleted_at IS NULL").as_deref(),
            Some("deleted_at IS NULL")
        );
        assert_eq!(partial_filter("CREATE INDEX ix ON t (a)"), None);
    }
}
