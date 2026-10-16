//! The `unstable-query` check: Queries whose duration swings wildly between runs with the same plan, a sign of parameter sensitivity or blocking.

use super::*;

pub fn unstable_query_info() -> CheckInfo {
    CheckInfo {
        id: "unstable-query".into(),
        title: "Unstable queries".into(),
        category: Category::Queries,
        engines: vec![Engine::Mssql, Engine::Postgres, Engine::Mysql, Engine::Sqlite],
        requires: vec![],
        description: "Queries whose duration swings wildly between runs with the same plan, a sign of parameter sensitivity or blocking.".into(),
    }
}

check!(UnstableQueryCheck: unstable_query_info, unstable_query);

/// How far a statement's run times spread: the standard deviation over the average, and the slowest run over it.
pub(super) const UNSTABLE_CV: f64 = 3.0;
const UNSTABLE_MAX_FACTOR: f64 = 50.0;

/// The slow runs behind a wide spread, when it takes more than one slow run to explain it. One outlier among many fast
/// runs (a lock wait during a migration, a cold cache after a restart) inflates the standard deviation on its own: the
/// spread has to stay wide without the slowest run's share of it. Returns the least time the slow runs spent beyond
/// the average (ms): every slow run is at most the slowest one, so the variance they explain needs at least that much.
pub(super) fn unstable_excess_ms(q: &QueryStat) -> Option<f64> {
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
    // Each slow run is at most the slowest, so it took at least excess / (max − mean) of them: two runs that hit one
    // blocking incident aren't a statement whose runs swing, however wide they make the spread.
    (excess > 0.0 && excess / (max - mean) >= crate::queries::MIN_TAIL_RUNS).then_some(excess)
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
            let slow_share = workload.share_of(excess, q);
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
            f.summary = if snapshot.server.engine.has_server() {
                format!(
                    "{same}, very different run times: the slow runs took at least {} more than the average, together. Either some parameter values match far more rows than the plan was built for (parameter sensitivity), or some runs wait on locks. Check the waits for this query, then the parameter values of the slow runs.",
                    format::duration(excess)
                )
            } else {
                // SQLite: no server to record waits; a run that waits is waiting for the file's write lock.
                format!(
                    "{same}, very different run times: the slow runs took at least {} more than the average, together. Either some values match far more rows than the plan expects, or some runs wait for the file's write lock (up to the application's busy_timeout). Compare the slow runs' values in the log.",
                    format::duration(excess)
                )
            };
            f.queries = query_refs(&[q]);
            f.metrics.push(Metric::new("Average", q.mean_time_ms, Unit::Ms));
            f.metrics.push(Metric::new("Slowest", max, Unit::Ms));
            f.metrics.push(Metric::new("Std deviation", stddev, Unit::Ms));
            f.metrics.push(Metric::new("Slow runs' extra time (at least)", excess, Unit::Ms));
            // The same ladders as `parameter-sensitive`: the cause is usually the same, only the evidence is coarser.
            f.fix = Some(match snapshot.server.engine {
                Engine::Mssql => crate::queries::mssql_fix(snapshot, q, None),
                Engine::Postgres => crate::queries::postgres_fix(),
                Engine::Mysql => crate::queries::mysql_fix(0.0, snapshot.server.is_mariadb()),
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
                    .map(|at| format!("since {}", crate::queries::short_when(at)))
                    .unwrap_or_else(|| workload.window_phrase().replacen("the ", "in the ", 1));
                f.caveats.push(format!(
                    "PostgreSQL keeps one minimum, average, maximum and standard deviation per statement {since}, so the slowest runs could be from any time in that span (a lock wait during a migration counts too), and Culprit can't tell which values were slow or how many calls were."
                ));
            }
            Some(f)
        })
        .collect()
}
