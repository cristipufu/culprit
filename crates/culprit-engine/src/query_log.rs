//! A query log an application's framework already writes, read into a scan's workload (`culprit-querylog`), for an
//! engine that keeps no statement history of its own (SQLite). Nothing in the application changes. The file is read
//! line by line and never kept: only the statement shapes, with their literal values taken out, reach the report.
//!
//! A connection names its log, or Culprit finds it ([`discover`]): frameworks write their log beside the database by
//! convention (Rails keeps `db/production.sqlite3` and logs to `log/production.log`).

use std::io::{BufRead, BufReader};
use std::path::Path;

use culprit_core::{Capability, CollectorNote, Error, ErrorCode, Provider, Result, Snapshot, SnapshotOptions};
use culprit_querylog::{Aggregator, Reader};

/// Shared source descriptor, independent of which database provider supplied the catalog.
const QUERY_LOG_SOURCE: culprit_core::Source =
    culprit_core::Source { name: "query log", reads: "the application's query log", delivers: &[Capability::QueryLog] };
/// Lines read to tell the log's format.
const SAMPLE_LINES: usize = 5_000;
/// At most this much of a file is read to tell whether it's a query log, however long its lines.
const SAMPLE_BYTES: u64 = 4 * 1024 * 1024;
/// Logs looked at when finding one, so a folder full of logs costs a few reads at most.
const DISCOVER_FILES: usize = 20;
/// The environments frameworks name their databases and logs after (Rails: `db/production.sqlite3`, `log/production.log`).
const ENVIRONMENTS: &[&str] = &["development", "production", "staging", "test"];

/// The format of the log at `path` ("Rails"), told from its first lines; an error that says why when it holds no
/// statement in a format Culprit reads (most often: the framework logs SQL only at debug level).
pub fn recognize(path: &Path) -> Result<&'static str> {
    let file = std::fs::File::open(path)
        .map_err(|e| Error::new(ErrorCode::NotFound, format!("Can't read {}: {e}", path.display())))?;
    let sample: Vec<String> = BufReader::new(std::io::Read::take(file, SAMPLE_BYTES))
        .split(b'\n')
        .take(SAMPLE_LINES)
        .map_while(std::result::Result::ok)
        .map(|l| String::from_utf8_lossy(&l).trim_end_matches('\r').to_string())
        .collect();
    culprit_querylog::detect(sample.iter().map(String::as_str)).map(|f| f.name()).ok_or_else(|| no_statements(path))
}

fn no_statements(path: &Path) -> Error {
    Error::invalid(format!("{} has no statements in a format Culprit reads", path.display())).with_hint(
        "Culprit reads the logs Rails (debug level), Django (django.db.backends at DEBUG), Entity Framework Core \
         (Database.Command at Information) and Phoenix/Ecto (debug) write, and JSON lines with sql and ms. A framework \
         at its production log level usually leaves SQL out.",
    )
}

/// The log an application writes beside its SQLite file, if one holds statements Culprit reads: `.log` files in the
/// database's folder, a `log` folder beside it, and the parent folder's `log` (Rails: `db/` and `storage/` beside
/// `log/`). One named like the database (`production.sqlite3` → `production.log`) comes first, then the most recently
/// written.
pub fn discover(database: &Path) -> Option<std::path::PathBuf> {
    let folder = database.parent()?;
    let stem = database.file_stem()?.to_string_lossy().to_ascii_lowercase();
    // A database named for an environment (`production.sqlite3`, `production_cache.sqlite3`) is never paired with another
    // environment's log: development timings aren't production's.
    let environment = |name: &str| ENVIRONMENTS.iter().find(|e| name.starts_with(*e)).copied();
    let ours = environment(&stem);
    let mut places = vec![folder.to_path_buf(), folder.join("log")];
    if let Some(app) = folder.parent() {
        places.push(app.join("log"));
    }
    let mut logs: Vec<(bool, std::time::SystemTime, std::path::PathBuf)> = places
        .iter()
        .filter_map(|dir| std::fs::read_dir(dir).ok())
        .flat_map(|entries| entries.filter_map(std::result::Result::ok))
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x.eq_ignore_ascii_case("log")) && p.is_file())
        .filter(|p| {
            let theirs = p.file_stem().and_then(|s| environment(&s.to_string_lossy().to_ascii_lowercase()));
            !matches!((ours, theirs), (Some(a), Some(b)) if a != b)
        })
        .map(|p| {
            let named = p.file_stem().is_some_and(|s| s.to_string_lossy().to_ascii_lowercase() == stem);
            let written = std::fs::metadata(&p).and_then(|m| m.modified()).unwrap_or(std::time::UNIX_EPOCH);
            (named, written, p)
        })
        .collect();
    logs.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.cmp(&a.1)));
    logs.into_iter().take(DISCOVER_FILES).map(|(_, _, p)| p).find(|p| recognize(p).is_ok())
}

