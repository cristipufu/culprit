//! The `heavy-query` check: Single queries that take a large share of database time, with what their plan spends it on.

use super::*;

pub fn heavy_query_info() -> CheckInfo {
    CheckInfo {
        id: "heavy-query".into(),
        title: "Queries that dominate database time".into(),
        category: Category::Queries,
        engines: vec![Engine::Mssql, Engine::Postgres, Engine::Mysql, Engine::Sqlite],
        requires: vec![],
        description: "Single queries that take a large share of database time, with what their plan spends it on."
            .into(),
    }
}

check!(HeavyQueryCheck: heavy_query_info, heavy_query);

pub fn heavy_query(snapshot: &Snapshot) -> Vec<Finding> {
    let Some(workload) = &snapshot.workload else {
        return Vec::new();
    };
    let mut queries: Vec<&QueryStat> = workload
        .queries
        .iter()
        .filter(|q| !is_monitoring_query(q) && workload.share_of(q.total_time_ms, q) >= HEAVY_SHARE)
        .collect();
    queries.sort_by(|a, b| b.total_time_ms.total_cmp(&a.total_time_ms));
    queries.into_iter().take(6).map(|q| heavy_query_finding(snapshot, q)).collect()
}

pub(super) fn heavy_query_finding(snapshot: &Snapshot, q: &QueryStat) -> Finding {
    let impact = query_impact(snapshot, &[q]).expect("workload present");
    let handle = query_handle(q);
    let time = time_noun(snapshot);
    let mut f = finding("heavy-query", &q.id, Category::Queries, subject(SubjectKind::Query, &q.id));
    apply_impact(snapshot, &mut f, &impact);
    f.severity = severity_for_workload_share(snapshot, impact.share);
    f.confidence = Confidence::High;
    let per_day_runs = rate_per_day(snapshot, q.executions);
    let kind = statement_kind(&q.text);
    // Rows returned measure waste only for a read that returns what it finds: an aggregate returns one row per group
    // whatever it reads, and a write returns none.
    let reads_per_row = match (q.reads_per_execution(), q.rows_per_execution()) {
        (Some(reads), Some(rows)) if kind == StatementKind::Read => Some(reads / rows.max(1.0)),
        _ => None,
    };
    let idle = idle_time(snapshot, q);
    let hot = q.plan.as_ref().and_then(|p| Some((p, p.hottest()?))).filter(|_| idle.is_none());
    let share = format::percent(impact.share);
    f.title = match (&idle, reads_per_row) {
        (Some(idle), _) => format!("{} takes {share} of {time}, {}", capitalize(&handle), idle.title_tail()),
        (None, Some(r)) if r >= 1000.0 && snapshot.server.engine == Engine::Mysql => {
            format!("{} examines {} rows for every row it returns", capitalize(&handle), format::count(r))
        }
        (None, Some(r)) if r >= 1000.0 => {
            format!("{} reads {} pages for every row it returns", capitalize(&handle), format::count(r))
        }
        _ => format!("{} takes {share} of {time}", capitalize(&handle)),
    };
    // With time the source couldn't attribute, the share is of what it could.
    let of = if snapshot.workload.as_ref().is_some_and(|w| w.untracked_time_ms.is_some()) {
        format!("of the {time} {} could attribute", snapshot.workload.as_ref().map(|w| w.source.as_str()).unwrap_or(""))
    } else {
        format!("of all {time}")
    };
    let mut summary = format!(
        "It ran {} at {}{}: {share} {of} in {}.",
        runs_phrase(snapshot, q.executions),
        format::duration(q.mean_time_ms),
        if q.executions > 1.0 { " on average" } else { "" },
        snapshot.workload.as_ref().map(window_phrase).unwrap_or_default()
    );
    if let Some(idle) = &idle {
        summary.push(' ');
        summary.push_str(&idle.evidence);
    } else if let Some((plan, node)) = hot {
        let pct = if plan.total_cost > 0.0 { node.self_cost / plan.total_cost } else { 0.0 };
        summary.push_str(&format!(" {} of its estimated cost is {}.", format::percent(pct), describe_node(node)));
    }
    f.summary = summary;
    f.queries = vec![QueryRef { id: q.id.clone(), plan_node: hot.map(|(_, n)| n.id), note: None }];
    f.metrics.push(Metric::new(share_metric_label(snapshot), impact.share, Unit::Ratio));
    if let Some(runs) = snapshot.workload.as_ref().and_then(|w| w.per_day_measured(q.executions)) {
        f.metrics.push(Metric::new("Runs per day", runs, Unit::Count));
    }
    f.metrics.push(Metric::new("Average duration", q.mean_time_ms, Unit::Ms));
    if let Some(r) = q.reads_per_execution() {
        let label =
            if snapshot.server.engine == Engine::Mysql { "Rows examined per run" } else { "Pages read per run" };
        f.metrics.push(Metric::new(label, r, Unit::Pages));
    }
    if let (Some(r), Some(label)) = (q.rows_per_execution(), rows_label(snapshot.server.engine, kind)) {
        f.metrics.push(Metric::new(label, r, Unit::Rows));
    }
    if let Some(cpu) = q.cpu_time_ms.filter(|_| q.total_time_ms > 0.0) {
        f.metrics.push(Metric::new("CPU share of its time", (cpu / q.total_time_ms).min(1.0), Unit::Ratio));
    }
    match &idle {
        Some(idle) => {
            if idle.kind == IdleKind::Sleep || idle.kind == IdleKind::AppLock {
                // A deliberate pause holds a connection, not the server's resources: listed, ranked low.
                f.severity = Severity::Low;
                f.impact.score = f.impact.score.min(base_score(Severity::Low));
            }
            f.confidence = idle.confidence;
            f.fix = Some(Fix { summary: idle.fix(), sql: None, cost: None });
            f.verify = vec![idle.verify().into()];
        }
        None => {
            f.fix = Some(Fix {
                summary: heavy_fix_text(hot.map(|(_, n)| n), per_day_runs, q, kind),
                sql: None,
                cost: None,
            });
            f.verify = vec![format!(
                "After the change ships, re-scan and compare this query's share of {time} and reads per run."
            )];
        }
    }
    f
}

