//! What a SQLite file says about itself: the library's version, the settings that live in the file, the schema
//! (tables, columns, indexes, foreign keys), the planner's statistics (`sqlite_stat1`) and, when the library has the
//! `dbstat` table and the file is small enough to read whole without holding up the application's writes
//! ([`whole_file_read`]), each table's and index's size and each table's rows. Every read is a SELECT or a read-only
//! pragma.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;
use std::time::{Duration, Instant};

use culprit_core::{
    Capability, ColumnInfo, Coverage, Engine, Environment, ForeignKey, IndexStat, MissingCapability, Result, ResultSet,
    SNAPSHOT_FORMAT, ServerInfo, Setting, Snapshot, TableStat,
};
use rusqlite::Connection;
use rusqlite::types::ValueRef;

use crate::map_error;

/// The settings that live in the file (or describe it), recorded for the checks and the settings view.
const FILE_PRAGMAS: &[&str] = &["journal_mode", "auto_vacuum", "page_size", "page_count", "freelist_count", "encoding"];

/// The largest file read whole for sizes and row counts. `dbstat` visits every page inside one read transaction: in
/// WAL mode the application's writers carry on beside it, but with a rollback journal it holds the lock a writer needs
/// to commit, so writes wait for it (and fail with SQLITE_BUSY past their busy_timeout).
const WHOLE_READ_MAX_BYTES_WAL: f64 = 4.0 * 1024.0 * 1024.0 * 1024.0;
const WHOLE_READ_MAX_BYTES_ROLLBACK: f64 = 128.0 * 1024.0 * 1024.0;
/// However large the file, the read stops after this long and sizes stay unknown.
const WHOLE_READ_BUDGET_WAL: Duration = Duration::from_secs(3);
const WHOLE_READ_BUDGET_ROLLBACK: Duration = Duration::from_millis(500);

/// Whether the file may be read whole (`dbstat`) for sizes and row counts, and for how long at most; why not otherwise.
pub fn whole_file_read(conn: &Connection) -> std::result::Result<Duration, String> {
    if conn.prepare("SELECT 1 FROM dbstat LIMIT 0").is_err() {
        return Err("this SQLite library has no dbstat table to measure them".into());
    }
    let bytes = number(conn, "page_count").unwrap_or(0.0) * number(conn, "page_size").unwrap_or(0.0);
    let wal = pragma(conn, "journal_mode").is_some_and(|m| m.eq_ignore_ascii_case("wal"));
    let (limit, budget) = if wal {
        (WHOLE_READ_MAX_BYTES_WAL, WHOLE_READ_BUDGET_WAL)
    } else {
        (WHOLE_READ_MAX_BYTES_ROLLBACK, WHOLE_READ_BUDGET_ROLLBACK)
    };
    if bytes > limit {
        return Err(if wal {
            format!("the file is {}: Culprit doesn't read that much of it to count", culprit_core::format::bytes(bytes))
        } else {
            format!(
                "the file is {} with a rollback journal, where reading it whole would make the application's writes wait",
                culprit_core::format::bytes(bytes)
            )
        });
    }
    Ok(budget)
}

