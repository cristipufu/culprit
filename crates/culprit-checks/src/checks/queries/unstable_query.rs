//! The `unstable-query` check: Queries whose duration swings wildly between runs with the same plan, a sign of parameter sensitivity or blocking.

use super::*;
use crate::evidence::{QueryMeasure, Requirement as Evidence};

pub fn unstable_query_info() -> CheckInfo {
    CheckInfo {
        id: "unstable-query".into(),
        title: "Unstable queries".into(),
        category: Category::Queries,
        engines: vec![Engine::Mssql, Engine::Postgres, Engine::Mysql, Engine::Sqlite],
        requires: vec![],
        description: "Queries whose duration swings wildly between runs with the same plan, a sign of parameter sensitivity or blocking.".into(),
    }
}

check!(UnstableQueryCheck: unstable_query_info, unstable_query, &[Evidence::Query(QueryMeasure::Variance)]);

/// How far a statement's run times spread: the standard deviation over the average, and the slowest run over it.
pub(super) const UNSTABLE_CV: f64 = 3.0;
const UNSTABLE_MAX_FACTOR: f64 = 50.0;

/// The slow runs behind a wide spread, when it takes more than one slow run to explain it. One outlier among many fast
/// runs (a lock wait during a migration, a cold cache after a restart) inflates the standard deviation on its own: the
/// spread has to stay wide without the slowest run's share of it. Returns the least time the slow runs spent beyond
/// the average (ms): every slow run is at most the slowest one, so the variance they explain needs at least that much.
pub(super) fn unstable_excess_ms(q: &QueryStat) -> Option<f64> {
    let (sd, max, n, mean) = (q.stddev_time_ms?, q.max_time_ms?, q.executions, q.mean_time_ms);
    if n < 50.0 || mean <= 0.0 || sd / mean < UNSTABLE_CV || max < mean * UNSTABLE_MAX_FACTOR || max < 1000.0 {
        return None;
    }
    let variance = sd * sd;
    let rest_variance = variance - (max - mean).powi(2) / n;
    let rest_mean = (mean * n - max) / (n - 1.0);
    if rest_variance <= 0.0 || rest_mean <= 0.0 || rest_variance.sqrt() / rest_mean < UNSTABLE_CV {
        return None;
    }
    // Σ(x − mean)² over the slow runs ≤ (max − mean)·Σ(x − mean); the fast runs add at most mean² each.
    let excess = ((variance - mean * mean).max(0.0) * n / (max - mean)).min(q.total_time_ms);
    // Each slow run is at most the slowest, so it took at least excess / (max − mean) of them: two runs that hit one
    // blocking incident aren't a statement whose runs swing, however wide they make the spread.
    (excess > 0.0 && excess / (max - mean) >= crate::queries::MIN_TAIL_RUNS).then_some(excess)
}

