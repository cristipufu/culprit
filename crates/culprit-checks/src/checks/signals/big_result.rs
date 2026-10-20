//! The `big-result` check: Reads that send tens of thousands of rows to the application on every run, with an estimate in bytes from the plan's row width where the engine gives one.

use super::*;
use crate::evidence::{QueryMeasure, Requirement as Evidence};

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

check!(BigResultCheck: big_result_info, big_result, &[Evidence::Query(QueryMeasure::Rows)]);

/// What a statement's big result adds to the spill finding on it, which carries both (refine's "a spill carries the
/// result it sorts"): one statement, one share, one finding. Its summary says the rows each run sends, its fix how to
/// return fewer, and its verify what a later scan shows once it does (nothing for an export, whose rows are the point).
pub(crate) struct ResultOfASpill {
    pub summary: String,
    pub fix: String,
    pub verify: Option<String>,
}

/// [`ResultOfASpill`] for `q`, worded the way [`big_result`] words it; `None` when its runs return no rows count.
pub(crate) fn result_of_a_spill(snapshot: &Snapshot, q: &QueryStat) -> Option<ResultOfASpill> {
    let rows = per_run(q.rows?, q.executions);
    let width = q.costs.as_ref().and_then(|c| c.row_bytes).or_else(|| q.plan.as_ref().and_then(plan_row_bytes));
    let sends = format!(
        "{} rows{}",
        format::count(rows),
        width.map(|w| format!(" (≈ {})", format::bytes(w * rows))).unwrap_or_default()
    );
    Some(if looks_like_export(snapshot, q) {
        ResultOfASpill {
            summary: format!(
                " Each run also sends {sends} to the application, the whole table: an export, a dump or an ETL job looks like this, and for those the rows are the point."
            ),
            fix: " If it is an export or an ETL job, the memory is the fix; run it on a replica or off-peak if it competes with the application.".into(),
            verify: None,
        }
    } else {
        ResultOfASpill {
            summary: format!(
                " Each run also sends {sends} to the application, and few screens or jobs need that many at once: returning fewer can keep the sort in memory too."
            ),
            fix: format!(
                " Where the application doesn't use every row it receives, {}",
                lowercase_first(RETURN_WHAT_IS_USED)
            ),
            // On running totals the spill's own step names the two scans to compare and says why, once.
            verify: Some(if snapshot.workload.as_ref().is_some_and(|w| !w.windowed) {
                format!(
                    "If it now returns fewer rows, the rows it returns per run between those two scans should drop to {USED}."
                )
            } else {
                format!("If it now returns fewer rows, re-scan: rows per run should drop to {USED}.")
            }),
        }
    })
}

/// How many rows a statement that returns only what's used sends a run.
const USED: &str = "what a page or a job actually uses";

/// The verify step for a statement that returns too many rows a run: on a source of running totals, what two scans
/// show of the rows it returns between them ([`crate::queries::verify_counts_between_scans`]); else a later scan's
/// rows per run.
fn rows_verify(snapshot: &Snapshot, q: &QueryStat) -> String {
    let now = format!("{} rows in {} runs", format::count(q.rows.unwrap_or(0.0)), format::count(q.executions));
    match crate::queries::verify_counts_between_scans(snapshot, "rows and runs", &now) {
        Some(compare) => format!("Between two scans, the rows it returns per run should drop to {USED}: {compare}."),
        None => format!("Re-scan: rows per run should drop to {USED}."),
    }
}

/// How to send the application only the rows it uses.
const RETURN_WHAT_IS_USED: &str = "Return only what's used: page the results (keyset pagination, or LIMIT / OFFSET … FETCH), filter in SQL rather than in the application, select only the columns you need, and aggregate in the database.";

fn lowercase_first(text: &str) -> String {
    let mut chars = text.chars();
    chars.next().map(|c| c.to_lowercase().collect::<String>() + chars.as_str()).unwrap_or_default()
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
                "{} {} {} rows{} per run",
                capitalize(&handle),
                if crate::all_stopped(snapshot, &[q]) { "returned" } else { "returns" },
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
                ran_phrase(snapshot, q),
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
            } else if rows >= 100_000.0 || window_at_least(snapshot, q, q.rows.unwrap_or(0.0), 100_000_000.0) {
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
            let page = RETURN_WHAT_IS_USED;
            f.fix = Some(Fix {
                summary: if export {
                    format!("If it is an export or ETL job, leave it as it is, and run it on a replica or off-peak if it competes with the application. If the application runs it: {page}")
                } else {
                    page.into()
                },
                sql: None,
                cost: None,
            });
            f.verify = vec![rows_verify(snapshot, q)];
            f
        })
        .collect();
    findings.sort_by(|a, b| b.impact.score.total_cmp(&a.impact.score));
    findings.truncate(5);
    findings
}