/// Read `path` into `snapshot`'s workload and plan its heaviest shapes, unless the database keeps its own history.
/// `found`: Culprit found the log beside the database ([`discover`]) rather than being given it, which the note says.
pub async fn attach(
    provider: &dyn Provider,
    snapshot: &mut Snapshot,
    path: &Path,
    options: &SnapshotOptions,
    found: bool,
) -> Result<()> {
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "the log".into());
    if snapshot.workload.is_some() {
        let mut coverage = culprit_core::Coverage::default();
        coverage.not_applicable(&QUERY_LOG_SOURCE, "The database already provides statement history.");
        coverage.append_to(snapshot);
        snapshot
            .notes
            .push(note(format!("{} keeps its own statement history, so {name} wasn't read.", snapshot.server.name())));
        return Ok(());
    }
    let engine = snapshot.server.engine;
    let file = path.to_path_buf();
    let (format, aggregator) = tokio::task::spawn_blocking(move || read_file(&file, engine))
        .await
        .map_err(|e| Error::internal(format!("Reading the query log stopped: {e}")))??;
    let mut aggregator = aggregator;
    // Another database's statements (a log shared by an app's primary, cache and queue files): one that names tables
    // and none of them is here isn't this database's time.
    let here: std::collections::HashSet<String> = snapshot.tables.iter().map(|t| t.name.to_ascii_lowercase()).collect();
    let elsewhere = if here.is_empty() {
        0
    } else {
        aggregator.retain(|text| {
            let named = culprit_core::sql::tables_in(text, engine);
            named.is_empty() || named.iter().any(|n| here.contains(n.rsplit('.').next().unwrap_or(n)))
        })
    };
    let read = aggregator.entries();
    snapshot.operations.log_failures = aggregator.failures();
    if !snapshot.operations.log_failures.is_empty() {
        snapshot.notes.push(note("Recognized structured error records were retained without raw messages or values. An error without duration is absent from performance totals, not an instant execution. Counts are log records and can include retries or duplicate logging; dates describe only records that include a time zone.".into()));
    }
    let mut workload = aggregator.workload(format, options.top_queries.max(1) as usize);
    // Each shape is planned as text someone else wrote (the application did), under the engine's rules for recorded
    // statements; SQLite plans `?` placeholders as NULL. One the engine can't plan stays unplanned.
    let mut planned = 0;
    for q in workload.queries.iter_mut().take(options.plans as usize) {
        if let Ok(plan) = provider.explain_recorded(&q.text, None).await {
            q.plan = Some(plan);
            planned += 1;
        }
    }
    let server = &mut snapshot.server;
    server.missing.retain(|m| m.capability != Capability::QueryLog);
    if !server.has(Capability::QueryLog) {
        server.capabilities.push(Capability::QueryLog);
    }
    if planned > 0 && !server.has(Capability::QueryPlans) {
        server.capabilities.push(Capability::QueryPlans);
    }
    let found = if found { ", found beside the database; change or remove it on the connection" } else { "" };
    snapshot.notes.push(note(format!(
        "Read {} statements from {name} ({format}{found}): {} shapes, the {} heaviest kept, {planned} planned.{}",
        culprit_core::format::count(read as f64),
        culprit_core::format::count(workload.query_count),
        workload.queries.len(),
        if workload.window_unknown() {
            " The log has no times to tell how long it covers, so shares hold and rates aren't claimed."
        } else {
            ""
        }
    )));
    snapshot.notes.push(note("Only statements logged by the application are visible. Table names filter unrelated statements but cannot distinguish databases with the same table names; use a log for this database. Explicit request IDs support bounded request-group counts, not distributed traces or proof of an N+1 bug.".into()));
    if elsewhere > 0 {
        snapshot.notes.push(note(format!(
            "Left out {} statements on tables this database doesn't have: the log is shared with another database.",
            culprit_core::format::count(elsewhere as f64)
        )));
    }
    let mut coverage = culprit_core::Coverage::default();
    coverage.completed(&QUERY_LOG_SOURCE, workload.queries.len() + snapshot.operations.log_failures.len());
    coverage.append_to(snapshot);
    snapshot.workload = Some(workload);
    if let Some(identity) = snapshot.identity.clone() {
        snapshot.bind_identity(identity);
    }
    Ok(())
}

