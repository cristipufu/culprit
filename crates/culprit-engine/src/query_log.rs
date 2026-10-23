//! A query log an application's framework already writes, read into a scan's workload (`culprit-querylog`), for an
//! engine that keeps no statement history of its own (SQLite). Nothing in the application changes. The file is read
//! line by line and never kept: only the statement shapes, with their literal values taken out, reach the report.
//!
//! A connection names its log, or Culprit finds it ([`discover`]): frameworks write their log beside the database by
//! convention (Rails keeps `db/production.sqlite3` and logs to `log/production.log`).

use std::io::{BufRead, BufReader, Read};
use std::path::Path;
use std::time::{Duration, Instant};

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

/// File extensions Culprit checks when looking for an application's query log beside a SQLite database.
pub const DISCOVERED_LOG_EXTENSIONS: [&str; 3] = ["log", "jsonl", "ndjson"];

/// The log an application writes beside its SQLite file, if one holds statements Culprit reads: `.log`, `.jsonl` and
/// `.ndjson` files in the database's folder, a `log` or `logs` folder beside it, and the parent folder's `log` or
/// `logs` (Rails: `db/` and `storage/` beside `log/`). One named like the database (`production.sqlite3` →
/// `production.log` or `production.jsonl`) comes first, then the most recently written.
pub fn discover(database: &Path) -> Option<std::path::PathBuf> {
    let folder = database.parent()?;
    let stem = database.file_stem()?.to_string_lossy().to_ascii_lowercase();
    // A database named for an environment (`production.sqlite3`, `production_cache.sqlite3`) is never paired with another
    // environment's log: development timings aren't production's.
    let environment = |name: &str| ENVIRONMENTS.iter().find(|e| name.starts_with(*e)).copied();
    let ours = environment(&stem);
    let mut places = vec![folder.to_path_buf(), folder.join("log"), folder.join("logs")];
    if let Some(app) = folder.parent() {
        places.push(app.join("log"));
        places.push(app.join("logs"));
    }
    let mut logs: Vec<(bool, std::time::SystemTime, std::path::PathBuf)> = places
        .iter()
        .filter_map(|dir| std::fs::read_dir(dir).ok())
        .flat_map(|entries| entries.filter_map(std::result::Result::ok))
        .map(|e| e.path())
        .filter(|p| {
            p.extension()
                .and_then(|x| x.to_str())
                .is_some_and(|x| DISCOVERED_LOG_EXTENSIONS.iter().any(|ext| x.eq_ignore_ascii_case(ext)))
                && p.is_file()
        })
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
    logs.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.cmp(&a.1)).then(a.2.cmp(&b.2)));
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
    let read = tokio::task::spawn_blocking(move || read_file(&file, engine))
        .await
        .map_err(|e| Error::internal(format!("Reading the query log stopped: {e}")))
        .and_then(|result| result);
    let mut coverage = culprit_core::Coverage::default();
    let Some((format, mut aggregator, limitation)) = coverage.read(&QUERY_LOG_SOURCE, read) else {
        coverage.append_to(snapshot);
        return Ok(());
    };
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
    coverage.completed(&QUERY_LOG_SOURCE, workload.queries.len() + snapshot.operations.log_failures.len());
    if let Some(reason) = limitation {
        coverage.partial(&QUERY_LOG_SOURCE, reason);
    }
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
#[derive(Clone, Copy)]
struct LogLimits {
    bytes: u64,
    line: u64,
    time: Duration,
}
impl Default for LogLimits {
    fn default() -> Self {
        Self { bytes: 64 * 1024 * 1024, line: 256 * 1024, time: Duration::from_secs(5) }
    }
}

