//! The `chatty-query` check: Tiny queries executed so often that the round trips cost more than the work, typically a loop in application code.

use super::*;

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

check!(ChattyQueryCheck: chatty_query_info, chatty_query);

/// Round trips a day for a chatty statement to be Medium: at about a millisecond each, the minute a day
/// ([`MEDIUM_MS_A_DAY`]) Medium asks of database time.
pub(super) const MEDIUM_ROUND_TRIPS_A_DAY: f64 = MEDIUM_MS_A_DAY;

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
        // BEGIN and COMMIT around every transaction, a pool's SELECT 1, a SET per checkout: frequent and tiny by
        // nature, and no loop in the application sends them.
        .filter(|q| !is_monitoring_query(q) && !culprit_core::sql::is_session_statement(&q.text))
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