pub fn unstable_query(snapshot: &Snapshot) -> Vec<Finding> {
    let Some(workload) = snapshot.workload.as_ref() else { return Vec::new() };
    app_queries(snapshot)
        .filter_map(|q| {
            if q.plans.len() > 1 {
                return None;
            }
            let (stddev, max) = (q.stddev_time_ms?, q.max_time_ms?);
            let excess = unstable_excess_ms(q)?;
            let impact = query_impact(snapshot, &[q])?;
            let slow_share = workload.share_of(excess, q);
            if impact.share < 0.01 || slow_share < 0.005 {
                return None;
            }
            let mut f = finding("unstable-query", &q.id, Category::Queries, subject(SubjectKind::Query, &q.id));
            apply_impact(snapshot, &mut f, &impact);
            // What the slow runs cost, not the whole statement's time: the share and the time a day shown beside the
            // finding (its row, the dashboard, the digest, an agent's view) are what its label says, and so are the
            // grade and the rank. The statement's own share is on the statement (a brief's "Queries involved").
            f.impact.db_time_share = Some(slow_share);
            f.impact.time_ms_per_day = workload.per_day_measured(excess).filter(|_| workload.runs_per_day_of(q).is_some());
            f.severity = severity_for_time_of(snapshot, &[q], excess, slow_share);
            f.impact.score = score_for_share(slow_share);
            f.impact.label = format!("{} of {} in slow runs", format::percent(slow_share), time_label(snapshot));
            f.confidence = Confidence::Medium;
            // A visible values-only write narrows the diagnostic possibilities. Waits, triggers and constraints
            // can do work absent from this plan, so it does not prove that every slow run was waiting.
            let waited = (snapshot.server.engine.has_server() && q.plan.as_ref().is_some_and(writes_without_reading))
                .then(|| WriteWaits::read(snapshot, q, excess));
            // A statement that stopped running is told in the past.
            let (averages, take) = if crate::all_stopped(snapshot, &[q]) { ("averaged", "took") } else { ("averages", "take") };
            // Its average as every surface states it (over Culprit's scans where they measured it); the spread below is
            // the source's own, over its totals.
            f.title = format!(
                "{} {averages} {} per run, but some runs {take} far longer (up to {})",
                capitalize(&query_handle(q)),
                format::duration(workload.statement_mean_ms(q)),
                format::duration(max)
            );
            // One plan in Query Store backs "same plan"; elsewhere the engine keeps no plan identity per run.
            let same = if q.plans.len() == 1 { "Same query, same plan" } else { "Same statement" };
            // What the engine recorded of its waits says which of the two causes it is, where it recorded any.
            let recorded = RecordedWaits::read(snapshot, q, excess);
            f.summary = if let Some(w) = &waited {
                format!(
                    "{same}, very different run times: the slow runs took at least {} more than the average, together. {}",
                    format::duration(excess),
                    w.summary(snapshot)
                )
            } else if snapshot.server.engine.has_server() {
                format!(
                    "{same}, very different run times: the slow runs took at least {} more than the average, together. {}",
                    format::duration(excess),
                    recorded.sentence
                )
            } else {
                // SQLite: no server to record waits; a run that waits is waiting for the file's write lock.
                format!(
                    "{same}, very different run times: the slow runs took at least {} more than the average, together. Either some values match far more rows than the plan expects, or some runs wait for the file's write lock (up to the application's busy_timeout). Compare the slow runs' values in the log.",
                    format::duration(excess)
                )
            };
            // The spread is the source's, over its totals: where the title's average (Culprit's scans') is twice or half
            // theirs, the one the slow runs are measured against is named.
            if let Some(s) = workload.scans_of(q).filter(|s| s.mean_differs(q)) {
                f.summary = f.summary.replacen(
                    "more than the average, together.",
                    &format!(
                        "more, together, than its average of {} a run over {} ({} over {}).",
                        format::duration(q.mean_time_ms),
                        workload.window_phrase(),
                        format::duration(s.mean_ms().unwrap_or(q.mean_time_ms)),
                        s.phrase()
                    ),
                    1,
                );
            } else if let Some(basis) = crate::support::totals_basis(snapshot, q) {
                // The title's average is the scans', the slow runs' excess the totals': each said over what it is.
                f.summary = f.summary.replacen(
                    "more than the average, together.",
                    &format!("more than the average, together over {basis}."),
                    1,
                );
            }
            f.queries = query_refs(&[q]);
            // As the title states it, named over Culprit's scans where they measured it; where those measured one twice
            // or half the spread's own (since the totals began), that one beside it.
            match workload.scans_of(q) {
                Some(s) => {
                    f.metrics.push(Metric::new("Average over Culprit's scans", workload.statement_mean_ms(q), Unit::Ms));
                    if s.mean_differs(q) {
                        let since = format!("Average over {}", workload.window_phrase());
                        f.metrics.push(Metric::new(since, q.mean_time_ms, Unit::Ms));
                    }
                }
                None => f.metrics.push(Metric::new("Average", workload.statement_mean_ms(q), Unit::Ms)),
            }
            f.metrics.push(Metric::new("Slowest", max, Unit::Ms));
            f.metrics.push(Metric::new("Std deviation", stddev, Unit::Ms));
            // A part of its time the source keeps only in its totals: over those, where the average beside it is the
            // scans'.
            let extra = match crate::support::totals_basis(snapshot, q) {
                Some(basis) => format!("Slow runs' extra time over {basis} (at least)"),
                None => "Slow runs' extra time (at least)".to_string(),
            };
            f.metrics.push(Metric::new(extra, excess, Unit::Ms));
            if let Some(io) = waited.as_ref().and_then(|w| w.io_ms) {
                f.metrics.push(Metric::new("Data-file reads and writes", io, Unit::Ms));
            }
            // The same ladders as `parameter-sensitive`: the cause is usually the same, only the evidence is coarser.
            let fewest = (excess / (max - q.mean_time_ms).max(1.0)).ceil();
            f.fix = Some(match (&waited, snapshot.server.engine) {
                (Some(w), _) => w.fix(snapshot),
                // Lock waits the engine recorded explain the slow runs: what holds the locks is the fix, not a plan.
                (None, Engine::Mssql) if recorded.locks_explain => {
                    let LockHolder { lead, sql } = find_lock_holder(snapshot, q, Some(fewest));
                    Fix {
                        summary: format!(
                            "{lead} Keep the transactions that hold those rows short: commit sooner, and don't wait for a user or another service inside a transaction. Inspect the blocking transaction and any trigger or constraint work before deciding whether a query, index or transaction change will help."
                        ),
                        sql,
                        cost: None,
                    }
                }
                (None, Engine::Mssql) => crate::queries::mssql_fix(snapshot, q, None, excess),
                (None, Engine::Postgres) => crate::queries::postgres_fix(),
                (None, Engine::Mysql) => crate::queries::mysql_fix(snapshot, q, recorded.lock_share, Some(fewest)),
                (None, Engine::Sqlite) => Fix {
                    summary: "Compare a slow and a fast value with EXPLAIN QUERY PLAN. If some values match far more rows, an index that narrows them, or ANALYZE, lets SQLite choose better.".into(),
                    sql: Some("ANALYZE;".into()),
                    cost: None,
                },
            });
            f.verify = vec![verify_new_runs(snapshot)];
            if snapshot.server.engine == Engine::Postgres {
                let since = q
                    .first_seen
                    .as_deref()
                    .map(|at| format!("since {}", crate::queries::short_when(at)))
                    .unwrap_or_else(|| workload.window_phrase().replacen("the ", "in the ", 1));
                // A plan that reads no table has no values to blame: which ones were slow doesn't matter.
                let unknown = if waited.is_some() {
                    "how many calls were slow"
                } else {
                    "which values were slow or how many calls were"
                };
                f.caveats.push(format!(
                    "PostgreSQL keeps one minimum, average, maximum and standard deviation per statement {since}, so the slowest runs could be from any time in that span (a lock wait during a migration counts too), and Culprit can't tell {unknown}."
                ));
            }
            Some(f)
        })
        .collect()
}

