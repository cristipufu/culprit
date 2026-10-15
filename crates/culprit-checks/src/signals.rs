//! Checks on signals beyond time and reads. Per query: runs that fail or time out, planning and JIT time, log volume,
//! lock time and result size. Per server: slow storage, connection churn, replica lag. Per object: row-lock and
//! last-page contention on indexes, constraints the optimizer can't trust, missing histograms, non-HOT updates and
//! table bloat. Pure functions of the snapshot; each says plainly what its engine measures.

use std::collections::BTreeMap;

use culprit_core::waits::category_key;
use culprit_core::{
    Category, CheckInfo, Confidence, ConstraintIssue, ConstraintKind, Engine, FileIo, FileKind, Finding, Fix,
    IndexStat, Metric, QueryRef, QueryStat, Replica, ReplicaRole, Severity, Snapshot, SubjectKind, TableStat, Unit,
    format, signals::plan_row_bytes,
};

use crate::support::*;

fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    chars.next().map(|c| c.to_uppercase().collect::<String>() + chars.as_str()).unwrap_or_default()
}

fn engine_name(snapshot: &Snapshot) -> &'static str {
    match snapshot.server.engine {
        Engine::Mysql if snapshot.server.product.to_ascii_lowercase().contains("mariadb") => "MariaDB",
        Engine::Mysql => "MySQL",
        Engine::Postgres => "PostgreSQL",
        Engine::Mssql => "SQL Server",
    }
}

fn is_mariadb(snapshot: &Snapshot) -> bool {
    engine_name(snapshot) == "MariaDB"
}

/// "in the last 7 days" for windowed sources, "since its statistics began 3 days ago" otherwise.
fn window_phrase(snapshot: &Snapshot) -> String {
    match snapshot.workload.as_ref() {
        Some(w) if w.windowed => format!("in the last {}", format::duration(w.window_hours * 3_600_000.0)),
        Some(w) if w.window_hours > 0.0 => {
            format!("since its statistics began {} ago", format::duration(w.window_hours * 3_600_000.0))
        }
        _ => "since its statistics began".into(),
    }
}

/// "since SQL Server started 3 days ago", for server counters.
fn since_start(snapshot: &Snapshot) -> String {
    let name = engine_name(snapshot);
    match snapshot.pressure.as_ref().and_then(|p| p.uptime_hours) {
        Some(h) => {
            format!("since {name} started or its statistics were reset, {} ago", format::duration(h * 3_600_000.0))
        }
        None => format!("since {name} started"),
    }
}

fn verb(text: &str) -> String {
    text.trim_start_matches(|c: char| c.is_whitespace() || c == '(')
        .split(|c: char| !c.is_ascii_alphabetic())
        .next()
        .unwrap_or("")
        .to_ascii_lowercase()
}

fn is_read(q: &QueryStat) -> bool {
    matches!(verb(&q.text).as_str(), "select" | "with" | "values" | "table")
}

/// A read whose rows go to the client. `SELECT … INTO #t` (or a new table), `SELECT … INTO OUTFILE` and `SELECT @v = …`
/// / `SELECT @v := …` start with SELECT but keep their rows on the server.
fn returns_rows(q: &QueryStat) -> bool {
    if !is_read(q) {
        return false;
    }
    let lower = q.text.to_ascii_lowercase().replace(['\n', '\r', '\t'], " ");
    let words: Vec<&str> = lower.split_whitespace().collect();
    // `INTO` before the first FROM, outside parentheses and strings, is SELECT INTO; MySQL also writes it last.
    let mut depth = 0i32;
    for w in &words {
        depth += w.matches('(').count() as i32 - w.matches(')').count() as i32;
        if depth > 0 {
            continue;
        }
        match *w {
            "from" => break,
            "into" => return false,
            _ => {}
        }
    }
    if lower.contains(" into outfile") || lower.contains(" into dumpfile") || lower.contains(" into @") {
        return false;
    }
    // The first item of the select list assigns a variable: `SELECT @total = SUM(x)` (SQL Server), `@v := …` (MySQL).
    let mut list = words.iter().skip(1).skip_while(|w| matches!(**w, "distinct" | "all" | "top")).peekable();
    if let Some(first) = list.next() {
        let first = if first.starts_with('(') || first.chars().all(|c| c.is_ascii_digit()) {
            list.next().copied().unwrap_or(first)
        } else {
            first
        };
        if first.starts_with('@') && !first.starts_with("@@") {
            let rest = &lower[lower.find(first).unwrap_or(0) + first.len()..];
            let assigns = first.contains(":=")
                || first.ends_with('=')
                || rest.trim_start().starts_with('=')
                || rest.trim_start().starts_with(":=");
            if assigns {
                return false;
            }
        }
    }
    true
}

/// A read that copies a whole table out: mysqldump's `SELECT SQL_NO_CACHE * FROM t`, or an export or ETL job reading
/// without a filter a few times a day. Its rows are the point; paging them only spreads the same rows over more runs.
fn looks_like_export(snapshot: &Snapshot, q: &QueryStat) -> bool {
    let lower = q.text.to_ascii_lowercase();
    if lower.contains("sql_no_cache") {
        return true;
    }
    let unfiltered = !lower.contains(" where ") && !lower.contains(" limit ") && !lower.contains(" top ");
    let few_a_day = snapshot.workload.as_ref().and_then(|w| w.per_day_measured(q.executions)).is_some_and(|d| d <= 4.0);
    unfiltered && few_a_day
}

fn is_write(q: &QueryStat) -> bool {
    matches!(verb(&q.text).as_str(), "insert" | "update" | "delete" | "merge" | "copy" | "replace" | "upsert")
}

fn lock_ms(q: &QueryStat) -> f64 {
    q.waits.iter().filter(|w| category_key(&w.category) == "lock").map(|w| w.wait_ms).sum()
}

fn severity_score(f: &mut Finding) {
    f.impact.score = f.impact.score.max(base_score(f.severity));
}

/// Attach the query's share of database time, keep a score at least as high as the severity's.
fn with_query(snapshot: &Snapshot, f: &mut Finding, q: &QueryStat) {
    if let Some(impact) = query_impact(snapshot, &[q]) {
        apply_query_impact(f, &impact);
    }
    f.queries = query_refs(&[q]);
    f.last_seen = q.last_seen.clone();
}

/// "40%", or "almost all" when rounding would claim 100%.
fn share_words(share: f64) -> String {
    if share >= 0.97 { "almost all".into() } else { format::percent(share) }
}

fn per_run(total: f64, runs: f64) -> f64 {
    if runs > 0.0 { total / runs } else { 0.0 }
}

/// Whether the workload window's start is only a lower bound: pg_stat_statements before PostgreSQL 14 keeps no reset
/// time, so the window is dated from the server's start, but the counts survive a clean restart and may be far older.
fn window_start_unknown(snapshot: &Snapshot) -> bool {
    snapshot.server.engine == Engine::Postgres
        && !culprit_core::version_at_least(&snapshot.server.version, [14, 0, 0])
        && snapshot.workload.as_ref().is_some_and(|w| !w.windowed)
}

/// [`at_least_per_day`], false where the window's start is unknown (see [`window_start_unknown`]).
fn window_at_least(snapshot: &Snapshot, total: f64, per_day: f64) -> bool {
    !window_start_unknown(snapshot) && at_least_per_day(snapshot, total, per_day)
}

/// [`rate_per_day`] for a threshold, `None` where the window's start is unknown (see [`window_start_unknown`]): a
/// window that may be days too short would make the rate days too high.
fn window_rate_per_day(snapshot: &Snapshot, value: f64) -> Option<f64> {
    if window_start_unknown(snapshot) {
        return None;
    }
    rate_per_day(snapshot, value)
}

/// The grade of a count kept since the server started (or its statistics were reset), the one way every check here
/// grades one: such a count keeps no time for each event, so a burst months ago reads the same as one this morning.
/// It escalates only on a daily rate measured over an hour or more of counters ([`counter_rate_per_day`]) reaching
/// `high` or `medium`; without one it is history, Low.
fn since_start_severity(rate_per_day: Option<f64>, high: f64, medium: f64) -> Severity {
    match rate_per_day {
        Some(r) if r >= high => Severity::High,
        Some(r) if r >= medium => Severity::Medium,
        _ => Severity::Low,
    }
}

/// The caveat on a since-start count that has no measured rate: what was counted, over how long, and why it isn't
/// graded higher.
fn history_caveat(snapshot: &Snapshot, what: &str) -> String {
    let engine = engine_name(snapshot);
    match counter_days(snapshot) {
        // Counting for less than an hour: too little for a rate, whatever the total.
        Some(_) => format!(
            "{engine} has counted these {what} only {}: too little time to make a rate, so the total doesn't raise it. A scan after an hour or more of counting can.",
            since_phrase(snapshot).trim_start_matches("in ")
        ),
        None => format!(
            "{engine} counts {what} from when it started, with no time for each, and this scan doesn't know when that was: they may come from one burst long ago. Culprit raises it only on a rate measured over an hour or more of counting."
        ),
    }
}

// ── failing-query ──────────────────────────────────────────────────────────────────────────────────

pub fn failing_query_info() -> CheckInfo {
    CheckInfo {
        id: "failing-query".into(),
        title: "Queries that fail or time out".into(),
        category: Category::Queries,
        engines: vec![Engine::Mssql, Engine::Mysql],
        requires: vec![],
        description: "Statements whose runs end in an error or are cancelled by the client (usually a command timeout): Query Store's aborted and exception runs, performance_schema's errors per digest. PostgreSQL keeps no per-statement error count.".into(),
    }
}

/// A client command timeout: cancelled runs that last about as long as a common default (30 s, 60 s…).
fn looks_like_timeout(mean_ms: f64) -> Option<f64> {
    [15.0, 30.0, 60.0, 120.0].into_iter().find(|s| (mean_ms / 1000.0 - s).abs() <= s * 0.1)
}

/// Error 3572 is FOR UPDATE NOWAIT finding the row taken: the statement asked not to wait, and the application expects
/// the error (a try-lock), so it isn't a run that failed.
fn deliberate_error(costs: &culprit_core::QueryCosts) -> bool {
    costs.top_error.as_deref().is_some_and(|e| e.starts_with("3572 "))
}

pub fn failing_query(snapshot: &Snapshot) -> Vec<Finding> {
    let mssql = snapshot.server.engine == Engine::Mssql;
    let mut findings: Vec<Finding> = app_queries(snapshot)
        .filter_map(|q| {
            let costs = q.costs.as_ref()?;
            if deliberate_error(costs) {
                return None;
            }
            let unfinished = costs.unfinished()?;
            let rate = per_run(unfinished, q.executions);
            (unfinished >= 5.0 && rate >= 0.01).then_some((q, costs, unfinished, rate))
        })
        .map(|(q, costs, unfinished, rate)| {
            let mut f = finding("failing-query", &q.id, Category::Queries, subject(SubjectKind::Query, &q.id));
            let handle = query_handle(q);
            let aborted = costs.aborted.unwrap_or(0.0);
            let failed = costs.failed.unwrap_or(0.0);
            let timeout = costs.aborted_mean_ms.and_then(looks_like_timeout).filter(|_| aborted >= failed);
            let how_often =
                if rate >= 0.999 { "on every run".to_string() } else { format!("{} of the time", format::percent(rate)) };
            f.title = match (aborted > 0.0, failed > 0.0) {
                (true, true) => format!("{} fails or times out {how_often}", capitalize(&handle)),
                (true, false) => match timeout {
                    Some(s) => format!("{} times out {how_often}, at about {s:.0} s", capitalize(&handle)),
                    None => format!("{} is cancelled {how_often}", capitalize(&handle)),
                },
                _ => match &costs.top_error {
                    Some(e) => format!("{} fails {how_often} ({e})", capitalize(&handle)),
                    None => format!("{} fails {how_often}", capitalize(&handle)),
                },
            };
            let mut parts = vec![if unfinished >= q.executions {
                format!("None of its {} runs {} finished.", format::count(q.executions), window_phrase(snapshot))
            } else {
                format!(
                    "{} of its {} runs {} didn't finish.",
                    format::count(unfinished),
                    format::count(q.executions),
                    window_phrase(snapshot)
                )
            }];
            if aborted > 0.0 {
                parts.push(format!(
                    "{} were cancelled by the client{}{}.",
                    format::count(aborted),
                    costs.aborted_mean_ms.map(|ms| format!(" after {} on average", format::duration(ms))).unwrap_or_default(),
                    if timeout.is_some() { ", which is what a command timeout looks like" } else { "" }
                ));
            }
            if failed > 0.0 {
                parts.push(match &costs.top_error {
                    Some(e) => format!("{} ended in an error; the recent ones were {e}.", format::count(failed)),
                    None => format!("{} ended in an error.", format::count(failed)),
                });
            }
            parts.push("A run that doesn't finish still costs the time it ran, and the application retries it or shows an error.".into());
            f.summary = parts.join(" ");
            // MySQL and MariaDB count errors per digest from when it was first seen, with no time for each: an old
            // storm keeps the share up long after it ended. Only a failed run among the recent statements
            // performance_schema keeps (the error named above) shows it still happens.
            let recent = mssql || costs.top_error.is_some();
            f.severity = if !recent {
                Severity::Low
            } else if rate >= 0.10 {
                Severity::High
            } else if rate >= 0.02 {
                Severity::Medium
            } else {
                Severity::Low
            };
            with_query(snapshot, &mut f, q);
            severity_score(&mut f);
            f.impact.label = format!("{} of runs fail", format::percent(rate));
            f.confidence = Confidence::High;
            f.metrics = vec![Metric::new("Runs", q.executions, Unit::Count), Metric::new("Didn't finish", rate, Unit::Ratio)];
            if aborted > 0.0 {
                f.metrics.push(Metric::new("Cancelled", aborted, Unit::Count));
            }
            if failed > 0.0 {
                f.metrics.push(Metric::new("Errors", failed, Unit::Count));
            }
            if let Some(ms) = costs.aborted_mean_ms {
                f.metrics.push(Metric::new("Cancelled after", ms, Unit::Ms));
            }
            if let Some(e) = &costs.top_error {
                f.metrics.push(Metric::text("Recent error", e.clone()));
            }
            f.fix = Some(failing_fix(snapshot, q, costs, timeout, aborted > failed));
            f.verify = vec!["Re-scan after the change: runs that fail or are cancelled should drop to near zero.".into()];
            if !recent {
                f.caveats.push(format!(
                    "{} counts errors per statement from when it first saw the statement, with no time for each, and none of its runs in the recent statement history performance_schema keeps failed: these failures may be old. Culprit raises it once a recent run fails too.",
                    engine_name(snapshot)
                ));
            } else if !mssql {
                f.caveats.push(format!("{} counts errors per statement since it started, so old failures stay in the total until it restarts.", engine_name(snapshot)));
            }
            f
        })
        .collect();
    findings.sort_by(|a, b| b.impact.score.total_cmp(&a.impact.score));
    findings.truncate(5);
    findings
}

fn failing_fix(
    snapshot: &Snapshot,
    q: &QueryStat,
    costs: &culprit_core::QueryCosts,
    timeout: Option<f64>,
    mostly_cancelled: bool,
) -> Fix {
    let timeouts = timeout.is_some() || mostly_cancelled;
    let error = costs.top_error.as_deref().unwrap_or("");
    let code = error.split_whitespace().next().unwrap_or("");
    match snapshot.server.engine {
        Engine::Mssql if timeouts => Fix {
            summary: format!(
                "Cancelled runs are almost always the application's command timeout ({}30 s is the default in ADO.NET and most drivers). Make the statement faster first: open its plan. If it legitimately runs long, raise the timeout for this call only.",
                timeout.map(|s| format!("here about {s:.0} s; ")).unwrap_or_default()
            ),
            sql: None,
            cost: None,
        },
        Engine::Mssql => {
            let azure = snapshot.server.on(culprit_core::Platform::AzureSqlDatabase);
            let scope = if azure { "DATABASE" } else { "SERVER" };
            Fix {
                summary: "Query Store counts the errors but doesn't keep them. Capture them for a while with an Extended Events session on error_reported for this database, read the ring buffer, then drop the session.".into(),
                sql: Some(format!(
                    "-- Capture errors raised in this database; drop the session when you're done\nCREATE EVENT SESSION culprit_errors ON {scope}\nADD EVENT sqlserver.error_reported (\n  ACTION (sqlserver.client_app_name, sqlserver.sql_text)\n  WHERE severity >= 11 AND sqlserver.database_name = N'{}')\nADD TARGET package0.ring_buffer;\nALTER EVENT SESSION culprit_errors ON {scope} STATE = START;\n-- Later: DROP EVENT SESSION culprit_errors ON {scope};",
                    snapshot.server.database.replace('\'', "''")
                )),
                cost: None,
            }
        }
        _ => {
            let advice = match code {
                "1205" => {
                    "Another transaction holds the rows it needs for longer than innodb_lock_wait_timeout: find the holder in Live while it happens and keep transactions short."
                }
                "1213" => "It loses deadlocks: see Deadlocks for the other statement and the order to take locks in.",
                "3024" | "1969" => {
                    "It hits the statement time limit: make it faster (open its plan), or raise the limit for this statement only."
                }
                "1062" => "It inserts rows that already exist: use INSERT … ON DUPLICATE KEY UPDATE, or check first.",
                _ => {
                    "Read the recent errors with the statement below, then fix the cause in the application or the statement."
                }
            };
            Fix {
                summary: format!("{advice} The recent errors and their messages are in performance_schema."),
                sql: q.query_hash.as_ref().map(|digest| {
                    format!(
                        "SELECT MYSQL_ERRNO, RETURNED_SQLSTATE, MESSAGE_TEXT\n  FROM performance_schema.events_statements_history\n WHERE DIGEST = '{}' AND MYSQL_ERRNO <> 0;",
                        digest.replace('\'', "")
                    )
                }),
                cost: None,
            }
        }
    }
}

// ── planning-time ──────────────────────────────────────────────────────────────────────────────────

pub fn planning_time_info() -> CheckInfo {
    CheckInfo {
        id: "planning-time".into(),
        title: "Queries that spend their time being planned".into(),
        category: Category::Queries,
        engines: vec![Engine::Mssql, Engine::Postgres],
        requires: vec![],
        description: "Statements where planning (PostgreSQL, with pg_stat_statements.track_planning on) or compiling (SQL Server Query Store) takes a large part of their time, usually because the plan isn't reused.".into(),
    }
}

pub fn planning_time(snapshot: &Snapshot) -> Vec<Finding> {
    let pg = snapshot.server.engine == Engine::Postgres;
    let mut findings: Vec<Finding> = app_queries(snapshot)
        .filter_map(|q| {
            let costs = q.costs.as_ref()?;
            let plan_ms = costs.plan_ms.filter(|ms| *ms >= 5_000.0)?;
            let share = plan_ms / (plan_ms + q.total_time_ms);
            (share >= 0.25 && q.executions >= 50.0).then_some((q, plan_ms, costs.plans.unwrap_or(0.0), share))
        })
        .map(|(q, plan_ms, plans, share)| {
            let mut f = finding("planning-time", &q.id, Category::Queries, subject(SubjectKind::Query, &q.id));
            let handle = query_handle(q);
            let word = if pg { "Planning" } else { "Compiling" };
            f.title = format!("{word} takes {} of the time of {handle}", share_words(share));
            let per_plan = per_run(plan_ms, plans.max(1.0));
            let every_run = plans >= q.executions * 0.9;
            let reuse = if every_run {
                "a new plan for every run".to_string()
            } else {
                format!("a plan reused about {} times", format::count(per_run(q.executions, plans.max(1.0))))
            };
            f.summary = format!(
                "Each {} takes {} and the statement then runs for {}: {} of its time goes to {}, {} {} ({}). \
                 Planning gets expensive with many joins, partitions or large statistics targets, and it repeats when the plan isn't reused.",
                if pg { "plan" } else { "compile" },
                format::duration(per_plan),
                format::duration(q.mean_time_ms),
                share_words(share),
                if pg { "planning" } else { "compiling" },
                format::duration(plan_ms),
                window_phrase(snapshot),
                reuse
            );
            f.severity = if share >= 0.5 && window_at_least(snapshot, plan_ms, 60_000.0) { Severity::Medium } else { Severity::Low };
            with_query(snapshot, &mut f, q);
            severity_score(&mut f);
            f.impact.label = format!("{} planning", format::percent(share));
            f.confidence = if pg { Confidence::High } else { Confidence::Medium };
            f.metrics = vec![
                Metric::new("Planning share", share, Unit::Ratio),
                Metric::new("Per plan", per_plan, Unit::Ms),
                Metric::new("Plans", plans, Unit::Count),
                Metric::new("Runs", q.executions, Unit::Count),
            ];
            f.fix = Some(if pg {
                Fix {
                    summary: "Prepare it once and reuse it: most drivers do when you bind parameters (pgjdbc prepareThreshold, Npgsql Max Auto Prepare, psycopg prepare=True). Behind PgBouncer in transaction mode, prepared statements need 1.21+ with max_prepared_statements set. If planning stays slow when reused, look at the number of joins and partitions it touches.".into(),
                    sql: None,
                    cost: None,
                }
            } else if every_run {
                Fix {
                    summary: "It compiles on every run: look for OPTION (RECOMPILE), a procedure created WITH RECOMPILE, or literal values in the text instead of parameters (sp_executesql with parameters reuses one plan). Forced parameterization reuses plans for statements that differ only in literals, for the whole database: test it first.".into(),
                    sql: Some("-- Database-wide: statements that differ only in literals share a plan\nALTER DATABASE CURRENT SET PARAMETERIZATION FORCED;".into()),
                    cost: None,
                }
            } else {
                Fix {
                    summary: "Plans are reused but each compile is slow: simplify the statement (fewer joins and views), or keep its plan with a Query Store hint or plan forcing so it isn't recompiled after statistics change.".into(),
                    sql: None,
                    cost: None,
                }
            });
            f.verify = vec!["Re-scan: planning should be a small part of its time.".into()];
            if !pg {
                f.caveats.push("Query Store counts compiles over the query's whole life but keeps runtime statistics only for its retention period: Culprit scales the compiles to the period it keeps runs for and spreads them over the window's runs, so the time per run is an estimate.".into());
            }
            f
        })
        .collect();
    findings.sort_by(|a, b| b.impact.score.total_cmp(&a.impact.score));
    findings.truncate(5);
    findings
}

