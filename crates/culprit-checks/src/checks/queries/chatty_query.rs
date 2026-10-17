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
            let one_row = |rows: f64| format!("{} row{}", format::count(rows), if rows <= 1.0 { "" } else { "s" });
            // On a source of running totals the counts keep every run from before the change: two scans show it.
            let fewer_runs = |between: &str| {
                verify_between_scans(snapshot, q, between).map_or_else(
                    || format!("It ran {runs}; after the change {between}."),
                    |scans| format!("It ran {runs}. {scans}"),
                )
            };
            let fewer = fewer_runs("it should run orders of magnitude fewer times");
            let engine = snapshot.server.engine;
            // Shapes that look alike in the counters and need different fixes: a poll that mostly finds nothing, a
            // counter handed out one number at a time, a write repeated row by row (or a heartbeat), and a read inside
            // a loop (N+1).
            let (title, summary, fix, verify, caveat) = if rows < 0.5 {
                (
                    format!("{handle} runs {runs} and most runs find nothing"),
                    format!(
                        "This statement is {exec} of every statement the database executes, and on average a run {} {} row{}: the app is polling, asking on a timer whether there is work. Each run is cheap ({share} in all), but every instance of the app asks on its own, so the cost grows with the fleet, not with the work.",
                        if writes { "changes" } else { "returns" },
                        format::count(rows),
                        if rows == 1.0 { "" } else { "s" },
                    ),
                    "Ask less often when the last run found nothing (back off to a few seconds), let one instance poll for the fleet, or have the writer wake the poller (a queue, a notification, a channel). Keep the poll on a small index, for example one filtered to the pending rows.".to_string(),
                    fewer_runs("it should run an order of magnitude fewer times while the work it finds stays the same"),
                    "Some frequent checks are legitimate (a lease or a lock checked on every request). Check the calling code before changing how often it asks.",
                )
            } else if writes {
                let write = WriteKind::of(&q.text, engine);
                let (summary, fix, verify, caveat) = match &write {
                    WriteKind::Counter { column, table } => (
                        format!(
                            "This statement is {exec} of every statement the database executes. Each run adds to {column} in one row of {table} and returns the new value: a counter handed out one number at a time. Each is a round trip, and the row stays locked until its transaction commits, so callers that need the next number of the same row can wait for each other. In all it takes {share}{}.",
                            lock_waits(snapshot, q).map(|w| format!(", and {w}")).unwrap_or_default()
                        ),
                        counter_fix(snapshot),
                        fewer_runs("it should no longer run, or run far fewer times"),
                        "A sequence's numbers only increase: they can skip values and aren't counted per row. Check whether the application relies on gapless numbers for each row before replacing the counter.",
                    ),
                    WriteKind::Update => (
                        format!(
                            "This statement is {exec} of every statement the database executes, and each run changes about {}. Updates that small and that frequent are usually a loop updating rows one at a time, or a heartbeat or status kept in one row; each one is a round trip and a log flush ({share} in all).",
                            one_row(rows)
                        ),
                        format!(
                            "Where the application changes many rows the same way, update them in one statement ({}) inside one transaction; for a heartbeat or lease, renew less often.",
                            many_keys(engine, "UPDATE")
                        ),
                        fewer,
                        "Some frequent single-row updates are legitimate (a status that changes as its work progresses). Check the calling code before batching.",
                    ),
                    WriteKind::Delete => (
                        format!(
                            "This statement is {exec} of every statement the database executes, and each run changes about {}. Deletes that small and that frequent are usually a loop deleting rows one at a time; each one is a round trip and a log flush ({share} in all).",
                            one_row(rows)
                        ),
                        format!(
                            "Delete many rows per statement ({}, or by a condition the rows share) inside one transaction.",
                            many_keys(engine, "DELETE")
                        ),
                        fewer,
                        "Some frequent single-row deletes are legitimate (a row removed as its work completes). Check the calling code before batching.",
                    ),
                    WriteKind::Insert => (
                        format!(
                            "This statement is {exec} of every statement the database executes, and each run changes about {}. Writes that small and that frequent are usually a loop writing row by row, or a heartbeat; each one is a round trip and a log flush ({share} in all).",
                            one_row(rows)
                        ),
                        format!(
                            "Write many rows per statement ({}) inside one transaction; for a heartbeat or lease, renew less often.",
                            bulk_insert(engine)
                        ),
                        fewer,
                        "Some frequent single-row writes are legitimate (each event recorded as it happens). Check the calling code before batching.",
                    ),
                };
                (format!("{handle} runs {runs}, one row each time"), summary, fix, verify, caveat)
            } else {
                (
                    format!("{handle} runs {runs}, one row at a time"),
                    format!(
                        "This statement is {exec} of every statement the database executes, and each run returns about {}. That pattern usually comes from a loop in application code (N+1): one query per item instead of one query for all items. Each run is cheap ({share} in all), but each call costs a network round trip.",
                        one_row(rows)
                    ),
                    "Load the rows in one query (WHERE key IN (…) or a join) before the loop, or cache them. ORMs usually have an eager-loading option for this.".to_string(),
                    fewer,
                    "Some high-frequency single-row lookups are legitimate (for example, by primary key on a hot path). Check the calling code before batching.",
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
            f.fix = Some(Fix { summary: fix, sql: None, cost: None });
            f.verify = vec![verify];
            f.caveats.push(caveat.into());
            f
        })
        .collect()
}