/// A spread must be checked on new runs: cumulative extrema and variance cannot isolate the runs after a fix.
fn verify_new_runs(snapshot: &Snapshot) -> String {
    let workload = snapshot.workload.as_ref().expect("the check needs a workload");
    let compare = "Compare their durations and any recorded lock waits: runs with comparable parameter values should be faster; if locks caused the slow runs, those waits should be shorter or absent.";
    match workload.kind() {
        Some(culprit_core::WorkloadKind::QueryStore) => format!(
            "Compare equal Query Store windows before and after the change, with the later window containing only runs after it. Check the query's average and slowest run times, and lock waits per run where recorded, under a representative workload. {compare}"
        ),
        Some(culprit_core::WorkloadKind::PlanCache) => format!(
            "On a test copy, capture actual execution plans and duration samples for representative parameter values before and after the change, under comparable load. {compare} {} keeps running totals, so its cached maximum and standard deviation do not isolate runs after the change; compare the new execution samples instead.",
            workload.source
        ),
        Some(culprit_core::WorkloadKind::SlowLog | culprit_core::WorkloadKind::QueryLog) => format!(
            "Collect equal-length query-log samples before and after the change, keeping the same logging threshold and a representative workload. Include only runs whose timestamps fall in their respective period. {compare}"
        ),
        _ => {
            let log = if snapshot.server.engine == Engine::Mysql { "slow query log" } else { "server log" };
            format!(
                "Capture slow runs in the {log} before the change and again after it, with the same logging threshold and a representative workload. {compare} {} keeps running totals, so its historical maximum and standard deviation do not isolate runs after the change; use the new log samples to judge it.",
                workload.source
            )
        }
    }
}

/// What the runs of a statement that writes the values it is given can have waited for, from the catalog and the
/// statement's own counters: the rows its foreign keys check, its data-file time, and, once it stopped, what writes
/// the same table now.
struct WriteWaits<'a> {
    /// Its foreign keys, when it inserts: each new row is checked against the referenced row, which is read and
    /// share-locked.
    keys: Vec<&'a culprit_core::ForeignKey>,
    /// The unique index whose columns its column list names (the primary key first): every run looks its key up there
    /// before it writes, a read its plan doesn't show.
    key_index: Option<&'a str>,
    /// Whether another transaction can insert the key it inserts: the table has a unique index whose columns its
    /// column list names (a key the database generates, an identity or a sequence, never repeats). Unknown is true.
    same_key: bool,
    /// The share of its runs that inserted nothing: an insert of one row that skips a key already there (`ON CONFLICT
    /// DO NOTHING`, `INSERT IGNORE`), counted from the rows it inserted.
    inserted_nothing: Option<f64>,
    /// Its data-file reads and writes, when the engine timed them and they are a small part of the slow runs' time.
    io_ms: Option<f64>,
    /// The slow runs' extra time (ms), at least.
    excess: f64,
    /// The fewest slow runs that add up to it: each is at most the slowest run.
    fewest_slow: Option<f64>,
    /// The average run (ms), to tell a slow run from the others.
    mean_ms: f64,
    /// It stopped running (see `query_stopped`).
    stopped: bool,
    /// Once it stopped: a statement still running that writes the same table the same way, what may have replaced it.
    successor: Option<&'a QueryStat>,
}

/// A small recorded I/O total is context, not proof that every historical run was timed.
const SMALL_RECORDED_IO: f64 = 0.1;

impl<'a> WriteWaits<'a> {
    fn read(snapshot: &'a Snapshot, q: &QueryStat, excess: f64) -> Self {
        let engine = snapshot.server.engine;
        let table = write_targets(q, engine)
            .into_iter()
            .find_map(|o| snapshot.tables.iter().find(|t| names_table(&o, &t.schema, &t.name)));
        let verb = first_verb(main_statement(&q.text)).map(|(_, v)| v);
        let keys = match table {
            Some(t) if verb == Some("insert") => snapshot
                .foreign_keys
                .iter()
                .filter(|k| k.schema.eq_ignore_ascii_case(&t.schema) && k.table.eq_ignore_ascii_case(&t.name))
                .collect(),
            _ => Vec::new(),
        };
        let (same_key, key_index) = match (table, verb) {
            // No index listed for it at all is no index read, not a table without keys.
            (Some(t), Some("insert")) if snapshot.indexes_on(&t.schema, &t.name).next().is_some() => {
                let key = inserted_unique_key(snapshot, q, t);
                (key.is_some(), key.map(|i| i.name.as_str()))
            }
            _ => (true, None),
        };
        // One row a run, skipped when its key is there: the rows it inserted say how many runs wrote nothing.
        let squeezed = squeeze(&q.text);
        let skips_existing = (squeezed.contains(" on conflict") && squeezed.contains(" do nothing"))
            || squeezed.starts_with("insert ignore ");
        let inserted_nothing = q
            .rows_handled_per_execution()
            .filter(|rows| skips_existing && values_rows(main_statement(&q.text)) == Some(1) && *rows < 0.5)
            .map(|rows| 1.0 - rows);
        let fewest_slow = q.max_time_ms.map(|max| excess / (max - q.mean_time_ms).max(1.0));
        // Only a time the engine measured: no entry is no measurement, not zero.
        let io_ms = q
            .waits
            .iter()
            .filter(|w| w.category == "IO")
            .map(|w| w.wait_ms)
            .reduce(|a, b| a + b)
            .filter(|io| *io < excess * SMALL_RECORDED_IO);
        let stopped = crate::query_stopped(snapshot, q).is_some();
        let successor = stopped.then(|| crate::refine::query_successor(snapshot, q)).flatten();
        WriteWaits {
            keys,
            key_index,
            same_key,
            inserted_nothing,
            io_ms,
            excess,
            fewest_slow,
            mean_ms: q.mean_time_ms,
            stopped,
            successor,
        }
    }