// ── jit-overhead ───────────────────────────────────────────────────────────────────────────────────

pub fn jit_overhead_info() -> CheckInfo {
    CheckInfo {
        id: "jit-overhead".into(),
        title: "JIT compilation that costs more than it saves".into(),
        category: Category::Queries,
        engines: vec![Engine::Postgres],
        requires: vec![],
        description: "Statements where PostgreSQL's JIT compilation (pg_stat_statements jit_* columns, 15+) takes a large part of their run time: usually short queries whose cost the planner overestimates.".into(),
    }
}

pub fn jit_overhead(snapshot: &Snapshot) -> Vec<Finding> {
    let threshold = snapshot.setting("jit_above_cost").map(|s| s.value.clone());
    let mut findings: Vec<Finding> = app_queries(snapshot)
        .filter_map(|q| {
            let jit = q.costs.as_ref()?.jit_ms.filter(|ms| *ms >= 1_000.0)?;
            let share = per_run(jit, q.total_time_ms);
            (share >= 0.25).then_some((q, jit, share))
        })
        .map(|(q, jit, share)| {
            let mut f = finding("jit-overhead", &q.id, Category::Queries, subject(SubjectKind::Query, &q.id));
            let handle = query_handle(q);
            f.title = if share >= 0.5 {
                format!("JIT compilation takes longer than running {handle}")
            } else {
                format!("JIT compilation takes {} of the time of {handle}", format::percent(share))
            };
            f.summary = format!(
                "PostgreSQL compiles parts of this statement to machine code before running it, because the planner estimates its cost above jit_above_cost{}. \
                 That takes {} of its {} per run ({} in all {}). JIT pays off for long analytical queries; for a statement that runs in {} the compiling costs more than it saves, usually because the planner overestimates its cost.",
                threshold.as_ref().map(|t| format!(" ({t})")).unwrap_or_default(),
                format::duration(per_run(jit, q.executions)),
                format::duration(q.mean_time_ms),
                format::duration(jit),
                window_phrase(snapshot),
                format::duration(q.mean_time_ms)
            );
            f.severity = if share >= 0.5 || window_at_least(snapshot, jit, 60_000.0) { Severity::Medium } else { Severity::Low };
            with_query(snapshot, &mut f, q);
            severity_score(&mut f);
            f.impact.label = format!("{} JIT", format::percent(share));
            f.confidence = Confidence::High;
            f.metrics = vec![
                Metric::new("JIT share", share, Unit::Ratio),
                Metric::new("JIT per run", per_run(jit, q.executions), Unit::Ms),
                Metric::new("Run time", q.mean_time_ms, Unit::Ms),
            ];
            f.fix = Some(jit_fix(snapshot, threshold.as_deref()));
            f.verify = vec!["Re-scan: JIT time per run should drop to zero or near it, and the average run time with it.".into()];
            f
        })
        .collect();
    findings.sort_by(|a, b| b.impact.score.total_cmp(&a.impact.score));
    findings.truncate(5);
    findings
}

/// The default jit_above_cost, and the threshold the fix raises it to from there: ten times as high.
const JIT_ABOVE_COST_DEFAULT: f64 = 100_000.0;

/// Raise jit_above_cost where this server is configured, from what it is now: ten times the default, or ten times the
/// current value when that is already higher, never lower. Or turn JIT off for the application's role.
fn jit_fix(snapshot: &Snapshot, current: Option<&str>) -> Fix {
    let current = current.and_then(|v| v.trim().parse::<f64>().ok()).unwrap_or(JIT_ABOVE_COST_DEFAULT);
    let target = (current.max(JIT_ABOVE_COST_DEFAULT) * 10.0).round();
    let change = setting_change(&snapshot.server, "jit_above_cost", &format!("{target:.0}"));
    let role = "-- Or for the application's role only:\n-- ALTER ROLE app_user SET jit = off;";
    Fix {
        summary: format!(
            "Raise the cost threshold so JIT only starts for plans ten times as expensive as now ({} today): {} Or turn JIT off for the application's role. Fixing the row estimate that inflates the cost also helps.",
            format::count(current),
            change.how
        ),
        sql: Some(match change.sql {
            Some(sql) => format!(
                "-- JIT only for plans the planner thinks are 10× more expensive than today's threshold\n{sql}\n{role}"
            ),
            None => role.to_string(),
        }),
        cost: None,
    }
}

// ── log-volume ─────────────────────────────────────────────────────────────────────────────────────

pub fn log_volume_info() -> CheckInfo {
    CheckInfo {
        id: "log-volume".into(),
        title: "Writes that produce most of the log".into(),
        category: Category::Queries,
        engines: vec![Engine::Mssql, Engine::Postgres],
        requires: vec![],
        description: "Statements that write a large share of the WAL (PostgreSQL 13+) or transaction log (SQL Server 2017+), or many kilobytes of it per row changed. Log volume drives replication lag, backup size and checkpoints. MySQL keeps no redo bytes per statement.".into(),
    }
}

const GB: f64 = 1024.0 * 1024.0 * 1024.0;
const MB: f64 = 1024.0 * 1024.0;

pub fn log_volume(snapshot: &Snapshot) -> Vec<Finding> {
    let pg = snapshot.server.engine == Engine::Postgres;
    let log = if pg { "WAL" } else { "transaction log" };
    let mut findings: Vec<Finding> = app_queries(snapshot)
        .filter(|q| is_write(q))
        .filter_map(|q| {
            let costs = q.costs.as_ref()?;
            let bytes = costs.log_bytes?;
            let share = costs.log_share.unwrap_or(0.0);
            let rows = q.rows.unwrap_or(0.0);
            let per_row = (rows >= 1_000.0).then(|| bytes / rows);
            // A share of the log is a threshold only with a volume a day behind it: 30% of a quiet hour is nothing.
            let heavy_share = share >= 0.3 && window_at_least(snapshot, bytes, GB);
            let heavy_rows = per_row.is_some_and(|b| b >= 4_096.0) && bytes >= 100.0 * MB;
            (heavy_share || heavy_rows).then_some((q, costs, bytes, share, per_row))
        })
        .map(|(q, costs, bytes, share, per_row)| {
            let mut f = finding("log-volume", &q.id, Category::Queries, subject(SubjectKind::Query, &q.id));
            let handle = query_handle(q);
            f.title = match per_row {
                Some(b) if b >= 4_096.0 => format!("{} writes {} of {log} per row it changes", capitalize(&handle), format::bytes(b)),
                _ => format!("{} writes {} of the database's {log}", capitalize(&handle), format::percent(share)),
            };
            // pg_stat_statements counts full-page images, not their bytes. A whole page is 8 KB at most: PostgreSQL
            // leaves out the empty middle of each page, and wal_compression shrinks the rest, so 8 KB each is an upper
            // bound, and close to the truth only with compression off.
            // wal_compression is off, on, or a method (pglz, lz4, zstd from 15).
            let compression = pg
                .then(|| snapshot.setting("wal_compression"))
                .flatten()
                .map(|s| !matches!(s.value.trim().to_ascii_lowercase().as_str(), "off" | "false" | "0" | "no"));
            let images = costs.log_full_pages.filter(|n| *n > 0.0);
            let image_share = images.filter(|_| compression == Some(false)).map(|n| (n * 8_192.0 / bytes).min(1.0));
            f.summary = format!(
                "It wrote {} of {log} {}: {} of what statements wrote{}. Every byte of {log} is also written to backups and sent to replicas, so it drives replication lag, backup size and checkpoints.{}",
                format::bytes(bytes),
                window_phrase(snapshot),
                format::percent(share),
                per_row.map(|b| format!(", about {} per row it changed", format::bytes(b))).unwrap_or_default(),
                match (image_share, images) {
                    (Some(s), _) if s >= 0.5 => format!(" Up to {} of it is full-page images: the first change to each page after a checkpoint logs the whole page, 8 KB at most.", format::percent(s)),
                    (None, Some(n)) if pg && n * 8_192.0 >= 0.5 * bytes => format!(
                        " {} of its records are full-page images: the first change to each page after a checkpoint logs the whole page{}.",
                        format::count(n),
                        if compression == Some(true) { ", compressed with wal_compression" } else { "" }
                    ),
                    _ => String::new(),
                }
            );
            let busy = share >= 0.5 && window_at_least(snapshot, bytes, GB);
            f.severity = if busy || per_row.is_some_and(|b| b >= 8_192.0) { Severity::Medium } else { Severity::Low };
            with_query(snapshot, &mut f, q);
            severity_score(&mut f);
            f.impact.label = format!("{} of {log}", format::bytes(bytes));
            f.confidence = Confidence::High;
            f.metrics = vec![Metric::new("Log written", bytes, Unit::Bytes), Metric::new("Share of log", share, Unit::Ratio)];
            if let Some(b) = per_row {
                f.metrics.push(Metric::new("Per row", b, Unit::Bytes));
            }
            f.fix = Some(if pg {
                // wal_compression only where it is known to be off; on or unknown, the checkpoint interval is the lever.
                let compress = image_share.filter(|s| *s >= 0.5).map(|_| setting_change(&snapshot.server, "wal_compression", "on"));
                let images = match (&compress, compression) {
                    (Some(change), _) => format!(" Full-page images dominate and wal_compression is off: turning it on shrinks them (lz4 or zstd on PostgreSQL 15+). {}", change.how),
                    (None, Some(true)) => " wal_compression already shrinks full-page images; a longer checkpoint interval (checkpoint_timeout, max_wal_size) logs fewer of them.".to_string(),
                    (None, _) => " If full-page images dominate, a longer checkpoint interval (checkpoint_timeout, max_wal_size) logs fewer of them, and wal_compression shrinks them if it is off.".to_string(),
                };
                Fix {
                    summary: format!("Touch each page once per checkpoint: batch the changes (one statement for many rows, sorted by key) rather than many scattered single-row writes. Every index on the table adds its own WAL: drop indexes nobody reads (see Indexes).{images}"),
                    sql: compress.and_then(|c| c.sql),
                    cost: None,
                }
            } else {
                Fix {
                    summary: "Log per row this high usually means wide rows rewritten whole, many indexes updated, or one transaction per row. Batch writes into transactions of about a thousand rows, update only the columns that change, and drop indexes nobody reads (see Indexes).".into(),
                    sql: None,
                    cost: None,
                }
            });
            f.verify = vec![format!("Re-scan: its share of the {log} and the bytes per row should drop.")];
            f
        })
        .collect();
    findings.sort_by(|a, b| b.impact.score.total_cmp(&a.impact.score));
    findings.truncate(5);
    findings
}

// ── lock-time ──────────────────────────────────────────────────────────────────────────────────────

pub fn lock_time_info() -> CheckInfo {
    CheckInfo {
        id: "lock-time".into(),
        title: "Queries that spend their time waiting for locks".into(),
        category: Category::Queries,
        engines: vec![Engine::Mssql, Engine::Mysql],
        requires: vec![],
        description: "Statements that spend a large part of their time waiting for locks other sessions hold: Query Store's lock waits per query, MySQL and MariaDB lock time per digest. PostgreSQL keeps no lock time per statement; Live shows blocking as it happens.".into(),
    }
}

/// Lock waits a day that make a statement's waiting High, or Medium: half an hour and five minutes.
const LOCK_HIGH_MS_PER_DAY: f64 = 30.0 * 60_000.0;
const LOCK_MEDIUM_MS_PER_DAY: f64 = 5.0 * 60_000.0;

/// Runs that end in lock wait timeouts often enough for failing-query to report them.
fn times_out_on_locks(q: &QueryStat) -> bool {
    q.costs.as_ref().is_some_and(|c| {
        c.top_error.as_deref().is_some_and(|e| e.starts_with("1205"))
            && c.unfinished().is_some_and(|n| n >= 5.0 && per_run(n, q.executions) >= 0.01)
    })
}

pub fn lock_time(snapshot: &Snapshot) -> Vec<Finding> {
    let total_db = snapshot.workload.as_ref().map(|w| w.total_time_ms).unwrap_or(0.0);
    let mut findings: Vec<Finding> = app_queries(snapshot)
        // A statement that names no table (SELECT SLEEP(…)) isn't waiting on the application's rows.
        .filter(|q| query_handle(q) != "a query")
        // Lock wait timeouts are already the failing-query finding, with the same fix.
        .filter(|q| !times_out_on_locks(q))
        .filter_map(|q| {
            let lock = lock_ms(q);
            let share = per_run(lock, q.total_time_ms);
            (lock >= 10_000.0 && share >= 0.25).then_some((q, lock, share.min(1.0)))
        })
        .map(|(q, lock, share)| {
            let mut f = finding("lock-time", &q.id, Category::Queries, subject(SubjectKind::Query, &q.id));
            let handle = query_handle(q);
            let each = per_run(lock, q.executions);
            f.title = if share >= 0.95 {
                format!("{} spends nearly all its time waiting on locks, {} per run", capitalize(&handle), format::duration(each))
            } else {
                format!(
                    "{} waits on locks for {} of its time, {} per run",
                    capitalize(&handle),
                    format::percent(share),
                    format::duration(each)
                )
            };
            f.summary = format!(
                "Of its {} {}, {} went to waiting for locks other sessions held. That time belongs to the sessions holding the locks: making this statement faster won't help, shortening the transactions that hold the rows will.{}",
                format::duration(q.total_time_ms),
                window_phrase(snapshot),
                format::duration(lock),
                if snapshot.server.engine == Engine::Mysql && !is_mariadb(snapshot) {
                    " MySQL counts row-lock waits in a statement's lock time from 8.0.28; earlier versions count table locks only."
                } else {
                    ""
                }
            );
            let db_share = if total_db > 0.0 { lock / total_db } else { 0.0 };
            // A share of database time alone makes one two-minute incident in a quiet database High: the waiting must
            // also add up to minutes a day, measured over an hour or more.
            let by_minutes = since_start_severity(
                window_rate_per_day(snapshot, lock),
                LOCK_HIGH_MS_PER_DAY,
                LOCK_MEDIUM_MS_PER_DAY,
            );
            f.severity = severity_for_share(db_share).min(by_minutes);
            if window_rate_per_day(snapshot, lock).is_none() {
                f.caveats.push(
                    "The statistics don't cover enough time (an hour or more from a known start) to say how many minutes of waiting a day this is, so it isn't raised on its share alone."
                        .into(),
                );
            }
            with_query(snapshot, &mut f, q);
            f.impact.score = score_for_share(db_share).max(base_score(f.severity));
            f.impact.label = format!("{} waiting on locks", format::duration(lock));
            f.confidence = Confidence::High;
            f.metrics = vec![
                Metric::new("Lock wait", lock, Unit::Ms),
                Metric::new("Share of its time", share, Unit::Ratio),
                Metric::new("Per run", each, Unit::Ms),
            ];
            f.fix = Some(Fix {
                summary: "Find the holder while it happens (Live shows who blocks whom). Keep the holding transactions short and commit before slow work; make UPDATE and DELETE filter on an index so they lock only the rows they change; touch rows in the same order everywhere.".into(),
                sql: None,
                cost: None,
            });
            f.verify = vec!["Re-scan: lock waits should be a small part of its time.".into()];
            f
        })
        .collect();
    findings.sort_by(|a, b| b.impact.score.total_cmp(&a.impact.score));
    findings.truncate(5);
    findings
}

// ── big-result ─────────────────────────────────────────────────────────────────────────────────────

pub fn big_result_info() -> CheckInfo {
    CheckInfo {
        id: "big-result".into(),
        title: "Queries that return huge results".into(),
        category: Category::Queries,
        engines: vec![Engine::Mssql, Engine::Postgres, Engine::Mysql],
        requires: vec![],
        description: "Reads that send tens of thousands of rows to the application on every run, with an estimate in bytes from the plan's row width where the engine gives one.".into(),
    }
}

pub fn big_result(snapshot: &Snapshot) -> Vec<Finding> {
    let mut findings: Vec<Finding> = app_queries(snapshot)
        .filter(|q| returns_rows(q) && q.executions >= 20.0)
        .filter_map(|q| {
            let rows = per_run(q.rows?, q.executions);
            (rows >= 10_000.0).then_some((q, rows))
        })
        .map(|(q, rows)| {
            let mut f = finding("big-result", &q.id, Category::Queries, subject(SubjectKind::Query, &q.id));
            let handle = query_handle(q);
            let width = q.costs.as_ref().and_then(|c| c.row_bytes).or_else(|| q.plan.as_ref().and_then(plan_row_bytes));
            let bytes = width.map(|w| w * rows);
            let export = looks_like_export(snapshot, q);
            f.title = format!(
                "{} returns {} rows{} per run",
                capitalize(&handle),
                format::count(rows),
                bytes.map(|b| format!(" (≈ {})", format::bytes(b))).unwrap_or_default()
            );
            f.summary = format!(
                "Each run sends {} rows to the application{}; its {} runs {} moved {} rows. {}",
                format::count(rows),
                match (bytes, width) {
                    (Some(b), Some(w)) => format!(", about {} at the plan's estimated {} per row", format::bytes(b), format::bytes(w)),
                    _ => String::new(),
                },
                format::count(q.executions),
                window_phrase(snapshot),
                format::count(q.rows.unwrap_or(0.0)),
                if export {
                    format!(
                        "It reads the whole table {}: an export, a dump or an ETL job looks like this, and for those the rows are the point. It needs work only if a screen or a request runs it.",
                        runs_phrase(snapshot, q.executions)
                    )
                } else {
                    "Few screens or jobs need that many at once: the database reads them, the network carries them and the application holds them in memory.".to_string()
                }
            );
            f.severity = if export {
                Severity::Low
            } else if rows >= 100_000.0 || window_at_least(snapshot, q.rows.unwrap_or(0.0), 100_000_000.0) {
                Severity::Medium
            } else {
                Severity::Low
            };
            with_query(snapshot, &mut f, q);
            severity_score(&mut f);
            f.impact.label = format!("{} rows per run", format::count(rows));
            f.confidence = if width.is_some() { Confidence::High } else { Confidence::Medium };
            f.metrics = vec![Metric::new("Rows per run", rows, Unit::Rows), Metric::new("Runs", q.executions, Unit::Count)];
            if let Some(b) = bytes {
                f.metrics.push(Metric::new("Bytes per run (estimated)", b, Unit::Bytes));
            }
            let page = "Return only what's used: page the results (keyset pagination, or LIMIT / OFFSET … FETCH), filter in SQL rather than in the application, select only the columns you need, and aggregate in the database.";
            f.fix = Some(Fix {
                summary: if export {
                    format!("If it is an export or ETL job, leave it as it is, and run it on a replica or off-peak if it competes with the application. If the application runs it: {page}")
                } else {
                    page.into()
                },
                sql: None,
                cost: None,
            });
            f.verify = vec!["Re-scan: rows per run should drop to what a page or a job actually uses.".into()];
            f
        })
        .collect();
    findings.sort_by(|a, b| b.impact.score.total_cmp(&a.impact.score));
    findings.truncate(5);
    findings
}

// ── slow-storage ───────────────────────────────────────────────────────────────────────────────────

pub fn slow_storage_info() -> CheckInfo {
    CheckInfo {
        id: "slow-storage".into(),
        title: "Storage that's slow to read or write".into(),
        category: Category::Configuration,
        engines: vec![Engine::Mssql, Engine::Postgres, Engine::Mysql],
        requires: vec![],
        description: "Average read and write latency per database file since startup: sys.dm_io_virtual_file_stats, pg_stat_io (16+, with track_io_timing), performance_schema file I/O.".into(),
    }
}

/// Reads (or log writes) at least this large on average are bulk I/O: backups, CHECKDB, read-ahead of large scans. Each
/// takes longer than the page-sized reads queries wait on, so their average says little about the storage.
const BULK_IO_BYTES: f64 = 64.0 * 1024.0;

/// The files of one kind in one database, added up for the operation that matters for that kind: reads of data and
/// temporary files, writes of the log (every commit waits for them).
struct FileGroup<'a> {
    files: Vec<&'a FileIo>,
    database: Option<String>,
    kind: FileKind,
    write: bool,
    ops: f64,
    ms: f64,
    /// Bytes moved, when every file says.
    bytes: Option<f64>,
}

impl FileGroup<'_> {
    /// Average time per operation, weighted by operations across the files: one quiet file read in large chunks by a
    /// backup doesn't stand for the database's storage.
    fn avg_ms(&self) -> f64 {
        per_run(self.ms, self.ops)
    }

    /// Average bytes per operation, when known.
    fn avg_bytes(&self) -> Option<f64> {
        self.bytes.map(|b| per_run(b, self.ops))
    }

    fn bulk(&self) -> bool {
        self.avg_bytes().is_some_and(|b| b >= BULK_IO_BYTES)
    }
}

