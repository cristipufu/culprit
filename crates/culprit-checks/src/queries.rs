use culprit_core::insights::{converts_column, current_conversions, mismatch_fix};
use culprit_core::{
    Capability, Category, CheckInfo, Confidence, Engine, Finding, Fix, Metric, NodeKind, PlanNode, PlanVariant,
    PlanWarning, QueryRef, QueryStat, Severity, Snapshot, SubjectKind, Unit, format,
};

use crate::support::*;

const HEAVY_SHARE: f64 = 0.05;

pub fn heavy_query_info() -> CheckInfo {
    CheckInfo {
        id: "heavy-query".into(),
        title: "Queries that dominate database time".into(),
        category: Category::Queries,
        engines: vec![Engine::Mssql, Engine::Postgres, Engine::Mysql],
        requires: vec![],
        description: "Single queries that take a large share of database time, with what their plan spends it on."
            .into(),
    }
}

pub fn heavy_query(snapshot: &Snapshot) -> Vec<Finding> {
    let Some(workload) = &snapshot.workload else {
        return Vec::new();
    };
    let mut queries: Vec<&QueryStat> = workload
        .queries
        .iter()
        .filter(|q| !is_monitoring_query(q) && workload.share(q.total_time_ms) >= HEAVY_SHARE)
        .collect();
    queries.sort_by(|a, b| b.total_time_ms.total_cmp(&a.total_time_ms));
    queries.into_iter().take(6).map(|q| heavy_query_finding(snapshot, q)).collect()
}

fn heavy_query_finding(snapshot: &Snapshot, q: &QueryStat) -> Finding {
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
    let hot = q.plan.as_ref().map(|p| (p, p.hottest())).filter(|_| idle.is_none());
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
    let mut summary = format!(
        "It ran {} at {}{}: {share} of all {time} in {}.",
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

/// Text lowercased with names unquoted, whitespace collapsed and no space before an opening parenthesis, so `COUNT ( * )`
/// (MySQL's digest text) and `[dbo].[f] (x)` read like `count(*)` and `dbo.f(x)`.
fn squeeze(text: &str) -> String {
    let lower = text.to_ascii_lowercase().replace(['`', '[', ']', '"'], "");
    let collapsed = lower.split_whitespace().collect::<Vec<_>>().join(" ");
    collapsed.replace(" (", "(").replace("( ", "(").replace(" )", ")").replace(" . ", ".")
}

/// Whether squeezed text calls `function` (written with its opening parenthesis), as a whole name.
fn calls(squeezed: &str, function: &str) -> bool {
    squeezed.match_indices(function).any(|(at, _)| {
        squeezed[..at].chars().next_back().is_none_or(|c| !(c.is_ascii_alphanumeric() || c == '_' || c == '.'))
    })
}

/// "Rows returned per run", or what the engine's row count means for a write. MySQL counts no rows sent for a write.
fn rows_label(engine: Engine, kind: StatementKind) -> Option<&'static str> {
    match (kind, engine) {
        (StatementKind::Write, Engine::Postgres) => Some("Rows changed per run"),
        (StatementKind::Write, Engine::Mssql) => Some("Rows per run"),
        (StatementKind::Write, Engine::Mysql) => None,
        _ => Some("Rows returned per run"),
    }
}

/// Where a statement's time goes when it isn't reading or computing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum IdleKind {
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

struct Idle {
    kind: IdleKind,
    /// One or two sentences: what shows it.
    evidence: String,
    confidence: Confidence,
    /// The engine named the cause (Query Store's waits, MySQL's lock time) rather than Culprit inferring it.
    measured: bool,
}

impl Idle {
    fn title_tail(&self) -> String {
        let most = if self.measured { "most of it" } else { "most likely" };
        match self.kind {
            IdleKind::Sleep => "sleeping".into(),
            IdleKind::AppLock => "waiting for an application lock".into(),
            IdleKind::Locks => format!("{most} waiting for locks"),
            IdleKind::Client => "most of it waiting for the application to read the results".into(),
            IdleKind::Memory => "most of it waiting for memory".into(),
            IdleKind::Latch => "most of it waiting for pages other sessions are using".into(),
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
const SLEEPS: [(&str, &str); 6] = [
    ("pg_sleep(", "pg_sleep"),
    ("pg_sleep_for(", "pg_sleep_for"),
    ("pg_sleep_until(", "pg_sleep_until"),
    ("sleep(", "SLEEP"),
    ("waitfor delay", "WAITFOR"),
    ("waitfor time", "WAITFOR"),
];

/// Application locks a statement waits for until another session releases them.
const APP_LOCKS: [(&str, &str); 5] = [
    ("get_lock(", "GET_LOCK"),
    ("pg_advisory_lock(", "pg_advisory_lock"),
    ("pg_advisory_xact_lock(", "pg_advisory_xact_lock"),
    ("pg_advisory_lock_shared(", "pg_advisory_lock_shared"),
    ("sp_getapplock", "sp_getapplock"),
];

/// Runs this long or longer are worth explaining by what they wait for.
const IDLE_MIN_MS: f64 = 100.0;
/// Generous costs of reading: a page from a slow disk, a row examined from one (MySQL). Reads that can't take a
/// tenth of a run's time even at these rates don't explain it.
const PAGE_READ_MS: f64 = 10.0;
const ROW_READ_MS: f64 = 1.0;
/// CPU under this share of the time: the statement isn't computing for most of it.
const CPU_SMALL: f64 = 0.2;

/// When a statement's time isn't work: it sleeps, waits for a lock, or does too little reading and computing to fill
/// its run time. Uses the engine's own account (Query Store waits, MySQL lock time) where there is one.
fn idle_time(snapshot: &Snapshot, q: &QueryStat) -> Option<Idle> {
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
        });
    }
    None
}

/// Whether the workload adds up only the runs the slow query log kept (those over long_query_time), so a share is of
/// the logged slow runs' time, not of all database time.
pub(crate) fn slow_log_only(snapshot: &Snapshot) -> bool {
    snapshot.workload.as_ref().is_some_and(|w| w.source == "slow query log")
        && !snapshot
            .setting("long_query_time")
            .and_then(|s| s.value.trim().parse::<f64>().ok())
            .is_some_and(|seconds| seconds <= 0.0)
}

/// What a workload's times add up to, in sentences: "database time", or "logged slow-run time" when the slow query log
/// keeps only runs over long_query_time.
pub(crate) fn time_noun(snapshot: &Snapshot) -> &'static str {
    if slow_log_only(snapshot) { "logged slow-run time" } else { "database time" }
}

pub(crate) fn share_metric_label(snapshot: &Snapshot) -> &'static str {
    if slow_log_only(snapshot) { "Share of logged slow-run time" } else { "Share of DB time" }
}

