//! The `unstable-query` check: Queries whose duration swings wildly between runs with the same plan, a sign of parameter sensitivity or blocking.

use super::*;

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

check!(UnstableQueryCheck: unstable_query_info, unstable_query);

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
            f.impact.time_ms_per_day = workload.per_day_measured(excess);
            f.severity = severity_for_time(snapshot, excess, slow_share);
            f.impact.score = score_for_share(slow_share);
            f.impact.label = format!("{} of {} in slow runs", format::percent(slow_share), time_label(snapshot));
            f.confidence = Confidence::Medium;
            // A plan that reads no table (an INSERT of the values it is given) does about the same work whatever the
            // values: its slow runs most likely waited, and nothing about parameters or statistics applies.
            let waited = (snapshot.server.engine.has_server() && q.plan.as_ref().is_some_and(writes_without_reading))
                .then(|| WriteWaits::read(snapshot, q, excess));
            f.title = format!(
                "{} averages {} per run, but some runs take far longer (up to {})",
                capitalize(&query_handle(q)),
                format::duration(q.mean_time_ms),
                format::duration(max)
            );
            // One plan in Query Store backs "same plan"; elsewhere the engine keeps no plan identity per run.
            let same = if q.plans.len() == 1 { "Same query, same plan" } else { "Same statement" };
            f.summary = if let Some(w) = &waited {
                format!(
                    "{same}, very different run times: the slow runs took at least {} more than the average, together. {}",
                    format::duration(excess),
                    w.summary(snapshot)
                )
            } else if snapshot.server.engine.has_server() {
                format!(
                    "{same}, very different run times: the slow runs took at least {} more than the average, together. Either some parameter values match far more rows than the plan was built for (parameter sensitivity), or some runs wait on locks. Check the waits for this query, then the parameter values of the slow runs.",
                    format::duration(excess)
                )
            } else {
                // SQLite: no server to record waits; a run that waits is waiting for the file's write lock.
                format!(
                    "{same}, very different run times: the slow runs took at least {} more than the average, together. Either some values match far more rows than the plan expects, or some runs wait for the file's write lock (up to the application's busy_timeout). Compare the slow runs' values in the log.",
                    format::duration(excess)
                )
            };
            f.queries = query_refs(&[q]);
            f.metrics.push(Metric::new("Average", q.mean_time_ms, Unit::Ms));
            f.metrics.push(Metric::new("Slowest", max, Unit::Ms));
            f.metrics.push(Metric::new("Std deviation", stddev, Unit::Ms));
            f.metrics.push(Metric::new("Slow runs' extra time (at least)", excess, Unit::Ms));
            if let Some(io) = waited.as_ref().and_then(|w| w.io_ms) {
                f.metrics.push(Metric::new("Data-file reads and writes", io, Unit::Ms));
            }
            // The same ladders as `parameter-sensitive`: the cause is usually the same, only the evidence is coarser.
            f.fix = Some(match (&waited, snapshot.server.engine) {
                (Some(w), _) => w.fix(snapshot),
                (None, Engine::Mssql) => crate::queries::mssql_fix(snapshot, q, None),
                (None, Engine::Postgres) => crate::queries::postgres_fix(),
                (None, Engine::Mysql) => crate::queries::mysql_fix(0.0, snapshot.server.is_mariadb()),
                (None, Engine::Sqlite) => Fix {
                    summary: "Compare a slow and a fast value with EXPLAIN QUERY PLAN. If some values match far more rows, an index that narrows them, or ANALYZE, lets SQLite choose better.".into(),
                    sql: Some("ANALYZE;".into()),
                    cost: None,
                },
            });
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