/// What a statement does with the rows it touches, as far as its text says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StatementKind {
    /// Returns the rows it finds.
    Read,
    /// Returns one row per group, or one row in all: COUNT, SUM, GROUP BY, DISTINCT.
    Aggregate,
    /// INSERT, UPDATE, DELETE, MERGE: changes rows and returns none.
    Write,
    Other,
}

pub(crate) fn statement_kind(text: &str) -> StatementKind {
    let lower = squeeze(text);
    let body = lower.trim_start_matches(|c: char| c.is_whitespace() || c == '(');
    if ["insert", "update", "delete", "merge", "replace"].iter().any(|v| body.starts_with(v)) {
        return StatementKind::Write;
    }
    if !(body.starts_with("select") || body.starts_with("with")) {
        return StatementKind::Other;
    }
    let aggregate = ["count(", "sum(", "avg(", "min(", "max(", "string_agg(", "array_agg(", "group_concat("]
        .iter()
        .any(|f| calls(&lower, f))
        || lower.contains(" group by ")
        || lower.contains("select distinct ");
    if aggregate { StatementKind::Aggregate } else { StatementKind::Read }
}

/// Whether squeezed text calls `function` (written with its opening parenthesis), as a whole name.
pub(super) fn calls(squeezed: &str, function: &str) -> bool {
    squeezed.match_indices(function).any(|(at, _)| {
        squeezed[..at].chars().next_back().is_none_or(|c| !(c.is_ascii_alphanumeric() || c == '_' || c == '.'))
    })
}

/// "Rows returned per run", or what the engine's row count means for a write. MySQL counts no rows sent for a write.
pub(super) fn rows_label(engine: Engine, kind: StatementKind) -> Option<&'static str> {
    match (kind, engine) {
        (StatementKind::Write, Engine::Postgres) => Some("Rows changed per run"),
        (StatementKind::Write, Engine::Mssql) => Some("Rows per run"),
        (StatementKind::Write, Engine::Mysql) => None,
        _ => Some("Rows returned per run"),
    }
}

/// Where a statement's time goes when it isn't reading or computing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum IdleKind {
    /// It calls a sleep function: the pause is what it was asked to do.
    Sleep,
    /// It waits for an application lock (GET_LOCK, pg_advisory_lock, sp_getapplock).
    AppLock,
    /// Locks other transactions hold.
    Locks,
    /// The application reading its results.
    Client,
    /// A memory grant.
    Memory,
    /// Pages other sessions are using (latches).
    Latch,
    /// Something the engine doesn't name.
    Unknown,
}

pub(super) struct Idle {
    kind: IdleKind,
    /// One or two sentences: what shows it.
    evidence: String,
    confidence: Confidence,
    /// The engine named the cause (Query Store's waits, MySQL's lock time) rather than Culprit inferring it.
    measured: bool,
    /// The wait's share of the statement's time, where that is what named it: "most of it" only from half on.
    share: Option<f64>,
}