/// Every kind of file in every database whose operation-weighted average crosses its threshold over 1,000 operations.
/// tempdb's log is left out: SQL Server doesn't harden it at commit, so no commit waits for those writes.
fn slow_groups<'a>(snapshot: &Snapshot, files: &'a [FileIo]) -> Vec<FileGroup<'a>> {
    let mut groups: BTreeMap<(u8, Option<String>), FileGroup<'a>> = BTreeMap::new();
    for f in files {
        let (order, write) = match f.kind {
            FileKind::Data => (0, false),
            FileKind::Temp => (1, false),
            FileKind::Log => (2, true),
            FileKind::Other => continue,
        };
        if snapshot.server.engine == Engine::Mssql && f.kind == FileKind::Log && f.database.as_deref() == Some("tempdb")
        {
            continue;
        }
        let (ops, ms, bytes) =
            if write { (f.writes, f.write_ms, f.write_bytes) } else { (f.reads, f.read_ms, f.read_bytes) };
        // An operation nobody timed says nothing about its time.
        let Some(ms) = ms.filter(|_| ops > 0.0) else { continue };
        let g = groups.entry((order, f.database.clone())).or_insert_with(|| FileGroup {
            files: Vec::new(),
            database: f.database.clone(),
            kind: f.kind,
            write,
            ops: 0.0,
            ms: 0.0,
            bytes: Some(0.0),
        });
        g.files.push(f);
        g.ops += ops;
        g.ms += ms;
        g.bytes = g.bytes.zip(bytes).map(|(a, b)| a + b);
    }
    groups.into_values().filter(|g| g.ops >= 1_000.0 && g.avg_ms() >= if g.write { 10.0 } else { 20.0 }).collect()
}

fn file_label(snapshot: &Snapshot, g: &FileGroup) -> String {
    let one = (g.files.len() == 1).then(|| g.files[0].name.clone());
    match (snapshot.server.engine, g.kind, g.database.as_deref()) {
        (Engine::Mssql, _, Some("tempdb")) => "tempdb".into(),
        (Engine::Mssql, FileKind::Log, Some(db)) => format!("{db}'s log"),
        (Engine::Mssql, _, Some(db)) => match one {
            Some(name) => format!("{db}'s data file {name}"),
            None => format!("{db}'s data files"),
        },
        (Engine::Postgres, FileKind::Log, _) => "WAL".into(),
        (Engine::Postgres, FileKind::Temp, _) => "temporary tables".into(),
        (Engine::Postgres, _, _) => "table and index files".into(),
        (_, kind, _) => one.unwrap_or_else(|| {
            match kind {
                FileKind::Temp => "temporary files",
                FileKind::Log => "the log files",
                _ => "data files",
            }
            .into()
        }),
    }
}

/// How storage that answers slowly is fixed depends on where it runs.
fn storage_fix(snapshot: &Snapshot) -> &'static str {
    match snapshot.server.platform() {
        // Azure SQL Database runs on storage you don't manage: the tier decides the latency and the I/O limits.
        Some(culprit_core::Platform::AzureSqlDatabase) => {
            "In Azure SQL Database the service tier decides this: General Purpose reads from remote storage (5 to 10 ms is normal there), Hyperscale serves most reads from a local cache and the rest from page servers, Business Critical uses local SSDs. Check avg_data_io_percent and avg_log_write_percent in sys.dm_db_resource_stats: near 100% means the tier's I/O limit is throttling, so add vCores or move to a faster tier; well below it, cut the reads instead (indexes, fewer full scans). A since-startup average hides bursts: sample Waits → Right now during the busy hour."
        }
        // Managed Instance General Purpose keeps each file on a remote disk sized from the file.
        Some(culprit_core::Platform::AzureSqlManagedInstance) => {
            "On Managed Instance General Purpose each data and log file is a remote disk whose IOPS and throughput come from the file's size (up to 128 GB about 500 IOPS, up to 512 GB about 2,300, up to 1 TB 5,000): growing a busy file past the next size step raises its limit, and the next-gen General Purpose tier sets IOPS apart from size. Business Critical uses local SSDs. Compare each file's latency in sys.dm_io_virtual_file_stats with its size, and cut the reads too (indexes, fewer full scans). A since-startup average hides bursts: sample Waits → Right now during the busy hour."
        }
        _ => {
            "Check the volume's limits first: cloud disks throttle at their provisioned IOPS and throughput, and burst credits run out. Then look for other load on the same disk and antivirus scanning data files, and give the log its own low-latency volume. A since-startup average hides bursts: sample Waits → Right now during the busy hour."
        }
    }
}

pub fn slow_storage(snapshot: &Snapshot) -> Vec<Finding> {
    let Some(p) = snapshot.pressure.as_ref() else { return Vec::new() };
    let files = culprit_core::signals::merge_logs(&p.files);
    slow_groups(snapshot, &files)
        .into_iter()
        .map(|g| {
            let (avg, ops, write) = (g.avg_ms(), g.ops, g.write);
            let label = file_label(snapshot, &g);
            let what = match g.files.as_slice() {
                [one] => one.name.clone(),
                _ => format!("{} files", match g.kind {
                    FileKind::Temp => "temp",
                    FileKind::Log => "log",
                    _ => "data",
                }),
            };
            let key = format!("{}:{what}", g.database.as_deref().unwrap_or(""));
            let mut f = finding("slow-storage", &key, Category::Configuration, subject(SubjectKind::Database, &label));
            // Only reads come in bulk sizes that slow each one down; a log is judged on its commits.
            let bulk = !write && g.bulk();
            f.title = match (write, bulk) {
                (true, _) => format!("Writes to {label} average {}: every commit waits for them", format::duration(avg)),
                (false, false) => format!("Reads from {label} average {}: storage is slow", format::duration(avg)),
                (false, true) => format!(
                    "Reads from {label} average {}, mostly large reads of {}",
                    format::duration(avg),
                    format::bytes(g.avg_bytes().unwrap_or(0.0))
                ),
            };
            let cache_note = match snapshot.server.engine {
                Engine::Postgres => " PostgreSQL's numbers include reads the operating system served from its cache, so reads that reached the disk were slower still.",
                Engine::Mysql => " The timings include reads the operating system served from its cache unless InnoDB uses O_DIRECT.",
                Engine::Mssql => "",
            };
            // The label starts with a database name: never capitalize it, lead with the operation instead.
            f.summary = format!(
                "{} {} took {} on average across {} {} {}. Healthy SSD storage answers in a few milliseconds{}; this slows every query that misses memory{}.{}",
                if write { "Writes to" } else { "Reads from" },
                label,
                format::duration(avg),
                format::count(ops),
                if write { "writes" } else { "reads" },
                since_start(snapshot),
                if write { " and log writes in under 2 ms" } else { "" },
                if write { " and every commit" } else { "" },
                cache_note
            );
            if bulk {
                f.summary.push_str(&format!(
                    " These reads average {} each: mostly large sequential reads (backups, CHECKDB, read-ahead of big scans), which take longer each than the page-sized reads queries wait on, so the average overstates what queries see.",
                    format::bytes(g.avg_bytes().unwrap_or(0.0))
                ));
            }
            f.severity = match (write, avg) {
                (false, _) if bulk => Severity::Low,
                (true, a) if a >= 30.0 => Severity::High,
                (false, a) if a >= 50.0 => Severity::High,
                _ => Severity::Medium,
            };
            severity_score(&mut f);
            f.impact.label = format!("{} per {}", format::duration(avg), if write { "write" } else { "read" });
            f.confidence = Confidence::Medium;
            f.metrics = vec![
                Metric::new(if write { "Average write" } else { "Average read" }, avg, Unit::Ms),
                Metric::new(if write { "Writes" } else { "Reads" }, ops, Unit::Count),
                Metric::text(
                    if g.files.len() == 1 { "File" } else { "Files" },
                    g.files.iter().map(|f| f.name.clone()).collect::<Vec<_>>().join(", "),
                ),
            ];
            if let Some(b) = g.avg_bytes().filter(|_| !write) {
                f.metrics.push(Metric::new("Average read size", b, Unit::Bytes));
            }
            f.fix = Some(Fix { summary: storage_fix(snapshot).into(), sql: None, cost: None });
            f.verify = vec![if write {
                "Re-scan after the change: average log write time should be under 5 ms.".into()
            } else {
                "Re-scan after the change: average read time should be under 10 ms.".into()
            }];
            f.caveats.push("Averages since startup: a slow hour long ago still counts.".into());
            f
        })
        .collect()
}

// ── connection-churn ───────────────────────────────────────────────────────────────────────────────

pub fn connection_churn_info() -> CheckInfo {
    CheckInfo {
        id: "connection-churn".into(),
        title: "Applications that connect for every request".into(),
        category: Category::Configuration,
        engines: vec![Engine::Mssql, Engine::Postgres, Engine::Mysql],
        requires: vec![],
        description: "New connections per second since startup (SQL Server logins, PostgreSQL 14+ sessions, MySQL and MariaDB Connections), with how long sessions last and how many came from a pool where the engine says.".into(),
    }
}

pub fn connection_churn(snapshot: &Snapshot) -> Vec<Finding> {
    let Some(p) = snapshot.pressure.as_ref() else { return Vec::new() };
    let Some(rate) = p.signal("new-connections").map(|s| s.value) else { return Vec::new() };
    if p.uptime_hours.is_some_and(|h| h < 1.0) {
        return Vec::new();
    }
    let engine = snapshot.server.engine;
    // On Azure SQL Database the login counters belong to the server process behind the database, shared by its
    // elastic pool: another database's logins aren't this one's to fix.
    if engine == Engine::Mssql && snapshot.server.on(culprit_core::Platform::AzureSqlDatabase) {
        return Vec::new();
    }
    let limit = if engine == Engine::Postgres { 2.0 } else { 10.0 };
    if rate < limit {
        return Vec::new();
    }
    // Keyed on `server`, never the connection's name: finding ids travel into exports and agent briefs.
    let mut f = finding(
        "connection-churn",
        "server",
        Category::Configuration,
        subject(SubjectKind::Database, &snapshot.server.database),
    );
    // PostgreSQL counts sessions to this database; SQL Server's logins and MySQL's connections count the whole server,
    // every database and application on it.
    f.title = if engine == Engine::Postgres {
        format!("The application opens {} connections a second: use a pool", format::count(rate))
    } else {
        format!("The server accepts {} new connections a second: use a pool", format::count(rate))
    };
    let detail = match engine {
        Engine::Postgres => {
            let length = p.signal("session-length").map(|s| s.value);
            format!(
                "Each new PostgreSQL connection starts a server process and pays for TLS and authentication: milliseconds of CPU and a few MB of memory before the first query.{}",
                length
                    .map(|s| format!(" Sessions here last {} on average.", format::duration(s * 1000.0)))
                    .unwrap_or_default()
            )
        }
        Engine::Mssql => match p.signal("pooled-reuse").map(|s| s.value) {
            Some(pct) => format!(
                "Only {:.0}% of connection uses were served from a pool (SQL Server counts a reset when a pooled connection is reused); the rest logged in from scratch.",
                pct
            ),
            None => "Every login pays for TLS, authentication and session setup.".into(),
        },
        Engine::Mysql => match p.signal("thread-cache-misses").map(|s| s.value) {
            Some(pct) => format!("{:.0}% of them needed a new thread because the thread cache was empty.", pct),
            None => "Every connection pays for TLS, authentication and a thread.".into(),
        },
    };
    let scope = match engine {
        Engine::Postgres => " to this database",
        Engine::Mssql => ", counting every database on the instance",
        Engine::Mysql => ", counting every schema and application on it (failed attempts left out)",
    };
    f.summary = format!(
        "{} the server accepted {} new connections a second on average{scope}. {detail} An application that connects for each request spends more time connecting than querying under load.",
        capitalize(&since_start(snapshot)),
        format::count(rate)
    );
    f.severity = if rate >= limit * 5.0 { Severity::Medium } else { Severity::Low };
    severity_score(&mut f);
    f.impact.label = format!("{}/s new connections", format::count(rate));
    f.confidence = Confidence::Medium;
    f.metrics = vec![Metric::new("New connections per second", rate, Unit::Count)];
    // A larger thread cache, where the server is configured, and never smaller than it is.
    const THREAD_CACHE: f64 = 64.0;
    let thread_cache = snapshot.setting("thread_cache_size").and_then(|s| s.value.trim().parse::<f64>().ok());
    let cache_change = (engine == Engine::Mysql && thread_cache.is_none_or(|n| n < THREAD_CACHE))
        .then(|| setting_change(&snapshot.server, "thread_cache_size", &format!("{THREAD_CACHE:.0}")));
    f.fix = Some(match engine {
        Engine::Postgres => Fix {
            summary: "Pool connections in the application (every mainstream driver has a pool), or put PgBouncer in front in transaction mode so thousands of clients share a few dozen server connections.".into(),
            sql: None,
            cost: None,
        },
        Engine::Mssql => Fix {
            summary: "Keep connection pooling on (it is by default in ADO.NET and Microsoft.Data.SqlClient; JDBC needs a pool such as HikariCP) and close or dispose connections promptly so they go back to the pool. Every distinct connection string gets its own pool.".into(),
            sql: None,
            cost: None,
        },
        Engine::Mysql => Fix {
            summary: match (&cache_change, thread_cache) {
                (Some(change), _) => format!(
                    "Pool connections in the application. Until then, a larger thread cache lets new connections reuse threads: {}",
                    change.how
                ),
                (None, Some(n)) => format!(
                    "Pool connections in the application: the thread cache already keeps {} threads, so each connection still pays for TLS and authentication.",
                    format::count(n)
                ),
                (None, None) => "Pool connections in the application.".into(),
            },
            sql: cache_change.and_then(|c| c.sql),
            cost: None,
        },
    });
    f.verify = vec![format!(
        "Scan again a day after the change: new connections should be under {} a second.",
        format::count(limit)
    )];
    f.caveats.push("A rate since startup: one busy reconnect storm counts for the whole period.".into());
    vec![f]
}

// ── hot-rows ───────────────────────────────────────────────────────────────────────────────────────

pub fn hot_rows_info() -> CheckInfo {
    CheckInfo {
        id: "hot-rows".into(),
        title: "Rows that many sessions fight over".into(),
        category: Category::Indexes,
        engines: vec![Engine::Mssql],
        requires: vec![],
        description: "Indexes where sessions waited a long time for row and page locks, and where locks escalated to the whole table (sys.dm_db_index_operational_stats). MySQL, MariaDB and PostgreSQL keep no lock waits per table; see lock-time and Live.".into(),
    }
}

fn index_label(i: &IndexStat) -> String {
    format!("{}.{}", i.schema, i.table)
}

/// Lock waits a day on one table that make hot-rows High: ten minutes. A minute a day of waits a second or longer each
/// is Medium.
const HOT_ROWS_HIGH_MS_PER_DAY: f64 = 600_000.0;
const HOT_ROWS_MEDIUM_MS_PER_DAY: f64 = 60_000.0;

pub fn hot_rows(snapshot: &Snapshot) -> Vec<Finding> {
    let rcsi = snapshot.setting("is_read_committed_snapshot_on").map(|s| crate::config::is_on(&s.value));
    // One finding per table: its indexes' waits are the same rows fought over, whichever index the lock was taken on.
    let mut by_table: BTreeMap<(String, String), Vec<(&IndexStat, &culprit_core::IndexContention)>> = BTreeMap::new();
    for i in &snapshot.indexes {
        if let Some(c) = i.contention.as_ref().filter(|c| c.lock_waits() >= 1.0 && c.lock_wait_ms() > 0.0) {
            by_table.entry((i.schema.clone(), i.table.clone())).or_default().push((i, c));
        }
    }
    let mut findings: Vec<Finding> = by_table
        .into_iter()
        .filter_map(|((schema, table), mut indexes)| {
            let waits: f64 = indexes.iter().map(|(_, c)| c.lock_waits()).sum();
            let wait_ms: f64 = indexes.iter().map(|(_, c)| c.lock_wait_ms()).sum();
            (wait_ms >= 10_000.0).then(|| {
                indexes.sort_by(|a, b| b.1.lock_wait_ms().total_cmp(&a.1.lock_wait_ms()));
                (schema, table, indexes, waits, wait_ms)
            })
        })
        .map(|(schema, table, indexes, waits, wait_ms)| {
            let name = format!("{schema}.{table}");
            // The subject is the index most of the waiting was on; the finding is the table's.
            let (top, _) = indexes[0];
            let mut f = finding("hot-rows", &name, Category::Indexes, subject(SubjectKind::Index, format!("{}.{}", index_label(top), top.name)));
            let escalations: f64 = indexes.iter().map(|(_, c)| c.lock_escalations).sum();
            let avg = per_run(wait_ms, waits);
            f.title = format!("Sessions wait {} each on row locks in {table}", format::duration(avg));
            let names: Vec<String> = indexes.iter().map(|(i, _)| i.name.clone()).collect();
            let on = match names.as_slice() {
                [one] => format!("Since {one} was loaded into memory, sessions waited"),
                _ => format!("Since its indexes were loaded into memory, sessions waited on {}", format::list(&names)),
            };
            let escalated = if escalations > 0.0 {
                format!(" {} times SQL Server escalated many row locks to a lock on the whole table, which blocks every other session.", format::count(escalations))
            } else {
                String::new()
            };
            f.summary = format!(
                "{on} {} times for locks on the rows and pages of {table}, {} in all, {} each on average.{escalated} \
                 Waits like these come from transactions that hold rows for long, or from writes whose filter isn't indexed and so lock more rows than they change.",
                format::count(waits),
                format::duration(wait_ms),
                format::duration(avg)
            );
            let touching = queries_touching(snapshot, &schema, &table);
            let mut locked: Vec<&QueryStat> = touching.into_iter().filter(|q| lock_ms(q) > 0.0).collect();
            locked.sort_by(|a, b| lock_ms(b).total_cmp(&lock_ms(a)));
            locked.truncate(3);
            f.queries = query_refs(&locked);
            // Counted since the indexes were loaded, with no time for each wait: graded on a rate measured over an
            // hour or more, never on the total.
            let rate = counter_rate_per_day(snapshot, wait_ms);
            f.severity = match rate {
                Some(r) if r >= HOT_ROWS_HIGH_MS_PER_DAY => Severity::High,
                Some(r) if r >= HOT_ROWS_MEDIUM_MS_PER_DAY && avg >= 1_000.0 => Severity::Medium,
                _ => Severity::Low,
            };
            severity_score(&mut f);
            f.impact.label = format!("{} waiting on locks", format::duration(wait_ms));
            f.confidence = Confidence::High;
            f.metrics = vec![
                Metric::new("Lock waits", waits, Unit::Count),
                Metric::new("Time waiting", wait_ms, Unit::Ms),
                Metric::new("Average wait", avg, Unit::Ms),
            ];
            if let Some(r) = rate.filter(|_| counter_days(snapshot).is_some_and(per_day_is_measured)) {
                f.metrics.push(Metric::new("Waiting a day", r, Unit::Ms));
            }
            if escalations > 0.0 {
                f.metrics.push(Metric::new(ESCALATIONS_METRIC, escalations, Unit::Count));
            }
            f.fix = Some(Fix {
                summary: format!(
                    "Keep the transactions that change {table} short and commit before slow work; make their WHERE use an index so they lock only the rows they change.{}",
                    if rcsi == Some(false) {
                        " If readers are the ones waiting, READ_COMMITTED_SNAPSHOT lets them read the last committed version instead: it changes what readers see and uses tempdb, so test it first."
                    } else {
                        ""
                    }
                ),
                sql: (rcsi == Some(false)).then(|| "-- Readers stop waiting for writers; test first\nALTER DATABASE CURRENT SET READ_COMMITTED_SNAPSHOT ON WITH ROLLBACK IMMEDIATE;".into()),
                cost: None,
            });
            f.verify = vec![format!("Re-scan a day later: lock waits on {table} should stop growing.")];
            f.caveats.push("Counted since the indexes' metadata was loaded into memory, usually since SQL Server started.".into());
            if rate.is_none() {
                f.caveats.push(history_caveat(snapshot, "lock waits"));
            }
            f
        })
        .collect();
    findings.sort_by(|a, b| b.impact.score.total_cmp(&a.impact.score));
    findings.truncate(5);
    findings
}

// ── lock-escalations ───────────────────────────────────────────────────────────────────────────────

/// The metric `lock-escalations` (and `hot-rows`) report escalations under; the digest reads it back.
pub(crate) const ESCALATIONS_METRIC: &str = "Escalations since SQL Server started";

/// The metric `lock-escalations` reports the escalations since the previous scan under, when there is one.
pub(crate) const ESCALATIONS_SINCE_METRIC: &str = "Escalations since the previous scan";

/// SQL Server considers escalating once one statement holds about this many locks on one index.
const ESCALATION_THRESHOLD: f64 = 5_000.0;

/// Escalations a day since SQL Server started that make a first scan's finding High (one a day: something recurring)
/// or Medium (one a week). Fewer may all be old.
const ESCALATIONS_HIGH_PER_DAY: f64 = 1.0;
const ESCALATIONS_MEDIUM_PER_DAY: f64 = 1.0 / 7.0;

pub fn lock_escalations_info() -> CheckInfo {
    CheckInfo {
        id: "lock-escalations".into(),
        title: "Statements that lock a whole table".into(),
        category: Category::Indexes,
        engines: vec![Engine::Mssql],
        requires: vec![],
        description: "Tables where one statement took so many row or page locks that SQL Server replaced them with a lock on the whole table (sys.dm_db_index_operational_stats). MySQL, MariaDB and PostgreSQL never escalate row locks.".into(),
    }
}

#[derive(Default)]
struct TableEscalations {
    escalations: f64,
    lock_waits: f64,
    lock_wait_ms: f64,
}

/// The Extended Events session the lock-escalations fix starts, so the next escalation is kept with its statement.
/// Culprit never starts it; a later Culprit can read it by this name.
pub(crate) const ESCALATION_SESSION: &str = "culprit_lock_escalation";