/// What a SQLite snapshot can hold, and what SQLite doesn't keep (`have`, `missing`). `rows`: why row counts are
/// unknown, if they are.
pub fn capabilities(
    conn: &Connection,
    rows: std::result::Result<(), String>,
) -> (Vec<Capability>, Vec<MissingCapability>) {
    let mut coverage = Coverage::with(vec![Capability::Showplan], Vec::new());
    let stat1 = has_stat1(conn);
    if stat1 {
        coverage.delivered(Capability::Statistics);
    } else {
        coverage.lacking(
            Capability::Statistics,
            "The file has no planner statistics (sqlite_stat1): the planner guesses how many rows each step reads.",
            Some("ANALYZE;".into()),
        );
    }
    match rows {
        Ok(()) => coverage.delivered(Capability::TableStats),
        Err(_) if stat1 => coverage.delivered(Capability::TableStats),
        Err(why) => coverage.lacking(Capability::TableStats, format!("Row counts are unknown: {why}."), None),
    }
    coverage.lacking(
        Capability::QueryLog,
        "SQLite keeps no history of the statements it ran. Culprit can read them from the log Rails, Django, Entity \
         Framework Core or Phoenix already writes: add it to the connection (`culprit conn set <name> --query-log <file>`, \
         or Add query log in the app).",
        None,
    );
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
    // What the file's sources could read (`crate::sources`): a source that can't be read is a note.
    let mut coverage = Coverage::default();

    let schema = read_schema(conn).map_err(|e| map_error(&e))?;
    let without_rowid: HashSet<&str> =
        schema.tables.iter().filter(|t| t.without_rowid).map(|t| t.name.as_str()).collect();
    // Counted rows where the file was read whole; the planner's statistics otherwise.
    let (sizes, counted, whole) = match whole_file_read(conn).and_then(|budget| {
        object_sizes(conn, &without_rowid, budget)
            .map_err(|e| format!("reading every page stopped ({})", map_error(&e).message.to_lowercase()))
    }) {
        Ok((sizes, counted)) => (sizes, counted, Ok(())),
        Err(why) => {
            let error = culprit_core::Error::internal(format!("Table and index sizes are unknown: {why}."));
            coverage.source_failed(&crate::sources::SIZES, &error);
            (HashMap::new(), HashMap::new(), Err(why))
        }
    };
    let mut rows = table_rows(conn);
    rows.extend(counted);
    let (have, missing) = capabilities(conn, whole);
    let statistics = if has_stat1(conn) {
        coverage.read(
            &crate::sources::PLANNER_STATISTICS,
            (|| {
                let mut query = conn.prepare("SELECT idx FROM sqlite_stat1 WHERE idx IS NOT NULL")?;
                query.query_map([], |r| r.get::<_, String>(0))?.collect::<rusqlite::Result<HashSet<String>>>()
            })()
            .map_err(|e| map_error(&e)),
        )
    } else {
        Some(HashSet::new())
    };

    let mut tables = Vec::new();
    let mut indexes = Vec::new();
    let mut columns = Vec::new();
    let mut foreign_keys = Vec::new();
    for t in &schema.tables {
        let virtual_table = t.kind == "virtual";
        let table_columns = match read_columns(conn, &t.name) {
            Ok(columns) => columns,
            Err(error) if virtual_table => {
                // The application can load a virtual-table module that Culprit's SQLite library doesn't include.
                // Its CREATE statement remains useful; missing columns must not lose the rest of the database.
                coverage.note(
                    crate::sources::CATALOG.name,
                    format!(
                        "Columns of main.{} are unavailable: {}. The table's original definition is retained.",
                        t.name,
                        map_error(&error).message
                    ),
                );
                Vec::new()
            }
            Err(error) => return Err(map_error(&error)),
        };
        let primary_key: Vec<String> = {
            let mut pk: Vec<(i64, String)> =
                table_columns.iter().filter(|c| c.pk > 0).map(|c| (c.pk, c.name.clone())).collect();
            pk.sort();
            pk.into_iter().map(|(_, n)| n).collect()
        };
        let mut table_indexes = if virtual_table {
            Vec::new()
        } else {
            read_indexes(conn, &t.name, t.without_rowid, &schema.index_sql, &sizes).map_err(|e| map_error(&e))?
        };
        for index in &mut table_indexes {
            index.has_statistics = statistics.as_ref().map(|names| names.contains(&index.name));
        }
        let index_bytes = table_indexes.iter().filter_map(|i| i.size_bytes).fold(0.0, |a, b| a + b);
        let data_bytes = sizes.get(&t.name).copied();
        tables.push(table(t, rows.get(&t.name).copied(), data_bytes, index_bytes, !primary_key.is_empty()));
        for c in table_columns.iter().filter(|c| c.hidden != 1) {
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
        if !virtual_table {
            foreign_keys.extend(read_foreign_keys(conn, &t.name).map_err(|e| map_error(&e))?);
        }
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
    let mut operations = culprit_core::operations::OperationalStats::default();
    if pragma(conn, "journal_mode").is_some_and(|m| m.eq_ignore_ascii_case("wal")) {
        let mut wal = path.as_os_str().to_os_string();
        wal.push("-wal");
        let size = match std::fs::metadata(std::path::PathBuf::from(wal)) {
            Ok(metadata) => Ok(metadata.len() as f64),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(0.0),
            Err(_) => {
                Err(culprit_core::Error::internal("WAL file size could not be read; no file contents were inspected."))
            }
        };
        let wal_bytes = coverage.read(&crate::sources::WAL_FILE, size);
        operations.sqlite = Some(culprit_core::operations::SqliteFileHealth { wal_bytes, before: None });
    }
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
        notes: coverage.finish().2,
        pressure: None,
        options: None,
        query_families: Vec::new(),
        plan_cache: None,
        tuning: None,
        deadlocks: None,
        constraints: Vec::new(),
        histograms: None,
        escalations_before: None,
        contention_before: None,
        evictions_before: None,
        sizes_before: None,
        operations,
    })
}