    /// The tables its foreign keys check, as a sentence names them: "apps", "apps and users".
    fn parents(&self) -> String {
        let mut names: Vec<String> = Vec::new();
        for k in &self.keys {
            if !names.iter().any(|n| n.eq_ignore_ascii_case(&k.ref_table)) {
                names.push(k.ref_table.clone());
            }
        }
        format::list(&names)
    }

    /// Why its runs swing and what they may have waited for: a guess the engine's numbers narrow, said as one.
    fn summary(&self, snapshot: &Snapshot) -> String {
        let engine = snapshot.server.engine;
        // What each run reads that its plan doesn't show: its key in the unique index, and the rows its foreign keys
        // check.
        let checks = (!self.keys.is_empty()).then(|| {
            let columns: Vec<String> = self.keys.iter().map(|k| k.columns.join(", ")).collect();
            let one = self.keys.len() == 1;
            format!(
                "its foreign key{} on {} check{} each row it inserts against {}: the referenced row is looked up and share-locked{}",
                if one { "" } else { "s" },
                format::list(&columns),
                if one { "s" } else { "" },
                self.parents(),
                if engine == Engine::Postgres { " (FOR KEY SHARE)" } else { "" }
            )
        });
        let same_work = "the captured plan does not show all work in triggers, constraints or called functions";
        let mut reads = match (self.key_index, checks) {
            (None, None) => format!("Its plan reads no table (it writes the values it is given), {same_work}."),
            (None, Some(checks)) => {
                format!("Its plan reads no table (it writes the values it is given), but {checks}.")
            }
            (Some(index), None) => format!(
                "Its plan shows no read, but each run looks its key up in {index} to see whether a row already has it: one lookup whatever the values, {same_work}."
            ),
            (Some(index), Some(checks)) => format!(
                "Its plan shows no read, but each run looks its key up in {index} to see whether a row already has it, and {checks}."
            ),
        };
        if let Some(nothing) = self.inserted_nothing {
            reads.push_str(&format!(
                " {} of its runs found their key already there and inserted nothing.",
                format::percent(nothing)
            ));
        }
        let io = self
            .io_ms
            .map(|io| {
                format!(
                    " Its recorded data-file I/O took {} in all, against at least {} of extra time in the slow runs. I/O timing may not cover every historical run, so this does not rule out storage delays.",
                    format::duration(io),
                    format::duration(self.excess)
                )
            })
            .unwrap_or_default();
        let mut waits: Vec<String> = Vec::new();
        if !self.keys.is_empty() {
            waits.push(format!("for a transaction that {}", self.referenced(engine)));
        }
        if self.same_key {
            waits.push("for another transaction inserting the same key".into());
        }
        waits.push("for a lock on the table (a migration's, say)".into());
        waits.push(self.storage(engine).into());
        let last = waits.pop().unwrap_or_default();
        format!(
            "{reads}{io} The slow runs may have been waiting {}, or {last}. Triggers and other work absent from this plan can also vary between runs; measure a slow run before choosing a cause.",
            waits.join(", ")
        )
    }