/// The SQL that starts [`ESCALATION_SESSION`] and reads what it caught: per database on Azure SQL Database, where a
/// session can't see other databases, per server (filtered to `database`) elsewhere.
fn escalation_session_sql(database: &str, azure_sql_database: bool) -> String {
    let (scope, sessions, targets, filter, startup) = if azure_sql_database {
        ("DATABASE", "sys.dm_xe_database_sessions", "sys.dm_xe_database_session_targets", String::new(), "")
    } else {
        (
            "SERVER",
            "sys.dm_xe_sessions",
            "sys.dm_xe_session_targets",
            format!("\n    WHERE sqlserver.database_name = N'{}'", database.replace('\'', "''")),
            "\nWITH (STARTUP_STATE = ON)",
        )
    };
    format!(
        "-- SQL Server doesn't record which statement escalated: this session keeps the next ones with their statement\n\
         CREATE EVENT SESSION {ESCALATION_SESSION} ON {scope}\n\
         ADD EVENT sqlserver.lock_escalation (\n    \
             SET collect_statement = 1\n    \
             ACTION (sqlserver.sql_text, sqlserver.client_app_name){filter})\n\
         ADD TARGET package0.ring_buffer{startup};\n\
         ALTER EVENT SESSION {ESCALATION_SESSION} ON {scope} STATE = START;\n\
         \n\
         -- Then read what it caught\n\
         SELECT e.x.value('@timestamp', 'datetime2') AS at_utc,\n       \
                OBJECT_NAME(e.x.value('(data[@name=\"object_id\"]/value)[1]', 'int')) AS table_name,\n       \
                e.x.value('(data[@name=\"escalated_lock_count\"]/value)[1]', 'int') AS locks,\n       \
                e.x.value('(data[@name=\"statement\"]/value)[1]', 'nvarchar(max)') AS statement,\n       \
                e.x.value('(action[@name=\"client_app_name\"]/value)[1]', 'nvarchar(256)') AS app\n  \
           FROM (SELECT CAST(t.target_data AS xml) AS d\n          \
                   FROM {targets} t\n          \
                   JOIN {sessions} s ON s.address = t.event_session_address\n         \
                  WHERE s.name = N'{ESCALATION_SESSION}' AND t.target_name = N'ring_buffer') r\n \
          CROSS APPLY r.d.nodes('RingBufferTarget/event') e(x);"
    )
}

pub fn lock_escalations(snapshot: &Snapshot) -> Vec<Finding> {
    let mut by_table: BTreeMap<(String, String), TableEscalations> = BTreeMap::new();
    for i in &snapshot.indexes {
        let Some(c) = i.contention.as_ref() else { continue };
        // Escalations that happened, never attempts alone: SQL Server's attempt counter moves with its own background
        // work, so on a table of a few dozen rows every index can show an attempt an hour that no statement made.
        if c.lock_escalations <= 0.0 {
            continue;
        }
        let t = by_table.entry((i.schema.clone(), i.table.clone())).or_default();
        t.escalations += c.lock_escalations;
        t.lock_waits += c.lock_waits();
        t.lock_wait_ms += c.lock_wait_ms();
    }
    // SQL Server counts escalations from when it started and keeps no time for each: only an earlier scan tells the
    // recent ones from old ones.
    let before = snapshot.escalations_before.as_ref();
    let started = snapshot.server.started_at.as_deref();
    let azure = snapshot.server.on(culprit_core::Platform::AzureSqlDatabase);
    let mut findings: Vec<Finding> = by_table
        .into_iter()
        .map(|((schema, table), t)| {
            let name = format!("{schema}.{table}");
            let mut f = finding("lock-escalations", &name, Category::Indexes, subject(SubjectKind::Table, &name));
            let times = |n: f64| if n == 1.0 { "once".to_string() } else { format!("{} times", format::count(n)) };
            let since_start = match started {
                Some(at) => format!("since SQL Server started ({})", format::friendly_date(at, &snapshot.taken_at)),
                None => "since its indexes were loaded into memory".to_string(),
            };
            let started_on = started.map(crate::trends::when_phrase);
            // How many since the earlier scan, when there is one to compare with.
            let new = before.map(|b| (b, b.since(&name, t.escalations)));
            let historical = new.is_some_and(|(_, n)| n == 0.0);
            // Without one, how many a day since SQL Server started, over an hour or more of counting.
            let first_scan_rate = if new.is_none() { counter_rate_per_day(snapshot, t.escalations) } else { None };
            let happened = match new {
                Some((b, n)) if n > 0.0 => {
                    let total = if b.restarted {
                        format!(
                            "SQL Server restarted since then{}, and these are all it counted since.",
                            started_on.as_deref().map(|on| format!(" ({on})")).unwrap_or_default()
                        )
                    } else if n < t.escalations {
                        format!("It counted {} in all {since_start}.", format::count(t.escalations))
                    } else {
                        format!("All {} it counted {since_start} came after that scan.", format::count(t.escalations))
                    };
                    f.title = format!(
                        "Row locks on {table} escalated to a table lock {} since {}",
                        times(n),
                        crate::trends::short_when(&b.taken_at)
                    );
                    format!(
                        "SQL Server replaced the row and page locks on {table} with one lock on the whole table {} since the previous scan, {}. {total}",
                        times(n),
                        crate::trends::when_phrase(&b.taken_at)
                    )
                }
                Some((b, _)) => {
                    f.title = format!(
                        "Row locks on {table} escalated to a table lock {} {since_start}, none since {}",
                        times(t.escalations),
                        crate::trends::short_when(&b.taken_at)
                    );
                    format!(
                        "These escalations are history, not something happening now: SQL Server counts them from when it started{}, \
                         and the count on {table} hasn't grown since the previous scan, {}. It doesn't record when each one happened.",
                        started_on.as_deref().map(|on| format!(", {on}")).unwrap_or_default(),
                        crate::trends::when_phrase(&b.taken_at)
                    )
                }
                None => {
                    f.title = format!("Row locks on {table} escalated to a table lock {} {since_start}", times(t.escalations));
                    // Without an earlier scan, only how often they happened on average says whether they recur.
                    let often = match first_scan_rate {
                        Some(r) if r >= ESCALATIONS_HIGH_PER_DAY => {
                            " That is once a day or more on average since then: often enough to be still happening."
                        }
                        Some(r) if r >= ESCALATIONS_MEDIUM_PER_DAY => {
                            " That is once a week or more on average since then: likely to happen again."
                        }
                        Some(_) => " That is less than once a week on average since then: they may all be old.",
                        None => " SQL Server started too recently to tell how often it happens.",
                    };
                    format!(
                        "SQL Server replaced the row and page locks on {table} with one lock on the whole table {} {}. It keeps no time for each, \
                         and Culprit has no earlier scan to compare with, so they may be old: the next scan compares with this one and says \
                         whether any happened since.{often}",
                        times(t.escalations),
                        started_on.as_deref().map(|on| format!("since it started, {on}")).unwrap_or_else(|| since_start.clone())
                    )
                }
            };

            // SQL Server doesn't record which statement escalated: the writes to the table that change the most rows
            // per run are the likely ones.
            let mut writes: Vec<&QueryStat> = queries_touching(snapshot, &schema, &table)
                .into_iter()
                .filter(|q| is_write(q) && !crate::support::is_monitoring_query(q))
                .collect();
            let rows_per_run = |q: &QueryStat| q.rows.map(|r| per_run(r, q.executions));
            writes.sort_by(|a, b| {
                rows_per_run(b).unwrap_or(0.0).total_cmp(&rows_per_run(a).unwrap_or(0.0)).then(b.total_time_ms.total_cmp(&a.total_time_ms))
            });
            writes.truncate(3);
            let suspects = if historical {
                if writes.is_empty() {
                    " None of the statements in the query history writes to it now.".to_string()
                } else {
                    " The writes to it in the query history are listed below for reference; none of them has escalated since the previous scan.".to_string()
                }
            } else {
                match writes.first() {
                    Some(q) => match rows_per_run(q) {
                        Some(rows) if rows >= ESCALATION_THRESHOLD / 5.0 => format!(
                            " SQL Server doesn't record which statement it was; the likeliest is {}, which changes about {} rows a run.",
                            query_handle(q),
                            format::count(rows)
                        ),
                        _ => " SQL Server doesn't record which statement it was; the writes to it are listed below, and the one to look for changes thousands of rows in one run: a purge, a cascade, a backfill.".to_string(),
                    },
                    None => " SQL Server doesn't record which statement it was, and none of the statements in the query history writes to it: look at jobs, migrations and maintenance that change many rows at once.".to_string(),
                }
            };
            f.summary = format!(
                "{happened} An escalation happens when one statement takes about 5,000 locks on one index: \
                 SQL Server swaps them for a single lock on the whole table, and every other session that writes to {table}, or reads it without \
                 row versioning, waits until that statement's transaction ends.{suspects}"
            );
            f.queries = writes
                .iter()
                .map(|q| QueryRef {
                    id: q.id.clone(),
                    plan_node: None,
                    note: rows_per_run(q).map(|rows| format!("Changes about {} rows a run.", format::count(rows))),
                })
                .collect();
            f.last_seen = writes.first().and_then(|q| q.last_seen.clone());

            let waited = t.lock_waits >= 1.0;
            // One escalation is a lock on the whole table: every other writer waited for it, however briefly. New ones
            // since the previous scan are High; old ones with none since are history: worth preventing, not an
            // emergency. Without an earlier scan the count keeps no time, so only a measured rate raises it.
            f.severity = match new {
                Some(_) if historical => Severity::Low,
                Some(_) => Severity::High,
                None => since_start_severity(first_scan_rate, ESCALATIONS_HIGH_PER_DAY, ESCALATIONS_MEDIUM_PER_DAY),
            };
            severity_score(&mut f);
            let locks = |n: f64, new: &str| {
                format!("{} {new}table lock{}", format::count(n), if n == 1.0 { "" } else { "s" })
            };
            f.impact.label = match new {
                Some((_, n)) if n > 0.0 => locks(n, "new "),
                Some(_) => format!("{}, none new", locks(t.escalations, "")),
                None => locks(t.escalations, ""),
            };
            f.confidence = Confidence::High;
            f.metrics = vec![Metric::new(ESCALATIONS_METRIC, t.escalations, Unit::Count)];
            if let Some((_, n)) = new {
                f.metrics.push(Metric::new(ESCALATIONS_SINCE_METRIC, n, Unit::Count));
            }
            if waited {
                f.metrics.push(Metric::new("Lock waits on its indexes", t.lock_waits, Unit::Count));
                f.metrics.push(Metric::new("Time waiting", t.lock_wait_ms, Unit::Ms));
            }
            let target = qualified(Engine::Mssql, &schema, &table);
            let session = escalation_session_sql(&snapshot.server.database, azure);
            let lead = if historical {
                format!("None happened since the previous scan. If a statement still changes thousands of rows of {table} at once, change it into")
            } else {
                format!("Change the statement that touches thousands of rows of {table} into")
            };
            f.fix = Some(Fix {
                summary: format!(
                    "{lead} a loop over at most 1,000 rows at a time, each its own statement, so none takes enough locks to escalate. \
                     An UPDATE loop's filter must leave out the rows it already changed, or it never ends. Inside one transaction the loop still holds every row it changed until the commit, \
                     but other sessions then wait only for those rows, not for the whole table. \
                     To learn which statement it is, start the event session below ({ESCALATION_SESSION}): it keeps each escalation with its statement, in memory, at almost no cost."
                ),
                sql: Some(format!(
                    "-- One statement over every matching row escalates; 1,000 at a time doesn't\n\
                     WHILE 1 = 1\n\
                     BEGIN\n    \
                         DELETE TOP (1000) FROM {target} WHERE /* the same filter */;\n    \
                         IF @@ROWCOUNT = 0 BREAK;\n\
                     END;\n\
                     \n\
                     {session}"
                )),
                cost: None,
            });
            f.verify = vec![format!(
                "Scan again later: Culprit compares with this scan, and the finding says how many escalations on {table} happened since. None means it's fixed."
            )];
            f.caveats.push(
                "SQL Server counts escalations since the table's indexes were loaded into memory, usually since it started, and keeps no time for each."
                    .into(),
            );
            f
        })
        .collect();
    findings.sort_by(|a, b| b.impact.score.total_cmp(&a.impact.score).then(a.id.cmp(&b.id)));
    findings
}

// ── last-page-insert ───────────────────────────────────────────────────────────────────────────────

pub fn last_page_insert_info() -> CheckInfo {
    CheckInfo {
        id: "last-page-insert".into(),
        title: "Inserts that queue for the last page".into(),
        category: Category::Indexes,
        engines: vec![Engine::Mssql],
        requires: vec![],
        description: "Indexes keyed on an ever-increasing value (identity, sequence, insert time) where concurrent inserts wait for the latch on the last page (PAGELATCH_EX).".into(),
    }
}

pub fn last_page_insert(snapshot: &Snapshot) -> Vec<Finding> {
    snapshot
        .indexes
        .iter()
        .filter_map(|i| {
            let c = i.contention.as_ref()?;
            (c.sequential_key
                && c.page_latch_waits >= 1_000.0
                && c.page_latch_wait_ms >= 10_000.0
                && c.optimize_for_sequential_key != Some(true))
            .then_some((i, c))
        })
        .map(|(i, c)| {
            let table = index_label(i);
            let mut f = finding(
                "last-page-insert",
                &format!("{table}.{}", i.name),
                Category::Indexes,
                subject(SubjectKind::Index, format!("{table}.{}", i.name)),
            );
            f.title = format!("Inserts into {} queue for the last page of {}", i.table, i.name);
            f.summary = format!(
                "{} starts with a value that only grows ({}), so every insert goes to the same last page. Concurrent inserts wait for each other's latch on it (PAGELATCH_EX): {} waits, {} in all since the index was loaded. It gets worse with more concurrent writers.",
                i.name,
                i.columns.first().cloned().unwrap_or_default(),
                format::count(c.page_latch_waits),
                format::duration(c.page_latch_wait_ms)
            );
            // Counted since the index was loaded, with no time for each wait: graded on a rate measured over an hour or
            // more (ten minutes a day High, one Medium), never on the total.
            let rate = counter_rate_per_day(snapshot, c.page_latch_wait_ms);
            f.severity = since_start_severity(rate, 600_000.0, 60_000.0);
            if rate.is_none() {
                f.caveats.push(history_caveat(snapshot, "latch waits"));
            }
            severity_score(&mut f);
            f.impact.label = format!("{} latch waits", format::duration(c.page_latch_wait_ms));
            f.confidence = Confidence::Medium;
            f.metrics = vec![
                Metric::new("Page latch waits", c.page_latch_waits, Unit::Count),
                Metric::new("Time waiting", c.page_latch_wait_ms, Unit::Ms),
            ];
            if let Some(inserts) = c.leaf_inserts {
                f.metrics.push(Metric::new("Inserts", inserts, Unit::Count));
            }
            f.fix = Some(match c.optimize_for_sequential_key {
                Some(_) => Fix {
                    summary: "SQL Server 2019 can pace inserts into a sequential key so the page isn't handed back and forth: turn OPTIMIZE_FOR_SEQUENTIAL_KEY on for this index. It's a metadata change and takes effect at once.".into(),
                    sql: Some(format!(
                        "ALTER INDEX {} ON {} SET (OPTIMIZE_FOR_SEQUENTIAL_KEY = ON);",
                        quote(Engine::Mssql, &i.name),
                        qualified(Engine::Mssql, &i.schema, &i.table)
                    )),
                    cost: None,
                },
                None => Fix {
                    summary: "Before SQL Server 2019 the remedies change the key: lead the index with a value that spreads inserts (a hashed or partitioned key column), or batch inserts so fewer sessions compete.".into(),
                    sql: None,
                    cost: None,
                },
            });
            f.verify = vec!["Re-scan under the same load: page latch waits on this index should stop growing.".into()];
            f
        })
        .collect()
}

// ── untrusted-constraint ───────────────────────────────────────────────────────────────────────────

pub fn untrusted_constraint_info() -> CheckInfo {
    CheckInfo {
        id: "untrusted-constraint".into(),
        title: "Constraints the optimizer can't rely on".into(),
        category: Category::Maintenance,
        engines: vec![Engine::Mssql, Engine::Postgres],
        requires: vec![],
        description: "SQL Server foreign keys and check constraints added or re-enabled WITH NOCHECK (or switched off), and PostgreSQL check constraints added NOT VALID. MySQL and MariaDB don't use constraints when planning.".into(),
    }
}

/// Whether PostgreSQL's planner reads these CHECK constraints at all: constraint exclusion looks at inheritance
/// children only (constraint_exclusion = partition, the default), or at every table with constraint_exclusion = on.
fn planner_reads_checks(snapshot: &Snapshot, list: &[&ConstraintIssue]) -> bool {
    list.iter().any(|c| c.inheritance_child)
        || snapshot.setting("constraint_exclusion").is_some_and(|s| crate::config::is_on(&s.value))
}

pub fn untrusted_constraint(snapshot: &Snapshot) -> Vec<Finding> {
    let mssql = snapshot.server.engine == Engine::Mssql;
    let engine = snapshot.server.engine;
    let mut by_table: BTreeMap<(String, String), Vec<&ConstraintIssue>> = BTreeMap::new();
    for c in &snapshot.constraints {
        by_table.entry((c.schema.clone(), c.table.clone())).or_default().push(c);
    }
    by_table
        .into_iter()
        .map(|((schema, table), list)| {
            let name = format!("{schema}.{table}");
            let mut f = finding("untrusted-constraint", &name, Category::Maintenance, subject(SubjectKind::Table, &name));
            let disabled = list.iter().filter(|c| c.disabled).count();
            let kind = |c: &ConstraintIssue| match c.kind {
                ConstraintKind::ForeignKey => "foreign key",
                ConstraintKind::Check => "check constraint",
            };
            f.title = match (list.as_slice(), mssql) {
                ([one], _) if one.disabled => format!("The {} {} on {table} is switched off", kind(one), one.name),
                ([one], true) => format!("SQL Server doesn't trust the {} {} on {table}", kind(one), one.name),
                ([one], false) => format!("The check constraint {} on {table} was never validated", one.name),
                (_, true) => format!("SQL Server doesn't trust {} constraints on {table}", list.len()),
                (_, false) => format!("{} check constraints on {table} were never validated", list.len()),
            };
            let names = format::list(&list.iter().map(|c| c.name.clone()).collect::<Vec<_>>());
            f.summary = if mssql {
                format!(
                    "{names} {} created or re-enabled WITH NOCHECK, so existing rows were never checked against {}. The optimizer only relies on trusted constraints: it can't remove a join to the referenced table when no columns from it are used, or rule out values a check constraint excludes, so it plans work it could skip.{}",
                    if list.len() == 1 { "was" } else { "were" },
                    if list.len() == 1 { "it" } else { "them" },
                    if disabled > 0 { " A switched-off constraint doesn't check new rows either, so the data may already break it." } else { "" }
                )
            } else if planner_reads_checks(snapshot, &list) {
                format!(
                    "{names} {} added NOT VALID, so rows that existed then were never checked. Until it is validated the planner can't use it to skip the table when a query's filter contradicts it (constraint exclusion{}).",
                    if list.len() == 1 { "was" } else { "were" },
                    if list.iter().any(|c| c.inheritance_child) { ", which it applies to this inheritance child" } else { ", which constraint_exclusion = on applies to every table" }
                )
            } else {
                // The planner reads CHECK constraints only on inheritance children (constraint_exclusion = partition,
                // the default): here a NOT VALID one is about the data, and often deliberate.
                format!(
                    "{names} {} added NOT VALID: PostgreSQL checks new and changed rows against {} but never checked the rows that existed then, so some may break {}. That is often deliberate: NOT VALID adds a constraint without a long scan under lock. The planner doesn't read it here either way (constraint exclusion looks at CHECK constraints only on inheritance children unless constraint_exclusion is on). Validating proves the old rows, and lets later steps skip their own full scan: SET NOT NULL (12+) and ATTACH PARTITION when the constraint implies them.",
                    if list.len() == 1 { "was" } else { "were" },
                    if list.len() == 1 { "it" } else { "them" },
                    if list.len() == 1 { "it" } else { "them" }
                )
            };
            f.severity = if disabled > 0 { Severity::Medium } else { Severity::Low };
            severity_score(&mut f);
            f.impact.label = format!("{} untrusted", list.len());
            f.confidence = Confidence::High;
            f.metrics = vec![Metric::new("Constraints", list.len() as f64, Unit::Count)];
            let target = qualified(engine, &schema, &table);
            f.fix = Some(if mssql {
                Fix {
                    summary: format!(
                        "Check the existing rows so SQL Server trusts {} again. It reads the whole table and locks it while it runs: schedule it off-peak. It fails if a row breaks the constraint; fix those rows first.",
                        if list.len() == 1 { "it" } else { "them" }
                    ),
                    sql: Some(
                        list.iter()
                            .map(|c| format!("ALTER TABLE {target} WITH CHECK CHECK CONSTRAINT {};", quote(engine, &c.name)))
                            .collect::<Vec<_>>()
                            .join("\n"),
                    ),
                    cost: Some("Reads the whole table".into()),
                }
            } else {
                Fix {
                    summary: "Validate it: PostgreSQL scans the table but takes only a SHARE UPDATE EXCLUSIVE lock, so reads and writes continue. It fails if a row breaks the constraint; fix those rows first.".into(),
                    sql: Some(
                        list.iter()
                            .map(|c| format!("ALTER TABLE {target} VALIDATE CONSTRAINT {};", quote(engine, &c.name)))
                            .collect::<Vec<_>>()
                            .join("\n"),
                    ),
                    cost: Some("Reads the whole table".into()),
                }
            });
            f.verify = vec!["Re-scan: the constraint should no longer be listed.".into()];
            f
        })
        .collect()
}