/// [`apply_query_impact`], with the label naming the time the share is of.
pub(crate) fn apply_impact(snapshot: &Snapshot, f: &mut Finding, impact: &QueryImpact) {
    apply_query_impact(f, impact);
    if slow_log_only(snapshot) {
        f.impact.label = format!("{} of logged slow-run time", format::percent(impact.share));
    }
}

fn heavy_fix_text(hot: Option<&PlanNode>, per_day_runs: Option<f64>, q: &QueryStat, kind: StatementKind) -> String {
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

fn describe_node(node: &PlanNode) -> String {
    let what = node.op.to_ascii_lowercase();
    // "an index scan", "a seq scan".
    let a = if what.starts_with(['a', 'e', 'i', 'o', 'u']) { "an" } else { "a" };
    match (&node.object, node.rows_read.or(node.table_rows)) {
        (Some(object), Some(rows)) => format!("{a} {what} of {object} that reads {} rows", format::count(rows)),
        (Some(object), None) => format!("{a} {what} of {object}"),
        _ => format!("{a} {what}"),
    }
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(first) => first.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

pub fn chatty_query_info() -> CheckInfo {
    CheckInfo {
        id: "chatty-query".into(),
        title: "Chatty queries (N+1)".into(),
        category: Category::Queries,
        engines: vec![Engine::Mssql, Engine::Postgres, Engine::Mysql],
        requires: vec![],
        description: "Tiny queries executed so often that the round trips cost more than the work, typically a loop in application code.".into(),
    }
}

/// Round trips a day for a chatty statement to be Medium: at about a millisecond each, the minute a day
/// ([`MEDIUM_MS_A_DAY`]) Medium asks of database time.
const MEDIUM_ROUND_TRIPS_A_DAY: f64 = MEDIUM_MS_A_DAY;

pub fn chatty_query(snapshot: &Snapshot) -> Vec<Finding> {
    let Some(workload) = &snapshot.workload else {
        return Vec::new();
    };
    if workload.total_executions <= 0.0 {
        return Vec::new();
    }
    workload
        .queries
        .iter()
        .filter(|q| !is_monitoring_query(q))
        .filter(|q| {
            let exec_share = q.executions / workload.total_executions;
            let busy = at_least_per_day(snapshot, q.executions, 20_000.0);
            let small = q.rows_handled_per_execution().is_some_and(|r| r <= 2.0) && q.mean_time_ms < 5.0;
            small && exec_share >= 0.08 && busy
        })
        .take(4)
        .map(|q| {
            let exec_share = q.executions / workload.total_executions;
            let per_day_runs = workload.per_day_measured(q.executions);
            let impact = query_impact(snapshot, &[q]).expect("workload present");
            let mut f = finding("chatty-query", &q.id, Category::Queries, subject(SubjectKind::Query, &q.id));
            // Rows changed for a write, rows returned otherwise: MySQL counts a write's rows apart, and returns none.
            let rows = q.rows_handled_per_execution().unwrap_or(1.0);
            let handle = capitalize(&query_handle(q));
            let runs = runs_phrase(snapshot, q.executions);
            let writes = q.writes_rows();
            let share = format!("{} of {}", format::percent(impact.share), time_noun(snapshot));
            let exec = format::percent(exec_share);
            // Three shapes look alike in the counters and need different fixes: a poll that mostly finds nothing, a
            // write repeated row by row (or a heartbeat), and a read inside a loop (N+1).
            let (title, summary, fix, verify) = if rows < 0.5 {
                (
                    format!("{handle} runs {runs} and most runs find nothing"),
                    format!(
                        "This statement is {exec} of every statement the database executes, and on average a run {} {} row{}: the app is polling, asking on a timer whether there is work. Each run is cheap ({share} in all), but every instance of the app asks on its own, so the cost grows with the fleet, not with the work.",
                        if writes { "changes" } else { "returns" },
                        format::count(rows),
                        if rows == 1.0 { "" } else { "s" },
                    ),
                    "Ask less often when the last run found nothing (back off to a few seconds), let one instance poll for the fleet, or have the writer wake the poller (a queue, a notification, a channel). Keep the poll on a small index, for example one filtered to the pending rows.",
                    "After the change, this statement's runs per day should drop by an order of magnitude while the work it finds stays the same.",
                )
            } else if writes {
                (
                    format!("{handle} runs {runs}, one row each time"),
                    format!(
                        "This statement is {exec} of every statement the database executes, and each run changes about {} row{}. Writes that small and that frequent are usually a loop writing row by row, or a heartbeat; each one is a round trip and a log flush ({share} in all).",
                        format::count(rows),
                        if rows <= 1.0 { "" } else { "s" },
                    ),
                    "Write many rows per statement (a multi-row VALUES, a table-valued parameter, or a bulk insert) inside one transaction; for a heartbeat or lease, renew less often.",
                    "After the change, this statement's runs per day should drop by orders of magnitude.",
                )
            } else {
                (
                    format!("{handle} runs {runs}, one row at a time"),
                    format!(
                        "This statement is {exec} of every statement the database executes, and each run returns about {} row{}. That pattern usually comes from a loop in application code (N+1): one query per item instead of one query for all items. The time is small ({share}), but each call costs a network round trip and a connection.",
                        format::count(rows),
                        if rows <= 1.0 { "" } else { "s" },
                    ),
                    "Load the rows in one query (WHERE key IN (…) or a join) before the loop, or cache them. ORMs usually have an eager-loading option for this.",
                    "After the change, this statement's executions per day should drop by orders of magnitude.",
                )
            };
            f.title = title;
            f.summary = summary;
            // A share of the executions is easy to reach on a quiet database; the amount is the round trips: each costs
            // the application about a millisecond in the same region, so Medium asks for the minute a day it asks of
            // database time.
            let medium = exec_share >= 0.25 && at_least_per_day(snapshot, q.executions, MEDIUM_ROUND_TRIPS_A_DAY);
            f.severity = if medium { Severity::Medium } else { Severity::Low };
            f.impact.score = (score_for_share(impact.share) + exec_share * 40.0).min(60.0);
            f.impact.db_time_share = Some(impact.share);
            f.impact.time_ms_per_day = impact.per_day_ms;
            f.impact.label = format!("{} of executions", format::percent(exec_share));
            f.confidence = Confidence::Medium;
            f.queries = query_refs(&[q]);
            if let Some(runs) = per_day_runs {
                f.metrics.push(Metric::new("Runs per day", runs, Unit::Count));
            }
            f.metrics.push(Metric::new("Share of executions", exec_share, Unit::Ratio));
            f.metrics.push(Metric::new("Average duration", q.mean_time_ms, Unit::Ms));
            f.fix = Some(Fix { summary: fix.into(), sql: None, cost: None });
            f.verify = vec![verify.into()];
            f.caveats.push("Some high-frequency single-row lookups are legitimate (for example, by primary key on a hot path). Check the calling code before batching.".into());
            f
        })
        .collect()
}

pub fn plan_warnings_info() -> CheckInfo {
    CheckInfo {
        id: "plan-warnings".into(),
        title: "Plan warnings".into(),
        category: Category::Queries,
        engines: vec![Engine::Mssql, Engine::Postgres, Engine::Mysql],
        requires: vec![Capability::QueryPlans],
        description: "Implicit conversions, spills, missing join predicates and missing statistics in the plans of heavy queries.".into(),
    }
}

pub fn plan_warnings(snapshot: &Snapshot) -> Vec<Finding> {
    let mut out = Vec::new();
    for q in app_queries(snapshot) {
        let Some(plan) = &q.plan else { continue };
        let Some(impact) = query_impact(snapshot, &[q]) else {
            continue;
        };
        if impact.share < 0.005 {
            continue;
        }
        for warning in [
            PlanWarning::ImplicitConversion,
            PlanWarning::Spill,
            PlanWarning::NoJoinPredicate,
            PlanWarning::MissingStatistics,
        ] {
            let nodes: Vec<&PlanNode> = plan.nodes().into_iter().filter(|n| n.warnings.contains(&warning)).collect();
            if nodes.is_empty() && !plan.warnings.contains(&warning) {
                continue;
            }
            if let Some(f) = plan_warning_finding(snapshot, q, &impact, warning, nodes.first().copied()) {
                out.push(f);
            }
        }
    }
    merge_same_cause(snapshot, out)
}

/// One root cause (a type mismatch on one column) is one finding, however many queries hit it.
fn merge_same_cause(snapshot: &Snapshot, findings: Vec<Finding>) -> Vec<Finding> {
    let mut merged: Vec<Finding> = Vec::new();
    for f in findings {
        match merged.iter_mut().find(|m| m.id == f.id) {
            Some(m) => {
                for r in f.queries {
                    if !m.queries.iter().any(|x| x.id == r.id) {
                        m.queries.push(r);
                    }
                }
            }
            None => merged.push(f),
        }
    }
    for m in merged.iter_mut().filter(|m| m.queries.len() > 1) {
        let queries: Vec<&QueryStat> =
            m.queries.iter().filter_map(|r| snapshot.workload.as_ref()?.query(&r.id)).collect();
        let Some(impact) = query_impact(snapshot, &queries) else { continue };
        apply_query_impact(m, &impact);
        m.severity = m.severity.max(severity_for_workload_share(snapshot, impact.share));
        let column = m.subject.name.clone();
        m.title = format!("A type mismatch on {column} stops {} queries from using an index", queries.len());
        m.summary.push_str(&format!(
            " The same mismatch appears in {} queries that together take {} of database time.",
            queries.len(),
            format::percent(impact.share)
        ));
        m.metrics.retain(|x| x.label != "Share of DB time" && x.label != "Average duration");
        m.metrics.insert(0, Metric::new("Share of DB time", impact.share, Unit::Ratio));
        m.metrics.insert(1, Metric::new("Queries affected", queries.len() as f64, Unit::Count));
        if let Some(per_day_ms) = impact.per_day_ms {
            m.metrics.insert(2, Metric::new("DB time per day", per_day_ms, Unit::Ms));
        }
    }
    merged
}

fn plan_warning_finding(
    snapshot: &Snapshot,
    q: &QueryStat,
    impact: &QueryImpact,
    warning: PlanWarning,
    node: Option<&PlanNode>,
) -> Option<Finding> {
    let key = format!("{}:{}", q.id, warning_key(warning));
    let mut f = finding("plan-warnings", &key, Category::Queries, subject(SubjectKind::Query, &q.id));
    apply_impact(snapshot, &mut f, impact);
    f.queries = vec![QueryRef { id: q.id.clone(), plan_node: node.map(|n| n.id), note: Some(warning.label().into()) }];
    f.metrics.push(Metric::new(share_metric_label(snapshot), impact.share, Unit::Ratio));
    f.metrics.push(Metric::new("Average duration", q.mean_time_ms, Unit::Ms));
    let handle = query_handle(q);
    match warning {
        PlanWarning::ImplicitConversion => {
            let node_props = node.into_iter().flat_map(|n| n.props.values());
            let plan_props = q.plan.iter().flat_map(|p| p.props.values());
            let named: Vec<&str> =
                node_props.chain(plan_props).flat_map(|v| v.lines()).filter(|l| l.contains("CONVERT")).collect();
            // Only a column converted to match a value of another type is a mismatch (the rule the plan parser applies;
            // scans saved before it flagged every conversion SQL Server warned about).
            // A plan captured before a column changed type still shows its old conversion (`current_conversions`).
            let still =
                |l: &str| q.plan.as_ref().map_or_else(|| converts_column(l), |p| !current_conversions(p, l).is_empty());
            let lines: Vec<&str> = named.iter().copied().filter(|l| still(l)).collect();
            if !named.is_empty() && lines.is_empty() {
                return None;
            }
            let issue = lines.iter().find(|l| l.contains("Seek Plan")).or(lines.first()).copied();
            let blocks_seek = issue.is_some_and(|l| l.contains("Seek Plan"));
            let converted = issue.and_then(|l| current_conversions(q.plan.as_ref()?, l).into_iter().next());
            let column = converted.as_ref().map(|(c, _)| c.clone());
            let target = converted.map(|(_, t)| t);
            let unicode = target.as_deref().is_some_and(|t| t.to_ascii_lowercase().starts_with("nvarchar"));
            let convert = issue.map(|l| l.split_once(": ").map(|(_, e)| e).unwrap_or(l).to_string());
            f.title = match &column {
                Some(c) => format!("A type mismatch on {c} stops {handle} from using an index"),
                None => format!("A type mismatch slows down {handle}"),
            };
            f.summary = format!(
                "The database has to convert {} on every row before comparing it with a parameter of a different type{}. A converted column can't be searched with an index, so it reads every row instead.{}",
                column.as_deref().unwrap_or("a column"),
                convert.map(|c| format!(" (`{}`)", truncate(&c, 140))).unwrap_or_default(),
                match &q.parameters {
                    Some(p) if p.to_ascii_lowercase().contains("nvarchar") => format!(" The application declares its parameters as `{}`: Unicode strings compared with a non-Unicode column.", truncate(p, 120)),
                    Some(p) => format!(" The application declares its parameters as `{}`.", truncate(p, 120)),
                    None if unicode => " This often happens when an application sends strings as nvarchar (Unicode) to a varchar column.".to_string(),
                    None => String::new(),
                }
            );
            f.severity = if blocks_seek { severity_for_workload_share(snapshot, impact.share) } else { Severity::Low };
            f.confidence = if blocks_seek { Confidence::High } else { Confidence::Medium };
            f.fix = Some(Fix { summary: mismatch_fix(column.as_deref(), target.as_deref()), sql: None, cost: None });
            if let Some(c) = &column {
                f.id = format!("plan-warnings:implicit-conversion:{c}");
                f.subject = subject(SubjectKind::Table, c);
                f.metrics.push(Metric::text("Column", c.clone()));
            }
        }
        PlanWarning::Spill => {
            f.title = format!("{} runs out of memory and spills to disk", capitalize(&handle));
            f.summary = "A sort or hash in this plan needed more memory than it was granted and wrote the overflow to disk (tempdb or temp files). Spills usually come from a row estimate that was far too low, or a sort the index could have avoided.".into();
            f.severity = severity_for_workload_share(snapshot, impact.share);
            f.confidence = Confidence::High;
            f.fix = Some(Fix {
                summary: match snapshot.server.engine {
                    Engine::Mssql => "Update statistics on the tables involved; if the estimate is still far off, add an index that delivers rows in the needed order so the sort disappears.".into(),
                    Engine::Postgres => "Raise work_mem for this workload (per role or per session), or add an index that delivers rows in the needed order so the sort disappears.".into(),
                    Engine::Sqlite => "Give the sort an index that returns rows in the order asked for; otherwise PRAGMA temp_store = MEMORY keeps temporary b-trees in memory, and a larger cache_size helps them fit.".into(),
                    Engine::Mysql => "Raise tmp_table_size and max_heap_table_size for this workload so the temporary table stays in memory, or add an index that returns rows already ordered or grouped.".into(),
                },
                sql: None,
                cost: None,
            });
        }
        PlanWarning::NoJoinPredicate => {
            let engine = snapshot.server.name();
            match join_condition(&q.text) {
                JoinCondition::Missing => {
                    f.title = format!("{} joins tables without a join condition", capitalize(&handle));
                    f.summary = "Part of this plan combines every row of one input with every row of another (a cross join), and the statement has no condition joining them: no ON clause and no comparison of one table's column with another's. A missing join condition multiplies the rows the query reads and returns.".into();
                    f.severity = severity_for_workload_share(snapshot, impact.share);
                    f.confidence = Confidence::Medium;
                    f.fix = Some(Fix {
                        summary: "Check the query's joins and add the missing condition.".into(),
                        sql: None,
                        cost: None,
                    });
                }
                JoinCondition::Present => {
                    f.title = format!("{}'s plan joins two inputs without a join condition", capitalize(&handle));
                    f.summary = format!(
                        "Part of this plan combines every row of one input with every row of another, though the statement does join them. {engine} does this when both join columns are compared with the same value (a.x = @v AND a.x = b.x): it filters each side by the value and drops the join condition as redundant. That is harmless while both sides keep few rows."
                    );
                    f.severity = Severity::Low;
                    f.confidence = Confidence::Medium;
                    f.fix = Some(Fix {
                        summary: "Nothing to change while both inputs keep few rows. If the plan multiplies many rows, check the join columns and the filters on them in the plan.".into(),
                        sql: None,
                        cost: None,
                    });
                }
                JoinCondition::CrossJoin => {
                    f.title = format!("{} asks for a cross join", capitalize(&handle));
                    f.summary = "Part of this plan combines every row of one input with every row of another, and the statement asks for it (CROSS JOIN or CROSS APPLY). That multiplies rows: fine for a small list of values, costly for two large tables.".into();
                    f.severity = Severity::Low;
                    f.confidence = Confidence::Medium;
                    f.fix = Some(Fix {
                        summary: "Make sure the cross join is intended and both inputs stay small.".into(),
                        sql: None,
                        cost: None,
                    });
                }
            }
        }
        PlanWarning::MissingStatistics => {
            f.title = format!("{} is planned without statistics on some columns", capitalize(&handle));
            f.summary = "The optimizer had to guess how many rows match because some columns have no statistics. Guesses lead to the wrong join type or memory grant.".into();
            f.severity = Severity::Low;
            f.confidence = Confidence::Medium;
            f.fix = Some(Fix {
                summary: "Make sure automatic statistics creation is on, or create statistics on the columns named in the plan warning.".into(),
                sql: None,
                cost: None,
            });
        }
        _ => return None,
    }
    f.verify = vec!["Re-scan after the change; the warning should be gone from this query's plan.".into()];
    Some(f)
}

fn warning_key(w: PlanWarning) -> &'static str {
    match w {
        PlanWarning::ImplicitConversion => "implicit-conversion",
        PlanWarning::Spill => "spill",
        PlanWarning::NoJoinPredicate => "no-join-predicate",
        PlanWarning::MissingStatistics => "missing-statistics",
        PlanWarning::MemoryGrant => "memory-grant",
        PlanWarning::Misestimate => "misestimate",
        PlanWarning::ResidualPredicate => "residual-predicate",
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    s.chars().take(max).collect::<String>() + "…"
}

/// What a statement's text says about the join a plan made without a join predicate.
#[derive(Debug, PartialEq, Eq)]
enum JoinCondition {
    /// No ON clause, and no comparison of one table's column with another's.
    Missing,
    /// It joins its tables on a condition: the engine dropped it (both sides compared with the same value).
    Present,
    /// It asks for every combination.
    CrossJoin,
}

fn join_condition(text: &str) -> JoinCondition {
    let t = format!(" {} ", squeeze(text));
    if t.contains(" cross join ") || t.contains(" cross apply ") {
        return JoinCondition::CrossJoin;
    }
    let joins = t.matches(" join ").count();
    let ons = t.matches(" on ").count() + t.matches(" using(").count() + t.matches(" using ").count();
    if (joins > 0 && ons >= joins) || compares_two_tables(&t) {
        return JoinCondition::Present;
    }
    JoinCondition::Missing
}

/// A comparison of one table's column with another's (`o.customer_id = c.id`): an old-style join in WHERE.
fn compares_two_tables(squeezed: &str) -> bool {
    let name = |c: char| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '$' | '@');
    let qualifier = |side: &str| -> Option<String> {
        let (q, column) = side.rsplit_once('.')?;
        let q = q.rsplit('.').next()?;
        let starts = |s: &str| s.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_');
        (starts(q) && starts(column)).then(|| q.to_string())
    };
    squeezed.match_indices('=').any(|(at, _)| {
        let before = squeezed[..at].trim_end_matches(['<', '>', '!', ' ']);
        if squeezed[..at].ends_with(['<', '>', '!']) {
            return false;
        }
        let left: String =
            before.chars().rev().take_while(|c| name(*c)).collect::<Vec<_>>().into_iter().rev().collect();
        let right: String = squeezed[at + 1..].trim_start().chars().take_while(|c| name(*c)).collect();
        matches!((qualifier(&left), qualifier(&right)), (Some(a), Some(b)) if a != b)
    })
}

pub fn plan_regression_info() -> CheckInfo {
    CheckInfo {
        id: "plan-regression".into(),
        title: "Plan regressions".into(),
        category: Category::Queries,
        engines: vec![Engine::Mssql],
        requires: vec![Capability::QueryStore],
        description: "Queries whose current plan is much slower than a plan they used before.".into(),
    }
}

/// A plan with this many runs or more counts as one the query used.
const PLAN_MIN_RUNS: f64 = 5.0;

/// When a plan ran, from its first to its last run (epoch seconds).
fn plan_span(p: &PlanVariant) -> Option<(f64, f64)> {
    Some((parse_epoch_seconds(p.first_seen.as_deref()?)?, parse_epoch_seconds(p.last_seen.as_deref()?)?))
}

/// The plan a query runs with now, much slower than one it ran with before and stopped using. Both claims need times:
/// the fast plan's last run comes before the slow plan's first, and no other plan ran alongside the slow one (plans
/// used side by side for different values are parameter sensitivity, not a plan that changed).
fn replaced_plan(q: &QueryStat) -> Option<(&PlanVariant, &PlanVariant)> {
    let usable: Vec<&PlanVariant> = q.plans.iter().filter(|p| p.executions >= PLAN_MIN_RUNS).collect();
    if usable.len() < 2 {
        return None;
    }
    let spans: Vec<(f64, f64)> = usable.iter().map(|p| plan_span(p)).collect::<Option<_>>()?;
    let (latest_at, (latest_first, latest_last)) =
        spans.iter().copied().enumerate().max_by(|a, b| a.1.1.total_cmp(&b.1.1))?;
    let latest = usable[latest_at];
    let overlaps = spans
        .iter()
        .enumerate()
        .any(|(i, (first, last))| i != latest_at && *first <= latest_last && latest_first <= *last);
    if overlaps || latest.is_forced {
        return None;
    }
    let best = usable
        .iter()
        .zip(&spans)
        .filter(|(_, (_, last))| *last < latest_first)
        .map(|(p, _)| *p)
        .min_by(|a, b| a.mean_time_ms.total_cmp(&b.mean_time_ms))?;
    (!best.is_forced
        && latest.mean_time_ms >= best.mean_time_ms * 2.0
        && latest.mean_time_ms - best.mean_time_ms >= 5.0)
        .then_some((best, latest))
}

pub fn plan_regression(snapshot: &Snapshot) -> Vec<Finding> {
    let Some(workload) = snapshot.workload.as_ref() else { return Vec::new() };
    app_queries(snapshot)
        .filter_map(|q| {
            let (best, latest) = replaced_plan(q)?;
            let impact = query_impact(snapshot, &[q])?;
            let factor = latest.mean_time_ms / best.mean_time_ms.max(0.001);
            let wasted = (latest.mean_time_ms - best.mean_time_ms) * latest.executions;
            // The same bar as `regression`: the slow plan's extra time, or its own time, is a real part of the workload.
            let (wasted_share, latest_share) = (workload.share(wasted), workload.share(latest.total_time_ms));
            if wasted_share < 0.01 && latest_share < 0.03 {
                return None;
            }
            let mut f = finding("plan-regression", &q.id, Category::Queries, subject(SubjectKind::Query, &q.id));
            apply_impact(snapshot, &mut f, &impact);
            let wasted_per_day = workload.per_day_measured(wasted);
            f.impact.time_ms_per_day = wasted_per_day;
            f.impact.score = score_for_share(latest_share.max(wasted_share));
            f.impact.label = format!("{} slower", format::factor(factor));
            // Its time a day since it took over, so a regression that began this morning isn't diluted by the week.
            let since = latest.first_seen.as_deref().and_then(parse_epoch_seconds);
            let end = workload.window_end.as_deref().or(Some(snapshot.taken_at.as_str())).and_then(parse_epoch_seconds);
            let amount = match (since, end) {
                (Some(since), Some(end)) => severity_for_recent_time((end - since) / 3600.0, latest.total_time_ms, 1.0),
                _ => severity_for_time(snapshot, latest.total_time_ms, 1.0),
            };
            f.severity = severity_for_share(latest_share).min(amount);
            if factor >= 3.0 && wasted_share >= 0.05 {
                f.severity = f.severity.max(Severity::Medium.min(amount));
            }
            f.confidence = Confidence::High;
            f.title = format!("{} got {} slower after its plan changed", capitalize(&query_handle(q)), format::factor(factor));
            let when = |at: Option<&str>| at.map(crate::trends::short_when).unwrap_or_default();
            f.summary = format!(
                "Query Store saw this query run with plan {} at {} on average until {}, and with plan {} at {} since {} ({} runs). The plan choice changed, typically after a statistics update, a schema change or a recompile with unusual parameter values.",
                best.id,
                format::duration(best.mean_time_ms),
                when(best.last_seen.as_deref()),
                latest.id,
                format::duration(latest.mean_time_ms),
                when(latest.first_seen.as_deref()),
                format::count(latest.executions)
            );
            f.queries = query_refs(&[q]);
            f.metrics.push(Metric::new("Fast plan", best.mean_time_ms, Unit::Ms));
            f.metrics.push(Metric::new("Current plan", latest.mean_time_ms, Unit::Ms));
            if let Some(extra) = wasted_per_day {
                f.metrics.push(Metric::new("Extra time per day", extra, Unit::Ms));
            }
            f.fix = Some(Fix {
                summary: "Force the fast plan now to stop the bleeding, then find out why the optimizer changed its mind (statistics, parameter values, schema). Unforce once the cause is fixed.".into(),
                sql: Some(format!("EXEC sp_query_store_force_plan @query_id = {}, @plan_id = {};", q.id, best.id)),
                cost: None,
            });
            f.verify = vec!["The query's average duration in Query Store should return to the fast plan's level within an interval or two.".into()];
            f.caveats.push("A forced plan can fail to apply after schema changes; Query Store reports that in sys.query_store_plan.last_force_failure_reason_desc.".into());
            Some(f)
        })
        .collect()
}

pub fn unstable_query_info() -> CheckInfo {
    CheckInfo {
        id: "unstable-query".into(),
        title: "Unstable queries".into(),
        category: Category::Queries,
        engines: vec![Engine::Mssql, Engine::Postgres, Engine::Mysql],
        requires: vec![],
        description: "Queries whose duration swings wildly between runs with the same plan, a sign of parameter sensitivity or blocking.".into(),
    }
}

/// How far a statement's run times spread: the standard deviation over the average, and the slowest run over it.
const UNSTABLE_CV: f64 = 3.0;
const UNSTABLE_MAX_FACTOR: f64 = 50.0;

/// The slow runs behind a wide spread, when it takes more than one slow run to explain it. One outlier among many fast
/// runs (a lock wait during a migration, a cold cache after a restart) inflates the standard deviation on its own: the
/// spread has to stay wide without the slowest run's share of it. Returns the least time the slow runs spent beyond
/// the average (ms): every slow run is at most the slowest one, so the variance they explain needs at least that much.
fn unstable_excess_ms(q: &QueryStat) -> Option<f64> {
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
    (excess > 0.0).then_some(excess)
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
            let slow_share = workload.share(excess);
            if impact.share < 0.01 || slow_share < 0.005 {
                return None;
            }
            let mut f = finding("unstable-query", &q.id, Category::Queries, subject(SubjectKind::Query, &q.id));
            apply_impact(snapshot, &mut f, &impact);
            // What the slow runs cost, not the whole statement's time.
            f.severity = severity_for_time(snapshot, excess, slow_share);
            f.impact.score = score_for_share(slow_share);
            f.confidence = Confidence::Medium;
            f.title = format!(
                "{} averages {} per run, but some runs take far longer (up to {})",
                capitalize(&query_handle(q)),
                format::duration(q.mean_time_ms),
                format::duration(max)
            );
            // One plan in Query Store backs "same plan"; elsewhere the engine keeps no plan identity per run.
            let same = if q.plans.len() == 1 { "Same query, same plan" } else { "Same statement" };
            f.summary = format!(
                "{same}, very different run times: the slow runs took at least {} more than the average, together. Either some parameter values match far more rows than the plan was built for (parameter sensitivity), or some runs wait on locks. Check the waits for this query, then the parameter values of the slow runs.",
                format::duration(excess)
            );
            f.queries = query_refs(&[q]);
            f.metrics.push(Metric::new("Average", q.mean_time_ms, Unit::Ms));
            f.metrics.push(Metric::new("Slowest", max, Unit::Ms));
            f.metrics.push(Metric::new("Std deviation", stddev, Unit::Ms));
            f.metrics.push(Metric::new("Slow runs' extra time (at least)", excess, Unit::Ms));
            // The same ladders as `parameter-sensitive`: the cause is usually the same, only the evidence is coarser.
            f.fix = Some(match snapshot.server.engine {
                Engine::Mssql => crate::sensitivity::mssql_fix(snapshot, q, None),
                Engine::Postgres => crate::sensitivity::postgres_fix(),
                Engine::Mysql => crate::sensitivity::mysql_fix(0.0, snapshot.server.name() == "MariaDB"),
                Engine::Sqlite => Fix {
                    summary: "Compare a slow and a fast value with EXPLAIN QUERY PLAN. If some values match far more rows, an index that narrows them, or ANALYZE, lets SQLite choose better.".into(),
                    sql: Some("ANALYZE;".into()),
                    cost: None,
                },
            });
            if snapshot.server.engine == Engine::Postgres {
                let since = q
                    .first_seen
                    .as_deref()
                    .map(|at| format!("since {}", crate::trends::short_when(at)))
                    .unwrap_or_else(|| workload.window_phrase().replacen("the ", "in the ", 1));
                f.caveats.push(format!(
                    "PostgreSQL keeps one minimum, average, maximum and standard deviation per statement {since}, so the slowest runs could be from any time in that span (a lock wait during a migration counts too), and Culprit can't tell which values were slow or how many calls were."
                ));
            }
            Some(f)
        })
        .collect()
}