struct LogLines {
    reader: BufReader<std::io::Take<std::fs::File>>,
    buffer: Vec<u8>,
    deadline: Instant,
    line_limit: u64,
    clipped: bool,
    limitation: Option<String>,
}
impl LogLines {
    fn next(&mut self) -> Result<Option<String>> {
        if self.limitation.is_some() {
            return Ok(None);
        }
        if Instant::now() >= self.deadline {
            self.limitation =
                Some("Stopped at the query-log reading deadline; only complete earlier records were collected.".into());
            return Ok(None);
        }
        self.buffer.clear();
        let read = Read::by_ref(&mut self.reader)
            .take(self.line_limit + 1)
            .read_until(b'\n', &mut self.buffer)
            .map_err(|e| Error::new(ErrorCode::QueryFailed, format!("Reading the query log failed: {e}")))?;
        let at_limit = self.clipped && self.reader.get_ref().limit() == 0 && self.reader.buffer().is_empty();
        if at_limit && (read == 0 || !self.buffer.ends_with(b"\n")) {
            self.limitation = Some("The log is larger than Culprit can read in one scan; only complete earlier records were collected. Use a smaller or rotated log for recent activity.".into());
            return Ok(None);
        }
        if read as u64 > self.line_limit {
            self.limitation =
                Some("Stopped before an oversized log line; only complete earlier records were collected.".into());
            return Ok(None);
        }
        if read == 0 {
            return Ok(None);
        }
        Ok(Some(String::from_utf8_lossy(&self.buffer).trim_end_matches(['\n', '\r']).to_string()))
    }
}

fn read_file(path: &Path, engine: culprit_core::Engine) -> Result<(&'static str, Aggregator, Option<String>)> {
    read_file_with_limits(path, engine, LogLimits::default())
}