    /// The storage a run can wait on, as this engine times a statement. PostgreSQL's statement time is the executor's:
    /// an autocommit statement's commit, and the WAL flush it waits for, come after it. SQL Server's Query Store and
    /// MySQL's statement events time an autocommit statement's commit with it.
    fn storage(&self, engine: Engine) -> &'static str {
        match engine {
            Engine::Postgres => "on storage (writing WAL or data pages while a checkpoint runs)",
            _ => "on storage (the log flush at commit, when it commits on its own, or a checkpoint)",
        }
    }

    /// What holds the referenced row a foreign key checks, the same words in the summary and the fix: PostgreSQL's
    /// FOR KEY SHARE waits only for a row lock, a delete or a change of the key; the others' shared lock waits for any
    /// change of the row.
    fn referenced(&self, engine: Engine) -> String {
        let holders = match engine {
            Engine::Postgres => "locks (SELECT … FOR UPDATE), deletes or changes the key of",
            _ => "locks, updates or deletes",
        };
        format!("{holders} the referenced row of {}", self.parents())
    }

    /// Where the slow runs are too few to catch while they happen: the fewest that add up to the extra time come less
    /// than once an hour over the data's window. Their count, and the window as a sentence names it.
    fn too_rare_to_watch(&self, snapshot: &Snapshot) -> Option<(f64, String)> {
        let w = snapshot.workload.as_ref()?;
        let fewest = self.fewest_slow?.ceil();
        too_rare_to_catch(snapshot, fewest).then(|| (fewest, w.window_phrase()))
    }

    /// How to see the slow runs Live is unlikely to catch: the engine's log of slow statements and lock waits, set the
    /// way settings are set on this server (with its SQL, where it runs), or what Query Store already keeps; the one
    /// way every check that names a wait too rare to catch says it ([`log_next_waits`]).
    fn record_slow_runs(&self, snapshot: &Snapshot, mean_ms: f64) -> Option<(String, Option<String>)> {
        // A run well past the average, and no lower than a tenth of a second: what the log keeps.
        let bar = [100.0, 200.0, 500.0, 1_000.0, 2_000.0, 5_000.0, 10_000.0]
            .into_iter()
            .find(|b| *b >= mean_ms * 20.0)
            .unwrap_or(10_000.0);
        match snapshot.server.engine {
            Engine::Mssql => Some((
                "Query Store keeps this query's waits for each interval (sys.query_store_wait_stats): the intervals of its slowest runs say what they waited on."
                    .into(),
                None,
            )),
            Engine::Sqlite => None,
            _ => {
                let LockHolder { lead, sql } = log_next_waits(snapshot, Some(bar));
                Some((lead, sql))
            }
        }
    }

    /// What to do: watch the waits while it runs, have the next slow runs logged where they are too rare to watch,
    /// or, once it stopped, what took its place.
    fn fix(&self, snapshot: &Snapshot) -> Fix {
        if self.stopped {
            let successor = self
                .successor
                .map(|q| {
                    let measured = q
                        .max_time_ms
                        .map(|ms| format!(" Its slowest recorded run took {}.", format::duration(ms)))
                        .unwrap_or_default();
                    format!(" Query {}, {} too, still runs and may have replaced it.{measured}", q.id, query_handle(q))
                })
                .unwrap_or_default();
            return Fix {
                summary: format!(
                    "This statement no longer runs.{successor} These historical outliers do not establish a current problem in its replacement or justify a logging change. If slow runs recur, collect their durations and waits and inspect trigger and constraint work before choosing a fix."
                ),
                sql: None,
                cost: None,
            };
        }
        let engine = snapshot.server.engine;
        let watch = "Live shows each session's wait and any blocking chain";
        let rare = self
            .too_rare_to_watch(snapshot)
            .and_then(|(fewest, window)| Some((fewest, window, self.record_slow_runs(snapshot, self.mean_ms)?)));
        let mut sql = None;
        let lead = match rare {
            Some((fewest, window, (record, record_sql))) => {
                sql = record_sql;
                format!(
                    "The extra time can come from as few as {} slow runs in {window}, too few to count on catching one while it happens. {record}",
                    format::count(fewest)
                )
            }
            None => format!("Find what the slow runs wait for while they happen ({watch})."),
        };
        let referenced = self.referenced(engine);
        let holds = "holds this one until it commits or rolls back";
        let blockers = match (self.same_key, self.keys.is_empty()) {
            (true, true) => format!("Another transaction inserting the same key {holds}"),
            (true, false) => {
                format!("Another transaction inserting the same key {holds}, and so does one that {referenced}")
            }
            (false, false) => format!("A transaction that {referenced} {holds}"),
            (false, true) => format!("A transaction that locks the table (a migration's, say) {holds}"),
        };
        let storage = match (self.io_ms.is_some(), engine) {
            (true, _) => "",
            (false, Engine::Postgres) => " Compare storage timing during a slow run with its other work.",
            (false, _) => " Compare log and storage timing during a slow run with its other work.",
        };
        Fix {
            summary: format!(
                "{lead} {blockers}: keep those transactions short.{storage} Inspect the blocking transaction and any trigger or constraint work before deciding whether a query, index or transaction change will help."
            ),
            sql,
            cost: None,
        }
    }
}

/// What the engine recorded of a statement's waits, said against its slow runs' extra time: Query Store's waits per
/// query, MySQL's and MariaDB's lock time. Lock waits that make up half the extra time or more explain the slow runs; a
/// record with none or few of them rules locks out, and its largest other wait says what the slow runs spent their time
/// on. PostgreSQL keeps no waits per statement, and a Query Store without wait statistics recorded none: then both
/// causes stay open, and the sentence says why.
struct RecordedWaits {
    /// The sentence for the summary, after the extra time.
    sentence: String,
    /// Lock waits recorded for it make up half the slow runs' extra time or more.
    locks_explain: bool,
    /// The lock share `mysql_fix` reads: all of it where locks explain the slow runs, none otherwise.
    lock_share: f64,
}

impl RecordedWaits {
    fn read(snapshot: &Snapshot, q: &QueryStat, excess: f64) -> Self {
        let both = "Either some parameter values match far more rows than the plan was built for (parameter sensitivity), or some runs wait on locks";
        let values = "Compare the parameter values of the slow runs with those of the fast ones.";
        let open = |why: &str| RecordedWaits {
            sentence: format!("{both}{why}. {values}"),
            locks_explain: false,
            lock_share: 0.0,
        };
        let engine = snapshot.server.engine;
        let lock_waits = q.waits.iter().filter(|w| w.category == "Lock").map(|w| w.wait_ms).reduce(|a, b| a + b);
        let (source, lock) = match engine {
            Engine::Mssql if !q.waits.is_empty() => ("Query Store".to_string(), lock_waits.unwrap_or(0.0)),
            Engine::Mssql => return open(": Query Store recorded no waits for it to tell which"),
            Engine::Mysql => match q.spread.as_ref().and_then(|s| s.lock_ms).or(lock_waits) {
                Some(lock) => (snapshot.server.name().to_string(), lock),
                None => {
                    return open(&format!(": {} recorded no lock time for it to tell which", snapshot.server.name()));
                }
            },
            Engine::Postgres => return open(" (pg_stat_statements doesn't record what a statement waited for)"),
            Engine::Sqlite => return open(""),
        };
        let extra = format::duration(excess);
        if lock >= excess * 0.5 {
            return RecordedWaits {
                sentence: format!(
                    "{source} recorded {} of lock waits for it, against at least {extra} of extra time in the slow runs: they waited for locks other transactions held, not on a plan built for other values.",
                    format::duration(lock)
                ),
                locks_explain: true,
                lock_share: 1.0,
            };
        }
        let locks = if lock <= 0.0 {
            format!("{source} recorded no lock waits for it")
        } else {
            format!(
                "{source} recorded only {} of lock waits for it, against at least {extra} of extra time",
                format::duration(lock)
            )
        };
        // The largest other wait, where it has a plain meaning: what the slow runs spent their time on.
        let other = q
            .waits
            .iter()
            .filter(|w| !matches!(w.category.as_str(), "Lock" | "CPU"))
            .max_by(|a, b| a.wait_ms.total_cmp(&b.wait_ms))
            .and_then(|w| {
                let meaning = match culprit_core::waits::category_key(&w.category) {
                    "network" => "the application reading the results, which takes longer the more rows a run returns",
                    "io" => "reading pages from storage that weren't in memory, which grows with the rows a run reads",
                    "memory" => "waiting for a memory grant, which grows with the rows a run sorts or hashes",
                    _ => return None,
                };
                Some(format!(
                    "; its largest wait is {} ({}): {meaning}",
                    w.wait_type.as_deref().unwrap_or(&w.category),
                    format::duration(w.wait_ms)
                ))
            })
            .unwrap_or_default();
        RecordedWaits {
            sentence: format!(
                "{locks}, so locks weren't what made them slow{other}. Most likely some parameter values match far more rows than the plan was built for (parameter sensitivity). {values}"
            ),
            locks_explain: false,
            lock_share: 0.0,
        }
    }
}

