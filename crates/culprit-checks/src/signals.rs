//! Checks on signals beyond time and reads. Per query: runs that fail or time out, planning and JIT time, log volume,
//! lock time and result size. Per server: slow storage, connection churn, replica lag. Per object: row-lock and
//! last-page contention on indexes, constraints the optimizer can't trust, missing histograms, non-HOT updates and
//! table bloat. Pure functions of the snapshot; each says plainly what its engine measures.

use std::collections::BTreeMap;

use culprit_core::waits::category_key;
use culprit_core::{
    Category, CheckInfo, Confidence, ConstraintIssue, ConstraintKind, Engine, FileIo, FileKind, Finding, Fix,
    IndexStat, Metric, QueryStat, Replica, ReplicaRole, Severity, Snapshot, SubjectKind, TableStat, Unit, format,
    signals::plan_row_bytes,
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

pub fn failing_query(snapshot: &Snapshot) -> Vec<Finding> {
    let mssql = snapshot.server.engine == Engine::Mssql;
    let mut findings: Vec<Finding> = app_queries(snapshot)
        .filter_map(|q| {
            let costs = q.costs.as_ref()?;
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
            f.severity = if rate >= 0.10 {
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
            if !mssql {
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
            let azure = snapshot.server.cloud.as_deref() == Some("Azure SQL Database");
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
            f.severity = if share >= 0.5 && per_day(snapshot, plan_ms) >= 60_000.0 { Severity::Medium } else { Severity::Low };
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
                f.caveats.push("Query Store keeps compile time for the query's whole life, so the time is spread over the window's runs.".into());
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
            f.severity = if share >= 0.5 || per_day(snapshot, jit) >= 60_000.0 { Severity::Medium } else { Severity::Low };
            with_query(snapshot, &mut f, q);
            severity_score(&mut f);
            f.impact.label = format!("{} JIT", format::percent(share));
            f.confidence = Confidence::High;
            f.metrics = vec![
                Metric::new("JIT share", share, Unit::Ratio),
                Metric::new("JIT per run", per_run(jit, q.executions), Unit::Ms),
                Metric::new("Run time", q.mean_time_ms, Unit::Ms),
            ];
            f.fix = Some(Fix {
                summary: "Raise the cost threshold so JIT only starts for genuinely long queries, or turn it off for the application's role. Fixing the row estimate that inflates the cost also helps.".into(),
                sql: Some("-- JIT only for plans the planner thinks are 10× more expensive than the default threshold\nALTER SYSTEM SET jit_above_cost = 1000000;\nSELECT pg_reload_conf();\n-- Or for the application's role only:\n-- ALTER ROLE app_user SET jit = off;".into()),
                cost: None,
            });
            f.verify = vec!["Re-scan: JIT time per run should drop to zero or near it, and the average run time with it.".into()];
            f
        })
        .collect();
    findings.sort_by(|a, b| b.impact.score.total_cmp(&a.impact.score));
    findings.truncate(5);
    findings
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
            let heavy_share = share >= 0.3 && bytes >= GB;
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
            // A full-page image is a whole 8 KB page; their share says whether checkpoints drive the volume.
            let image_share = costs.log_full_pages.map(|n| (n * 8_192.0 / bytes).min(1.0));
            f.summary = format!(
                "It wrote {} of {log} {}: {} of what statements wrote{}. Every byte of {log} is also written to backups and sent to replicas, so it drives replication lag, backup size and checkpoints.{}",
                format::bytes(bytes),
                window_phrase(snapshot),
                format::percent(share),
                per_row.map(|b| format!(", about {} per row it changed", format::bytes(b))).unwrap_or_default(),
                match image_share {
                    Some(s) if s >= 0.5 => format!(" About {} of it is full-page images: the first change to each page after a checkpoint logs the whole 8 KB page.", format::percent(s)),
                    _ => String::new(),
                }
            );
            f.severity = if share >= 0.5 || per_row.is_some_and(|b| b >= 8_192.0) { Severity::Medium } else { Severity::Low };
            with_query(snapshot, &mut f, q);
            severity_score(&mut f);
            f.impact.label = format!("{} of {log}", format::bytes(bytes));
            f.confidence = Confidence::High;
            f.metrics = vec![Metric::new("Log written", bytes, Unit::Bytes), Metric::new("Share of log", share, Unit::Ratio)];
            if let Some(b) = per_row {
                f.metrics.push(Metric::new("Per row", b, Unit::Bytes));
            }
            f.fix = Some(if pg {
                Fix {
                    summary: "Touch each page once per checkpoint: batch the changes (one statement for many rows, sorted by key) rather than many scattered single-row writes. Every index on the table adds its own WAL: drop indexes nobody reads (see Indexes). When full-page images dominate, wal_compression shrinks them, and a longer checkpoint interval logs fewer of them.".into(),
                    sql: image_share.filter(|s| *s >= 0.5).map(|_| "ALTER SYSTEM SET wal_compression = on;  -- lz4 or zstd on PostgreSQL 15+\nSELECT pg_reload_conf();".into()),
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
            f.severity = severity_for_share(db_share);
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
        .filter(|q| is_read(q) && q.executions >= 20.0)
        .filter_map(|q| {
            let rows = per_run(q.rows?, q.executions);
            (rows >= 10_000.0).then_some((q, rows))
        })
        .map(|(q, rows)| {
            let mut f = finding("big-result", &q.id, Category::Queries, subject(SubjectKind::Query, &q.id));
            let handle = query_handle(q);
            let width = q.costs.as_ref().and_then(|c| c.row_bytes).or_else(|| q.plan.as_ref().and_then(plan_row_bytes));
            let bytes = width.map(|w| w * rows);
            f.title = format!(
                "{} returns {} rows{} per run",
                capitalize(&handle),
                format::count(rows),
                bytes.map(|b| format!(" (≈ {})", format::bytes(b))).unwrap_or_default()
            );
            f.summary = format!(
                "Each run sends {} rows to the application{}; its {} runs {} moved {} rows. Few screens or jobs need that many at once: the database reads them, the network carries them and the application holds them in memory.",
                format::count(rows),
                match (bytes, width) {
                    (Some(b), Some(w)) => format!(", about {} at the plan's estimated {} per row", format::bytes(b), format::bytes(w)),
                    _ => String::new(),
                },
                format::count(q.executions),
                window_phrase(snapshot),
                format::count(q.rows.unwrap_or(0.0))
            );
            f.severity = if rows >= 100_000.0 || per_day(snapshot, q.rows.unwrap_or(0.0)) >= 100_000_000.0 {
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
            f.fix = Some(Fix {
                summary: "Return only what's used: page the results (keyset pagination, or LIMIT / OFFSET … FETCH), filter in SQL rather than in the application, select only the columns you need, and aggregate in the database.".into(),
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

/// (file, average ms, operations, is a write) for the slowest file of each kind that crosses its threshold.
fn slow_files(files: &[FileIo]) -> Vec<(&FileIo, f64, f64, bool)> {
    let mut worst: BTreeMap<(u8, bool), (&FileIo, f64, f64, bool)> = BTreeMap::new();
    for f in files {
        let kind = match f.kind {
            FileKind::Data => 0,
            FileKind::Temp => 1,
            FileKind::Log => 2,
            FileKind::Other => continue,
        };
        // Reads of data and temp files; writes of the log, which every commit waits for.
        let candidates = [
            (f.avg_read_ms(), f.reads, false, f.kind != FileKind::Log, 20.0),
            (f.avg_write_ms(), f.writes, true, f.kind == FileKind::Log, 10.0),
        ];
        for (avg, ops, write, applies, limit) in candidates {
            if let Some(avg) = avg
                && applies
                && ops >= 1_000.0
                && avg >= limit
            {
                let key = (kind, write);
                if worst.get(&key).is_none_or(|w| avg > w.1) {
                    worst.insert(key, (f, avg, ops, write));
                }
            }
        }
    }
    worst.into_values().collect()
}

fn file_label(snapshot: &Snapshot, f: &FileIo) -> String {
    match (snapshot.server.engine, f.kind, f.database.as_deref()) {
        (Engine::Mssql, _, Some("tempdb")) if f.kind == FileKind::Log => "tempdb's log".into(),
        (Engine::Mssql, _, Some("tempdb")) => "tempdb".into(),
        (Engine::Mssql, FileKind::Log, Some(db)) => format!("{db}'s log"),
        (Engine::Mssql, _, Some(db)) => format!("{db}'s data file {}", f.name),
        (Engine::Postgres, FileKind::Log, _) => "WAL".into(),
        (Engine::Postgres, FileKind::Temp, _) => "temporary tables".into(),
        (Engine::Postgres, _, _) => "table and index files".into(),
        _ => f.name.clone(),
    }
}

pub fn slow_storage(snapshot: &Snapshot) -> Vec<Finding> {
    let Some(p) = snapshot.pressure.as_ref() else { return Vec::new() };
    let files = culprit_core::signals::merge_logs(&p.files);
    slow_files(&files)
        .into_iter()
        .map(|(file, avg, ops, write)| {
            let label = file_label(snapshot, file);
            let key = format!("{}:{}", file.database.as_deref().unwrap_or(""), file.name);
            let mut f = finding("slow-storage", &key, Category::Configuration, subject(SubjectKind::Database, &label));
            f.title = if write {
                format!("Writes to {label} average {}: every commit waits for them", format::duration(avg))
            } else {
                format!("Reads from {label} average {}: storage is slow", format::duration(avg))
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
            f.severity = match (write, avg) {
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
                Metric::text("File", file.name.clone()),
            ];
            // Azure SQL Database runs on storage you don't manage: the tier decides the latency and the I/O limits.
            let fix = if snapshot.server.cloud.as_deref() == Some("Azure SQL Database") {
                "In Azure SQL Database the service tier decides this: General Purpose reads from remote storage (5 to 10 ms is normal there), Hyperscale serves most reads from a local cache and the rest from page servers, Business Critical uses local SSDs. Check avg_data_io_percent and avg_log_write_percent in sys.dm_db_resource_stats: near 100% means the tier's I/O limit is throttling, so add vCores or move to a faster tier; well below it, cut the reads instead (indexes, fewer full scans). A since-startup average hides bursts: sample Waits → Right now during the busy hour."
            } else {
                "Check the volume's limits first: cloud disks throttle at their provisioned IOPS and throughput, and burst credits run out. Then look for other load on the same disk and antivirus scanning data files, and give the log its own low-latency volume. A since-startup average hides bursts: sample Waits → Right now during the busy hour."
            };
            f.fix = Some(Fix { summary: fix.into(), sql: None, cost: None });
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
    f.title = format!("The application opens {} connections a second: use a pool", format::count(rate));
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
    f.summary = format!(
        "{} the server accepted {} new connections a second on average. {detail} An application that connects for each request spends more time connecting than querying under load.",
        capitalize(&since_start(snapshot)),
        format::count(rate)
    );
    f.severity = if rate >= limit * 5.0 { Severity::Medium } else { Severity::Low };
    severity_score(&mut f);
    f.impact.label = format!("{}/s new connections", format::count(rate));
    f.confidence = Confidence::Medium;
    f.metrics = vec![Metric::new("New connections per second", rate, Unit::Count)];
    let mariadb = is_mariadb(snapshot);
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
            summary: "Pool connections in the application. Until then, a larger thread cache lets new connections reuse threads.".into(),
            sql: Some(if mariadb {
                "SET GLOBAL thread_cache_size = 64;  -- and in the server's configuration file".into()
            } else {
                "SET PERSIST thread_cache_size = 64;".into()
            }),
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

pub fn hot_rows(snapshot: &Snapshot) -> Vec<Finding> {
    let rcsi = snapshot.setting("is_read_committed_snapshot_on").map(|s| crate::config::is_on(&s.value));
    let mut findings: Vec<Finding> = snapshot
        .indexes
        .iter()
        .filter_map(|i| {
            let c = i.contention.as_ref()?;
            (c.lock_wait_ms() >= 10_000.0 && c.lock_waits() >= 1.0).then_some((i, c))
        })
        .map(|(i, c)| {
            let table = index_label(i);
            let mut f = finding(
                "hot-rows",
                &format!("{table}.{}", i.name),
                Category::Indexes,
                subject(SubjectKind::Index, format!("{table}.{}", i.name)),
            );
            let avg = per_run(c.lock_wait_ms(), c.lock_waits());
            f.title = format!("Sessions wait {} each on row locks in {}", format::duration(avg), i.table);
            let escalations = if c.lock_escalations > 0.0 {
                format!(" {} times SQL Server escalated many row locks to a lock on the whole table, which blocks every other session.", format::count(c.lock_escalations))
            } else {
                String::new()
            };
            f.summary = format!(
                "Since {} was loaded into memory, sessions waited {} times for locks on its rows and pages, {} in all, {} each on average.{escalations} \
                 Waits like these come from transactions that hold rows for long, or from writes whose filter isn't indexed and so lock more rows than they change.",
                i.name,
                format::count(c.lock_waits()),
                format::duration(c.lock_wait_ms()),
                format::duration(avg)
            );
            let touching = queries_touching(snapshot, &i.schema, &i.table);
            let mut locked: Vec<&QueryStat> = touching.into_iter().filter(|q| lock_ms(q) > 0.0).collect();
            locked.sort_by(|a, b| lock_ms(b).total_cmp(&lock_ms(a)));
            locked.truncate(3);
            f.queries = query_refs(&locked);
            f.severity = if c.lock_wait_ms() >= 600_000.0 {
                Severity::High
            } else if c.lock_wait_ms() >= 60_000.0 && avg >= 1_000.0 {
                Severity::Medium
            } else {
                Severity::Low
            };
            severity_score(&mut f);
            f.impact.label = format!("{} waiting on locks", format::duration(c.lock_wait_ms()));
            f.confidence = Confidence::High;
            f.metrics = vec![
                Metric::new("Lock waits", c.lock_waits(), Unit::Count),
                Metric::new("Time waiting", c.lock_wait_ms(), Unit::Ms),
                Metric::new("Average wait", avg, Unit::Ms),
            ];
            if c.lock_escalations > 0.0 {
                f.metrics.push(Metric::new("Escalations to a table lock", c.lock_escalations, Unit::Count));
            }
            f.fix = Some(Fix {
                summary: format!(
                    "Keep the transactions that change {} short and commit before slow work; make their WHERE use an index so they lock only the rows they change.{}",
                    i.table,
                    if rcsi == Some(false) {
                        " If readers are the ones waiting, READ_COMMITTED_SNAPSHOT lets them read the last committed version instead: it changes what readers see and uses tempdb, so test it first."
                    } else {
                        ""
                    }
                ),
                sql: (rcsi == Some(false)).then(|| "-- Readers stop waiting for writers; test first\nALTER DATABASE CURRENT SET READ_COMMITTED_SNAPSHOT ON WITH ROLLBACK IMMEDIATE;".into()),
                cost: None,
            });
            f.verify = vec!["Re-scan a day later: lock waits on this index should stop growing.".into()];
            f.caveats.push("Counted since the index's metadata was loaded into memory, usually since SQL Server started.".into());
            f
        })
        .collect();
    findings.sort_by(|a, b| b.impact.score.total_cmp(&a.impact.score));
    findings.truncate(5);
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
            f.severity = if c.page_latch_wait_ms >= 600_000.0 { Severity::High } else { Severity::Medium };
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
            } else {
                format!(
                    "{names} {} added NOT VALID, so rows that existed then were never checked. Until it is validated the planner can't use it to skip the table when a query's filter contradicts it (constraint exclusion).",
                    if list.len() == 1 { "was" } else { "were" }
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
            // MySQL's fixed guesses: 10% for =, 33.33% for ranges, 11.11% for BETWEEN.
            if let Some(filtered) = node.props.get("filtered").and_then(|v| v.parse::<f64>().ok())
                && [10.0, 33.33, 11.11].iter().any(|g| (filtered - g).abs() < 0.01)
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
            f.title = format!("{engine} guesses how many {table} rows match {list}: no histogram");
            let who = match queries.len() {
                1 => format!("{} filters", capitalize(&query_handle(queries[0]))),
                n => format!("{n} of the heaviest queries filter"),
            };
            f.summary = format!(
                "{who} {table} on {list}, which no index starts with and no histogram describes, so {engine} assumes a fixed share of rows match (10% for =, a third for ranges){}. When the real share is far off, common for status flags and skewed values, it picks the wrong join order or access path. A histogram costs nothing at query time and changes only when you run ANALYZE again.",
                guess.map(|g| format!("; EXPLAIN shows filtered = {g:.2}%, one of those guesses")).unwrap_or_default()
            );
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

pub fn hot_updates(snapshot: &Snapshot) -> Vec<Finding> {
    let hours = snapshot.pressure.as_ref().and_then(|p| p.uptime_hours);
    let mut findings: Vec<Finding> = snapshot
        .tables
        .iter()
        .filter_map(|t| {
            let s = t.storage.as_ref()?;
            let updates = t.updates.filter(|u| *u >= 10_000.0)?;
            let hot = s.hot_updates?;
            let ratio = hot / updates;
            (ratio < 0.5).then_some((t, s, updates, hot, ratio))
        })
        .map(|(t, s, updates, hot, ratio)| {
            let name = t.qualified();
            let mut f = finding("hot-updates", &name, Category::Maintenance, subject(SubjectKind::Table, &name));
            let cold = updates - hot;
            f.title = if hot < 1.0 {
                format!("No update to {} is HOT: each one rewrites every index", t.name)
            } else {
                format!("Only {} of updates to {} are HOT: the rest rewrite every index", format::percent(ratio), t.name)
            };
            let indexes = snapshot.indexes_on(&t.schema, &t.name).count();
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
            let per_day = hours.filter(|h| *h > 0.0).map(|h| cold / h * 24.0);
            f.severity = if per_day.is_some_and(|d| d >= 1_000_000.0) { Severity::Medium } else { Severity::Low };
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
            f.summary = format!(
                "From its row count and average row width, {}'s rows need about {}; it takes {}. New rows reuse freed space, but every scan still reads the empty pages, and the cache holds them. This is an estimate from statistics, good to tens of percent; pgstattuple measures it exactly.",
                t.name,
                format::bytes((size - bloat).max(0.0)),
                format::bytes(size)
            );
            f.severity = if bloat >= GB { Severity::Medium } else { Severity::Low };
            severity_score(&mut f);
            f.impact.label = format!("≈ {} empty", format::bytes(bloat));
            f.confidence = Confidence::Medium;
            f.metrics = vec![
                Metric::new("Empty space (estimated)", bloat, Unit::Bytes),
                Metric::new("Table size", size, Unit::Bytes),
                Metric::new("Share", share, Unit::Ratio),
            ];
            let target = qualified(Engine::Postgres, &t.schema, &t.name);
            f.fix = Some(Fix {
                summary: "Measure it exactly first. To give the space back, rewrite the table: pg_repack does it online; VACUUM FULL does it while blocking all reads and writes. If the table keeps growing, make autovacuum visit it more often so freed space is reused sooner.".into(),
                sql: Some(format!(
                    "-- Exact numbers (needs the pgstattuple extension)\nSELECT * FROM pgstattuple('{}');\n-- Rewrite offline: blocks reads and writes until it finishes\n-- VACUUM (FULL, ANALYZE) {target};",
                    target.replace('\'', "''")
                )),
                cost: Some("Rewrites the table".into()),
            });
            f.verify = vec!["Re-scan after the rewrite: the estimate should fall under 20%.".into()];
            f
        })
        .collect();
    findings.sort_by(|a, b| b.impact.score.total_cmp(&a.impact.score));
    findings.truncate(5);
    findings
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

fn stopped(r: &Replica) -> bool {
    r.state
        .as_deref()
        .is_some_and(|s| s.contains("stopped") || s == "not receiving" || s.eq_ignore_ascii_case("NOT SYNCHRONIZING"))
}

pub fn replica_lag(snapshot: &Snapshot) -> Vec<Finding> {
    let Some(p) = snapshot.pressure.as_ref() else { return Vec::new() };
    let pg = snapshot.server.engine == Engine::Postgres;
    p.replicas
        .iter()
        .filter(|r| match r.role {
            ReplicaRole::Slot => r.state.as_deref() == Some("inactive") && r.lag_bytes.is_some_and(|b| b >= GB),
            _ => stopped(r) || r.lag_seconds.is_some_and(|s| s >= 30.0) || r.lag_bytes.is_some_and(|b| b >= GB),
        })
        .map(|r| {
            let mut f = finding("replica-lag", &r.name, Category::Configuration, subject(SubjectKind::Database, &r.name));
            let lag = r
                .lag_seconds
                .map(|s| format::duration(s * 1000.0))
                .or_else(|| r.lag_bytes.map(|b| format!("{} of log", format::bytes(b))))
                .unwrap_or_else(|| "behind".into());
            match r.role {
                ReplicaRole::Slot => {
                    let kept = r.lag_bytes.unwrap_or(0.0);
                    f.title = format!("The inactive replication slot {} keeps {} of WAL", r.name, format::bytes(kept));
                    f.summary = format!(
                        "A replication slot keeps every WAL file its consumer hasn't confirmed. {} is inactive, so the WAL it holds keeps growing until the disk fills and the server stops accepting writes.",
                        r.name
                    );
                    f.severity = if kept >= 10.0 * GB { Severity::High } else { Severity::Medium };
                    f.fix = Some(Fix {
                        summary: "If nothing will consume this slot again, drop it. To keep a slot from filling the disk, cap what slots may keep (PostgreSQL 13+).".into(),
                        sql: Some(format!(
                            "SELECT pg_drop_replication_slot('{}');\n-- Cap what any slot may keep:\n-- ALTER SYSTEM SET max_slot_wal_keep_size = '50GB'; SELECT pg_reload_conf();",
                            r.name.replace('\'', "''")
                        )),
                        cost: None,
                    });
                    f.metrics = vec![Metric::new("WAL kept", kept, Unit::Bytes)];
                }
                _ if stopped(r) => {
                    f.title = match r.role {
                        ReplicaRole::ThisServer => format!("Replication to this server has stopped ({})", r.state.clone().unwrap_or_default()),
                        _ => format!("The replica {} isn't receiving changes ({})", r.name, r.state.clone().unwrap_or_default()),
                    };
                    f.summary = format!(
                        "{}The copy no longer keeps up, so its data is getting older by the minute and failing over to it now would lose changes.",
                        r.error.as_ref().map(|e| format!("The last error was {e}. ")).unwrap_or_default()
                    );
                    f.severity = Severity::High;
                    f.fix = Some(Fix {
                        summary: if snapshot.server.engine == Engine::Mysql {
                            "Read the full error with SHOW REPLICA STATUS on the replica, fix its cause, then START REPLICA.".into()
                        } else {
                            "Find why it stopped (the replica's error log, network, disk space), fix it, and let it catch up before relying on it.".into()
                        },
                        sql: None,
                        cost: None,
                    });
                }
                _ => {
                    f.title = match r.role {
                        ReplicaRole::ThisServer => format!("This server is {lag} behind its primary"),
                        _ => format!("The replica {} is {lag} behind", r.name),
                    };
                    f.summary = format!(
                        "Reads from the replica return data {lag} old, and failing over now would lose or wait for that much. Lag grows when the replica applies changes slower than the primary writes them: large transactions, long queries on the replica{}, or slower storage.",
                        if pg { " (which can pause replay)" } else { "" }
                    );
                    f.severity = if r.lag_seconds.is_some_and(|s| s >= 600.0) { Severity::High } else { Severity::Medium };
                    f.fix = Some(Fix {
                        summary: "Split large write transactions into batches, check the replica's storage and CPU, and move long reports off it or let them run against a snapshot.".into(),
                        sql: None,
                        cost: None,
                    });
                    if let Some(s) = r.lag_seconds {
                        f.metrics.push(Metric::new("Behind by", s * 1000.0, Unit::Ms));
                    }
                    if let Some(b) = r.lag_bytes {
                        f.metrics.push(Metric::new("Not yet applied", b, Unit::Bytes));
                    }
                }
            }
            severity_score(&mut f);
            f.impact.label = if r.role == ReplicaRole::Slot { "WAL kept".into() } else { format!("{lag} behind") };
            f.confidence = Confidence::High;
            f.verify = vec!["Re-scan: the lag should be seconds, or the slot gone.".into()];
            f
        })
        .collect()
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
        let f = log_volume(&s);
        assert_eq!(f[0].title, "An update of orders writes 10.2 KB of WAL per row it changes");
        assert!(f[0].summary.contains("full-page images"));
        assert!(f[0].fix.as_ref().unwrap().sql.as_ref().unwrap().contains("wal_compression"));
        // Reads are never log findings.
        first_query(&mut s).text = "SELECT 1".into();
        assert!(log_volume(&s).is_empty());
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

    #[test]
    fn untrusted_constraints_group_per_table() {
        let mut s = snapshot_with(Engine::Mssql);
        let c = |name: &str, kind, disabled| ConstraintIssue {
            schema: "dbo".into(),
            table: "orders".into(),
            name: name.into(),
            kind,
            disabled,
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

    #[test]
    fn replicas_lagging_stopped_and_slots() {
        let r = |name: &str, role, state: &str, lag_seconds, lag_bytes| Replica {
            name: name.into(),
            role,
            state: Some(state.into()),
            sync: None,
            lag_seconds,
            lag_bytes,
            unsent_bytes: None,
            error: None,
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