fn read_file_with_limits(
    path: &Path,
    engine: culprit_core::Engine,
    limits: LogLimits,
) -> Result<(&'static str, Aggregator, Option<String>)> {
    let file = std::fs::File::open(path)
        .map_err(|e| Error::new(ErrorCode::NotFound, format!("Can't read {}: {e}", path.display())))?;
    let metadata = file.metadata().map_err(|e| Error::invalid(e.to_string()))?;
    if !metadata.is_file() {
        return Err(Error::invalid("A query log must be a regular file"));
    }
    // Snapshot the length so a log growing faster than collection cannot keep a scan alive indefinitely.
    let mut lines = LogLines {
        reader: BufReader::new(file.take(metadata.len().min(limits.bytes))),
        buffer: Vec::new(),
        deadline: Instant::now() + limits.time,
        line_limit: limits.line,
        clipped: metadata.len() > limits.bytes,
        limitation: None,
    };
    let mut sample = Vec::new();
    let mut sample_bytes = 0;
    while sample.len() < SAMPLE_LINES && sample_bytes < SAMPLE_BYTES as usize {
        match lines.next()? {
            Some(line) => {
                sample_bytes += line.len();
                sample.push(line);
            }
            None => break,
        }
    }
    let mut reader = Reader::detect(sample.iter().map(String::as_str), engine).ok_or_else(|| {
        lines.limitation.as_ref().map_or_else(|| no_statements(path), |reason| Error::invalid(reason.clone()))
    })?;
    let format = reader.format();
    for line in &sample {
        reader.line(line);
        if reader.limited() {
            break;
        }
    }
    while !reader.limited() {
        let Some(line) = lines.next()? else { break };
        reader.line(&line);
    }
    let partial = lines.limitation.is_some() || reader.limited();
    let aggregator = if partial { reader.discard_pending() } else { reader.finish() };
    if aggregator.limited() && lines.limitation.is_none() {
        lines.limitation = Some(
            "Stopped at the query-log aggregation budget; only complete earlier statements were collected.".into(),
        );
    }
    Ok((format, aggregator, lines.limitation))
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

    #[cfg(feature = "sqlite")]
    #[tokio::test]
    async fn bounded_and_missing_logs_report_coverage_without_failing_collection() {
        let root = app("coverage");
        let database = root.join("db/production.sqlite3");
        let conn = rusqlite::Connection::open(&database).unwrap();
        conn.execute_batch("CREATE TABLE items (id INTEGER)").unwrap();
        drop(conn);
        let provider = culprit_sqlite::SqliteProvider::new(&culprit_core::ConnectionSpec {
            database: Some(database.to_string_lossy().into_owned()),
            ..Default::default()
        })
        .unwrap();
        let options = SnapshotOptions { plans: 0, ..Default::default() };
        let mut snapshot = provider.snapshot("test", &options).await.unwrap();
        let path = root.join("log/production.log");
        std::fs::write(&path, format!("{{\"sql\":\"SELECT * FROM items\",\"ms\":1}}\n{}", "x".repeat(300 * 1024)))
            .unwrap();
        attach(&provider, &mut snapshot, &path, &options, false).await.unwrap();
        assert_eq!(snapshot.source_outcome("query log").unwrap().state, culprit_core::SourceState::Partial);
        assert_eq!(snapshot.workload.as_ref().unwrap().total_executions, 1.0);
        let mut snapshot = provider.snapshot("test", &options).await.unwrap();
        attach(&provider, &mut snapshot, &root.join("missing.log"), &options, false).await.unwrap();
        assert_eq!(snapshot.source_outcome("query log").unwrap().state, culprit_core::SourceState::Unavailable);
        assert!(snapshot.workload.is_none());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn input_budgets_never_turn_partial_records_into_statements() {
        let root = app("budgets");
        let path = root.join("log/production.log");
        let first = "{\"sql\":\"SELECT 1\",\"ms\":1}\n";
        std::fs::write(&path, format!("{first}{{\"sql\":\"SELECT 999\",\"ms\":999}}\n")).unwrap();
        let limits = LogLimits { bytes: first.len() as u64 + 12, line: 1024, time: Duration::from_secs(1) };
        let (_, a, reason) = read_file_with_limits(&path, culprit_core::Engine::Sqlite, limits).unwrap();
        assert_eq!(a.entries(), 1);
        assert!(reason.is_some());
        std::fs::write(&path, format!("{first}{}\n", "x".repeat(200))).unwrap();
        let (_, a, reason) =
            read_file_with_limits(&path, culprit_core::Engine::Sqlite, LogLimits { bytes: 1024, line: 100, ..limits })
                .unwrap();
        assert_eq!(a.entries(), 1);
        assert!(reason.unwrap().contains("oversized"));
        let error =
            read_file_with_limits(&path, culprit_core::Engine::Sqlite, LogLimits { time: Duration::ZERO, ..limits })
                .err()
                .unwrap();
        assert!(error.message.contains("deadline"));
        std::fs::remove_dir_all(root).unwrap();
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
    fn jsonl_and_ndjson_logs_with_sql_and_durations_are_discovered_while_non_sql_jsonl_is_skipped() {
        let root = app("jsonl");
        // Structured event/search logs without SQL statements or without durations are never mistaken for query logs.
        std::fs::write(
            root.join("db/production.jsonl"),
            "{\"query\":\"select red shoes\",\"duration_ms\":4.2}\n{\"sql\":\"SELECT * FROM orders\"}\n",
        )
        .unwrap();
        std::fs::write(
            root.join("log/events.ndjson"),
            "{\"statement\":\"update profile settings\",\"ms\":1.5}\n{\"level\":\"info\",\"msg\":\"ready\"}\n",
        )
        .unwrap();
        assert_eq!(discover(&root.join("db/production.sqlite3")), None);

        // A .ndjson or .jsonl file whose records contain SQL statements and durations is discovered automatically.
        std::fs::create_dir_all(root.join("logs")).unwrap();
        std::fs::write(
            root.join("logs/queries.ndjson"),
            "{\"sql\":\"SELECT id FROM orders WHERE customer_id = ?\",\"duration_ms\":6.4,\"ts\":\"2026-10-15T12:00:00Z\"}\n",
        )
        .unwrap();
        assert_eq!(discover(&root.join("db/production.sqlite3")), Some(root.join("logs/queries.ndjson")));

        // A valid .jsonl file named after the database takes priority over other logs.
        std::fs::write(
            root.join("db/production.jsonl"),
            "{\"query\":\"SELECT id FROM orders WHERE customer_id = 1\",\"ms\":3.1,\"at\":\"2026-10-15T12:00:01Z\"}\n",
        )
        .unwrap();
        assert_eq!(discover(&root.join("db/production.sqlite3")), Some(root.join("db/production.jsonl")));
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