impl Idle {
    fn title_tail(&self) -> String {
        let part = match self.share {
            Some(s) if s < 0.5 => format!("{} of it", format::percent(s)),
            _ => "most of it".to_string(),
        };
        let most = if self.measured { part.clone() } else { "most likely".to_string() };
        match self.kind {
            IdleKind::Sleep => "sleeping".into(),
            IdleKind::AppLock => "waiting for an application lock".into(),
            IdleKind::Locks => format!("{most} waiting for locks"),
            IdleKind::Client => format!("{part} waiting for the application to read the results"),
            IdleKind::Memory => format!("{part} waiting for memory"),
            IdleKind::Latch => format!("{part} waiting for pages other sessions are using"),
            IdleKind::Unknown => "most of it waiting rather than working".into(),
        }
    }

    fn fix(&self) -> String {
        match self.kind {
            IdleKind::Sleep => "Find who sends it (a health check, a test, a retry loop with a back-off) and whether the pause belongs in the database at all: while it sleeps it holds a connection, and any locks its transaction took.".into(),
            IdleKind::AppLock => "Find what holds the lock and for how long (Live shows the sessions), and shorten that work; or take the lock without waiting (a try-lock, or a lock timeout) and retry later.".into(),
            IdleKind::Locks => "Find what holds the locks while it waits (Live shows the blocking chain) and keep those transactions short: commit sooner, and don't wait for a user or another service inside a transaction. An index or a rewrite of this statement won't shorten the wait.".into(),
            IdleKind::Client => "The application reads the results slowly, or row by row while it does other work. Read all rows first and process them afterwards, or fetch fewer.".into(),
            IdleKind::Memory => "It waits for a memory grant while other queries hold the memory: find the queries with the largest grants (Health) and reduce them, usually with better estimates or an index that avoids a big sort or hash.".into(),
            IdleKind::Latch => "Sessions queue for the same pages: typically inserts at the end of an index on an increasing key, or tempdb allocation. See the hot-rows and last-page findings, and Live while it happens.".into(),
            IdleKind::Unknown => "Find what it waits for while it runs: Live shows each session's wait and any blocking chain. Typical causes are locks other transactions hold, a slow client reading the results, or a call outside the database.".into(),
        }
    }

    fn verify(&self) -> &'static str {
        match self.kind {
            IdleKind::Sleep | IdleKind::AppLock => {
                "Re-scan after the change: this statement's time drops or it stops running."
            }
            _ => "Re-scan after the change: its average duration drops toward what its reads take.",
        }
    }
}

/// Sleep functions, by the name to show.
pub(super) const SLEEPS: [(&str, &str); 6] = [
    ("pg_sleep(", "pg_sleep"),
    ("pg_sleep_for(", "pg_sleep_for"),
    ("pg_sleep_until(", "pg_sleep_until"),
    ("sleep(", "SLEEP"),
    ("waitfor delay", "WAITFOR"),
    ("waitfor time", "WAITFOR"),
];

/// Application locks a statement waits for until another session releases them.
pub(super) const APP_LOCKS: [(&str, &str); 5] = [
    ("get_lock(", "GET_LOCK"),
    ("pg_advisory_lock(", "pg_advisory_lock"),
    ("pg_advisory_xact_lock(", "pg_advisory_xact_lock"),
    ("pg_advisory_lock_shared(", "pg_advisory_lock_shared"),
    ("sp_getapplock", "sp_getapplock"),
];

/// Runs this long or longer are worth explaining by what they wait for.
pub(super) const IDLE_MIN_MS: f64 = 100.0;
/// Generous costs of reading: a page from a slow disk, a row examined from one (MySQL). Reads that can't take a
/// tenth of a run's time even at these rates don't explain it.
const PAGE_READ_MS: f64 = 10.0;
const ROW_READ_MS: f64 = 1.0;
/// CPU under this share of the time: the statement isn't computing for most of it.
const CPU_SMALL: f64 = 0.2;