struct SchemaTable {
    name: String,
    without_rowid: bool,
    kind: String,
    definition: Option<String>,
}

struct Schema {
    tables: Vec<SchemaTable>,
    /// Each index's CREATE statement, for a partial index's WHERE (autoindexes have none).
    index_sql: HashMap<String, String>,
}

fn read_schema(conn: &Connection) -> rusqlite::Result<Schema> {
    let mut stmt = conn.prepare(
        "/* culprit */ SELECT s.type, s.name, s.sql, t.type, t.wr
           FROM main.sqlite_schema AS s
           LEFT JOIN pragma_table_list AS t ON t.schema = 'main' AND t.name = s.name AND s.type = 'table'
          WHERE s.type IN ('table', 'index') AND s.name NOT LIKE 'sqlite\\_%' ESCAPE '\\'
            AND coalesce(t.type, '') <> 'shadow'
          ORDER BY s.name",
    )?;
    let mut tables = Vec::new();
    let mut index_sql = HashMap::new();
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, Option<String>>(2)?,
            r.get::<_, Option<String>>(3)?,
            r.get::<_, Option<bool>>(4)?,
        ))
    })?;
    for row in rows {
        let (kind, name, sql, table_kind, without_rowid) = row?;
        if kind == "table" {
            // SQLite identifies module-owned storage itself. Keep the virtual table people query, not its shadow
            // tables; names alone cannot distinguish an application's table from a module's internal storage.
            tables.push(SchemaTable {
                name,
                without_rowid: without_rowid.unwrap_or(false),
                kind: table_kind.unwrap_or_else(|| "table".into()),
                definition: sql,
            });
        } else if let Some(sql) = sql {
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

/// Bytes per table and index, and each table's rows, from one pass over `dbstat` stopped after `budget`. A rowid
/// table's rows are the cells of its leaf pages (interior cells point to pages); a WITHOUT ROWID table is a b-tree of
/// entries, every one a row, interior pages included.
#[allow(clippy::type_complexity)]
fn object_sizes(
    conn: &Connection,
    without_rowid: &HashSet<&str>,
    budget: Duration,
) -> rusqlite::Result<(HashMap<String, f64>, HashMap<String, f64>)> {
    let started = Instant::now();
    conn.progress_handler(1_000, Some(move || started.elapsed() > budget))?;
    let read = (|| {
        let mut stmt = conn.prepare(
            "SELECT name, sum(pgsize),
                    sum(CASE WHEN pagetype = 'leaf' THEN ncell ELSE 0 END),
                    sum(CASE WHEN pagetype <> 'overflow' THEN ncell ELSE 0 END)
               FROM dbstat GROUP BY name",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, f64>(1)?, r.get::<_, f64>(2)?, r.get::<_, f64>(3)?))
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
    })();
    conn.progress_handler(0, None::<fn() -> bool>)?;
    let mut sizes = HashMap::new();
    let mut counted = HashMap::new();
    for (name, bytes, leaf_cells, cells) in read? {
        counted.insert(name.clone(), if without_rowid.contains(name.as_str()) { cells } else { leaf_cells });
        sizes.insert(name, bytes);
    }
    Ok((sizes, counted))
}

fn table(t: &SchemaTable, rows: Option<f64>, data: Option<f64>, index_bytes: f64, has_pk: bool) -> TableStat {
    TableStat {
        schema: "main".into(),
        name: t.name.clone(),
        kind: Some(t.kind.clone()),
        definition: t.definition.clone(),
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
        let sql = index_sql.get(&name).cloned().filter(|s| !s.is_empty());
        // An expression key is named by its expression, from the index's CREATE statement: `lower(email)` and
        // `substr(email, 1, 3)` are different indexes. One whose text can't be read is named after its index, so it
        // never matches another's. A key compared by a collation other than BINARY says so (NOCASE serves different
        // queries than the column's own order).
        let terms = sql.as_deref().map(index_terms).unwrap_or_default();
        let mut info = conn
            .prepare("SELECT seqno, name, \"desc\", coll FROM pragma_index_xinfo(?1) WHERE key = 1 ORDER BY seqno")?;
        let keys: Vec<(String, String)> = info
            .query_map([&name], |r| {
                let seqno: i64 = r.get(0)?;
                let column = match r.get::<_, Option<String>>(1)? {
                    Some(c) => c,
                    None => match terms.get(seqno as usize) {
                        Some(term) => format!("({term})"),
                        None => format!("(expression {} of {name})", seqno + 1),
                    },
                };
                let column = if r.get::<_, i64>(2)? != 0 { format!("{column} DESC") } else { column };
                let coll: String = r.get::<_, Option<String>>(3)?.unwrap_or_default();
                let coll = if coll.is_empty() || coll.eq_ignore_ascii_case("BINARY") {
                    String::new()
                } else {
                    format!("COLLATE {coll}")
                };
                Ok((column, coll))
            })?
            .collect::<rusqlite::Result<_>>()?;
        let opclasses = keys.iter().any(|(_, c)| !c.is_empty()).then(|| keys.iter().map(|(_, c)| c.clone()).collect());
        let columns: Vec<String> = keys.into_iter().map(|(c, _)| c).collect();
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
            state: None,
            maintained: None,
            has_statistics: None,
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
            opclasses,
            counters_since: None,
        });
    }
    Ok(out)
}

/// The key terms of a CREATE INDEX statement, as written: `CREATE INDEX i ON t (lower(email), status DESC)` →
/// `lower(email)`, `status`. Each without its ASC, DESC or COLLATE, which the catalog reports apart.
fn index_terms(sql: &str) -> Vec<String> {
    let upper = sql.to_ascii_uppercase();
    let Some(on) = upper.find(" ON ") else { return Vec::new() };
    let Some(open) = sql[on..].find('(').map(|i| on + i) else { return Vec::new() };
    let (mut depth, mut start, mut quote, mut out) = (0i32, open + 1, None::<char>, Vec::new());
    for (i, c) in sql[open..].char_indices().map(|(i, c)| (open + i, c)) {
        match (quote, c) {
            (Some(q), _) if c == q => quote = None,
            (Some(_), _) => {}
            (None, '\'' | '"' | '`' | '[') => quote = Some(if c == '[' { ']' } else { c }),
            (None, '(') => depth += 1,
            (None, ')') => {
                depth -= 1;
                if depth == 0 {
                    out.push(sql[start..i].to_string());
                    break;
                }
            }
            (None, ',') if depth == 1 => {
                out.push(sql[start..i].to_string());
                start = i + 1;
            }
            _ => {}
        }
    }
    out.into_iter()
        .map(|t| {
            let mut t = t.trim().to_string();
            loop {
                let u = t.to_ascii_uppercase();
                let cut = [" ASC", " DESC"].iter().find(|w| u.ends_with(*w)).map(|w| t.len() - w.len());
                let cut = cut.or_else(|| u.rfind(" COLLATE ").filter(|at| !u[*at + 9..].trim().contains(' ')));
                match cut {
                    Some(at) => t = t[..at].trim_end().to_string(),
                    None => break t,
                }
            }
        })
        .collect()
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
    fn an_unavailable_virtual_module_does_not_fail_the_scan() {
        let conn = Connection::open_in_memory().unwrap();
        // Emulate a catalog written by an application with a module that this reader does not have.
        conn.execute_batch(
            "CREATE TABLE ordinary(id INTEGER PRIMARY KEY);
             PRAGMA writable_schema = ON;
             INSERT INTO sqlite_schema(type, name, tbl_name, rootpage, sql)
             VALUES('table', 'vectors', 'vectors', 0, 'CREATE VIRTUAL TABLE vectors USING module_not_installed(vector)');
             PRAGMA writable_schema = OFF;
             PRAGMA schema_version = 100;",
        )
        .unwrap();
        let s = snapshot(&conn, Path::new("demo.db"), "demo", Environment::Local, Instant::now()).unwrap();
        assert!(s.tables.iter().any(|t| t.name == "ordinary" && t.rows == Some(0.0)));
        assert!(s.tables.iter().any(|t| t.name == "vectors" && t.is_virtual() && t.definition.is_some()));
        assert!(s.notes.iter().any(|n| n.collector == "catalog" && n.message.contains("module_not_installed")));
    }

    #[test]
    fn virtual_tables_keep_their_schema_without_exposing_module_storage() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE documents (id INTEGER PRIMARY KEY, title TEXT UNIQUE);
             CREATE INDEX documents_title ON documents(title);
             CREATE VIRTUAL TABLE search USING fts5(title, tokenize = 'unicode61');
             INSERT INTO search(title) VALUES ('sample document');
             CREATE VIRTUAL TABLE bounds USING rtree(id, min_x, max_x, min_y, max_y);
             INSERT INTO bounds VALUES (1, 0, 1, 0, 1);
             CREATE TABLE other_content (id INTEGER PRIMARY KEY);
             CREATE TABLE search_archive (id INTEGER PRIMARY KEY);
             CREATE TABLE ordinary (id INTEGER PRIMARY KEY, note TEXT DEFAULT 'WITHOUT ROWID', doubled INTEGER GENERATED ALWAYS AS (id * 2) STORED);
             ATTACH ':memory:' AS other;
             CREATE TABLE other.documents (id INTEGER PRIMARY KEY);",
        )
        .unwrap();
        let s = snapshot(&conn, Path::new("demo.db"), "demo", Environment::Local, Instant::now()).unwrap();
        let names: Vec<_> = s.tables.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, ["bounds", "documents", "ordinary", "other_content", "search", "search_archive"]);
        for name in ["search", "bounds"] {
            let t = s.tables.iter().find(|t| t.name == name).unwrap();
            assert!(t.is_virtual());
            assert!(t.definition.as_deref().unwrap().starts_with("CREATE VIRTUAL TABLE"));
            assert_eq!((t.rows, t.total_bytes, t.data_bytes, t.index_bytes), (None, None, None, None));
            assert!(!s.indexes.iter().any(|i| i.table == name));
        }
        let search_columns: Vec<_> =
            s.columns.iter().filter(|c| c.table == "search").map(|c| c.name.as_str()).collect();
        assert_eq!(search_columns, ["title"]);
        assert!(s.columns.iter().any(|c| c.table == "ordinary" && c.name == "doubled" && c.generated));
        assert!(s.columns.iter().all(|c| names.contains(&c.table.as_str())));
        assert!(s.indexes.iter().all(|i| i.table == "documents"));
        assert!(s.tables.iter().filter(|t| !t.is_virtual()).all(|t| t.rows == Some(0.0)));
    }

    #[test]
    fn a_partial_index_keeps_its_filter() {
        assert_eq!(
            partial_filter("CREATE INDEX ix ON t (a) WHERE deleted_at IS NULL").as_deref(),
            Some("deleted_at IS NULL")
        );
        assert_eq!(partial_filter("CREATE INDEX ix ON t (a)"), None);
    }

    #[test]
    fn statistics_on_an_older_index_do_not_cover_an_index_added_later() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE items(id INTEGER PRIMARY KEY, a TEXT, b TEXT); CREATE INDEX old_index ON items(a); INSERT INTO items(a,b) VALUES ('a','b'); ANALYZE; CREATE INDEX new_index ON items(b);").unwrap();
        let s = snapshot(&conn, Path::new("demo.db"), "demo", Environment::Local, Instant::now()).unwrap();
        assert!(s.server.has(Capability::Statistics));
        assert_eq!(s.indexes.iter().find(|i| i.name == "old_index").unwrap().has_statistics, Some(true));
        assert_eq!(s.indexes.iter().find(|i| i.name == "new_index").unwrap().has_statistics, Some(false));
    }

    // The false flags: two expression indexes were both "(expression)", and a NOCASE index the same as the plain one,
    // so each pair read as duplicates to drop.
    #[test]
    fn expression_keys_and_collations_tell_indexes_apart() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE u (id INTEGER PRIMARY KEY, email TEXT, status TEXT);
             CREATE INDEX a ON u (lower(email));
             CREATE INDEX b ON u (substr(email, 1, 3) DESC);
             CREATE INDEX c ON u (status COLLATE NOCASE);
             CREATE INDEX d ON u (status);",
        )
        .unwrap();
        let sql: HashMap<String, String> = conn
            .prepare("SELECT name, sql FROM sqlite_master WHERE type = 'index'")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        let indexes = read_indexes(&conn, "u", false, &sql, &HashMap::new()).unwrap();
        let of = |n: &str| indexes.iter().find(|i| i.name == n).unwrap();
        assert_eq!(of("a").columns, vec!["(lower(email))"]);
        assert_eq!(of("b").columns, vec!["(substr(email, 1, 3)) DESC"]);
        assert_eq!(of("c").opclasses, Some(vec!["COLLATE NOCASE".to_string()]));
        assert_eq!((of("d").columns.clone(), of("d").opclasses.clone()), (vec!["status".to_string()], None));
    }
}