pub fn temp_spill_info() -> CheckInfo {
    CheckInfo {
        id: "temp-spill".into(),
        title: "Queries writing temp files".into(),
        category: Category::Queries,
        engines: vec![Engine::Postgres, Engine::Mysql],
        requires: vec![],
        description: "Statements whose sorts, hashes or temporary tables outgrow memory and spill to disk.".into(),
    }
}

pub fn temp_spill(snapshot: &Snapshot) -> Vec<Finding> {
    match snapshot.server.engine {
        Engine::Postgres => {}
        Engine::Mysql => return mysql_disk_temp_tables(snapshot),
        Engine::Mssql | Engine::Sqlite => return Vec::new(),
    }
    let Some(workload) = snapshot.workload.as_ref() else { return Vec::new() };
    let work_mem_text = snapshot.setting("work_mem").map(|s| s.value.clone());
    let work_mem = work_mem_text.as_deref().and_then(pg_size_bytes).unwrap_or(4.0 * MB);
    let block = snapshot.setting("block_size").and_then(|s| s.value.trim().parse::<f64>().ok()).unwrap_or(8192.0);
    app_queries(snapshot)
        .filter(|q| q.temp_writes.is_some_and(|t| t > 0.0))
        .filter_map(|q| {
            let impact = query_impact(snapshot, &[q])?;
            if impact.share < 0.01 {
                return None;
            }
            let written = q.temp_writes? * block;
            let per_run = written / q.executions.max(1.0);
            // pg_stat_statements counts the blocks written, not the time: what writing them and reading them back can
            // take at a slow disk's pace, at most the run itself. A spill too small to matter to the run isn't why it's
            // slow (and mustn't stand in for the heavy-query finding that would say what is).
            let spill_ms = (per_run * 2.0 / SPILL_BYTES_PER_MS).min(q.mean_time_ms);
            if spill_ms < q.mean_time_ms * SPILL_MIN_SHARE_OF_RUN {
                return None;
            }
            let spill_share = workload.share(spill_ms * q.executions);
            let mut f = finding("temp-spill", &q.id, Category::Queries, subject(SubjectKind::Query, &q.id));
            apply_impact(snapshot, &mut f, &impact);
            f.severity = severity_for_time(snapshot, spill_ms * q.executions, spill_share);
            f.impact.score = score_for_share(spill_share);
            f.confidence = Confidence::High;
            f.title = format!(
                "{} writes about {} of temp files per run",
                capitalize(&query_handle(q)),
                format::bytes(per_run)
            );
            f.summary = format!(
                "A sort or hash in this statement needs more memory than work_mem ({}) and writes the overflow to temporary files: {} over {} runs. pg_stat_statements counts the total, so whether every run spills or only some is unknown. Writing temp files and reading them back is many times slower than sorting in memory.",
                work_mem_text.as_deref().unwrap_or("4MB, the default"),
                format::bytes(written),
                format::count(q.executions)
            );
            f.queries = query_refs(&[q]);
            f.metrics.push(Metric::new("Temp written per run", per_run, Unit::Bytes));
            f.metrics.push(Metric::new("Average duration", q.mean_time_ms, Unit::Ms));
            f.fix = Some(work_mem_fix(work_mem, work_mem_text.as_deref(), per_run));
            Some(f)
        })
        .collect()
}

