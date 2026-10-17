//! The `network-wait` check: Queries that spend much of their time waiting for the application to take results (ASYNC_NETWORK_IO), usually because they return large result sets.

use super::*;

pub fn network_wait_info() -> CheckInfo {
    CheckInfo {
        id: "network-wait".into(),
        title: "Results the application reads slowly".into(),
        category: Category::Queries,
        engines: vec![Engine::Mssql],
        requires: vec![],
        description: "Queries that spend much of their time waiting for the application to take results (ASYNC_NETWORK_IO), usually because they return large result sets.".into(),
    }
}

check!(NetworkWaitCheck: network_wait_info, network_wait);

pub fn network_wait(snapshot: &Snapshot) -> Vec<Finding> {
    let mut findings: Vec<Finding> = app_queries(snapshot)
        .filter_map(|q| {
            let network: f64 = q.waits.iter().filter(|w| category_key(&w.category) == "network").map(|w| w.wait_ms).sum();
            let share = if q.total_time_ms > 0.0 { network / q.total_time_ms } else { 0.0 };
            let query = query_impact(snapshot, &[q])?;
            if share < 0.25 || query.share < 0.01 {
                return None;
            }
            let rows = q.rows.filter(|_| q.executions > 0.0).map(|r| r / q.executions);
            let mut f = finding("network-wait", &q.id, Category::Queries, subject(SubjectKind::Query, &q.id));
            // Its weight is the time spent waiting on the application, not the query's whole time.
            let w = snapshot.workload.as_ref()?;
            // A day's worth only of runs that came that way, never a burst's spread over the window.
            let per_day_ms = w.per_day_measured(network).filter(|_| w.runs_per_day_of(q).is_some());
            let impact = QueryImpact { share: w.share_of(network, q), per_day_ms };
            apply_query_impact(&mut f, &impact);
            // A statement that stopped running is told in the past.
            let waits = if crate::all_stopped(snapshot, &[q]) { "waited" } else { "waits" };
            f.title = match rows {
                Some(r) if r >= 1_000.0 => format!(
                    "{} {waits} on the application {} of its time while sending {} rows per call",
                    capitalize(&query_handle(q)),
                    format::percent(share),
                    format::count(r)
                ),
                _ => format!(
                    "{} {waits} on the application {} of its time",
                    capitalize(&query_handle(q)),
                    format::percent(share)
                ),
            };
            f.summary = format!(
                "SQL Server had its results ready but the application took them slowly (ASYNC_NETWORK_IO), so each run holds its locks and memory longer. \
                 That is {} of this query's time. Usually the result set is bigger than needed, or the application does work for each row before reading the next.",
                format::percent(share)
            );
            f.severity = severity_for_share_of(snapshot, &[q], impact.share);
            f.confidence = Confidence::High;
            f.queries = query_refs(&[q]);
            f.metrics.push(Metric::new("Waiting on the app", share, Unit::Ratio));
            if let Some(r) = rows {
                f.metrics.push(Metric::new("Rows per call", r, Unit::Rows));
            }
            f.fix = Some(Fix {
                summary: "Return fewer rows (filter, or page with OFFSET/FETCH or keyset pagination), or read them faster: stream the results and do per-row work after the read, not while the result set is open.".into(),
                sql: None,
                cost: None,
            });
            f.verify = vec!["Re-scan: this query's Sending results (network) share should drop.".into()];
            Some(f)
        })
        .collect();
    findings.sort_by(|a, b| b.impact.score.total_cmp(&a.impact.score));
    findings.truncate(5);
    findings
}