/// When a statement's time isn't work: it sleeps, waits for a lock, or does too little reading and computing to fill
/// its run time. Uses the engine's own account (Query Store waits, MySQL lock time) where there is one.
pub(super) fn idle_time(snapshot: &Snapshot, q: &QueryStat) -> Option<Idle> {
    let text = squeeze(&q.text);
    let named = |list: &[(&str, &'static str)]| -> Option<&'static str> {
        list.iter()
            .find(|(pattern, _)| if pattern.ends_with('(') { calls(&text, pattern) } else { text.contains(pattern) })
            .map(|(_, name)| *name)
    };
    if let Some(name) = named(&SLEEPS) {
        return Some(Idle {
            kind: IdleKind::Sleep,
            evidence: format!(
                "It calls {name}: the time is a deliberate pause, not work, and no index or rewrite makes it shorter."
            ),
            confidence: Confidence::High,
            measured: true,
            share: None,
        });
    }
    if let Some(name) = named(&APP_LOCKS) {
        return Some(Idle {
            kind: IdleKind::AppLock,
            evidence: format!(
                "It calls {name}, which waits until the session holding that lock releases it: its time is how long others hold the lock, not work it does."
            ),
            confidence: Confidence::High,
            measured: true,
            share: None,
        });
    }
    if q.mean_time_ms < IDLE_MIN_MS || q.total_time_ms <= 0.0 {
        return None;
    }
    let engine = snapshot.server.engine;
    let run = format::duration(q.mean_time_ms);
    let read_cost = if engine == Engine::Mysql { ROW_READ_MS } else { PAGE_READ_MS };
    let reads = q.reads_per_execution();
    let reads_small =
        reads.is_some_and(|r| r * read_cost < q.mean_time_ms * 0.1) && q.temp_writes.is_none_or(|t| t <= 0.0);
    let cpu_share = q.cpu_time_ms.map(|c| c / q.total_time_ms);
    let cpu_small = cpu_share.is_some_and(|s| s < CPU_SMALL);
    let unit = if engine == Engine::Mysql { "rows examined" } else { "pages" };
    let reads_text = reads.map(|r| format!("{} {unit} per run", format::count(r))).unwrap_or_default();

    // MySQL's own lock time (row locks too from 8.0.28; the slow query log's lock time).
    let lock_ms = q.spread.as_ref().and_then(|s| s.lock_ms).or_else(|| {
        (engine == Engine::Mysql).then(|| q.waits.iter().filter(|w| w.category == "Lock").map(|w| w.wait_ms).sum())
    });
    if engine == Engine::Mysql
        && let Some(lock) = lock_ms.filter(|l| *l >= q.total_time_ms * 0.5)
    {
        return Some(Idle {
            kind: IdleKind::Locks,
            evidence: format!(
                "{} counts {} of its {} waiting for locks.",
                snapshot.server.name(),
                format::percent((lock / q.total_time_ms).min(1.0)),
                format::duration(q.total_time_ms)
            ),
            confidence: Confidence::High,
            measured: true,
            share: None,
        });
    }

    // Query Store's waits per query (summed over parallel threads, so a category names the cause, not a share).
    if engine == Engine::Mssql && cpu_small {
        let top = q
            .waits
            .iter()
            .filter(|w| w.category != "CPU" && w.wait_ms >= q.total_time_ms * 0.25)
            .max_by(|a, b| a.wait_ms.total_cmp(&b.wait_ms));
        // Categories as the SQL Server provider folds Query Store's (`wait_category`).
        let kind = match top.map(|w| w.category.as_str()) {
            Some("Lock") => Some(IdleKind::Locks),
            Some("Network") => Some(IdleKind::Client),
            Some("Memory") => Some(IdleKind::Memory),
            Some("Latch") => Some(IdleKind::Latch),
            // Reading from disk, writing the log, compiling or a parallel plan's threads are the statement's own work.
            Some("IO" | "Log" | "Parallelism" | "Compilation") => return None,
            Some(_) | None => None,
        };
        let cpu = format::percent(cpu_share.unwrap_or(0.0));
        if let (Some(kind), Some(w)) = (kind, top) {
            return Some(Idle {
                kind,
                evidence: format!(
                    "Only {cpu} of its time is CPU, and Query Store records {} of {} waits for it.",
                    format::duration(w.wait_ms),
                    w.wait_type.as_deref().unwrap_or(&w.category)
                ),
                confidence: Confidence::High,
                measured: true,
                // Summed over a parallel plan's threads it can pass the statement's time: then it is most of it.
                share: Some((w.wait_ms / q.total_time_ms).min(1.0)),
            });
        }
        if reads_small {
            return Some(Idle {
                kind: IdleKind::Unknown,
                evidence: format!(
                    "Only {cpu} of its time is CPU, and {reads_text} take far less than {run}: most of each run is spent waiting."
                ),
                confidence: Confidence::Medium,
                measured: false,
                share: None,
            });
        }
        return None;
    }
    if cpu_small && reads_small {
        return Some(Idle {
            kind: IdleKind::Unknown,
            evidence: format!(
                "Only {} of its time is CPU, and {reads_text} take far less than {run}: most of each run is spent waiting.",
                format::percent(cpu_share.unwrap_or(0.0))
            ),
            confidence: Confidence::Medium,
            measured: false,
            share: None,
        });
    }
    // No CPU time (PostgreSQL, the slow query log): a write that reads a handful of pages can't compute for seconds,
    // so it waited, most likely for a row lock. A read could be computing (functions), so it isn't called waiting.
    if cpu_share.is_none() && reads_small && statement_kind(&q.text) == StatementKind::Write {
        // MySQL counted its lock time and it wasn't most of the run: waiting, but not for what MySQL calls locks.
        if engine == Engine::Mysql && lock_ms.is_some() {
            return Some(Idle {
                kind: IdleKind::Unknown,
                evidence: format!(
                    "It examined {reads_text}, which takes milliseconds, yet ran {run}: most of that time it was waiting, and {} doesn't say for what.",
                    snapshot.server.name()
                ),
                confidence: Confidence::Medium,
                measured: false,
                share: None,
            });
        }
        let source = match engine {
            Engine::Postgres => "pg_stat_statements doesn't record what a statement waited for",
            _ => "the statistics don't record what it waited for",
        };
        return Some(Idle {
            kind: IdleKind::Locks,
            evidence: format!(
                "It read {reads_text}, which takes milliseconds, yet ran {run}: almost all of that time it was waiting, most likely for a row lock another transaction held ({source})."
            ),
            confidence: Confidence::Medium,
            measured: false,
            share: None,
        });
    }
    None
}

/// Whether the workload adds up only the runs the slow query log kept (those over long_query_time), so a share is of
/// the logged slow runs' time, not of all database time.
pub(crate) fn slow_log_only(snapshot: &Snapshot) -> bool {
    snapshot.workload.as_ref().is_some_and(|w| {
        // The collector says so; scans before it did are read from the setting.
        w.min_logged_ms.is_some()
            || (w.kind() == Some(culprit_core::WorkloadKind::SlowLog) && logs_slow_runs_only(snapshot))
    })
}

/// Whether the slow query log keeps only runs over long_query_time: unless it is 0, which logs every run.
pub(crate) fn logs_slow_runs_only(snapshot: &Snapshot) -> bool {
    !snapshot
        .setting("long_query_time")
        .and_then(|s| s.value.trim().parse::<f64>().ok())
        .is_some_and(|seconds| seconds <= 0.0)
}

pub(super) fn heavy_fix_text(
    hot: Option<&PlanNode>,
    per_day_runs: Option<f64>,
    q: &QueryStat,
    kind: StatementKind,
) -> String {
    // The driver or the pool sends these around the application's statements: its settings are the lever.
    if culprit_core::sql::is_session_statement(&q.text) {
        return "The driver or the connection pool sends this around the application's statements, not the application's own code: look at how the pool resets connections on checkout (DISCARD ALL, sp_reset_connection, RESET), whether it wraps every statement in its own transaction, and how often it checks a connection with a ping.".into();
    }
    if let Some(node) = hot {
        if node.kind == NodeKind::Scan && node.predicate.is_some() {
            let table = node.object.as_deref().unwrap_or("the table");
            return match node.kept_share() {
                // The bar every index suggestion uses: past it the scan is the right plan, and the fix is to read less.
                Some(kept) if kept > culprit_core::INDEX_KEEPS_AT_MOST => format!(
                    "The plan reads all of {table} and keeps about {} of the rows, too many for an index to help. Read only the rows and columns the caller needs, or page through them.",
                    format::percent(kept)
                ),
                _ => format!(
                    "The plan scans {table} and filters afterwards. An index on the filtered columns turns that into a seek; open the plan to see the predicate."
                ),
            };
        }
        if node.kind == NodeKind::Lookup {
            return "Most of the work is looking rows up in the table after finding them in an index. Including the selected columns in that index (a covering index) removes the lookups.".into();
        }
        if node.kind == NodeKind::Sort {
            return "Most of the work is sorting. An index whose key order matches the ORDER BY lets the database read rows already sorted.".into();
        }
    }
    if per_day_runs.is_some_and(|r| r >= 100_000.0) && q.mean_time_ms < 5.0 {
        return "Each run is fast; the cost is in how often it runs. Cache the result or batch the calls in the application.".into();
    }
    match kind {
        StatementKind::Write => "Find this statement in the application (copy the brief for your coding agent) and check that it finds the rows it changes through an index, and changes no more rows per run than it needs (split large updates into batches).".into(),
        StatementKind::Aggregate => "Find this statement in the application (copy the brief for your coding agent) and check whether it aggregates more rows than it needs, or whether the result could be kept up to date instead of recomputed on every run.".into(),
        _ => "Find this statement in the application (copy the brief for your coding agent) and check whether it reads more rows or columns than it needs.".into(),
    }
}