const MB: f64 = 1024.0 * 1024.0;
/// A slow disk's pace for temp files (100 MB/s), in bytes per millisecond.
const SPILL_BYTES_PER_MS: f64 = 100.0 * MB / 1000.0;
/// A spill that can take less than this share of a run isn't what makes the statement slow.
const SPILL_MIN_SHARE_OF_RUN: f64 = 0.1;
/// The most sort memory Culprit suggests for one role: past it, the fix is to sort less.
const WORK_MEM_MAX: f64 = 1024.0 * MB;

/// More work_mem than the spill needed, for the application's role or one transaction: never less than it has now.
fn work_mem_fix(current: f64, current_text: Option<&str>, per_run: f64) -> Fix {
    let index = "add an index that returns rows already in the order the sort needs, or sort fewer rows";
    let now = current_text.map(str::to_string).unwrap_or_else(|| format::bytes(current));
    let wanted = (current + per_run * 2.0).max(16.0 * MB);
    let proposed = 2f64.powf(wanted.log2().ceil());
    if proposed > WORK_MEM_MAX || proposed <= current {
        return Fix {
            summary: format!(
                "work_mem is already {now}; more sort memory per operation is no longer a reasonable fix, so {index}."
            ),
            sql: None,
            cost: None,
        };
    }
    let value = if proposed >= 1024.0 * MB {
        format!("{:.0}GB", proposed / (1024.0 * MB))
    } else {
        format!("{:.0}MB", proposed / MB)
    };
    Fix {
        summary: format!(
            "Give this statement more sort memory ({value} instead of {now}), for the role the application connects as or only around this statement, rather than server-wide; or {index}."
        ),
        sql: Some(format!(
            "-- For the role the application connects as (takes effect on its next connections):\nALTER ROLE <app_role> SET work_mem = '{value}';\n\n-- Or only for this statement, inside its transaction:\nSET LOCAL work_mem = '{value}';"
        )),
        cost: Some("work_mem is per sort or hash, per connection: size it with your connection count in mind.".into()),
    }
}

