//! Query logs an application's framework already writes, read into a workload. For an engine that keeps no statement
//! statistics (SQLite), the only record of which statements ran and how long they took is what the application logged;
//! Culprit asks for no change to get it, it reads the log a framework writes anyway:
//!
//! - Rails (`Product Load (12.4ms)  SELECT …`, its log at debug level),
//! - Django (`(0.012) SELECT …; args=…`, `django.db.backends` at DEBUG),
//! - Entity Framework Core (`Executed DbCommand (12ms) […]` and the SQL below it),
//! - Phoenix/Ecto (`QUERY OK … db=1.2ms` and the SQL below it),
//! - JSON lines (`{"sql": …, "ms": …, "at": …}`), for anything that already logs that way.
//!
//! Each format is a module implementing [`Format`]; [`formats`] lists them, and a new framework is one more. Entries are
//! fingerprinted as they arrive ([`Aggregator`]), so a log of any size is read once without keeping it, and the result
//! is an ordinary [`Workload`]: repeats of one statement shape add up and are ranked by total time.

mod formats;

use std::collections::HashMap;

use culprit_core::fingerprint::{fingerprint, id};
use culprit_core::{Engine, QueryStat, Workload};

pub use formats::formats;

/// When an entry ran: seconds since 1970, and whether the log said which time zone (a local time without one still
/// measures how long the log spans, but is never shown as UTC).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Moment {
    pub epoch: f64,
    pub zoned: bool,
}

/// One statement as the log recorded it.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct Entry {
    pub sql: String,
    pub ms: f64,
    pub at: Option<Moment>,
    pub error_code: Option<&'static str>,
    /// Consumed transiently and hashed; the original identifier never reaches a snapshot.
    pub request_id: Option<String>,
}

/// A framework's log format: recognizes its lines and turns them into entries. A format whose entry spans several lines
/// keeps it between calls and hands it over when the next line shows it ended, or at [`Format::finish`].
pub trait Format {
    /// How a person names it: "Rails".
    fn name(&self) -> &'static str;
    /// Whether a line is unmistakably this format's, to tell which format a log is.
    fn recognizes(&self, line: &str) -> bool;
    /// Read one line; push the entries it completes.
    fn line(&mut self, line: &str, out: &mut Vec<Entry>);
    /// The end of the log: push an entry still being read.
    fn finish(&mut self, _out: &mut Vec<Entry>) {}
}

/// Lines read to tell the format.
const DETECT_LINES: usize = 5_000;

/// The format whose lines `sample` holds most of, if any line is one of them.
pub fn detect<'a>(sample: impl IntoIterator<Item = &'a str>) -> Option<Box<dyn Format>> {
    let lines: Vec<&str> = sample.into_iter().take(DETECT_LINES).collect();
    formats()
        .into_iter()
        .map(|f| {
            let hits = lines.iter().filter(|l| f.recognizes(l)).count();
            (hits, f)
        })
        .filter(|(hits, _)| *hits > 0)
        .max_by_key(|(hits, _)| *hits)
        .map(|(_, f)| f)
}

/// Statements grouped by shape as they arrive.
pub struct Aggregator {
    engine: Engine,
    shapes: HashMap<String, Shape>,
    order: Vec<String>,
    first: Option<Moment>,
    last: Option<Moment>,
    entries: u64,
    failures: HashMap<(String, String), culprit_core::operations::LoggedFailure>,
}

struct Shape {
    display: String,
    runs: f64,
    total_ms: f64,
    squares: f64,
    max_ms: f64,
    first: Option<Moment>,
    last: Option<Moment>,
    requests: HashMap<String, u32>,
    requests_truncated: bool,
}

impl Aggregator {
    pub fn new(engine: Engine) -> Self {
        Aggregator {
            engine,
            shapes: HashMap::new(),
            order: Vec::new(),
            first: None,
            last: None,
            entries: 0,
            failures: HashMap::new(),
        }
    }