/// What a single-row write does, as its text says: the fix for each is different.
#[derive(Debug, PartialEq)]
enum WriteKind {
    /// `UPDATE … SET n = n + 1` returning the new value (`OUTPUT INSERTED.n`, `RETURNING n`, MySQL's
    /// `LAST_INSERT_ID(n + 1)`): a counter handed out one number at a time.
    Counter {
        column: String,
        table: String,
    },
    Update,
    Delete,
    /// INSERT, MERGE, REPLACE, and anything else that writes.
    Insert,
}

impl WriteKind {
    fn of(text: &str, engine: Engine) -> WriteKind {
        let main = main_statement(text);
        match first_verb(main).map(|(_, verb)| verb) {
            Some("update") => match (counter_column(main), write_target(text, engine)) {
                (Some(column), Some(table)) => {
                    let table = table.rsplit('.').next().unwrap_or(&table).to_string();
                    WriteKind::Counter { column, table }
                }
                _ => WriteKind::Update,
            },
            Some("delete") => WriteKind::Delete,
            _ => WriteKind::Insert,
        }
    }
}

/// The column an UPDATE adds to and returns: `SET [n] = [t].[n] + 1 … OUTPUT INSERTED.[n]`, `SET n = n + 1 RETURNING
/// n`, `SET n += 1 OUTPUT …`, MySQL's `SET n = LAST_INSERT_ID(n + 1)`. None for an update that returns nothing, or
/// changes no column by adding to it.
fn counter_column(text: &str) -> Option<String> {
    let lower = squeeze(text);
    // A keyword as a word: squeezed text writes `where (` as `where(`.
    let keyword = |text: &str, word: &str| {
        text.match_indices(word).map(|(at, _)| at).find(|at| {
            text[at + word.len()..].chars().next().is_none_or(|c| c == ' ' || c == '(') && text[..*at].ends_with(' ')
        })
    };
    let set = keyword(&lower, "set")? + "set".len();
    let rest = &lower[set..];
    // The assignments end where the statement says what it returns, or which rows.
    let end = ["output", "returning", "where", "from"].into_iter().filter_map(|k| keyword(rest, k)).min();
    let assignments = &rest[..end.unwrap_or(rest.len())];
    let returns = ["output", "returning"].into_iter().any(|k| keyword(rest, k).is_some());
    let leaf = |name: &str| name.trim().rsplit('.').next().unwrap_or("").to_string();
    let (mut depth, mut start) = (0i32, 0);
    let mut parts = Vec::new();
    for (i, c) in assignments.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => depth -= 1,
            ',' if depth == 0 => {
                parts.push(&assignments[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    parts.push(&assignments[start..]);
    parts.into_iter().find_map(|part| {
        if let Some((column, _)) = part.split_once("+=") {
            return returns.then(|| leaf(column));
        }
        let (column, value) = part.split_once('=')?;
        let column = leaf(column);
        let value = value.trim();
        // MySQL hands the new value back through LAST_INSERT_ID(n + 1).
        let (value, handed_back) = match value.strip_prefix("last_insert_id(") {
            Some(inner) => (inner, true),
            None => (value, returns),
        };
        let (first, added) = value.split_once('+')?;
        (handed_back && !column.is_empty() && leaf(first) == column && !added.trim().is_empty()).then_some(column)
    })
}

/// How long a statement waited for locks, where the engine measured it for that statement: Query Store's waits (a
/// statement with waits recorded and none of them on locks waited for none) and MySQL's lock time. PostgreSQL keeps
/// no waits per statement: nothing is said there.
fn lock_waits(snapshot: &Snapshot, q: &QueryStat) -> Option<String> {
    let of_its =
        |ms: f64| format!("{} of lock waits out of its {}", format::duration(ms), format::duration(q.total_time_ms));
    let locks = || q.waits.iter().filter(|w| w.category == "Lock").map(|w| w.wait_ms).reduce(|a, b| a + b);
    match snapshot.server.engine {
        Engine::Mssql if !q.waits.is_empty() => {
            Some(format!("Query Store recorded {}", of_its(locks().unwrap_or(0.0))))
        }
        Engine::Mysql => {
            let lock = q.spread.as_ref().and_then(|s| s.lock_ms).or_else(locks)?;
            Some(format!("{} counted {}", snapshot.server.name(), of_its(lock)))
        }
        _ => None,
    }
}

/// Let the database hand out the numbers: each engine's sequence, without a row to lock; or the number assigned in the
/// INSERT that uses it.
fn counter_fix(snapshot: &Snapshot) -> String {
    let sequence = match snapshot.server.engine {
        Engine::Mssql => "a SEQUENCE (`NEXT VALUE FOR`)",
        Engine::Postgres => "a sequence (`nextval()`, or an identity column)",
        Engine::Mysql if snapshot.server.is_mariadb() => "a SEQUENCE (`NEXT VALUE FOR`) or an AUTO_INCREMENT column",
        Engine::Mysql => "an AUTO_INCREMENT column",
        Engine::Sqlite => "an INTEGER PRIMARY KEY",
    };
    format!(
        "If the numbers only need to increase, let the database hand them out: {sequence} gives each caller the next number without locking a row. If each row needs its own numbers in order, assign the number in the INSERT that uses it, in the same statement, so this UPDATE and its round trip go away."
    )
}