/// A PostgreSQL size setting ("4MB", "64kB", "1GB", or a bare number of kB) in bytes.
fn pg_size_bytes(text: &str) -> Option<f64> {
    let t = text.trim();
    let split = t.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(t.len());
    let number: f64 = t[..split].parse().ok()?;
    let unit = match t[split..].trim().to_ascii_lowercase().as_str() {
        "" | "kb" => 1024.0,
        "b" => 1.0,
        "mb" => MB,
        "gb" => 1024.0 * MB,
        "tb" => 1024.0 * 1024.0 * MB,
        _ => return None,
    };
    Some(number * unit)
}

/// MySQL counts on-disk temporary tables per digest: a GROUP BY, DISTINCT or ORDER BY whose temporary table outgrew
/// memory, or held a value the in-memory engine can't (TEXT and BLOB, before MySQL 8.0's TempTable engine).
fn mysql_disk_temp_tables(snapshot: &Snapshot) -> Vec<Finding> {
    let Some(workload) = snapshot.workload.as_ref() else { return Vec::new() };
    let limits = TempLimits::of(snapshot);
    app_queries(snapshot)
        .filter(|q| q.temp_writes.is_some_and(|t| t > 0.0))
        .filter_map(|q| {
            let impact = query_impact(snapshot, &[q])?;
            if impact.share < 0.01 {
                return None;
            }
            let tables = q.temp_writes?;
            let per_run = tables / q.executions.max(1.0);
            // MySQL counts tables, not runs: a run can build more than one, so the runs that spilled are at most this.
            let runs_share = per_run.min(1.0);
            let spilled_share = workload.share(q.total_time_ms * runs_share);
            let mut f = finding("temp-spill", &q.id, Category::Queries, subject(SubjectKind::Query, &q.id));
            apply_impact(snapshot, &mut f, &impact);
            // The time of the runs that can have spilled, not the whole statement's.
            f.severity = severity_for_time(snapshot, q.total_time_ms * runs_share, spilled_share);
            f.impact.score = score_for_share(spilled_share);
            f.confidence = Confidence::High;
            let handle = capitalize(&query_handle(q));
            f.title = if per_run >= 0.95 && per_run.round() <= 1.0 {
                format!("{handle} builds an on-disk temporary table about once per run")
            } else if per_run >= 0.95 {
                format!("{handle} builds about {} on-disk temporary tables per run", format::count(per_run.round()))
            } else {
                format!("{handle} builds on-disk temporary tables in up to {} of its runs", format::percent(runs_share))
            };
            let large = if limits.blob_bound { large_value_columns(snapshot, q) } else { Vec::new() };
            let name = snapshot.server.name();
            f.summary = if large.is_empty() {
                format!(
                    "The temporary table behind a GROUP BY, DISTINCT or ORDER BY in this statement outgrows {} and moves to disk, which is many times slower: {} on-disk tables in {} runs.",
                    limits.describe(),
                    format::count(tables),
                    format::count(q.executions)
                )
            } else {
                format!(
                    "The temporary table behind a GROUP BY, DISTINCT or ORDER BY in this statement holds {}, and {name} keeps a temporary table with TEXT or BLOB values on disk whatever its size: {} on-disk tables in {} runs. Raising the memory limit won't change that.",
                    format::list(&large),
                    format::count(tables),
                    format::count(q.executions)
                )
            };
            f.queries = query_refs(&[q]);
            f.metrics.push(Metric::new("On-disk temporary tables per run", per_run, Unit::Count));
            f.metrics.push(Metric::new("Average duration", q.mean_time_ms, Unit::Ms));
            f.fix = Some(if large.is_empty() {
                limits.fix(snapshot)
            } else {
                Fix {
                    summary: format!(
                        "Leave {} out of the grouped, distinct or sorted rows (group by the key, then fetch the large columns for the rows you keep), or add an index that delivers rows already grouped or ordered so no temporary table is needed.",
                        format::list(&large)
                    ),
                    sql: None,
                    cost: None,
                }
            });
            Some(f)
        })
        .collect()
}