    pub fn add(&mut self, entry: &Entry) {
        let print = fingerprint(&entry.sql, self.engine);
        if !print.key.is_empty()
            && let Some(code) = entry.error_code
        {
            let at = entry.at.filter(|a| a.zoned).map(|a| culprit_core::format::iso_from_epoch(a.epoch));
            let failure = self.failures.entry((print.key.clone(), code.into())).or_insert_with(|| {
                culprit_core::operations::LoggedFailure {
                    query_id: id(&print.key),
                    statement: print.display.clone(),
                    code: code.into(),
                    count: 0.0,
                    first_at: at.clone(),
                    last_at: at.clone(),
                }
            });
            failure.count += 1.0;
            if let Some(at) = at {
                if failure.first_at.as_ref().is_none_or(|first| &at < first) {
                    failure.first_at = Some(at.clone());
                }
                if failure.last_at.as_ref().is_none_or(|last| &at > last) {
                    failure.last_at = Some(at);
                }
            }
        }
        if print.key.is_empty() || !entry.ms.is_finite() || entry.ms < 0.0 {
            return;
        }
        self.entries += 1;
        let earliest = |a: Option<Moment>, b: Option<Moment>| match (a, b) {
            (Some(a), Some(b)) => Some(if b.epoch < a.epoch { b } else { a }),
            (a, b) => a.or(b),
        };
        let latest = |a: Option<Moment>, b: Option<Moment>| match (a, b) {
            (Some(a), Some(b)) => Some(if b.epoch > a.epoch { b } else { a }),
            (a, b) => a.or(b),
        };
        self.first = earliest(self.first, entry.at);
        self.last = latest(self.last, entry.at);
        if !self.shapes.contains_key(&print.key) {
            self.order.push(print.key.clone());
        }
        let shape = self.shapes.entry(print.key).or_insert_with(|| Shape {
            display: print.display,
            runs: 0.0,
            total_ms: 0.0,
            squares: 0.0,
            max_ms: 0.0,
            first: None,
            last: None,
            requests: HashMap::new(),
            requests_truncated: false,
        });
        shape.runs += 1.0;
        shape.total_ms += entry.ms;
        shape.squares += entry.ms * entry.ms;
        shape.max_ms = shape.max_ms.max(entry.ms);
        shape.first = earliest(shape.first, entry.at);
        shape.last = latest(shape.last, entry.at);
        if let Some(request) = &entry.request_id {
            let hashed = id(request);
            if shape.requests.contains_key(&hashed) || shape.requests.len() < 10_000 {
                let count = shape.requests.entry(hashed).or_default();
                *count = count.saturating_add(1);
            } else {
                shape.requests_truncated = true;
            }
        }
    }

    /// Statements read so far.
    pub fn entries(&self) -> u64 {
        self.entries
    }

    /// Keep only the shapes `keep` accepts (by their text): a log can hold another database's statements (one Rails log
    /// for an app's primary, cache and queue databases). Returns how many statements were left out; the totals and the
    /// workload are of the rest.
    pub fn retain(&mut self, keep: impl Fn(&str) -> bool) -> u64 {
        self.failures.retain(|_, failure| keep(&failure.statement));
        let before = self.entries;
        self.shapes.retain(|_, shape| {
            let kept = keep(&shape.display);
            if !kept {
                self.entries -= shape.runs as u64;
            }
            kept
        });
        self.order.retain(|key| self.shapes.contains_key(key));
        before - self.entries
    }

    pub fn failures(&self) -> Vec<culprit_core::operations::LoggedFailure> {
        let mut failures: Vec<_> = self.failures.values().cloned().collect();
        failures.sort_by(|a, b| (&a.query_id, &a.code).cmp(&(&b.query_id, &b.code)));
        failures
    }

