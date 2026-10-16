//! The `big-result` check: Reads that send tens of thousands of rows to the application on every run, with an estimate in bytes from the plan's row width where the engine gives one.

use super::*;

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

check!(BigResultCheck: big_result_info, big_result);

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