// ── missing-histogram ──────────────────────────────────────────────────────────────────────────────

pub fn missing_histogram_info() -> CheckInfo {
    CheckInfo {
        id: "missing-histogram".into(),
        title: "Filter columns the optimizer has to guess about".into(),
        category: Category::Maintenance,
        engines: vec![Engine::Mysql],
        requires: vec![],
        description: "Columns that heavy queries filter on with no index leading on them and no histogram (MySQL 8 COLUMN_STATISTICS, MariaDB mysql.column_stats), so the optimizer assumes a fixed share of rows match.".into(),
    }
}

/// The columns a missing-histogram finding names, for de-duplication against index findings.
pub(crate) fn histogram_columns(f: &Finding) -> Vec<String> {
    f.metrics
        .iter()
        .find(|m| m.label == "Columns")
        .and_then(|m| m.text.as_deref())
        .map(|t| t.split(", ").map(str::to_string).collect())
        .unwrap_or_default()
}

/// The table name a plan node reads, without schema or quotes.
fn node_table(object: &str) -> String {
    object.rsplit('.').next().unwrap_or(object).trim_matches(|c| matches!(c, '`' | '"' | '[' | ']')).to_string()
}

/// Per table: the columns the optimizer guesses about, the guessed share EXPLAIN showed, and the queries.
type Guessed<'a> = (Vec<String>, Option<f64>, Vec<&'a QueryStat>);

pub fn missing_histogram(snapshot: &Snapshot) -> Vec<Finding> {
    let Some(histograms) = snapshot.histograms.as_ref() else { return Vec::new() };
    let mariadb = is_mariadb(snapshot);
    let has_histogram = |table: &str, column: &str| {
        histograms.iter().any(|h| h.table.eq_ignore_ascii_case(table) && h.column.eq_ignore_ascii_case(column))
    };
    let leads_index = |table: &str, column: &str| {
        snapshot.indexes.iter().any(|i| {
            i.table.eq_ignore_ascii_case(table) && i.key_names().first().is_some_and(|c| c.eq_ignore_ascii_case(column))
        })
    };
    let mut by_table: BTreeMap<(String, String), Guessed> = BTreeMap::new();
    let mut heavy: Vec<&QueryStat> = app_queries(snapshot).filter(|q| q.plan.is_some()).collect();
    heavy.sort_by(|a, b| b.total_time_ms.total_cmp(&a.total_time_ms));
    for q in heavy.into_iter().take(20) {
        let Some(plan) = q.plan.as_ref() else { continue };
        for node in plan.nodes() {
            let (Some(object), Some(predicate)) = (node.object.as_deref(), node.predicate.as_deref()) else { continue };
            let table = node_table(object);
            let Some(stat) = snapshot.tables.iter().find(|t| t.name.eq_ignore_ascii_case(&table)) else { continue };
            if stat.rows.unwrap_or(0.0) < 1_000.0 {
                continue;
            }
            // MySQL prints typed literals as TIMESTAMP'…' and DATE'…'; they are values like any quoted one.
            let predicate = predicate.replace('`', "").replace("TIMESTAMP'", "'").replace("DATE'", "'");
            let columns = predicate_columns(&predicate);
            let guessed: Vec<String> = columns
                .equality
                .iter()
                .chain(columns.range.iter())
                .filter(|c| !leads_index(&table, c) && !has_histogram(&table, c))
                .cloned()
                .collect();
            if guessed.is_empty() {
                continue;
            }
            let entry = by_table.entry((stat.schema.clone(), stat.name.clone())).or_default();
            for c in guessed {
                if !entry.0.iter().any(|e| e.eq_ignore_ascii_case(&c)) {
                    entry.0.push(c);
                }
            }
            // MySQL's fixed guesses: 10% for =, 33.33% for ranges, 11.11% for BETWEEN. MariaDB without statistics for
            // the column assumes the condition filters nothing: 100%.
            let guesses: &[f64] = if mariadb { &[100.0] } else { &[10.0, 33.33, 11.11] };
            if let Some(filtered) = node.props.get("filtered").and_then(|v| v.parse::<f64>().ok())
                && guesses.iter().any(|g| (filtered - g).abs() < 0.01)
            {
                entry.1 = Some(filtered);
            }
            if !entry.2.iter().any(|e| e.id == q.id) {
                entry.2.push(q);
            }
        }
    }
    let engine = engine_name(snapshot);
    by_table
        .into_iter()
        .map(|((schema, table), (columns, guess, queries))| {
            let name = format!("{schema}.{table}");
            let mut f = finding("missing-histogram", &name, Category::Maintenance, subject(SubjectKind::Table, &name));
            let list = format::list(&columns);
            f.title = format!(
                "{engine} guesses how many {table} rows match {list}: no {}",
                if mariadb { "column statistics" } else { "histogram" }
            );
            let who = match queries.len() {
                1 => format!("{} filters", capitalize(&query_handle(queries[0]))),
                n => format!("{n} of the heaviest queries filter"),
            };
            f.summary = if mariadb {
                // MariaDB doesn't guess a share: without statistics for a column it counts the condition as keeping
                // every row.
                format!(
                    "{who} {table} on {list}, which no index starts with and no statistics describe, so {engine} assumes the condition keeps every row{}. When it actually keeps few, common for status flags and skewed values, it picks the wrong join order or access path. Column statistics cost nothing at query time and change only when you run ANALYZE again.",
                    guess.map(|g| format!(" (EXPLAIN shows filtered = {g:.0}%)")).unwrap_or_default()
                )
            } else {
                format!(
                    "{who} {table} on {list}, which no index starts with and no histogram describes, so {engine} assumes a fixed share of rows match (10% for =, a third for ranges){}. When the real share is far off, common for status flags and skewed values, it picks the wrong join order or access path. A histogram costs nothing at query time and changes only when you run ANALYZE again.",
                    guess.map(|g| format!("; EXPLAIN shows filtered = {g:.2}%, one of those guesses")).unwrap_or_default()
                )
            };
            f.queries = query_refs(&queries);
            let impact = query_impact(snapshot, &queries);
            let share = impact.as_ref().map(|i| i.share).unwrap_or(0.0);
            if let Some(impact) = impact {
                apply_query_impact(&mut f, &impact);
            }
            f.severity = if guess.is_some() && share >= 0.05 { Severity::Medium } else { Severity::Low };
            severity_score(&mut f);
            f.confidence = if guess.is_some() { Confidence::High } else { Confidence::Medium };
            f.metrics = vec![Metric::text("Columns", columns.join(", "))];
            if let Some(g) = guess {
                f.metrics.push(Metric::new("EXPLAIN filtered", g / 100.0, Unit::Ratio));
            }
            let target = qualified(Engine::Mysql, &schema, &table);
            let quoted = columns.iter().map(|c| quote(Engine::Mysql, c)).collect::<Vec<_>>().join(", ");
            f.fix = Some(if mariadb {
                Fix {
                    summary: "Collect a histogram for these columns. MariaDB uses it when use_stat_tables is PREFERABLY_FOR_QUERIES and optimizer_use_condition_selectivity is 4 or more, the defaults since 10.4. Run it again after large data changes.".into(),
                    sql: Some(format!("ANALYZE TABLE {target} PERSISTENT FOR COLUMNS ({quoted}) INDEXES ();")),
                    cost: Some("Reads the whole table".into()),
                }
            } else {
                Fix {
                    summary: "Build a histogram for these columns. It reads the table once; MySQL doesn't refresh it on its own, so run it again after large data changes.".into(),
                    sql: Some(format!("ANALYZE TABLE {target} UPDATE HISTOGRAM ON {quoted} WITH 64 BUCKETS;")),
                    cost: Some("Reads the table".into()),
                }
            });
            f.verify = vec!["Re-scan: the columns should have histograms, and EXPLAIN ANALYZE should show estimated rows close to actual for that filter.".into()];
            f
        })
        .collect()
}

// ── hot-updates ────────────────────────────────────────────────────────────────────────────────────

pub fn hot_updates_info() -> CheckInfo {
    CheckInfo {
        id: "hot-updates".into(),
        title: "Updates that rewrite every index".into(),
        category: Category::Maintenance,
        engines: vec![Engine::Postgres],
        requires: vec![],
        description: "Tables where few updates are HOT (heap-only): each other update writes a new entry in every index and leaves cleanup for VACUUM. Says whether pages are too full (fillfactor) or an indexed column changes (PostgreSQL 16+ tells them apart).".into(),
    }
}

/// Non-HOT updates a day to one table that make hot-updates Medium.
const COLD_UPDATES_MEDIUM_PER_DAY: f64 = 1_000_000.0;

pub fn hot_updates(snapshot: &Snapshot) -> Vec<Finding> {
    let mut findings: Vec<Finding> = snapshot
        .tables
        .iter()
        .filter_map(|t| {
            let s = t.storage.as_ref()?;
            let updates = t.updates.filter(|u| *u >= 10_000.0)?;
            let hot = s.hot_updates?;
            let ratio = hot / updates;
            // A table without indexes has no entries to rewrite: a non-HOT update there costs what a HOT one does.
            let indexes = snapshot.indexes_on(&t.schema, &t.name).count();
            (ratio < 0.5 && indexes > 0).then_some((t, s, updates, hot, ratio, indexes))
        })
        .map(|(t, s, updates, hot, ratio, indexes)| {
            let name = t.qualified();
            let mut f = finding("hot-updates", &name, Category::Maintenance, subject(SubjectKind::Table, &name));
            let cold = updates - hot;
            f.title = if hot < 1.0 {
                format!("No update to {} is HOT: each one rewrites every index", t.name)
            } else {
                format!("Only {} of updates to {} are HOT: the rest rewrite every index", format::percent(ratio), t.name)
            };
            let every_index = match indexes {
                1 => "its only index".to_string(),
                2 => "both of its indexes".to_string(),
                n => format!("all {n} of its indexes"),
            };
            let fillfactor = s.fillfactor.unwrap_or(100.0);
            let moved = s.new_page_updates.map(|n| (n / cold.max(1.0)).min(1.0));
            let cause = match moved {
                Some(m) if m >= 0.5 => format!(
                    " {} of them moved to another page because their own was full (fillfactor {fillfactor:.0}).",
                    format::percent(m)
                ),
                Some(m) => format!(
                    " Only {} of them lacked room on the page; the rest changed an indexed column, which rules HOT out.",
                    format::percent(m)
                ),
                None => " Either the pages are full (fillfactor {ff}) or the updates change an indexed column.".replace("{ff}", &format!("{fillfactor:.0}")),
            };
            let how_many = if hot < 1.0 {
                format!("None of {}'s {} updates were", t.name, format::count(updates))
            } else {
                format!("{} of {}'s {} updates weren't", format::count(cold), t.name, format::count(updates))
            };
            f.summary = format!(
                "An update that changes no indexed column and finds room on the same page is HOT: it writes no index entries and its old version is cleaned up without VACUUM. {how_many}, so each wrote a new entry in {every_index}.{cause}"
            );
            // The counts run from the database's last statistics reset, which PostgreSQL 15+ leaves empty until the first
            // one: then their age is unknown (the counters outlive restarts), and so is any rate. A one-time backfill
            // keeps the share low long after it ran, so only a measured rate raises it.
            let hours = s.counter_hours.filter(|h| *h >= culprit_core::RATE_MIN_HOURS);
            let per_day = hours.map(|h| cold / h * 24.0);
            f.severity = since_start_severity(per_day, f64::INFINITY, COLD_UPDATES_MEDIUM_PER_DAY);
            if per_day.is_none_or(|d| d < COLD_UPDATES_MEDIUM_PER_DAY) {
                f.caveats.push(match s.counter_hours {
                    Some(h) => format!(
                        "The counts cover the {} since the statistics were last reset: one large update or backfill in that time keeps the share low long after it ran. Compare how n_tup_hot_upd grows against n_tup_upd over a few days before changing the table.",
                        culprit_core::span_words(h)
                    ),
                    None => "How long these counts have run isn't known (PostgreSQL 15+ records no start until its statistics are first reset, and the counts outlive restarts): one large update or backfill long ago would keep the share low. Compare how n_tup_hot_upd grows against n_tup_upd over a few days before changing the table.".into(),
                });
            }
            severity_score(&mut f);
            f.impact.label = format!("{} HOT", format::percent(ratio));
            f.confidence = if moved.is_some() { Confidence::High } else { Confidence::Medium };
            f.metrics = vec![
                Metric::new("Updates", updates, Unit::Count),
                Metric::new("HOT", ratio, Unit::Ratio),
                Metric::new("Indexes", indexes as f64, Unit::Count),
                Metric::new("Fillfactor", fillfactor, Unit::Count),
            ];
            let room = moved.is_none_or(|m| m >= 0.5) && fillfactor >= 100.0;
            f.fix = Some(Fix {
                summary: if room {
                    "Leave room on each page so updated rows fit next to the old version: a fillfactor of 85–90 is typical for tables updated often. It applies to pages written from now on; existing pages get the room as rows move (pg_repack rewrites the table online if you want it at once). If an index covers a column that changes on every update (updated_at, status, counters), dropping it makes those updates HOT too.".into()
                } else {
                    "The updates change an indexed column. Find the index on a column that changes often (updated_at, status, counters) and drop it if reads don't need it; every update that touches it can then be HOT.".into()
                },
                sql: room.then(|| format!("ALTER TABLE {} SET (fillfactor = 85);", qualified(Engine::Postgres, &t.schema, &t.name))),
                cost: None,
            });
            f.verify = vec!["Re-scan a few days later: the HOT share counts since the last statistics reset, so compare how n_tup_hot_upd grows against n_tup_upd.".into()];
            f
        })
        .collect();
    findings.sort_by(|a, b| b.impact.score.total_cmp(&a.impact.score));
    findings.truncate(5);
    findings
}

// ── table-bloat ────────────────────────────────────────────────────────────────────────────────────

pub fn table_bloat_info() -> CheckInfo {
    CheckInfo {
        id: "table-bloat".into(),
        title: "Tables that are mostly empty space".into(),
        category: Category::Maintenance,
        engines: vec![Engine::Postgres],
        requires: vec![],
        description: "Tables whose pages hold far fewer rows than they could, estimated from row counts and column widths in pg_stats (no extension needed). Every scan still reads the empty space.".into(),
    }
}

fn table_heap_bytes(t: &TableStat) -> Option<f64> {
    t.data_bytes.or(t.total_bytes).filter(|b| *b > 0.0)
}

/// Dead rows explain the estimated empty space when their share of the row versions is at least this part of its
/// share: the estimate is good to tens of percent.
pub(crate) const DEAD_EXPLAINS_BLOAT: f64 = 0.9;

/// The share of a table's row versions that are dead (not yet vacuumed), when the engine counts them.
pub(crate) fn dead_share(t: &TableStat) -> Option<f64> {
    let dead = t.dead_rows.filter(|d| *d > 0.0)?;
    Some(dead / (dead + t.rows.unwrap_or(0.0).max(0.0)))
}

pub fn table_bloat(snapshot: &Snapshot) -> Vec<Finding> {
    let mut findings: Vec<Finding> = snapshot
        .tables
        .iter()
        .filter_map(|t| {
            let bloat = t.storage.as_ref()?.bloat_bytes?;
            let size = table_heap_bytes(t)?;
            let share = (bloat / size).min(1.0);
            ((bloat >= 100.0 * MB && share >= 0.4) || (bloat >= GB && share >= 0.3)).then_some((t, bloat, size, share))
        })
        .map(|(t, bloat, size, share)| {
            let name = t.qualified();
            let mut f = finding("table-bloat", &name, Category::Maintenance, subject(SubjectKind::Table, &name));
            f.title = format!("{} is about {} empty space (≈ {})", t.name, format::percent(share), format::bytes(bloat));
            // The estimate counts every byte beyond the live rows, dead rows VACUUM hasn't cleared yet included. When
            // they account for it, VACUUM is the fix, not a rewrite.
            let dead = dead_share(t).filter(|d| *d >= share * DEAD_EXPLAINS_BLOAT);
            f.summary = format!(
                "From its row count and average row width, {}'s rows need about {}; it takes {}. New rows reuse freed space, but every scan still reads the empty pages, and the cache holds them. This is an estimate from statistics, good to tens of percent; pgstattuple measures it exactly.{}",
                t.name,
                format::bytes((size - bloat).max(0.0)),
                format::bytes(size),
                dead.map(|d| format!(
                    " {} of its row versions are dead rows VACUUM hasn't cleared yet, enough to be most of this space: vacuum it first and estimate again before rewriting it.",
                    format::percent(d)
                ))
                .unwrap_or_default()
            );
            f.severity = if bloat >= GB && dead.is_none() { Severity::Medium } else { Severity::Low };
            severity_score(&mut f);
            f.impact.label = format!("≈ {} empty", format::bytes(bloat));
            f.confidence = Confidence::Medium;
            f.metrics = vec![
                Metric::new("Empty space (estimated)", bloat, Unit::Bytes),
                Metric::new("Table size", size, Unit::Bytes),
                Metric::new("Share", share, Unit::Ratio),
            ];
            let target = qualified(Engine::Postgres, &t.schema, &t.name);
            f.fix = Some(if dead.is_some() {
                Fix {
                    summary: "Vacuum it: VACUUM clears the dead rows without blocking reads or writes, and new rows then reuse their space. Make autovacuum visit it sooner so they don't pile up again. Rewrite the table (pg_repack online, VACUUM FULL offline) only if the estimate stays high after a vacuum.".into(),
                    sql: Some(format!("VACUUM (ANALYZE) {target};")),
                    cost: Some("VACUUM doesn't block reads or writes".into()),
                }
            } else {
                Fix {
                    summary: "Measure it exactly first. To give the space back, rewrite the table: pg_repack does it online; VACUUM FULL does it while blocking all reads and writes. If the table keeps growing, make autovacuum visit it more often so freed space is reused sooner.".into(),
                    sql: Some(format!(
                        "-- Exact numbers (needs the pgstattuple extension)\nSELECT * FROM pgstattuple('{}');\n-- Rewrite offline: blocks reads and writes until it finishes\n-- VACUUM (FULL, ANALYZE) {target};",
                        target.replace('\'', "''")
                    )),
                    cost: Some("Rewrites the table".into()),
                }
            });
            f.verify = vec!["Re-scan after the rewrite: the estimate should fall under 20%.".into()];
            f
        })
        .collect();
    findings.sort_by(|a, b| b.impact.score.total_cmp(&a.impact.score));
    findings.truncate(5);
    findings
}

// ── large-rows ─────────────────────────────────────────────────────────────────────────────────────

pub fn large_rows_info() -> CheckInfo {
    CheckInfo {
        id: "large-rows".into(),
        title: "Tables whose rows are mostly large values".into(),
        category: Category::Maintenance,
        engines: vec![Engine::Mssql, Engine::Postgres],
        requires: vec![],
        description: "Tables whose rows average many kilobytes, most of it in large values kept apart from the rows (SQL Server's LOB pages, PostgreSQL's TOAST): every statement that reads whole rows pulls them in, every write that sets them writes them again. From the catalog's page counts; no data is read.".into(),
    }
}

/// A row this big on average is mostly a payload, not a record.
const LARGE_ROW_BYTES: f64 = 16.0 * 1024.0;
/// Below this the large values cost too little to be worth a finding.
const LARGE_ROWS_TABLE_BYTES: f64 = 256.0 * MB;
/// The share of a table's data in large-value pages that makes them, not the rows, its size.
const LARGE_VALUE_SHARE: f64 = 0.5;
/// Pages a statement reads a run, and for each row it returns or changes, that mean it pulls whole rows: 16 pages is
/// 128 KB a row, far more than any index or record needs.
const WHOLE_ROW_PAGES: f64 = 50.0;
const WHOLE_ROW_PAGES_PER_ROW: f64 = 16.0;
/// Whole-row reads this often a day cost more than the table's size alone.
const WHOLE_ROW_RUNS_PER_DAY: f64 = 1_000.0;
/// Growth that matters: a tenth of the table a day, and at least this much.
const FAST_GROWTH_SHARE: f64 = 0.1;
const FAST_GROWTH_BYTES: f64 = 100.0 * MB;
/// A table this big is costly to keep, back up and restore whatever reads it.
const BIG_TABLE_BYTES: f64 = 10.0 * GB;
/// A data page, on SQL Server and PostgreSQL.
const PAGE_BYTES: f64 = 8_192.0;

/// A table's own data and the part of it in large-value pages: its in-row, large-value and row-overflow pages when the
/// engine splits them. `None` when it doesn't.
fn split_bytes(t: &TableStat) -> Option<(f64, f64)> {
    let large = t.lob_bytes? + t.overflow_bytes.unwrap_or(0.0);
    let total = match t.in_row_bytes {
        Some(in_row) => in_row + large,
        None => t.data_bytes?,
    };
    (total > 0.0).then_some((total, large))
}

/// A column type whose values the engine keeps apart from the row once they're large.
fn large_value_type(engine: Engine, data_type: &str) -> bool {
    let t = data_type.to_ascii_lowercase();
    match engine {
        Engine::Mssql => {
            t.ends_with("(max)") || matches!(t.as_str(), "xml" | "text" | "ntext" | "image" | "geography" | "geometry")
        }
        Engine::Postgres => {
            matches!(t.as_str(), "text" | "bytea" | "json" | "jsonb" | "xml" | "varchar" | "character varying")
        }
        Engine::Mysql => false,
    }
}