/// What keeps a MySQL or MariaDB internal temporary table in memory, as this server is set up.
struct TempLimits {
    /// The variables that cap one table in memory, with their values (bytes) where the scan read them.
    caps: Vec<(&'static str, Option<f64>)>,
    /// MySQL 8.0's TempTable engine also stops at temptable_max_ram, for all its tables together.
    shared_ram: Option<f64>,
    /// A TEXT or BLOB value sends the table to disk whatever its size (the MEMORY engine: MariaDB, MySQL 5.7).
    blob_bound: bool,
}

impl TempLimits {
    fn of(snapshot: &Snapshot) -> TempLimits {
        let value = |name: &str| snapshot.setting(name).and_then(|s| s.value.trim().parse::<f64>().ok());
        let version = &snapshot.server.version;
        let mysql8 = snapshot.server.name() == "MySQL" && culprit_core::version_at_least(version, [8, 0, 0]);
        let temptable = mysql8
            && snapshot
                .setting("internal_tmp_mem_storage_engine")
                .is_none_or(|s| s.value.trim().eq_ignore_ascii_case("TempTable"));
        if temptable {
            // TempTable ignores max_heap_table_size; tmp_table_size caps each table from 8.0.28.
            let caps = if culprit_core::version_at_least(version, [8, 0, 28]) {
                vec![("tmp_table_size", value("tmp_table_size"))]
            } else {
                Vec::new()
            };
            return TempLimits {
                caps,
                shared_ram: Some(value("temptable_max_ram").unwrap_or(1024.0 * MB)),
                blob_bound: false,
            };
        }
        TempLimits {
            caps: vec![
                ("tmp_table_size", value("tmp_table_size")),
                ("max_heap_table_size", value("max_heap_table_size")),
            ],
            shared_ram: None,
            // MySQL 8.0's MEMORY engine, MySQL 5.7 and MariaDB can't hold TEXT or BLOB values in memory.
            blob_bound: true,
        }
    }

