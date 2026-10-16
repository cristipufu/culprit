//! The `failing-query` check: Statements whose runs end in an error or are cancelled by the client (usually a command timeout): Query Store's aborted and exception runs, performance_schema's errors per digest. PostgreSQL keeps no per-statement error count.

use super::*;

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

check!(FailingQueryCheck: failing_query_info, failing_query);

/// A client command timeout: cancelled runs that last about as long as a common default (30 s, 60 s…).
pub(super) fn looks_like_timeout(mean_ms: f64) -> Option<f64> {
    [15.0, 30.0, 60.0, 120.0].into_iter().find(|s| (mean_ms / 1000.0 - s).abs() <= s * 0.1)
}

/// Error 3572 is FOR UPDATE NOWAIT finding the row taken: the statement asked not to wait, and the application expects
/// the error (a try-lock), so it isn't a run that failed.
pub(super) fn deliberate_error(costs: &culprit_core::QueryCosts) -> bool {
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
            // High needs it to keep happening, not one incident: ten unfinished runs a day (one blocking episode that
            // timed out five runs of a week's forty is a tenth of them, and Medium).
            let often = window_at_least(snapshot, unfinished, 10.0);
            f.severity = if !recent {
                Severity::Low
            } else if rate >= 0.10 && often {
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

pub(super) fn failing_fix(
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