/// Statements that read or rewrite whole rows of `t`: many pages a run, and many for each row they return or change.
/// One-off statements (a single run) aren't the application's. A scan of the rows' own pages reads about as many
/// pages as the table keeps in-row, and returns few rows (a count, a filter on an unindexed column): many pages a row
/// without a single large value. A statement reading about that many pages a run is left out; one reading far fewer
/// (a few rows by key) or more (the large values too) isn't.
fn whole_row_statements<'a>(snapshot: &'a Snapshot, t: &TableStat, size: f64, large: f64) -> Vec<&'a QueryStat> {
    let in_row_pages = t.in_row_bytes.unwrap_or(size - large).max(0.0) / PAGE_BYTES;
    let scans_in_row = |pages: f64| pages >= in_row_pages * 0.5 && pages <= in_row_pages * 1.2;
    let mut found: Vec<&QueryStat> = queries_touching(snapshot, &t.schema, &t.name)
        .into_iter()
        .filter(|q| {
            if is_monitoring_query(q) || q.executions < 2.0 {
                return false;
            }
            let Some(reads) = q.logical_reads else { return false };
            let per_run = reads / q.executions;
            let rows = q.rows.map(|r| r / q.executions).unwrap_or(1.0).max(1.0);
            per_run >= WHOLE_ROW_PAGES && per_run / rows >= WHOLE_ROW_PAGES_PER_ROW && !scans_in_row(per_run)
        })
        .collect();
    found.sort_by(|a, b| b.logical_reads.unwrap_or(0.0).total_cmp(&a.logical_reads.unwrap_or(0.0)));
    found.truncate(5);
    found
}

/// Whether a statement writes rows rather than reads them: its first statement keyword, past any parameter
/// declarations and comments the engine kept with the text.
fn writes_rows(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    let first = ["select", "insert", "update", "delete", "merge"]
        .into_iter()
        .filter_map(|k| {
            lower
                .match_indices(k)
                .find(|(at, _)| {
                    let before = lower[..*at].chars().next_back();
                    let after = lower[at + k.len()..].chars().next();
                    !before.is_some_and(|c| c.is_ascii_alphanumeric() || c == '_')
                        && !after.is_some_and(|c| c.is_ascii_alphanumeric() || c == '_')
                })
                .map(|(at, _)| (at, k))
        })
        .min_by_key(|(at, _)| *at);
    first.is_some_and(|(_, k)| k != "select")
}

/// The rows a table holds, as far as its size per row goes. PostgreSQL's live-row counter starts again from zero after
/// a statistics reset, a crash or a failover, while the planner's count (reltuples) doesn't: the larger of the two.
fn table_rows(t: &TableStat) -> Option<f64> {
    let planner = t.storage.as_ref().and_then(|s| s.planner_rows);
    match (t.rows, planner) {
        (Some(live), Some(planner)) => Some(live.max(planner)),
        (rows, planner) => rows.or(planner),
    }
    .filter(|r| *r >= 1.0)
}

pub fn large_rows(snapshot: &Snapshot) -> Vec<Finding> {
    let mut findings: Vec<Finding> = snapshot
        .tables
        .iter()
        .filter_map(|t| {
            let rows = table_rows(t)?;
            let (size, large) = split_bytes(t)?;
            // Dead row versions (not yet vacuumed) keep their large values on disk too: a queue whose deleted payloads
            // wait for VACUUM isn't a table of large rows. Per version, they must still be large.
            let versions = rows + t.dead_rows.unwrap_or(0.0).max(0.0);
            (size >= LARGE_ROWS_TABLE_BYTES && size / versions >= LARGE_ROW_BYTES && large / size >= LARGE_VALUE_SHARE)
                .then(|| large_rows_finding(snapshot, t, rows, size, large))
        })
        .collect();
    findings.sort_by(|a, b| b.impact.score.total_cmp(&a.impact.score));
    findings.truncate(5);
    findings
}

fn large_rows_finding(snapshot: &Snapshot, t: &TableStat, rows: f64, size: f64, large: f64) -> Finding {
    let engine = snapshot.server.engine;
    let name = t.qualified();
    let average = size / rows;
    let share = large / size;
    let mut f = finding("large-rows", &name, Category::Maintenance, subject(SubjectKind::Table, &name));
    f.title = format!(
        "{} holds {} in {} rows: {} a row, mostly large values",
        t.name,
        format::bytes(size),
        format::count(rows),
        format::bytes(average)
    );

    // The columns that can hold them, from the catalog: nothing is sampled.
    let mut columns: Vec<_> = snapshot
        .columns
        .iter()
        .filter(|c| {
            c.schema.eq_ignore_ascii_case(&t.schema)
                && c.table.eq_ignore_ascii_case(&t.name)
                && large_value_type(engine, &c.data_type)
        })
        .collect();
    columns.sort_by_key(|c| c.ordinal);
    let column_names: Vec<String> = columns.iter().take(6).map(|c| c.name.clone()).collect();
    // Which of them hold the bytes isn't known without reading the data: these are the columns that can.
    let held_in = match (column_names.as_slice(), columns.len()) {
        ([], _) => String::new(),
        ([one], 1) => format!(" (the column that can hold them: {one})"),
        (names, n) if n > names.len() => {
            format!(" (columns that can hold them: {} and {} more)", names.join(", "), n - names.len())
        }
        (names, _) => format!(" (columns that can hold them: {})", format::list(names)),
    };
    let mut summary = format!(
        "The {} rows of {} take {}: {} each on average, and {} of that is large values kept apart from the rows{held_in}. Every statement that reads whole rows pulls them in, every insert or update that sets them writes them again, and they keep their space until the rows are deleted and purged.",
        format::count(rows),
        t.name,
        format::bytes(size),
        format::bytes(average),
        share_words(share)
    );
    if engine == Engine::Postgres {
        summary.push_str(" PostgreSQL has already compressed them: these are the sizes on disk.");
    }

    // Dead versions hold their values until VACUUM: say how much of the table they are.
    if let Some(dead) = dead_share(t).filter(|d| *d >= 0.2) {
        summary.push_str(&format!(
            " {} of its row versions are dead rows not yet vacuumed, and their large values still take space until VACUUM clears them.",
            format::percent(dead)
        ));
    }

    // Statements reading whole rows: what makes the size cost on every run, not just on disk.
    let statements = whole_row_statements(snapshot, t, size, large);
    let runs: f64 = statements.iter().map(|q| q.executions).sum();
    let reads: f64 = statements.iter().filter_map(|q| q.logical_reads).sum();
    let hot = window_at_least(snapshot, runs, WHOLE_ROW_RUNS_PER_DAY);
    let runs_a_day = snapshot.workload.as_ref().and_then(|w| w.per_day_measured(runs));
    if !statements.is_empty() {
        let pages = (reads / runs.max(1.0)).round();
        let writes = statements.iter().filter(|q| writes_rows(&q.text)).count();
        let one = statements.len() == 1;
        let verb = match (writes, one) {
            (0, true) => "reads them",
            (0, false) => "read them",
            (1, true) => "writes them",
            (w, false) if w == statements.len() => "write them",
            _ => "read or write them",
        };
        summary.push_str(&format!(
            " {} below {verb} {}, going through about {} pages ({}) a run.",
            if one { "The statement".to_string() } else { format!("The {} statements", statements.len()) },
            runs_phrase(snapshot, runs),
            format::count(pages),
            format::bytes(pages * 8192.0)
        ));
    }

    // Growth since the previous scan, when there is one.
    let growth = snapshot.sizes_before.as_ref().and_then(|b| {
        let then = b.bytes(&name)?;
        let days = days_between(&b.taken_at, &snapshot.taken_at).filter(|d| *d >= 1.0 / 24.0)?;
        Some((b, size - then, days))
    });
    let mut fast = false;
    if let Some((before, delta, days)) = growth {
        let when = crate::trends::when_phrase(&before.taken_at);
        let a_day = delta / days;
        fast = a_day >= FAST_GROWTH_BYTES && a_day >= FAST_GROWTH_SHARE * size;
        summary.push(' ');
        summary.push_str(&if delta >= MB && days >= 20.0 / 24.0 {
            format!(
                "It grew by {} since the previous scan {when}, about {} a day.",
                format::bytes(delta),
                format::bytes(a_day)
            )
        } else if delta >= MB {
            format!(
                "It grew by {} in the {} since the previous scan {when}.",
                format::bytes(delta),
                culprit_core::span_words(days * 24.0)
            )
        } else if delta <= -MB {
            format!("It's {} smaller than at the previous scan {when}.", format::bytes(-delta))
        } else {
            format!("It hasn't grown since the previous scan {when}.")
        });
        if delta >= MB && days >= 20.0 / 24.0 {
            f.metrics.push(Metric::new("Growth a day", a_day, Unit::Bytes));
        }
    }
    f.summary = summary;

    let weight = usize::from(hot) + usize::from(fast) + usize::from(size >= BIG_TABLE_BYTES);
    f.severity = match weight {
        0 => Severity::Low,
        1 => Severity::Medium,
        _ => Severity::High,
    };
    f.confidence = Confidence::High;
    if let Some(impact) = query_impact(snapshot, &statements) {
        f.impact.db_time_share = Some(impact.share);
        f.impact.time_ms_per_day = impact.per_day_ms;
    }
    severity_score(&mut f);
    f.impact.label = format!("{}, {} a row", format::bytes(size), format::bytes(average));
    let mut metrics = vec![
        Metric::new("Table data", size, Unit::Bytes),
        Metric::new("Rows", rows, Unit::Rows),
        Metric::new("Average row", average, Unit::Bytes),
        Metric::new("In large-value pages", share, Unit::Ratio),
    ];
    if let Some(runs_a_day) = runs_a_day.filter(|_| !statements.is_empty()) {
        metrics.push(Metric::new("Whole-row statements a day", runs_a_day, Unit::Count));
    }
    metrics.append(&mut f.metrics);
    f.metrics = metrics;
    f.queries = query_refs(&statements);
    f.last_seen = statements.iter().filter_map(|q| q.last_seen.clone()).max();

    let compress = match engine {
        Engine::Postgres => {
            "LZ4 compresses them faster than the default (ALTER TABLE … ALTER COLUMN … SET COMPRESSION lz4, PostgreSQL 14+), or keep them in object storage and store a reference"
        }
        _ => {
            "COMPRESS() them as they're written (GZip; DECOMPRESS() reads them back), or keep them in object storage and store a reference"
        }
    };
    f.fix = Some(Fix {
        summary: format!(
            "Read only the columns a statement needs: a SELECT * or an ORM loading whole rows pulls every large value. Keep a value that many rows repeat in one place and point to it. Clear values a row no longer needs once its work is done. Check how long deleted rows are kept before they're purged: until then they keep their values. For values that must stay, {compress}."
        ),
        sql: None,
        cost: None,
    });
    f.verify = vec![
        "Re-scan after the change: the table's average row and its share of large values should drop, and the statements below should read fewer pages a run.".into(),
        "A later scan says whether it still grows: it compares the table's size with this scan's.".into(),
    ];
    f.caveats = vec!["Sizes are the pages the table has reserved, from the catalog: they include space allocated but not yet filled.".into()];
    f
}

// ── replica-lag ────────────────────────────────────────────────────────────────────────────────────

pub fn replica_lag_info() -> CheckInfo {
    CheckInfo {
        id: "replica-lag".into(),
        title: "Replicas that fall behind".into(),
        category: Category::Configuration,
        engines: vec![Engine::Mssql, Engine::Postgres, Engine::Mysql],
        requires: vec![],
        description: "Replicas far behind the primary or stopped, and PostgreSQL replication slots that keep WAL for a consumer that's gone: pg_stat_replication, availability group replica states, SHOW REPLICA STATUS.".into(),
    }
}

/// How a replica, a consumer or this server stands, as replica-lag reads it.
#[derive(Debug, PartialEq)]
enum ReplicaStatus {
    /// Replication failed or can't connect: the copy stopped with an error, or a standby meant to stream has no
    /// receiver.
    Stopped,
    /// Both MySQL replication threads are off and nothing failed: someone ran STOP REPLICA.
    StoppedByHand,
    /// The same on a server that takes writes: a former replica promoted to primary, its old configuration left behind.
    LeftConfigured,
    /// Behind by this many seconds beyond any configured delay (or by bytes, when that is all the engine says).
    Behind,
    Fine,
}

fn replica_status(snapshot: &Snapshot, r: &Replica) -> ReplicaStatus {
    // Group Replication's own channels idle by design; the group reports its members elsewhere.
    if r.name.to_ascii_lowercase().starts_with("group_replication_") {
        return ReplicaStatus::Fine;
    }
    let state = r.state.as_deref().unwrap_or("");
    if state.starts_with("configured, not running") {
        return ReplicaStatus::LeftConfigured;
    }
    if state.starts_with("stopped by hand") {
        return ReplicaStatus::StoppedByHand;
    }
    if state.contains("stopped") || state == "not receiving" || state.eq_ignore_ascii_case("NOT SYNCHRONIZING") {
        return ReplicaStatus::Stopped;
    }
    let seconds = replica_lag_seconds(snapshot, r);
    // A configured delay keeps WAL waiting on purpose: only the seconds beyond it say whether it is behind.
    let bytes = r.lag_bytes.filter(|_| r.delay_seconds.is_none() && seconds.is_none_or(|s| s > 0.0));
    if seconds.is_some_and(|s| s >= 30.0) || bytes.is_some_and(|b| b >= GB) {
        ReplicaStatus::Behind
    } else {
        ReplicaStatus::Fine
    }
}

/// Seconds behind beyond any configured delay. A PostgreSQL standby with nothing received left to replay is caught up,
/// whatever the time since its last replayed commit (older scans reported that time as lag).
fn replica_lag_seconds(snapshot: &Snapshot, r: &Replica) -> Option<f64> {
    if snapshot.server.engine == Engine::Postgres && r.role == ReplicaRole::ThisServer && r.lag_bytes == Some(0.0) {
        return Some(0.0);
    }
    r.lag_seconds
}

pub fn replica_lag(snapshot: &Snapshot) -> Vec<Finding> {
    let Some(p) = snapshot.pressure.as_ref() else { return Vec::new() };
    p.replicas
        .iter()
        .filter_map(|r| match r.role {
            ReplicaRole::Slot => slot_finding(snapshot, r),
            _ => replica_finding(snapshot, r),
        })
        .collect()
}

/// A replica, a logical consumer or this server, when it is stopped or behind.
fn replica_finding(snapshot: &Snapshot, r: &Replica) -> Option<Finding> {
    let status = replica_status(snapshot, r);
    if status == ReplicaStatus::Fine {
        return None;
    }
    let pg = snapshot.server.engine == Engine::Postgres;
    let mysql = snapshot.server.engine == Engine::Mysql;
    let consumer = r.is_logical();
    let seconds = replica_lag_seconds(snapshot, r);
    let mut f = finding("replica-lag", &r.name, Category::Configuration, subject(SubjectKind::Database, &r.name));
    let lag = seconds
        .map(|s| format::duration(s * 1000.0))
        .or_else(|| r.lag_bytes.map(|b| format!("{} of log", format::bytes(b))))
        .unwrap_or_else(|| "behind".into());
    let state = r.state.clone().unwrap_or_default();
    match status {
        ReplicaStatus::LeftConfigured => {
            f.title = "This server is still set up as a replica, but replication isn't running".into();
            f.summary = "Both replication threads are off and no error is recorded, so someone stopped them, and this server takes writes (read_only is off). A former replica promoted to primary looks like this: its old replication settings stay behind, and starting them by mistake would apply another server's changes on top of this one's.".into();
            f.severity = Severity::Low;
            f.fix = Some(Fix {
                summary: "If it is no longer a replica, remove the configuration with RESET REPLICA ALL (RESET SLAVE ALL before MySQL 8.0.22 and MariaDB 10.5.1). If it should replicate, make it read-only and START REPLICA.".into(),
                sql: None,
                cost: None,
            });
        }
        ReplicaStatus::StoppedByHand => {
            f.title = "Replication to this server was stopped by hand".into();
            f.summary = "Both replication threads are off and no error is recorded: someone ran STOP REPLICA, for a backup or maintenance perhaps. While it stays stopped the copy gets older by the minute, and failing over to it would lose the changes since.".into();
            f.severity = Severity::Medium;
            f.fix = Some(Fix {
                summary: "If it should replicate, START REPLICA and let it catch up before relying on it; if it is no longer a replica, RESET REPLICA ALL removes the configuration.".into(),
                sql: None,
                cost: None,
            });
        }
        ReplicaStatus::Stopped => {
            f.title = match r.role {
                ReplicaRole::ThisServer => format!("Replication to this server has stopped ({state})"),
                _ if consumer => format!("The logical consumer {} isn't receiving changes ({state})", r.name),
                _ => format!("The replica {} isn't receiving changes ({state})", r.name),
            };
            let error = r.error.as_ref().map(|e| format!("The last error was {e}. ")).unwrap_or_default();
            f.summary = if pg && r.role == ReplicaRole::ThisServer && state == "not receiving" {
                format!(
                    "{error}This standby is set up to stream from its primary (primary_conninfo is set) but no WAL receiver is running, so it isn't getting changes: its data is getting older by the minute and failing over to it now would lose them."
                )
            } else {
                format!(
                    "{error}The copy no longer keeps up, so its data is getting older by the minute and failing over to it now would lose changes."
                )
            };
            f.severity = Severity::High;
            f.fix = Some(Fix {
                summary: if mysql {
                    "Read the full error with SHOW REPLICA STATUS on the replica, fix its cause, then START REPLICA."
                        .into()
                } else {
                    "Find why it stopped (the replica's error log, network, disk space), fix it, and let it catch up before relying on it.".into()
                },
                sql: None,
                cost: None,
            });
        }
        _ => {
            let delay = r.delay_seconds.map(|d| {
                format!(
                    " It is also set up to apply changes {} late on purpose ({}); that part isn't counted.",
                    format::duration(d * 1000.0),
                    if pg { "recovery_min_apply_delay" } else { "SOURCE_DELAY" }
                )
            });
            if consumer {
                f.title = format!("The logical consumer {} is {lag} behind", r.name);
                f.summary = format!(
                    "{} receives this database's changes through logical decoding: a subscription, or a CDC tool such as Debezium. It has confirmed changes only up to {lag} ago, so whatever it feeds sees data that old, and its slot keeps the WAL since then on this server. A consumer falls behind when it applies changes slower than this server writes them: one very large transaction, a slow destination, or a connector that is throttled or failing.",
                    r.name
                );
                f.fix = Some(Fix {
                    summary: "Look at the consumer, not this server: the subscriber's apply worker (pg_stat_subscription and its log) or the CDC connector's lag and errors. Split very large transactions so changes flow in smaller pieces.".into(),
                    sql: None,
                    cost: None,
                });
            } else {
                f.title = match r.role {
                    ReplicaRole::ThisServer => format!("This server is {lag} behind its primary"),
                    _ => format!("The replica {} is {lag} behind", r.name),
                };
                f.summary = format!(
                    "Reads from the replica return data {lag} old, and failing over now would lose or wait for that much. Lag grows when the replica applies changes slower than the primary writes them: large transactions, long queries on the replica{}, or slower storage.{}",
                    if pg { " (which can pause replay)" } else { "" },
                    delay.clone().unwrap_or_default()
                );
                f.fix = Some(Fix {
                    summary: "Split large write transactions into batches, check the replica's storage and CPU, and move long reports off it or let them run against a snapshot.".into(),
                    sql: None,
                    cost: None,
                });
            }
            f.severity = if seconds.is_some_and(|s| s >= 600.0) { Severity::High } else { Severity::Medium };
            if let Some(s) = seconds {
                f.metrics.push(Metric::new("Behind by", s * 1000.0, Unit::Ms));
            }
            if let Some(b) = r.lag_bytes {
                f.metrics.push(Metric::new("Not yet applied", b, Unit::Bytes));
            }
            if let Some(d) = r.delay_seconds {
                f.metrics.push(Metric::new("Delayed on purpose by", d * 1000.0, Unit::Ms));
            }
        }
    }
    severity_score(&mut f);
    f.impact.label = match status {
        ReplicaStatus::LeftConfigured | ReplicaStatus::StoppedByHand | ReplicaStatus::Stopped => {
            r.state.clone().unwrap_or_else(|| "stopped".into())
        }
        _ => format!("{lag} behind"),
    };
    f.confidence = Confidence::High;
    f.verify = vec![match status {
        ReplicaStatus::LeftConfigured => "Re-scan: the leftover replication configuration should be gone.".into(),
        _ => "Re-scan: the lag should be seconds.".into(),
    }];
    Some(f)
}