    /// The binding in-memory limit, in words: "the in-memory limit (1 MB, the smaller of tmp_table_size and max_heap_table_size)".
    fn describe(&self) -> String {
        let known: Vec<f64> = self.caps.iter().filter_map(|(_, v)| *v).collect();
        let smallest = known.iter().copied().reduce(f64::min);
        match (self.caps.as_slice(), smallest, self.shared_ram) {
            ([(name, Some(v))], _, Some(ram)) => format!(
                "the in-memory limit ({name}, {}; and temptable_max_ram, {}, for all such tables together)",
                format::bytes(*v),
                format::bytes(ram)
            ),
            ([], _, Some(ram)) => {
                format!("temptable_max_ram ({}, for all temporary tables together)", format::bytes(ram))
            }
            (caps, Some(v), _) if caps.len() > 1 => format!(
                "the in-memory limit ({}, the smaller of {})",
                format::bytes(v),
                format::list(&caps.iter().map(|(n, _)| n.to_string()).collect::<Vec<_>>())
            ),
            _ => "the in-memory limit".to_string(),
        }
    }

    /// Raise every cap below the proposal to it, through the server's own way of changing settings; never lower one.
    fn fix(&self, snapshot: &Snapshot) -> Fix {
        let index = "Add an index that delivers rows already grouped or ordered so no temporary table is needed";
        let (names, current): (Vec<&str>, Option<f64>) = if self.caps.is_empty() {
            (vec!["temptable_max_ram"], self.shared_ram)
        } else {
            (self.caps.iter().map(|(n, _)| *n).collect(), self.caps.iter().filter_map(|(_, v)| *v).reduce(f64::min))
        };
        let floor = if self.caps.is_empty() { 2048.0 * MB } else { 64.0 * MB };
        let proposed = current.map_or(floor, |c| (c * 2.0).max(floor));
        let now = |name: &str| {
            if self.caps.is_empty() {
                self.shared_ram
            } else {
                self.caps.iter().find(|(n, _)| *n == name).and_then(|(_, v)| *v)
            }
        };
        // A value the scan didn't read could be higher than the proposal: no SQL that might lower it.
        if names.iter().any(|n| now(n).is_none()) {
            return Fix {
                summary: format!(
                    "{index}, or raise {} (the scan didn't read {}).",
                    format::list(&names.iter().map(|n| n.to_string()).collect::<Vec<_>>()),
                    if names.len() == 1 { "its current value" } else { "their current values" }
                ),
                sql: None,
                cost: None,
            };
        }
        let changes: Vec<SettingChange> = names
            .iter()
            .filter(|name| now(name).is_some_and(|v| v < proposed))
            .map(|name| setting_change(&snapshot.server, name, &format!("{proposed:.0}")))
            .collect();
        if changes.is_empty() {
            return Fix { summary: format!("{index}."), sql: None, cost: None };
        }
        let sql: Vec<String> = changes.iter().filter_map(|c| c.sql.clone()).collect();
        let how: Vec<String> = changes.iter().map(|c| c.how.clone()).collect();
        Fix {
            summary: format!(
                "{index}, or raise the in-memory limit to {}{}. {}",
                format::bytes(proposed),
                if self.caps.len() > 1 { " (both variables: the smaller one wins)" } else { "" },
                how.join(" ")
            ),
            sql: (!sql.is_empty()).then(|| sql.join("\n")),
            cost: Some(if self.caps.is_empty() {
                "temptable_max_ram is memory the server may use for all temporary tables together.".into()
            } else {
                "The limit is per temporary table per connection; size it with your connection count in mind.".into()
            }),
        }
    }
}

/// TEXT and BLOB columns of the statement's tables that its text names (or all of them, for `SELECT *`).
fn large_value_columns(snapshot: &Snapshot, q: &QueryStat) -> Vec<String> {
    let text = squeeze(&q.text);
    let star = text.contains("select *") || text.contains(".*");
    let mut out: Vec<String> = Vec::new();
    for c in &snapshot.columns {
        let t = c.data_type.to_ascii_lowercase();
        let large = t.contains("text") || t.contains("blob") || t == "json" || t == "longtext";
        if large
            && mentions_word(&text, &c.table.to_ascii_lowercase())
            && (star || mentions_word(&text, &c.name.to_ascii_lowercase()))
            && !out.contains(&c.name)
        {
            out.push(c.name.clone());
        }
    }
    out
}

#[cfg(test)]
mod wording_tests {
    use super::*;
    use culprit_core::{NodeKind, Properties};

    #[test]
    fn plan_steps_take_the_right_article() {
        let mut n = PlanNode {
            id: 1,
            op: "Index Scan".into(),
            kind: NodeKind::Scan,
            detail: None,
            object: Some("orders".into()),
            index: None,
            seek_predicate: None,
            predicate: None,
            est_rows: 1.0,
            est_executions: 1.0,
            act_rows: None,
            act_executions: None,
            rows_read: None,
            table_rows: None,
            subtree_cost: 1.0,
            self_cost: 1.0,
            act_time_ms: None,
            warnings: vec![],
            props: Properties::new(),
            children: vec![],
            runtime: None,
        };
        assert_eq!(describe_node(&n), "an index scan of orders");
        n.op = "Seq Scan".into();
        assert_eq!(describe_node(&n), "a seq scan of orders");
    }
}