fn note(message: String) -> CollectorNote {
    CollectorNote { collector: "query log".into(), message }
}

/// The file, line by line: its format told from the first lines, every statement fingerprinted. A line that isn't
/// UTF-8 is read as well as it can be rather than stopping the read.
fn read_file(path: &Path, engine: culprit_core::Engine) -> Result<(&'static str, Aggregator)> {
    let display = path.display().to_string();
    let file =
        std::fs::File::open(path).map_err(|e| Error::new(ErrorCode::NotFound, format!("Can't read {display}: {e}")))?;
    let mut lines = BufReader::new(file);
    let mut buffer = Vec::new();
    let mut next = move || -> Result<Option<String>> {
        buffer.clear();
        match lines.read_until(b'\n', &mut buffer) {
            Ok(0) => Ok(None),
            Ok(_) => Ok(Some(String::from_utf8_lossy(&buffer).trim_end_matches(['\n', '\r']).to_string())),
            Err(e) => Err(Error::new(ErrorCode::QueryFailed, format!("Reading {display} failed: {e}"))),
        }
    };
    let mut sample = Vec::new();
    while sample.len() < SAMPLE_LINES {
        match next()? {
            Some(line) => sample.push(line),
            None => break,
        }
    }
    let mut reader = Reader::detect(sample.iter().map(String::as_str), engine).ok_or_else(|| no_statements(path))?;
    let format = reader.format();
    for line in &sample {
        reader.line(line);
    }
    while let Some(line) = next()? {
        reader.line(&line);
    }
    Ok((format, reader.finish()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const RAILS: &str = "Started GET \"/\" for 127.0.0.1 at 2026-10-15 12:00:01 +0000\n  Order Load (12.4ms)  SELECT \"orders\".* FROM \"orders\" WHERE \"orders\".\"customer_id\" = ? LIMIT ?  [[\"customer_id\", 3], [\"LIMIT\", 20]]\n";

    fn app(name: &str) -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!("culprit-querylog-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("db")).unwrap();
        std::fs::create_dir_all(root.join("log")).unwrap();
        std::fs::write(root.join("db/production.sqlite3"), b"").unwrap();
        root
    }

    #[test]
    fn a_rails_log_is_found_beside_the_database() {
        let root = app("rails");
        // A log without SQL (production at info level) is no query log; the one named like the database wins.
        std::fs::write(root.join("log/production.log"), "I, [2026-10-15] INFO -- : Completed 200 OK\n").unwrap();
        assert_eq!(discover(&root.join("db/production.sqlite3")), None);
        // Another environment's log is never this database's.
        std::fs::write(root.join("log/development.log"), RAILS).unwrap();
        assert_eq!(discover(&root.join("db/production.sqlite3")), None);
        std::fs::write(root.join("log/production.log"), RAILS).unwrap();
        assert_eq!(discover(&root.join("db/production.sqlite3")), Some(root.join("log/production.log")));
        // Rails 8 keeps the cache and queue beside the primary, all writing production.log.
        std::fs::write(root.join("db/production_cache.sqlite3"), b"").unwrap();
        assert_eq!(discover(&root.join("db/production_cache.sqlite3")), Some(root.join("log/production.log")));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_file_without_statements_is_said_to_be_one() {
        let root = app("plain");
        std::fs::write(root.join("log/app.log"), "worker started\n").unwrap();
        let e = recognize(&root.join("log/app.log")).unwrap_err();
        assert!(e.message.contains("no statements"), "{}", e.message);
        assert!(e.hint.as_deref().unwrap_or("").contains("production log level"));
        let _ = std::fs::remove_dir_all(&root);
    }
}