/// How many rows an INSERT's VALUES list gives: its parenthesized groups, `None` without a VALUES list.
fn values_rows(text: &str) -> Option<usize> {
    let lower = text.to_ascii_lowercase();
    let at = lower.find("values")?;
    let (mut depth, mut rows, mut quote) = (0i32, 0usize, false);
    for c in lower[at + "values".len()..].chars() {
        match c {
            '\'' => quote = !quote,
            _ if quote => {}
            '(' => {
                if depth == 0 {
                    rows += 1;
                }
                depth += 1;
            }
            ')' => depth -= 1,
            // Past the list: what it does on a conflict, or what it returns.
            _ if depth == 0 && !matches!(c, ',' | ' ' | '\n' | '\t' | '\r') => break,
            _ => {}
        }
    }
    (rows > 0).then_some(rows)
}

/// Whether the visible plan writes supplied values without a table/index read. Triggers, constraints and called
/// functions can do additional work that an estimated plan does not show; this narrows possibilities, not the cause.
fn writes_without_reading(plan: &Plan) -> bool {
    let nodes = plan.nodes();
    nodes.iter().any(|n| n.kind == NodeKind::Modify)
        && !nodes.iter().any(|n| matches!(n.kind, NodeKind::Scan | NodeKind::Seek | NodeKind::Lookup))
}