/// An inactive replication slot that keeps a gigabyte or more of WAL. A physical slot without its replica keeps WAL for
/// nothing; a logical one belongs to a consumer (a subscription, a CDC tool) that may only be between syncs, and
/// dropping it forces that consumer to copy everything again. Whether it can fill the disk depends on
/// max_slot_wal_keep_size (13+): with a cap, PostgreSQL gives the slot up instead.
fn slot_finding(snapshot: &Snapshot, r: &Replica) -> Option<Finding> {
    let kept = r.lag_bytes.filter(|b| *b >= GB)?;
    if r.state.as_deref() != Some("inactive") || r.wal_status.as_deref() == Some("lost") {
        return None;
    }
    let logical = r.is_logical();
    // safe_wal_size exists from 13 and is empty when nothing caps what slots keep.
    let capped = r.safe_wal_bytes.is_some();
    let cap_known = r.wal_status.is_some();
    let mut f = finding("replica-lag", &r.name, Category::Configuration, subject(SubjectKind::Database, &r.name));
    f.title = if logical {
        format!("The logical slot {} keeps {} of WAL while its consumer is away", r.name, format::bytes(kept))
    } else {
        format!("The inactive replication slot {} keeps {} of WAL", r.name, format::bytes(kept))
    };
    let idle = r
        .inactive_seconds
        .map(|s| format!("{} has had no consumer for {}", r.name, format::duration(s * 1000.0)))
        .unwrap_or_else(|| format!("{} has no consumer connected", r.name));
    let kind = if logical {
        " It is a logical slot: a subscription or a CDC tool (Debezium, Fivetran, Airbyte…) reads changes through it, and many connect only between syncs. Dropping it makes that consumer copy everything again."
    } else {
        " It is a physical slot: a standby or a backup tool streams WAL through it."
    };
    // Before 13 nothing can cap a slot; from 13 the scan reads whether something does (older scans didn't).
    let can_cap = culprit_core::version_at_least(&snapshot.server.version, [13, 0, 0]);
    let risk = match (capped, cap_known, can_cap) {
        (true, _, _) => format!(
            " max_slot_wal_keep_size caps what it can keep: {} more WAL and PostgreSQL gives the slot up, so the disk is safe, but its consumer then has to start over.",
            format::bytes(r.safe_wal_bytes.unwrap_or(0.0))
        ),
        (false, true, _) => " Nothing caps what it keeps (max_slot_wal_keep_size is -1), so it grows until the disk fills and the server stops accepting writes.".into(),
        (false, false, true) => " Unless max_slot_wal_keep_size caps what slots keep, it grows until the disk fills and the server stops accepting writes.".into(),
        (false, false, false) => " Before PostgreSQL 13 nothing can cap it: it grows until the disk fills and the server stops accepting writes.".into(),
    };
    f.summary = format!(
        "A replication slot keeps every WAL file its consumer hasn't confirmed. {idle}, so the WAL it holds keeps growing.{kind}{risk}"
    );
    f.severity = match (capped, r.wal_status.as_deref()) {
        (true, Some("unreserved")) => Severity::Medium,
        (true, _) => Severity::Low,
        (false, _) if kept >= 10.0 * GB => Severity::High,
        _ => Severity::Medium,
    };
    let quoted = r.name.replace('\'', "''");
    // Only where nothing caps slots yet (or the scan couldn't tell): a cap already set is never lowered.
    let cap = (!capped && can_cap).then(|| setting_change(&snapshot.server, "max_slot_wal_keep_size", "50GB"));
    let mut sql = if logical {
        format!(
            "-- Only once its consumer is gone for good: dropping it makes the consumer copy everything again\n-- SELECT pg_drop_replication_slot('{quoted}');"
        )
    } else {
        format!("SELECT pg_drop_replication_slot('{quoted}');")
    };
    if let Some(statements) = cap.as_ref().and_then(|c| c.sql.as_deref()) {
        let lead = if cap_known {
            "Cap what any slot may keep"
        } else {
            "If max_slot_wal_keep_size isn't set, cap what any slot may keep"
        };
        sql.push_str(&format!("\n-- {lead}:\n{statements}"));
    }
    let cap_advice = match &cap {
        Some(c) if cap_known => format!(" To keep any slot from filling the disk, cap what slots may keep: {}", c.how),
        Some(c) => format!(
            " If max_slot_wal_keep_size isn't set, cap what slots may keep so none can fill the disk: {}",
            c.how
        ),
        None if !can_cap => " PostgreSQL 13 and later can cap what slots keep (max_slot_wal_keep_size).".into(),
        None => String::new(),
    };
    f.fix = Some(Fix {
        summary: if logical {
            format!(
                "Find its consumer first: the slot's name and plugin usually say which subscription or connector it is. If it still syncs, leave the slot: it catches up when the consumer connects. If the consumer is gone for good, drop it.{cap_advice}"
            )
        } else {
            format!("If nothing will stream through this slot again (a standby that was removed), drop it.{cap_advice}")
        },
        sql: Some(sql),
        cost: None,
    });
    f.metrics = vec![Metric::new("WAL kept", kept, Unit::Bytes)];
    if let Some(s) = r.inactive_seconds {
        f.metrics.push(Metric::new("Without a consumer for", s * 1000.0, Unit::Ms));
    }
    if let Some(b) = r.safe_wal_bytes {
        f.metrics.push(Metric::new("WAL left before it is given up", b, Unit::Bytes));
    }
    severity_score(&mut f);
    f.impact.label = format!("{} of WAL kept", format::bytes(kept));
    f.confidence = Confidence::High;
    f.verify = vec![if logical {
        "Re-scan once the consumer has synced: the WAL kept should drop, or the slot be gone.".into()
    } else {
        "Re-scan: the slot should be gone, or active again.".into()
    }];
    Some(f)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::waits::snapshot_with;
    use culprit_core::{
        ColumnHistogram, IndexContention, Pressure, QueryCosts, Signal, SignalScope, SignalUnit, TableStorage, WaitStat,
    };

    fn first_query(s: &mut Snapshot) -> &mut QueryStat {
        let w = s.workload.as_mut().unwrap();
        w.queries.sort_by(|a, b| b.total_time_ms.total_cmp(&a.total_time_ms));
        w.queries.iter_mut().find(|q| !crate::support::is_monitoring_query(q)).unwrap()
    }

    #[test]
    fn failing_runs_name_the_timeout() {
        let mut s = snapshot_with(Engine::Mssql);
        let q = first_query(&mut s);
        q.text = "SELECT id FROM dbo.orders WHERE customer_id = @P1".into();
        q.executions = 1_000.0;
        q.costs = Some(QueryCosts {
            aborted: Some(60.0),
            failed: Some(0.0),
            aborted_mean_ms: Some(30_050.0),
            ..Default::default()
        });
        let f = failing_query(&s);
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].title, "A read from orders times out 6% of the time, at about 30 s");
        assert_eq!(f[0].severity, Severity::Medium);
        assert!(f[0].fix.as_ref().unwrap().summary.contains("command timeout"));
        // A handful of failures is noise.
        first_query(&mut s).costs = Some(QueryCosts { failed: Some(3.0), ..Default::default() });
        assert!(failing_query(&s).is_empty());
    }

    #[test]
    fn mysql_errors_carry_the_code_not_the_message() {
        let mut s = snapshot_with(Engine::Mysql);
        let q = first_query(&mut s);
        q.text = "UPDATE `orders` SET `status` = ? WHERE `id` = ?".into();
        q.executions = 200.0;
        q.query_hash = Some("abc123".into());
        q.costs = Some(QueryCosts {
            failed: Some(30.0),
            top_error: Some("1205 lock wait timeout".into()),
            ..Default::default()
        });
        let f = failing_query(&s);
        assert_eq!(f[0].title, "An update of orders fails 15% of the time (1205 lock wait timeout)");
        assert_eq!(f[0].severity, Severity::High);
        let fix = f[0].fix.as_ref().unwrap();
        assert!(fix.summary.contains("innodb_lock_wait_timeout"));
        assert!(fix.sql.as_ref().unwrap().contains("DIGEST = 'abc123'"));
    }

    #[test]
    fn planning_share_needs_reuse_and_volume() {
        let mut s = snapshot_with(Engine::Postgres);
        let q = first_query(&mut s);
        q.text = "SELECT * FROM orders o JOIN customers c ON c.id = o.customer_id WHERE o.id = $1".into();
        q.executions = 10_000.0;
        q.total_time_ms = 20_000.0;
        q.mean_time_ms = 2.0;
        q.costs = Some(QueryCosts { plan_ms: Some(30_000.0), plans: Some(10_000.0), ..Default::default() });
        let f = planning_time(&s);
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].title, "Planning takes 60% of the time of a read from orders");
        assert!(f[0].summary.contains("a new plan for every run"));
    }

    #[test]
    fn jit_longer_than_the_query() {
        let mut s = snapshot_with(Engine::Postgres);
        let q = first_query(&mut s);
        q.text = "SELECT count(*) FROM orders WHERE status = $1".into();
        q.executions = 100.0;
        q.total_time_ms = 4_000.0;
        q.mean_time_ms = 40.0;
        q.costs = Some(QueryCosts { jit_ms: Some(2_500.0), jit_functions: Some(800.0), ..Default::default() });
        let f = jit_overhead(&s);
        assert_eq!(f[0].title, "JIT compilation takes longer than running a read from orders");
        assert!(f[0].fix.as_ref().unwrap().sql.as_ref().unwrap().contains("jit_above_cost"));
    }

    #[test]
    fn log_volume_per_row_and_share() {
        let mut s = snapshot_with(Engine::Postgres);
        let q = first_query(&mut s);
        q.text = "UPDATE orders SET note = $1 WHERE id = $2".into();
        q.rows = Some(20_000.0);
        q.costs = Some(QueryCosts {
            log_bytes: Some(200.0 * MB),
            log_share: Some(0.2),
            log_full_pages: Some(20_000.0),
            ..Default::default()
        });
        // Whether wal_compression is on decides both what the image count means and the advice: it was assumed off.
        let f = log_volume(&s);
        assert_eq!(f[0].title, "An update of orders writes 10.2 KB of WAL per row it changes");
        assert!(f[0].summary.contains("20K of its records are full-page images"), "{}", f[0].summary);
        assert!(f[0].fix.as_ref().unwrap().sql.is_none(), "unknown wal_compression: no setting to change");
        set(&mut s, "wal_compression", "off");
        let f = log_volume(&s);
        assert!(f[0].summary.contains("Up to 78% of it is full-page images"), "{}", f[0].summary);
        assert_eq!(
            f[0].fix.as_ref().unwrap().sql.as_deref(),
            Some("ALTER SYSTEM SET wal_compression = 'on';\nSELECT pg_reload_conf();")
        );
        // Already compressed: the checkpoint interval is the lever, not the setting it already has.
        set(&mut s, "wal_compression", "lz4");
        let f = log_volume(&s);
        assert!(f[0].fix.as_ref().unwrap().sql.is_none());
        assert!(f[0].fix.as_ref().unwrap().summary.contains("wal_compression already shrinks"));
        // On a managed service the advice is the platform's control.
        set(&mut s, "wal_compression", "off");
        s.server.cloud = Some("Amazon RDS".into());
        let fix = log_volume(&s)[0].fix.clone().unwrap();
        assert!(fix.sql.is_none() && fix.summary.contains("DB parameter group"), "{}", fix.summary);
        // Reads are never log findings.
        first_query(&mut s).text = "SELECT 1".into();
        assert!(log_volume(&s).is_empty());
    }

    // A share of the log needs a measured volume a day: 30% of a few quiet minutes is nothing.
    #[test]
    fn a_log_share_needs_a_daily_volume() {
        let mut s = snapshot_with(Engine::Postgres);
        let q = first_query(&mut s);
        q.text = "UPDATE orders SET note = $1 WHERE id = $2".into();
        q.rows = None;
        q.costs = Some(QueryCosts { log_bytes: Some(0.4 * GB), log_share: Some(0.6), ..Default::default() });
        s.workload.as_mut().unwrap().window_hours = 0.5;
        assert!(log_volume(&s).is_empty(), "0.4 GB in half an hour is a gigabyte a day only by stretching it");
        // 2 GB inside half an hour are 2 GB inside that day: nothing is stretched.
        first_query(&mut s).costs.as_mut().unwrap().log_bytes = Some(2.0 * GB);
        assert_eq!(log_volume(&s).len(), 1);
        s.workload.as_mut().unwrap().window_hours = 24.0;
        assert_eq!(log_volume(&s)[0].severity, Severity::Medium);
        // pg_stat_statements before 14 can't say when its counts began: the window may be days longer.
        s.server.version = "13.16".into();
        assert!(log_volume(&s).is_empty());
    }

    fn set(s: &mut Snapshot, name: &str, value: &str) {
        s.settings.retain(|x| x.name != name);
        s.settings.push(culprit_core::Setting {
            name: name.into(),
            value: value.into(),
            default_value: None,
            scope: "server".into(),
        });
    }

    #[test]
    fn lock_time_share_of_the_query() {
        let mut s = snapshot_with(Engine::Mysql);
        let q = first_query(&mut s);
        q.text = "UPDATE `stock` SET `qty` = `qty` - ? WHERE `sku` = ?".into();
        q.executions = 1_000.0;
        q.total_time_ms = 100_000.0;
        q.waits = vec![WaitStat {
            category: "Lock".into(),
            wait_type: Some("lock time".into()),
            wait_ms: 60_000.0,
            tasks: None,
        }];
        let f = lock_time(&s);
        assert_eq!(f[0].title, "An update of stock waits on locks for 60% of its time, 60 ms per run");
        assert!(f[0].summary.contains("8.0.28"));
    }

    #[test]
    fn big_results_with_bytes_from_the_plan_width() {
        let mut s = snapshot_with(Engine::Postgres);
        let q = first_query(&mut s);
        q.text = "SELECT id, note FROM orders WHERE created_at > $1".into();
        q.executions = 200.0;
        q.rows = Some(200.0 * 80_000.0);
        q.costs = Some(QueryCosts { row_bytes: Some(150.0), ..Default::default() });
        let f = big_result(&s);
        assert_eq!(f[0].title, "A read from orders returns 80K rows (≈ 11.4 MB) per run");
    }

    fn with_pressure(engine: Engine, f: impl FnOnce(&mut Pressure)) -> Snapshot {
        let mut s = snapshot_with(engine);
        let mut p = Pressure { uptime_hours: Some(72.0), ..Pressure::default() };
        f(&mut p);
        s.pressure = Some(p);
        s
    }

    #[test]
    fn slow_tempdb_and_log() {
        let file = |name: &str, db: &str, kind, reads: f64, read_ms: f64, writes: f64, write_ms: f64| FileIo {
            name: name.into(),
            kind,
            database: Some(db.into()),
            reads,
            read_ms: Some(read_ms),
            read_bytes: None,
            writes,
            write_ms: Some(write_ms),
            write_bytes: None,
        };
        let s = with_pressure(Engine::Mssql, |p| {
            p.files = vec![
                file("tempdev", "tempdb", FileKind::Temp, 10_000.0, 450_000.0, 100.0, 100.0),
                file("shop", "shop", FileKind::Data, 10_000.0, 30_000.0, 0.0, 0.0),
                file("shop_log", "shop", FileKind::Log, 0.0, 0.0, 50_000.0, 600_000.0),
            ];
        });
        let titles: Vec<String> = slow_storage(&s).into_iter().map(|f| f.title).collect();
        assert_eq!(
            titles,
            vec![
                "Reads from tempdb average 45 ms: storage is slow",
                "Writes to shop's log average 12 ms: every commit waits for them"
            ]
        );
    }

    #[test]
    fn churn_thresholds_differ_per_engine() {
        let signal = |rate| {
            Signal::new("new-connections", "New connections", rate, SignalUnit::PerSecond, SignalScope::SinceStartup)
        };
        let pg = with_pressure(Engine::Postgres, |p| p.signals = vec![signal(3.0)]);
        assert_eq!(connection_churn(&pg)[0].title, "The application opens 3 connections a second: use a pool");
        let mssql = with_pressure(Engine::Mssql, |p| p.signals = vec![signal(3.0)]);
        assert!(connection_churn(&mssql).is_empty());
        let mysql = with_pressure(Engine::Mysql, |p| p.signals = vec![signal(40.0)]);
        assert_eq!(
            connection_churn(&mysql)[0].fix.as_ref().unwrap().sql.as_deref(),
            Some("SET PERSIST thread_cache_size = 64;")
        );
    }

    #[test]
    fn hot_rows_and_last_page() {
        let mut s = snapshot_with(Engine::Mssql);
        let i = &mut s.indexes[0];
        let (table, index) = (i.table.clone(), i.name.clone());
        i.contention = Some(IndexContention {
            row_lock_waits: 40.0,
            row_lock_wait_ms: 120_000.0,
            page_latch_waits: 50_000.0,
            page_latch_wait_ms: 900_000.0,
            lock_escalations: 2.0,
            sequential_key: true,
            optimize_for_sequential_key: Some(false),
            ..Default::default()
        });
        let hot = hot_rows(&s);
        assert_eq!(hot[0].title, format!("Sessions wait 3.0 s each on row locks in {table}"));
        assert!(hot[0].summary.contains("escalated"));
        let page = last_page_insert(&s);
        assert_eq!(page[0].title, format!("Inserts into {table} queue for the last page of {index}"));
        assert!(page[0].fix.as_ref().unwrap().sql.as_ref().unwrap().contains("OPTIMIZE_FOR_SEQUENTIAL_KEY = ON"));
    }

    fn escalating(escalations: f64, attempts: f64, lock_wait_ms: f64) -> IndexContention {
        IndexContention {
            lock_escalations: escalations,
            lock_escalation_attempts: attempts,
            row_lock_waits: if lock_wait_ms > 0.0 { 10.0 } else { 0.0 },
            row_lock_wait_ms: lock_wait_ms,
            ..Default::default()
        }
    }

    /// The demo with no contention anywhere but on `dbo.<table>`'s two indexes.
    /// The demo's server started 44 seconds before its scan: too little counting for any rate. These tests start it 12
    /// hours earlier, at 00:13 UTC on the day of the scan.
    fn escalations_on(table: &str, first: IndexContention, second: IndexContention) -> Snapshot {
        let mut s = snapshot_with(Engine::Mssql);
        s.server.started_at = Some("2026-10-11T00:13:03Z".into());
        s.indexes.iter_mut().for_each(|i| i.contention = None);
        let mut a = s.indexes[0].clone();
        a.schema = "dbo".into();
        a.table = table.into();
        a.name = format!("pk_{table}");
        a.contention = Some(first);
        let mut b = a.clone();
        b.name = format!("ix_{table}_created_at");
        b.contention = Some(second);
        s.indexes.extend([a, b]);
        s
    }

    #[test]
    fn lock_escalations_add_up_per_table_and_name_the_write_that_changes_the_most_rows() {
        let mut s = escalations_on("orders", escalating(3.0, 6.0, 30_000.0), escalating(2.0, 3.0, 0.0));
        let q = first_query(&mut s);
        q.text = "DELETE FROM dbo.orders WHERE created_at < @P1".into();
        q.plan = None;
        q.executions = 4.0;
        q.rows = Some(40_000.0);
        let id = q.id.clone();
        let f = lock_escalations(&s);
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].id, "lock-escalations:dbo.orders");
        assert_eq!(
            f[0].title,
            "Row locks on orders escalated to a table lock 5 times since SQL Server started (Oct 11)"
        );
        // Attempts aren't reported: the counter moves without any statement behind it.
        assert!(!f[0].summary.contains("tried"), "{}", f[0].summary);
        assert!(
            f[0].summary.contains("the likeliest is a delete from orders, which changes about 10K rows a run."),
            "{}",
            f[0].summary
        );
        assert_eq!(f[0].queries[0].id, id);
        assert_eq!(f[0].severity, Severity::High);
        assert_eq!(f[0].metrics[0].label, ESCALATIONS_METRIC);
        assert_eq!(f[0].metrics[0].value, 5.0);
        assert!(f[0].fix.as_ref().unwrap().sql.as_ref().unwrap().contains("DELETE TOP (1000) FROM [dbo].[orders]"));
    }

    // On a server, every index of a table a few dozen rows long can show an escalation attempt an hour that no
    // statement made: the counter moves with SQL Server's own background work. Only escalations are reported.
    #[test]
    fn lock_escalation_attempts_alone_are_not_a_finding() {
        let s = escalations_on("audit_archive", escalating(0.0, 24.0, 0.0), escalating(0.0, 24.0, 0.0));
        assert!(lock_escalations(&s).is_empty());
        let s = escalations_on("orders", escalating(0.0, 0.0, 90_000.0), IndexContention::default());
        assert!(lock_escalations(&s).is_empty());
    }

    #[test]
    fn a_table_with_no_statement_writing_to_it_says_where_to_look() {
        let s = escalations_on("audit_archive", escalating(2.0, 0.0, 0.0), IndexContention::default());
        let f = lock_escalations(&s);
        assert_eq!(
            f[0].title,
            "Row locks on audit_archive escalated to a table lock 2 times since SQL Server started (Oct 11)"
        );
        assert!(f[0].summary.contains("none of the statements in the query history writes to it"), "{}", f[0].summary);
        assert!(f[0].queries.is_empty());
        assert_eq!(f[0].severity, Severity::High);
    }

    #[test]
    fn a_single_lock_escalation_is_high() {
        let s = escalations_on("orders", escalating(1.0, 1.0, 0.0), IndexContention::default());
        let f = lock_escalations(&s);
        assert_eq!(f[0].title, "Row locks on orders escalated to a table lock once since SQL Server started (Oct 11)");
        assert_eq!(f[0].severity, Severity::High);
    }

    /// `s` with the counts of an earlier scan of it, taken at 06:00 on the day it was taken.
    fn with_earlier_scan(mut s: Snapshot, counts: &[(&str, f64)], started_at: Option<&str>) -> Snapshot {
        let mut earlier = s.clone();
        earlier.taken_at = "2026-10-11T06:00:00Z".into();
        if let Some(at) = started_at {
            earlier.server.started_at = Some(at.into());
        }
        for i in earlier.indexes.iter_mut().filter_map(|i| i.contention.as_mut()) {
            i.lock_escalations = 0.0;
        }
        for (table, n) in counts {
            let i = earlier.indexes.iter_mut().find(|i| i.table == *table && i.contention.is_some()).expect("table");
            i.contention.as_mut().unwrap().lock_escalations = *n;
        }
        s.escalations_before = culprit_core::EscalationBaseline::from_scan(&earlier, &s);
        s
    }

    // SQL Server counts escalations since it started, with no time for each: without an earlier scan, Culprit can't
    // tell whether they're recent. The finding says when the count began, and only a rate measured since then raises
    // it: two in the 12 hours since the start is High.
    #[test]
    fn escalations_without_an_earlier_scan_are_graded_on_their_rate_since_the_start() {
        let s = escalations_on("orders", escalating(2.0, 0.0, 0.0), IndexContention::default());
        let f = &lock_escalations(&s)[0];
        assert_eq!(f.severity, Severity::High);
        assert!(
            f.summary.starts_with(
                "SQL Server replaced the row and page locks on orders with one lock on the whole table 2 times since it started, on Oct 11 at 00:13 UTC. \
                 It keeps no time for each, and Culprit has no earlier scan to compare with, so they may be old"
            ),
            "{}",
            f.summary
        );
        assert!(f.summary.contains("once a day or more on average since then"), "{}", f.summary);
        assert_eq!(f.metrics.len(), 1);
    }

    // The false flag: one escalation since a start six months ago was High on a first scan. Without an earlier scan
    // it is history unless the count since the start makes a rate.
    #[test]
    fn one_escalation_in_six_months_is_low_history_on_a_first_scan() {
        let mut s = escalations_on("orders", escalating(1.0, 0.0, 0.0), IndexContention::default());
        s.server.started_at = Some("2026-04-11T12:00:00Z".into());
        let f = &lock_escalations(&s)[0];
        assert_eq!(f.severity, Severity::Low);
        assert!(
            f.summary.contains("less than once a week on average since then: they may all be old"),
            "{}",
            f.summary
        );
        // A few a week since the start is Medium: likely to happen again.
        s.server.started_at = Some("2026-10-01T12:00:00Z".into());
        s.indexes.iter_mut().filter_map(|i| i.contention.as_mut()).for_each(|c| c.lock_escalations = 3.0);
        assert_eq!(lock_escalations(&s)[0].severity, Severity::Medium);
        // Minutes of counting make no rate: the demo's server started 44 seconds before the scan.
        s.server.started_at = Some("2026-10-11T12:13:03Z".into());
        let f = &lock_escalations(&s)[0];
        assert_eq!(f.severity, Severity::Low);
        assert!(f.summary.contains("started too recently to tell how often"), "{}", f.summary);
    }

    #[test]
    fn escalations_since_the_previous_scan_are_high_and_counted() {
        let s = escalations_on("orders", escalating(3.0, 0.0, 0.0), escalating(2.0, 0.0, 0.0));
        let f = &lock_escalations(&with_earlier_scan(s, &[("orders", 4.0)], None))[0];
        assert_eq!(f.severity, Severity::High);
        assert_eq!(f.title, "Row locks on orders escalated to a table lock once since Oct 11, 06:00 UTC");
        assert!(
            f.summary.starts_with(
                "SQL Server replaced the row and page locks on orders with one lock on the whole table once since the previous scan, \
                 on Oct 11 at 06:00 UTC. It counted 5 in all since SQL Server started (Oct 11)."
            ),
            "{}",
            f.summary
        );
        assert_eq!(f.impact.label, "1 new table lock");
        assert_eq!((f.metrics[0].label.as_str(), f.metrics[0].value), (ESCALATIONS_METRIC, 5.0));
        assert_eq!((f.metrics[1].label.as_str(), f.metrics[1].value), (ESCALATIONS_SINCE_METRIC, 1.0));
    }

    // The case that looked recent: the count is old, and the writes listed are only the table's writes now.
    #[test]
    fn escalations_with_none_since_the_previous_scan_are_low_history() {
        let mut s = escalations_on("orders", escalating(2.0, 0.0, 0.0), IndexContention::default());
        let q = first_query(&mut s);
        q.text = "UPDATE TOP (1000) dbo.orders SET archived = 1 WHERE archived = 0".into();
        q.plan = None;
        q.rows = Some(q.executions * 900.0);
        let f = &lock_escalations(&with_earlier_scan(s, &[("orders", 2.0)], None))[0];
        assert_eq!(f.severity, Severity::Low);
        assert_eq!(
            f.title,
            "Row locks on orders escalated to a table lock 2 times since SQL Server started (Oct 11), none since Oct 11, 06:00 UTC"
        );
        assert!(
            f.summary.starts_with(
                "These escalations are history, not something happening now: SQL Server counts them from when it started, \
                 on Oct 11 at 00:13 UTC, and the count on orders hasn't grown since the previous scan, on Oct 11 at 06:00 UTC."
            ),
            "{}",
            f.summary
        );
        assert!(f.summary.ends_with("none of them has escalated since the previous scan."), "{}", f.summary);
        assert!(!f.summary.contains("likeliest"), "{}", f.summary);
        assert_eq!(f.queries.len(), 1);
        assert_eq!(f.impact.label, "2 table locks, none new");
        assert_eq!(f.metrics[1].value, 0.0);
        assert!(f.fix.as_ref().unwrap().summary.starts_with("None happened since the previous scan."));
    }

    #[test]
    fn after_a_restart_every_escalation_counted_is_new() {
        let mut s = escalations_on("orders", escalating(2.0, 0.0, 0.0), IndexContention::default());
        s.server.started_at = Some("2026-10-11T12:13:03Z".into());
        // The earlier scan counted more, before the server restarted at 12:13 and started the count again.
        let s = with_earlier_scan(s, &[("orders", 9.0)], Some("2026-10-01T00:00:00Z"));
        assert!(s.escalations_before.as_ref().unwrap().restarted);
        let f = &lock_escalations(&s)[0];
        assert_eq!(f.severity, Severity::High);
        assert_eq!(f.title, "Row locks on orders escalated to a table lock 2 times since Oct 11, 06:00 UTC");
        assert!(
            f.summary.contains(
                "SQL Server restarted since then (on Oct 11 at 12:13 UTC), and these are all it counted since."
            ),
            "{}",
            f.summary
        );
    }

    #[test]
    fn the_fix_starts_an_event_session_that_keeps_the_statement() {
        let s = escalations_on("orders", escalating(2.0, 0.0, 0.0), IndexContention::default());
        let sql = lock_escalations(&s)[0].fix.clone().unwrap().sql.unwrap();
        assert!(sql.contains("CREATE EVENT SESSION culprit_lock_escalation ON SERVER"), "{sql}");
        assert!(sql.contains("WHERE sqlserver.database_name = N'culprit_demo')"), "{sql}");
        assert!(sql.contains("FROM sys.dm_xe_session_targets t"), "{sql}");
        let mut azure = escalations_on("orders", escalating(2.0, 0.0, 0.0), IndexContention::default());
        azure.server.cloud = Some("Azure SQL Database".into());
        let sql = lock_escalations(&azure)[0].fix.clone().unwrap().sql.unwrap();
        assert!(sql.contains("CREATE EVENT SESSION culprit_lock_escalation ON DATABASE"), "{sql}");
        assert!(sql.contains("FROM sys.dm_xe_database_session_targets t"), "{sql}");
        assert!(!sql.contains("STARTUP_STATE") && !sql.contains("database_name ="), "{sql}");
    }

    #[test]
    fn untrusted_constraints_group_per_table() {
        let mut s = snapshot_with(Engine::Mssql);
        let c = |name: &str, kind, disabled| ConstraintIssue {
            schema: "dbo".into(),
            table: "orders".into(),
            name: name.into(),
            kind,
            disabled,
            inheritance_child: false,
        };
        s.constraints = vec![c("FK_orders_customers", ConstraintKind::ForeignKey, false)];
        let f = untrusted_constraint(&s);
        assert_eq!(f[0].title, "SQL Server doesn't trust the foreign key FK_orders_customers on orders");
        assert_eq!(
            f[0].fix.as_ref().unwrap().sql.as_deref(),
            Some("ALTER TABLE [dbo].[orders] WITH CHECK CHECK CONSTRAINT [FK_orders_customers];")
        );
        s.constraints.push(c("CK_orders_total", ConstraintKind::Check, true));
        let f = untrusted_constraint(&s);
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].severity, Severity::Medium);
        let mut pg = snapshot_with(Engine::Postgres);
        pg.constraints =
            vec![ConstraintIssue { schema: "public".into(), ..c("orders_total_check", ConstraintKind::Check, false) }];
        assert_eq!(
            untrusted_constraint(&pg)[0].fix.as_ref().unwrap().sql.as_deref(),
            Some("ALTER TABLE public.orders VALIDATE CONSTRAINT orders_total_check;")
        );
    }

    #[test]
    fn histograms_for_guessed_filter_columns() {
        let mut s = snapshot_with(Engine::Mysql);
        let table = s.tables.iter().find(|t| t.rows.unwrap_or(0.0) >= 1_000.0).unwrap().clone();
        let q = s.workload.as_mut().unwrap().queries.iter_mut().find(|q| q.plan.is_some()).unwrap();
        q.text = format!("SELECT * FROM `{}` WHERE `flavor` = ?", table.name);
        let plan = q.plan.as_mut().unwrap();
        plan.root.object = Some(format!("`{}`.`{}`", table.schema, table.name));
        plan.root.predicate = Some(format!("(`{}`.`{}`.`flavor` = 'x')", table.schema, table.name));
        plan.root.props.insert("filtered".into(), "10.00".into());
        s.histograms = Some(vec![]);
        let all = missing_histogram(&s);
        let f: Vec<&Finding> = all.iter().filter(|f| f.subject.name.ends_with(&format!(".{}", table.name))).collect();
        assert_eq!(f.len(), 1, "{all:?}");
        assert_eq!(f[0].title, format!("MySQL guesses how many {} rows match flavor: no histogram", table.name));
        assert!(f[0].fix.as_ref().unwrap().sql.as_ref().unwrap().contains("UPDATE HISTOGRAM ON `flavor`"));
        s.histograms = Some(vec![ColumnHistogram {
            schema: table.schema.clone(),
            table: table.name.clone(),
            column: "flavor".into(),
            kind: Some("singleton".into()),
            buckets: Some(3.0),
        }]);
        assert!(!missing_histogram(&s).iter().any(|f| f.title.contains("flavor")));
        // MariaDB collects them with ANALYZE … PERSISTENT.
        s.histograms = Some(vec![]);
        s.server.product = "MariaDB 11.8.9".into();
        let maria = missing_histogram(&s);
        let f = maria.iter().find(|f| f.title.contains("flavor")).unwrap();
        assert!(f.title.starts_with("MariaDB guesses"));
        assert!(f.fix.as_ref().unwrap().sql.as_ref().unwrap().contains("PERSISTENT FOR COLUMNS (`flavor`) INDEXES ()"));
        // MySQL 5.7 has no histograms: no finding at all.
        s.histograms = None;
        assert!(missing_histogram(&s).is_empty());
    }

    #[test]
    fn hot_updates_say_why() {
        let mut s = snapshot_with(Engine::Postgres);
        let t = &mut s.tables[0];
        t.updates = Some(100_000.0);
        t.storage = Some(TableStorage {
            hot_updates: Some(10_000.0),
            new_page_updates: Some(80_000.0),
            fillfactor: Some(100.0),
            bloat_bytes: None,
            ..Default::default()
        });
        let name = t.name.clone();
        let f = hot_updates(&s);
        assert_eq!(f[0].title, format!("Only 10% of updates to {name} are HOT: the rest rewrite every index"));
        assert!(f[0].fix.as_ref().unwrap().sql.as_ref().unwrap().contains("fillfactor = 85"));
        s.tables[0].storage.as_mut().unwrap().new_page_updates = Some(1_000.0);
        let f = hot_updates(&s);
        assert!(f[0].fix.as_ref().unwrap().sql.is_none());
        assert!(f[0].summary.contains("changed an indexed column"));
    }

    #[test]
    fn bloat_needs_size_and_share() {
        let mut s = snapshot_with(Engine::Postgres);
        let t = &mut s.tables[0];
        t.data_bytes = Some(4.0 * GB);
        t.storage = Some(TableStorage { bloat_bytes: Some(2.4 * GB), ..Default::default() });
        let name = t.name.clone();
        assert_eq!(table_bloat(&s)[0].title, format!("{name} is about 60% empty space (≈ 2.4 GB)"));
        s.tables[0].storage.as_mut().unwrap().bloat_bytes = Some(50.0 * MB);
        assert!(table_bloat(&s).is_empty());
    }

    /// The demo's support tickets (its events on PostgreSQL) made into a table of 9.7K rows taking 2.9 GB, almost all
    /// of it large values, with two large-value columns.
    fn large_rows_snapshot(engine: Engine) -> Snapshot {
        let mut s = snapshot_with(engine);
        s.taken_at = "2026-10-13T09:00:00Z".into();
        let name = if engine == Engine::Postgres { "events" } else { "support_tickets" };
        let t = s.tables.iter_mut().find(|t| t.name == name).expect("the demo table");
        t.rows = Some(9_728.0);
        t.in_row_bytes = Some(31.0 * MB);
        t.lob_bytes = Some(2_900.0 * MB);
        t.overflow_bytes = Some(0.0);
        t.data_bytes = Some(2_931.0 * MB);
        let (schema, table) = (t.schema.clone(), t.name.clone());
        let (body, attachments) = match engine {
            Engine::Postgres => ("text", "bytea"),
            _ => ("nvarchar(max)", "varbinary(max)"),
        };
        for (ordinal, (name, data_type)) in
            [("id", "int"), ("body", body), ("attachments", attachments)].into_iter().enumerate()
        {
            s.columns.retain(|c| !(c.table == table && c.name == name));
            s.columns.push(culprit_core::ColumnInfo {
                schema: schema.clone(),
                table: table.clone(),
                name: name.into(),
                data_type: data_type.into(),
                nullable: true,
                default: None,
                generated: false,
                ordinal: 100 + ordinal as u32,
            });
        }
        s
    }

    fn large_rows_of(s: &Snapshot) -> Option<Finding> {
        let name = if s.server.engine == Engine::Postgres { "public.events" } else { "shop.support_tickets" };
        large_rows(s).into_iter().find(|f| f.id == format!("large-rows:{name}"))
    }

    /// A statement loading one whole support ticket by id, `runs` times over a 24-hour window, 280 pages a run.
    fn load_ticket(s: &mut Snapshot, runs: f64) -> String {
        let w = s.workload.as_mut().unwrap();
        w.window_hours = 24.0;
        let q = w.queries.iter_mut().find(|q| !crate::support::is_monitoring_query(q)).unwrap();
        q.text = "SELECT * FROM shop.support_tickets WHERE id = @P1".into();
        q.plan = None;
        q.executions = runs;
        q.rows = Some(runs);
        q.logical_reads = Some(280.0 * runs);
        q.id.clone()
    }

    #[test]
    fn large_rows_stay_quiet_for_small_tables_small_rows_or_rows_kept_in_row() {
        let mut s = large_rows_snapshot(Engine::Mssql);
        assert!(large_rows_of(&s).is_some());
        // Small rows: a million of them, 400 bytes each.
        let t = s.tables.iter_mut().find(|t| t.name == "support_tickets").unwrap();
        t.rows = Some(1_000_000.0);
        assert!(large_rows_of(&s).is_none());
        // A small table: 110 MB.
        let t = s.tables.iter_mut().find(|t| t.name == "support_tickets").unwrap();
        (t.rows, t.in_row_bytes, t.lob_bytes) = (Some(1_000.0), Some(10.0 * MB), Some(100.0 * MB));
        assert!(large_rows(&s).is_empty());
        // Large rows kept in the rows' own pages: wide, but not large values.
        let t = s.tables.iter_mut().find(|t| t.name == "support_tickets").unwrap();
        (t.rows, t.in_row_bytes, t.lob_bytes) = (Some(10_000.0), Some(300.0 * MB), Some(100.0 * MB));
        assert!(large_rows(&s).is_empty());
        // An engine that doesn't split its pages says nothing.
        let t = s.tables.iter_mut().find(|t| t.name == "support_tickets").unwrap();
        (t.in_row_bytes, t.lob_bytes) = (None, None);
        assert!(large_rows(&s).is_empty());
    }

    #[test]
    fn large_rows_name_the_table_its_columns_and_the_fix() {
        let s = large_rows_snapshot(Engine::Mssql);
        let f = large_rows_of(&s).expect("finding");
        assert_eq!(f.title, "support_tickets holds 2.9 GB in 9.7K rows: 309 KB a row, mostly large values");
        assert!(
            f.summary.contains(
                "almost all of that is large values kept apart from the rows (columns that can hold them: body and attachments)."
            ),
            "{}",
            f.summary
        );
        assert!(!f.summary.contains("previous scan"), "no growth line without an earlier scan: {}", f.summary);
        assert_eq!(f.severity, Severity::Low);
        assert_eq!(f.confidence, Confidence::High);
        assert!(f.queries.is_empty());
        let fix = f.fix.as_ref().unwrap();
        assert!(fix.sql.is_none());
        assert!(fix.summary.contains("Read only the columns a statement needs") && fix.summary.contains("COMPRESS()"));
        assert!(!f.verify.is_empty());
    }

    #[test]
    fn large_rows_read_whole_many_times_a_day_raise_it() {
        let mut s = large_rows_snapshot(Engine::Mssql);
        let id = load_ticket(&mut s, 12_700.0);
        let f = large_rows_of(&s).expect("finding");
        assert_eq!(f.severity, Severity::Medium);
        assert_eq!(f.queries.iter().map(|q| q.id.as_str()).collect::<Vec<_>>(), vec![id.as_str()]);
        assert!(
            f.summary.contains(
                "The statement below reads them about 13K times a day, going through about 280 pages (2.2 MB) a run."
            ),
            "{}",
            f.summary
        );
        // An insert writing them is said as one.
        let q = s.workload.as_mut().unwrap().queries.iter_mut().find(|q| q.id == id).unwrap();
        q.text = "(@P1 nvarchar(4000),@P2 varbinary(max))INSERT INTO shop.support_tickets (body, attachments) VALUES (@P1, @P2)".into();
        assert!(large_rows_of(&s).unwrap().summary.contains("The statement below writes them about 13K times a day"));
        // A seek that reads a few pages a row isn't a whole-row read.
        let q = s.workload.as_mut().unwrap().queries.iter_mut().find(|q| q.id == id).unwrap();
        q.logical_reads = Some(4.0 * 12_700.0);
        assert!(large_rows_of(&s).unwrap().queries.is_empty());
    }

    #[test]
    fn large_rows_say_how_fast_they_grow_since_the_previous_scan() {
        let mut s = large_rows_snapshot(Engine::Mssql);
        load_ticket(&mut s, 12_700.0);
        let tables = BTreeMap::from([("shop.support_tickets".to_string(), 2_331.0 * MB)]);
        s.sizes_before = Some(culprit_core::SizeBaseline { taken_at: "2026-10-12T09:00:00Z".into(), tables });
        let f = large_rows_of(&s).expect("finding");
        // Read whole thousands of times a day and growing by a fifth of its size a day.
        assert_eq!(f.severity, Severity::High);
        assert!(
            f.summary.contains("It grew by 600 MB since the previous scan on Oct 12 at 09:00 UTC, about 600 MB a day."),
            "{}",
            f.summary
        );
        assert!(f.metrics.iter().any(|m| m.label == "Growth a day"));

        // Six hours apart: the growth, without a daily rate drawn from a quarter of a day.
        s.sizes_before.as_mut().unwrap().taken_at = "2026-10-13T03:00:00Z".into();
        let f = large_rows_of(&s).unwrap();
        assert!(f.summary.contains("It grew by 600 MB in the 6 hours since the previous scan"), "{}", f.summary);
        assert!(!f.metrics.iter().any(|m| m.label == "Growth a day"));

        // Not grown.
        s.sizes_before.as_mut().unwrap().tables.insert("shop.support_tickets".into(), 2_931.0 * MB);
        let f = large_rows_of(&s).unwrap();
        assert!(f.summary.contains("It hasn't grown since the previous scan"), "{}", f.summary);
        assert_eq!(f.severity, Severity::Medium);
    }

    #[test]
    fn large_rows_on_postgres_count_toast() {
        let s = large_rows_snapshot(Engine::Postgres);
        let f = large_rows_of(&s).expect("finding");
        assert!(f.summary.contains("PostgreSQL has already compressed them"));
        assert!(f.fix.as_ref().unwrap().summary.contains("SET COMPRESSION lz4"));
        // The demo's own text and jsonb columns can hold them too.
        assert!(
            f.summary.contains("(columns that can hold them: kind, payload, body and attachments)"),
            "{}",
            f.summary
        );
        // MySQL doesn't report the split.
        let mut m = snapshot_with(Engine::Mysql);
        m.tables.iter_mut().for_each(|t| (t.rows, t.data_bytes) = (Some(9_728.0), Some(2_931.0 * MB)));
        assert!(large_rows(&m).is_empty());
    }

    #[test]
    fn replicas_lagging_stopped_and_slots() {
        let r = |name: &str, role, state: &str, lag_seconds, lag_bytes| Replica {
            state: Some(state.into()),
            lag_seconds,
            lag_bytes,
            ..Replica::new(name, role)
        };
        let s = with_pressure(Engine::Postgres, |p| {
            p.replicas = vec![
                r("reporting", ReplicaRole::Replica, "streaming", Some(95.0), None),
                r("healthy", ReplicaRole::Replica, "streaming", Some(0.4), Some(1_000.0)),
                r("old_etl", ReplicaRole::Slot, "inactive", None, Some(14.0 * GB)),
            ];
        });
        let titles: Vec<String> = replica_lag(&s).into_iter().map(|f| f.title).collect();
        assert_eq!(
            titles,
            vec!["The replica reporting is 2 min behind", "The inactive replication slot old_etl keeps 14.0 GB of WAL"]
        );
        let mut stopped = r("from 10.0.0.5", ReplicaRole::ThisServer, "applying stopped (SQL thread)", None, None);
        stopped.error = Some("1062 duplicate key".into());
        let s = with_pressure(Engine::Mysql, |p| p.replicas = vec![stopped]);
        let f = replica_lag(&s);
        assert_eq!(f[0].title, "Replication to this server has stopped (applying stopped (SQL thread))");
        assert!(f[0].summary.contains("1062 duplicate key"));
    }
}