    /// The workload of a log in `format` ("Rails"): every shape's runs and time, the `top` heaviest kept as statements
    /// (the rest count in the totals). Its period is what the log's timestamps span, dated only when they say their
    /// zone; without timestamps it is unknown, so shares hold and rates aren't claimed.
    pub fn workload(self, format: &str, top: usize) -> Workload {
        let iso = |m: Option<Moment>| m.filter(|m| m.zoned).map(|m| culprit_core::format::iso_from_epoch(m.epoch));
        let span_hours = match (self.first, self.last) {
            (Some(a), Some(b)) => ((b.epoch - a.epoch) / 3600.0).max(0.0),
            _ => 0.0,
        };
        let dated = span_hours > 0.0;
        let total_time_ms: f64 = self.shapes.values().map(|s| s.total_ms).sum();
        let total_executions: f64 = self.shapes.values().map(|s| s.runs).sum();
        let query_count = self.shapes.len() as f64;
        let mut shapes: Vec<(String, Shape)> = self.shapes.into_iter().collect();
        shapes.sort_by(|a, b| b.1.total_ms.total_cmp(&a.1.total_ms));
        // Transaction control and pragmas are the framework's bookkeeping, not statements to tune: they count in the
        // totals (a slow COMMIT is time the database spent), but aren't ranked as queries.
        let queries = shapes
            .into_iter()
            .filter(|(key, _)| !bookkeeping(key))
            .take(top)
            .map(|(key, s)| {
                let mean = s.total_ms / s.runs.max(1.0);
                let variance = (s.squares / s.runs.max(1.0) - mean * mean).max(0.0);
                QueryStat {
                    requests: (!s.requests.is_empty()).then(|| culprit_core::operations::RequestEvidence {
                        requests: s.requests.len() as u32,
                        repeated_requests: s.requests.values().filter(|v| **v >= 10).count() as u32,
                        most_calls: s.requests.values().copied().max().unwrap_or(0),
                        sample_truncated: s.requests_truncated,
                    }),
                    resolved_write_target: None,
                    id: id(&key),
                    query_hash: None,
                    text: s.display,
                    parameters: None,
                    object_name: None,
                    executions: s.runs,
                    total_time_ms: s.total_ms,
                    mean_time_ms: mean,
                    max_time_ms: Some(s.max_ms),
                    stddev_time_ms: (s.runs > 1.0).then(|| variance.sqrt()),
                    cpu_time_ms: None,
                    rows: None,
                    logical_reads: None,
                    physical_reads: None,
                    temp_writes: None,
                    first_seen: iso(s.first),
                    last_seen: iso(s.last),
                    plans: Vec::new(),
                    plan: None,
                    waits: Vec::new(),
                    trend: None,
                    spread: None,
                    costs: None,
                    rows_affected: None,
                    plan_id: None,
                }
            })
            .collect();
        Workload {
            history_inputs: Vec::new(),
            measurement: None,
            source: format!("{format} log"),
            kind: Some(culprit_core::WorkloadKind::QueryLog),
            // A record of its own period: never running totals to subtract from a previous scan's.
            windowed: true,
            log: Some(format.to_string()),
            window_start: if dated { iso(self.first) } else { None },
            window_end: if dated { iso(self.last) } else { None },
            window_hours: span_hours,
            total_time_ms,
            total_cpu_ms: None,
            total_executions,
            query_count,
            queries,
            timeline: None,
            untracked_time_ms: None,
            hourly_time_ms: None,
            min_logged_ms: None,
        }
    }
}

/// `BEGIN`, `COMMIT`, `SAVEPOINT x`, `PRAGMA foreign_keys = ?`: statements a framework sends around the real ones.
fn bookkeeping(key: &str) -> bool {
    culprit_core::sql::is_session_statement(key)
}

/// A log being read: its format, told from its first lines, and the shapes so far. Lines are fed one at a time, so a
/// log of any size is read without keeping it.
pub struct Reader {
    format: Box<dyn Format>,
    aggregator: Aggregator,
    out: Vec<Entry>,
}

impl Reader {
    /// A reader for the format `sample` (the log's first lines) is in; `None` when no line is a format Culprit reads.
    /// The sample isn't consumed: feed it with the rest.
    pub fn detect<'a>(sample: impl IntoIterator<Item = &'a str>, engine: Engine) -> Option<Reader> {
        Some(Reader { format: detect(sample)?, aggregator: Aggregator::new(engine), out: Vec::new() })
    }

    /// The format's name: "Rails".
    pub fn format(&self) -> &'static str {
        self.format.name()
    }

    pub fn line(&mut self, line: &str) {
        self.format.line(line, &mut self.out);
        for entry in self.out.drain(..) {
            self.aggregator.add(&entry);
        }
    }

    /// The end of the log: the shapes read.
    pub fn finish(mut self) -> Aggregator {
        self.format.finish(&mut self.out);
        for entry in self.out.drain(..) {
            self.aggregator.add(&entry);
        }
        self.aggregator
    }
}

/// Read a whole log held in memory. `None` when no line is a format Culprit reads.
pub fn read<'a>(
    lines: impl IntoIterator<Item = &'a str> + Clone,
    engine: Engine,
) -> Option<(&'static str, Aggregator)> {
    let mut reader = Reader::detect(lines.clone(), engine)?;
    let name = reader.format();
    for line in lines {
        reader.line(line);
    }
    Some((name, reader.finish()))
}