/// What a workload's times add up to, as an impact label says it: "DB time", or "logged slow-run time".
fn time_label(snapshot: &Snapshot) -> &'static str {
    if slow_log_only(snapshot) { "logged slow-run time" } else { "DB time" }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> Snapshot {
        let text =
            std::fs::read_to_string(format!("{}/../../fixtures/{name}", env!("CARGO_MANIFEST_DIR"))).expect("fixture");
        serde_json::from_str(&text).expect("snapshot")
    }

    /// The PostgreSQL demo with its query 3432126458605143571 made `text`, an insert of its values into
    /// order_items whose runs swing: 9,900 at 2 ms and 100 at 5 s. order_items gets a unique key on the order and
    /// the product, which the insert sets.
    fn unstable_insert(text: &str) -> Snapshot {
        let mut s = fixture("postgres-demo.json");
        let (runs, slow) = (10_000.0, 100.0);
        let mean = (2.0 * (runs - slow) + 5_000.0 * slow) / runs;
        let variance = ((2.0f64 - mean).powi(2) * (runs - slow) + (5_000.0f64 - mean).powi(2) * slow) / runs;
        let q = s.workload.as_mut().unwrap().queries.iter_mut().find(|q| q.id == "3432126458605143571").unwrap();
        q.text = text.into();
        (q.executions, q.mean_time_ms, q.total_time_ms) = (runs, mean, mean * runs);
        (q.stddev_time_ms, q.max_time_ms, q.rows) = (Some(variance.sqrt()), Some(5_000.0), Some(runs * 0.25));
        q.plan = Some(
            serde_json::from_value(serde_json::json!({
                "engine": "postgres", "kind": "generic", "total_cost": 0.01, "warnings": [], "index_hints": [],
                "props": {},
                "root": {
                    "id": 0, "op": "ModifyTable", "kind": "modify", "object": "public.order_items",
                    "detail": "Insert", "est_rows": 0.0, "est_executions": 1.0, "subtree_cost": 0.01,
                    "self_cost": 0.0, "warnings": [], "props": {"Conflict Resolution": "NOTHING"},
                    "children": [{
                        "id": 1, "op": "Result", "kind": "compute", "est_rows": 1.0, "est_executions": 1.0,
                        "subtree_cost": 0.01, "self_cost": 0.01, "warnings": [], "props": {}, "children": []
                    }]
                }
            }))
            .expect("plan"),
        );
        let total: f64 = s.workload.as_ref().unwrap().queries.iter().map(|q| q.total_time_ms).sum();
        s.workload.as_mut().unwrap().total_time_ms = total;
        let pkey = s.indexes.iter().find(|i| i.name == "order_items_pkey").cloned().expect("demo index");
        s.indexes.push(culprit_core::IndexStat {
            filter_facts: None,
            name: "order_items_order_product_key".into(),
            columns: vec!["order_id".into(), "product_id".into()],
            is_primary_key: false,
            ..pkey
        });
        s
    }

    fn unstable(s: Snapshot) -> Finding {
        crate::analyze(s)
            .findings
            .into_iter()
            .find(|f| f.id == "unstable-query:3432126458605143571")
            .expect("an unstable insert")
    }

    const SKIPS_EXISTING: &str =
        "INSERT INTO order_items (order_id, product_id, quantity) VALUES ($1, $2, $3) ON CONFLICT DO NOTHING";

    // The aptabase reading: "Its plan reads no table (it writes the values it is given)" for an INSERT … ON CONFLICT
    // DO NOTHING that looks its key up in the primary key on every run, and finds it there 3 times in 4; and the
    // summary's "locks or deletes the referenced row" against the fix's "… or changes its key".
    #[test]
    fn an_insert_that_skips_existing_keys_says_what_each_run_reads() {
        let f = unstable(unstable_insert(SKIPS_EXISTING));
        assert!(
            f.summary.contains(
                "Its plan shows no read, but each run looks its key up in order_items_order_product_key to see whether a row already has it, and its foreign keys on product_id and order_id check each row it inserts against products and orders"
            ),
            "{}",
            f.summary
        );
        assert!(
            f.summary.contains(" 75% of its runs found their key already there and inserted nothing."),
            "{}",
            f.summary
        );
        assert!(!f.summary.contains("reads no table"), "{}", f.summary);
        // The same holders of the referenced row, in the same words, in the summary and the fix.
        let holders =
            "locks (SELECT … FOR UPDATE), deletes or changes the key of the referenced row of products and orders";
        assert!(f.summary.contains(&format!("waiting for a transaction that {holders}")), "{}", f.summary);
        let fix = &f.fix.as_ref().unwrap().summary;
        assert!(fix.contains(&format!("and so does one that {holders}: keep those transactions short")), "{fix}");
        // A plain insert of its values: nothing skipped, nothing said about it.
        let f = unstable(unstable_insert(
            "INSERT INTO order_items (order_id, product_id, quantity) VALUES ($1, $2, $3), ($4, $5, $6) ON CONFLICT DO NOTHING",
        ));
        assert!(!f.summary.contains("inserted nothing"), "two rows a run: {}", f.summary);
    }

    // The aptabase reading: "Find what the slow runs wait for while they happen (Live…)" for slow runs that may number
    // 16 in 3.5 years. Too rare to catch live, the next ones are logged, the way settings are set on the server.
    #[test]
    fn slow_runs_too_rare_to_watch_are_logged_next_time() {
        // The demo's five hours: 100 slow runs, 20 an hour, are there to be watched.
        let f = unstable(unstable_insert(SKIPS_EXISTING));
        let fix = f.fix.as_ref().unwrap();
        assert!(fix.summary.starts_with("Find what the slow runs wait for while they happen (Live"), "{}", fix.summary);
        assert_eq!(fix.sql, None);
        // The same runs over three and a half years of counters.
        let rare = |cloud: Option<&str>| {
            let mut s = unstable_insert(SKIPS_EXISTING);
            s.workload.as_mut().unwrap().window_hours = 30_000.0;
            s.server.cloud = cloud.map(str::to_string);
            unstable(s).fix.unwrap()
        };
        let fix = rare(None);
        assert!(
            fix.summary.starts_with(
                "The extra time can come from as few as 100 slow runs in the 1250 days since statistics were reset, too few to count on catching one while it happens. Have the next ones logged instead. Set log_lock_waits to on and log_min_duration_statement to 2s"
            ),
            "{}",
            fix.summary
        );
        assert!(fix.summary.contains("with the process holding the lock"), "{}", fix.summary);
        let sql = fix.sql.unwrap_or_default();
        assert!(sql.contains("ALTER SYSTEM SET log_lock_waits") && sql.contains("pg_reload_conf()"), "{sql}");
        // A managed server sets them in its own control.
        let fix = rare(Some("Amazon RDS"));
        assert!(fix.summary.contains("DB parameter group"), "{}", fix.summary);
        assert_eq!(fix.sql, None);
    }

    // The aptabase-us reading: "watch them (Live shows each session's wait…)" for the statement that replaced a stopped
    // insert, whose slowest run so far took 88 ms: there is nothing to catch live, and its next slow runs are logged,
    // the way unstable-query logs any too rare to watch.
    #[test]
    fn historical_outliers_do_not_change_logging_for_a_fast_replacement() {
        let mut s = unstable_insert(SKIPS_EXISTING);
        let w = s.workload.as_mut().unwrap();
        w.window_hours = 48.0;
        let q = w.queries.iter_mut().find(|q| q.id == "3432126458605143571").unwrap();
        (q.first_seen, q.last_seen) = (Some("2026-10-09T12:00:00Z".into()), Some("2026-10-10T08:00:00Z".into()));
        let next = w.queries.iter_mut().find(|q| q.id == "2885972271926804069").unwrap();
        next.text = "INSERT INTO order_items (order_id, product_id, quantity, note) VALUES ($1, $2, $3, $4)".into();
        (next.first_seen, next.last_seen) = (Some("2026-10-10T08:05:00Z".into()), Some("2026-10-11T11:50:00Z".into()));
        (next.executions, next.mean_time_ms, next.total_time_ms, next.max_time_ms) =
            (5_000.0, 0.5, 2_500.0, Some(88.0));
        next.plan = None;
        let f = unstable(s);
        assert!(f.inactive, "it stopped");
        let fix = f.fix.unwrap();
        assert!(fix.summary.contains("This statement no longer runs."), "{}", fix.summary);
        assert!(fix.summary.contains("2885972271926804069") && fix.summary.contains("88 ms"));
        assert!(fix.summary.contains("do not establish a current problem"));
        assert!(!fix.summary.contains("Set log_") && !fix.summary.contains("Have the next ones logged"));
        assert_eq!(fix.sql, None, "a finding on a statement that stopped runs nothing for it");
    }

    // The SQL Server demo's fresh reading: "or some runs wait on locks. Check the waits for this query" where Query Store
    // recorded 3.4 s of Network IO, 43 ms of memory, 11 ms of CPU and no lock waits for it.
    #[test]
    fn the_summary_reads_the_waits_the_engine_recorded() {
        let mut s = crate::waits::snapshot_with(Engine::Mssql);
        let w = s.workload.as_mut().unwrap();
        let q = w.queries.iter_mut().find(|q| q.id == "10").expect("the demo's read of support_tickets");
        let (runs, slow) = (8_000.0, 10.0);
        let mean = (1.2 * (runs - slow) + 1_100.0 * slow) / runs;
        let variance = ((1.2f64 - mean).powi(2) * (runs - slow) + (1_100.0f64 - mean).powi(2) * slow) / runs;
        (q.executions, q.mean_time_ms, q.total_time_ms) = (runs, mean, mean * runs);
        (q.stddev_time_ms, q.max_time_ms) = (Some(variance.sqrt()), Some(1_100.0));
        q.plans.truncate(1);
        let wait = |category: &str, wait_type: &str, ms: f64| culprit_core::WaitStat {
            category: category.into(),
            wait_type: Some(wait_type.into()),
            wait_ms: ms,
            tasks: None,
        };
        q.waits =
            vec![wait("Network", "Network IO", 3_406.0), wait("Memory", "Memory", 43.0), wait("CPU", "CPU", 11.0)];
        let total: f64 = w.queries.iter().map(|q| q.total_time_ms).sum();
        w.total_time_ms = total;
        let found =
            |s: Snapshot| unstable_query(&s).into_iter().find(|f| f.id == "unstable-query:10").expect("unstable");
        let f = found(s.clone());
        assert!(
            f.summary.contains("Query Store recorded no lock waits for it, so locks weren't what made them slow; its largest wait is Network IO (3.4 s): the application reading the results, which takes longer the more rows a run returns. Most likely some parameter values match far more rows than the plan was built for"),
            "{}",
            f.summary
        );
        assert!(!f.summary.contains("Check the waits"), "{}", f.summary);
        // Lock waits that make up the slow runs' time are the cause: the fix is what holds them, not a plan.
        let q = s.workload.as_mut().unwrap().queries.iter_mut().find(|q| q.id == "10").unwrap();
        q.waits = vec![wait("Lock", "Lock", 10_000.0)];
        let f = found(s);
        assert!(f.summary.contains("Query Store recorded 10.0 s of lock waits for it"), "{}", f.summary);
        let fix = f.fix.unwrap();
        assert!(!fix.summary.contains("Pick the lightest option"), "{}", fix.summary);
        assert!(fix.summary.contains("Keep the transactions that hold those rows short"), "{}", fix.summary);
    }

    #[test]
    fn a_values_list_counts_its_rows() {
        assert_eq!(values_rows("INSERT INTO t (a, b) VALUES ($1, $2) ON CONFLICT DO NOTHING"), Some(1));
        assert_eq!(values_rows("INSERT INTO t (a, b) VALUES ($1, '(x)'), ($3, $4)"), Some(2));
        assert_eq!(values_rows("INSERT INTO t (a) SELECT a FROM u"), None);
    }

    #[test]
    fn verification_uses_new_runs_from_the_sources_own_periods() {
        for engine in ["postgres", "mysql"] {
            let s = fixture(&format!("{engine}-demo.json"));
            let step = verify_new_runs(&s);
            assert!(step.contains("keeps running totals") && step.contains("use the new log samples"), "{step}");
            assert!(step.contains("same logging threshold"), "{step}");
        }
        let mut s = fixture("mssql-demo.json");
        let step = verify_new_runs(&s);
        assert!(step.contains("equal Query Store windows") && step.contains("only runs after it"), "{step}");
        assert!(step.contains("lock waits per run where recorded"), "{step}");
        let workload = s.workload.as_mut().unwrap();
        workload.kind = Some(culprit_core::WorkloadKind::PlanCache);
        workload.source = "Plan cache".into();
        workload.windowed = false;
        let step = verify_new_runs(&s);
        assert!(step.contains("actual execution plans") && step.contains("Plan cache keeps running totals"), "{step}");
        let s = fixture("sqlite-demo.json");
        let step = verify_new_runs(&s);
        assert!(step.contains("equal-length query-log samples") && step.contains("timestamps"), "{step}");
        assert!(!step.contains("server log") && !step.contains("keeps running totals"), "{step}");
        let mut s = fixture("mysql-demo.json");
        s.workload.as_mut().unwrap().kind = Some(culprit_core::WorkloadKind::SlowLog);
        assert!(verify_new_runs(&s).contains("equal-length query-log samples"));
    }
}