/// Data-file time under this share of the slow runs' extra time can't be why they were slow.
const IO_RULED_OUT: f64 = 0.1;

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
            .filter(|io| *io < excess * IO_RULED_OUT);
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
        let same_work = "so no value makes one run do much more work than another";
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
                    " Its data-file reads and writes took {} in all, against at least {} of extra time in the slow runs: storage reads weren't it.",
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
        format!("{reads}{io} The slow runs may have been waiting {}, or {last}.", waits.join(", "))
    }

    /// The storage a run can wait on, as this engine times a statement. PostgreSQL's statement time is the executor's:
    /// an autocommit statement's commit, and the WAL flush it waits for, come after it. SQL Server's Query Store and
    /// MySQL's statement events time an autocommit statement's commit with it.
    fn storage(&self, engine: Engine) -> &'static str {
        match (engine, self.io_ms.is_some()) {
            (Engine::Postgres, false) => "on storage (writing WAL or data pages while a checkpoint runs)",
            (Engine::Postgres, true) => "on writing WAL (while a checkpoint runs, say)",
            (_, false) => "on storage (the log flush at commit, when it commits on its own, or a checkpoint)",
            (_, true) => "on the log flush at commit (when it commits on its own)",
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
        let w = snapshot.workload.as_ref().filter(|w| !w.window_unknown() && w.window_hours > 0.0)?;
        let fewest = self.fewest_slow?.ceil();
        (fewest / w.window_hours < 1.0).then(|| (fewest, w.window_phrase()))
    }

    /// How to see the slow runs Live is unlikely to catch: the engine's log of slow statements and lock waits, set the
    /// way settings are set on this server (with its SQL, where it runs), or what Query Store already keeps.
    fn record_slow_runs(&self, snapshot: &Snapshot) -> Option<(String, Option<String>)> {
        let server = &snapshot.server;
        // A run well past the average, and no lower than a tenth of a second: what the log keeps.
        let bar = [100.0, 200.0, 500.0, 1_000.0, 2_000.0, 5_000.0, 10_000.0]
            .into_iter()
            .find(|b| *b >= self.mean_ms * 20.0)
            .unwrap_or(10_000.0);
        match server.engine {
            Engine::Postgres => {
                let threshold = if bar >= 1_000.0 { format!("{}s", bar / 1_000.0) } else { format!("{bar}ms") };
                let change = crate::config::settings_change(
                    server,
                    &[("log_lock_waits", "on"), ("log_min_duration_statement", &threshold)],
                );
                let deadlock = match snapshot.setting("deadlock_timeout").map(|s| s.value.trim().to_string()) {
                    Some(v) if v.parse::<f64>().is_ok() => format!("{v} ms"),
                    Some(v) => v,
                    None => "1 s by default".into(),
                };
                Some((
                    format!(
                        "Have the next ones logged instead. {} log_lock_waits then logs each lock wait longer than deadlock_timeout ({deadlock}) with the process holding the lock, and log_min_duration_statement each statement slower than {threshold} (any statement, not only this one) with its duration.",
                        change.how
                    ),
                    change.sql,
                ))
            }
            Engine::Mysql => {
                let seconds = format!("{}", bar / 1000.0);
                let change = crate::config::settings_change(
                    server,
                    &[("slow_query_log", "ON"), ("long_query_time", &seconds)],
                );
                Some((
                    format!(
                        "Have the next ones logged instead. {} The slow query log then keeps each statement slower than {seconds} s with its lock time.",
                        change.how
                    ),
                    change.sql,
                ))
            }
            Engine::Mssql => Some((
                "Query Store keeps this query's waits for each interval (sys.query_store_wait_stats): the intervals of its slowest runs say what they waited on."
                    .into(),
                None,
            )),
            Engine::Sqlite => None,
        }
    }

    /// What to do: watch the waits while it runs, have the next slow runs logged where they are too rare to watch,
    /// or, once it stopped, what took its place.
    fn fix(&self, snapshot: &Snapshot) -> Fix {
        let engine = snapshot.server.engine;
        let watch = "Live shows each session's wait and any blocking chain";
        let rare = self
            .too_rare_to_watch(snapshot)
            .filter(|_| !self.stopped)
            .and_then(|(fewest, window)| Some((fewest, window, self.record_slow_runs(snapshot)?)));
        let mut sql = None;
        let lead = match (self.stopped, self.successor, rare) {
            (false, _, Some((fewest, window, (record, record_sql)))) => {
                sql = record_sql;
                format!(
                    "The extra time can come from as few as {} slow runs in {window}, too few to count on catching one while it happens. {record}",
                    format::count(fewest)
                )
            }
            (false, _, None) => format!("Find what the slow runs wait for while they happen ({watch})."),
            (true, Some(next), _) => format!(
                "This statement no longer runs. Query {}, {} too, still does: if it replaced this one, its runs can wait the same way, so watch them ({watch}).",
                next.id,
                query_handle(next)
            ),
            (true, None, _) => format!(
                "This statement no longer runs, so there is nothing to watch now; if it comes back, find what its slow runs wait for while they happen ({watch})."
            ),
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
            (false, Engine::Postgres) => " Waits on storage slow every write at once and point at the storage.",
            (false, _) => " Waits on the log or on storage slow every write at once and point at the storage.",
        };
        Fix {
            summary: format!(
                "{lead} {blockers}: keep those transactions short.{storage} Neither a different plan nor finer statistics changes it."
            ),
            sql,
            cost: None,
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

/// Whether the plan writes without reading any table or index: an INSERT of the values it is given (a modification over
/// a Result or a Constant Scan). Its values can't make it read more, so a run slower than the rest waited.
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

    #[test]
    fn a_values_list_counts_its_rows() {
        assert_eq!(values_rows("INSERT INTO t (a, b) VALUES ($1, $2) ON CONFLICT DO NOTHING"), Some(1));
        assert_eq!(values_rows("INSERT INTO t (a, b) VALUES ($1, '(x)'), ($3, $4)"), Some(2));
        assert_eq!(values_rows("INSERT INTO t (a) SELECT a FROM u"), None);
    }
}
