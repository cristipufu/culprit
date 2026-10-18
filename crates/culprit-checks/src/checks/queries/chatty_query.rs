//! The `chatty-query` check: Tiny queries executed so often that the round trips cost more than the work, typically a loop in application code.

use super::*;

pub fn chatty_query_info() -> CheckInfo {
    CheckInfo {
        id: "chatty-query".into(),
        title: "Chatty queries".into(),
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
            // Its share of the runs and its average as every surface states them: over Culprit's scans where they
            // measured it.
            let exec_share = workload.statement_runs_share(q);
            // Busy by the rate it ran at while it ran: a burst by its own total, never "a day" of six seconds.
            let busy = at_least_per_day_of(snapshot, q, q.executions, 20_000.0);
            let small = q.rows_handled_per_execution().is_some_and(|r| r <= 2.0) && workload.statement_mean_ms(q) < 5.0;
            small && exec_share >= 0.08 && busy
        })
        .take(4)
        .map(|q| {
            let exec_share = workload.statement_runs_share(q);
            let per_day_runs = workload.runs_per_day_of(q);
            let burst = ran_in_bursts(snapshot, q);
            let impact = query_impact(snapshot, &[q]).expect("workload present");
            let mut f = finding("chatty-query", &q.id, Category::Queries, subject(SubjectKind::Query, &q.id));
            // Rows changed for a write, rows returned otherwise: MySQL counts a write's rows apart, and returns none.
            let rows = q.rows_handled_per_execution().unwrap_or(1.0);
            let handle = capitalize(&query_handle(q));
            // As its runs came: a burst by when it ran ("ran 30K times on Oct 17 at 10:17 UTC").
            let runs = runs_of(snapshot, q);
            // A burst, or a statement that stopped running, is told in the past.
            let past = burst || crate::all_stopped(snapshot, &[q]);
            let (ran, find) = if past { ("ran", "found") } else { ("runs", "find") };
            let (are, is, returns, changes) =
                if past { ("were", "was", "returned", "changed") } else { ("are", "is", "returns", "changes") };
            let writes = q.writes_rows();
            let share = format!("{} of {}", format::percent(impact.share), time_noun(snapshot));
            let exec = format::percent(exec_share);
            let one_row = |rows: f64| format!("{} row{}", format::count(rows), if rows <= 1.0 { "" } else { "s" });
            // Its runs and their share as Culprit's scans measured them, where they did (not a burst's, said by when it
            // ran); the rows a run handles are kept only in the totals, named so beside them.
            let scans = workload.scans_of(q).filter(|_| !burst);
            let runs_over = scans.as_ref().map(|s| format!(" over {}", s.phrase())).unwrap_or_default();
            let rows_over = crate::support::totals_basis(snapshot, q)
                .filter(|_| scans.is_some())
                .map(|basis| format!(" (counted over {basis})"))
                .unwrap_or_default();
            let rows_said = |rows: f64| format!("{}{rows_over}", one_row(rows));
            // On a source of running totals the counts keep every run from before the change: two scans show it.
            let fewer_runs = |between: &str| {
                verify_between_scans(snapshot, q, between).map_or_else(
                    || format!("It ran {runs}; after the change {between}."),
                    |scans| format!("It ran {runs}. {scans}"),
                )
            };
            let fewer = fewer_runs("it should run fewer times and take less time in all while the useful work stays the same");
            let engine = snapshot.server.engine;
            // Shapes that look alike in the counters and need different fixes: a poll that mostly finds nothing, a
            // counter handed out one number at a time, a write repeated row by row (or a heartbeat), and a read inside
            // a loop (N+1).
            let (title, summary, fix, verify, caveat) = if rows < 0.5 {
                (
                    format!("{handle} {ran} {runs} and most runs {find} nothing"),
                    format!(
                        "Its runs {are} {exec} of all statement runs{runs_over}, and on average a run {} {} row{}{rows_over}. Empty results can come from polling, request-driven checks or legitimate misses; the counters do not identify the caller or prove a timer. Each run {is} cheap ({share} in all), but unnecessary calls still add round trips.",
                        if writes { changes } else { returns },
                        format::count(rows),
                        if rows == 1.0 { "" } else { "s" },
                    ),
                    "Check the caller first. If it polls for work, back off after empty results, coordinate pollers, or have the writer wake them (a queue or notification). For request-driven checks, reuse an answer only while it remains valid. Keep necessary checks and their correctness guarantees.".to_string(),
                    fewer_runs("unnecessary calls and total database time should fall while the useful work stays the same"),
                    "Some frequent checks are legitimate (a lease or a lock checked on every request). Check the calling code before changing how often it asks.",
                )
            } else if writes {
                let write = WriteKind::of(&q.text, engine);
                let (summary, fix, verify, caveat) = match &write {
                    WriteKind::Counter { column, table } => (
                        format!(
                            "Its runs {are} {exec} of all statement runs{runs_over}. Each run adds to {column} in one row of {table} and returns the new value: a counter handed out one number at a time. Each is a round trip, and the row stays locked until its transaction commits, so callers that need the next number of the same row can wait for each other. In all it takes {share}{}.",
                            lock_waits(snapshot, q).map(|w| format!(", and {w}")).unwrap_or_default()
                        ),
                        counter_fix(snapshot),
                        fewer_runs("it should no longer run, or run far fewer times"),
                        "A sequence's numbers only increase: they can skip values and aren't counted per row. Check whether the application relies on gapless numbers for each row before replacing the counter.",
                    ),
                    WriteKind::Update => (
                        format!(
                            "Its runs {are} {exec} of all statement runs{runs_over}, and each run {changes} about {}. Frequent small updates can be independent requests, row-by-row loops, heartbeats or status changes; each call can add a round trip, and each committing transaction can flush the log ({share} in all). The counters do not show whether calls already share a transaction.",
                            rows_said(rows)
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
                            "Its runs {are} {exec} of all statement runs{runs_over}, and each run {changes} about {}. Frequent small deletes can be independent removals or a row-by-row loop; each call can add a round trip, and each committing transaction can flush the log ({share} in all). The counters do not show whether calls already share a transaction.",
                            rows_said(rows)
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
                            "Its runs {are} {exec} of all statement runs{runs_over}, and each run {changes} about {}. Frequent small writes can be individual events, heartbeats or a row-by-row loop; each call can add a round trip, and each committing transaction can flush the log ({share} in all). The counters do not show whether calls already share a transaction.",
                            rows_said(rows)
                        ),
                        format!(
                            "Write many rows per statement ({}) inside one transaction; for a heartbeat or lease, renew less often.",
                            bulk_insert(engine)
                        ),
                        fewer,
                        "Some frequent single-row writes are legitimate (each event recorded as it happens). Check the calling code before batching.",
                    ),
                };
                // "One row" only where a run changes about one: 1.6 rows a run is said as it is.
                let each = if format::count(rows) == "1" { "one row".to_string() } else { format!("about {}", one_row(rows)) };
                (format!("{handle} {ran} {runs}, {each} each time"), summary, fix, verify, caveat)
            } else {
                // A read in a loop asks for one key a call, whatever the rows each key finds (1.6 a run is still one
                // lookup).
                (
                    format!("{handle} {ran} {runs}, one key at a time"),
                    format!(
                        "Its runs {are} {exec} of all statement runs{runs_over}, and each run {returns} about {}. Repeated lookups can be per-request checks or a loop in application code (N+1). Each run {is} cheap ({share} in all), but each call can add a network round trip.",
                        rows_said(rows)
                    ),
                    "Reuse a result the application already has, or cache it for as long as it remains valid; access checks must still reflect permission changes. If the caller loops over items, load their rows in one query (WHERE key IN (…) or a join) before the loop; ORMs usually have an eager-loading option for this.".to_string(),
                    fewer_runs("it should run fewer times and take less time in all"),
                    "Some high-frequency single-row lookups are legitimate (for example, by primary key on a hot path). Check the calling code before batching.",
                )
            };
            f.title = title;
            f.summary = summary;
            // A share of the executions is easy to reach on a quiet database; the amount is the round trips: each costs
            // the application about a millisecond in the same region, so Medium asks for the minute a day it asks of
            // database time; a burst's are its own total, never extrapolated to a day.
            let medium = exec_share >= 0.25 && at_least_per_day_of(snapshot, q, q.executions, MEDIUM_ROUND_TRIPS_A_DAY);
            f.severity = if medium { Severity::Medium } else { Severity::Low };
            f.impact.score = (score_for_share(impact.share) + exec_share * 40.0).min(60.0);
            f.impact.db_time_share = Some(impact.share);
            f.impact.time_ms_per_day = impact.per_day_ms;
            f.impact.label = format!("{} of executions", format::percent(exec_share));
            f.confidence = Confidence::Medium;
            f.queries = query_refs(&[q]);
            // Named for what they are over, as the title and summary say them: Culprit's scans where they measured it,
            // with the totals' beside where the two differ twofold (never for a recent entry, nor a burst's rate).
            let (over, beside) = match &scans {
                Some(s) => (" over Culprit's scans", (!s.is_recent(q)).then(|| format!(" over {}", workload.window_phrase()))),
                None => ("", None),
            };
            match per_day_runs {
                Some(runs) => f.metrics.push(Metric::new(format!("Runs per day{over}"), runs, Unit::Count)),
                // Runs that came in bursts: what was counted, never a rate a day.
                None if burst => f.metrics.push(Metric::new("Runs", q.executions, Unit::Count)),
                None => {}
            }
            if let (Some(s), Some(totals)) = (&scans, &beside)
                && s.runs_differ(workload, q)
            {
                f.metrics.push(Metric::new(format!("Runs per day{totals}"), workload.per_day(q.executions), Unit::Count));
            }
            f.metrics.push(Metric::new(format!("Share of executions{over}"), exec_share, Unit::Ratio));
            if let (Some(s), Some(totals)) = (&scans, &beside)
                && s.runs_share_differs(workload, q)
            {
                let all = q.executions / workload.total_executions;
                f.metrics.push(Metric::new(format!("Share of executions{totals}"), all, Unit::Ratio));
            }
            f.metrics.extend(average_metrics(snapshot, q));
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
