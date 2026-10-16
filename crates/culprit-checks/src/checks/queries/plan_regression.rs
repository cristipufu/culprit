//! The `plan-regression` check: Queries whose current plan is much slower than a plan they used before.

use super::*;

pub fn plan_regression_info() -> CheckInfo {
    CheckInfo {
        id: "plan-regression".into(),
        title: "Plan regressions".into(),
        category: Category::Queries,
        engines: vec![Engine::Mssql],
        requires: vec![Capability::QueryStore],
        description: "Queries whose current plan is much slower than a plan they used before.".into(),
    }
}

check!(PlanRegressionCheck: plan_regression_info, plan_regression);

/// A plan with this many runs or more counts as one the query used.
pub(super) const PLAN_MIN_RUNS: f64 = 5.0;
/// The faster plan is the baseline only over this many runs, the bar `regression` sets for one: five quick runs in nine
/// minutes are a moment, not how the query ran.
const BASELINE_MIN_RUNS: f64 = 20.0;
/// A slower plan does more work per run than the one before by this factor, in CPU or in reads, where the source keeps
/// them per plan: otherwise its runs only waited longer (blocking, load, a busier time of day), which a plan change
/// doesn't explain and forcing the old plan doesn't fix.
const MORE_WORK: f64 = 1.5;

/// Whether `latest` does more work per run than `best`: `None` when the source doesn't keep it per plan.
pub(super) fn does_more_work(best: &PlanVariant, latest: &PlanVariant) -> Option<bool> {
    let per_run = |total: Option<f64>, p: &PlanVariant| total.map(|t| t / p.executions.max(1.0));
    let cpu = per_run(latest.cpu_time_ms, latest).zip(per_run(best.cpu_time_ms, best));
    let reads = per_run(latest.logical_reads, latest).zip(per_run(best.logical_reads, best));
    if cpu.is_none() && reads.is_none() {
        return None;
    }
    let worse = |(now, then): (f64, f64)| now >= then.max(0.001) * MORE_WORK;
    Some(cpu.is_some_and(worse) || reads.is_some_and(worse))
}

/// When a plan ran, from its first to its last run (epoch seconds).
pub(super) fn plan_span(p: &PlanVariant) -> Option<(f64, f64)> {
    Some((parse_epoch_seconds(p.first_seen.as_deref()?)?, parse_epoch_seconds(p.last_seen.as_deref()?)?))
}

/// The plan a query runs with now, much slower than one it ran with before and stopped using. Both claims need times:
/// the fast plan's last run comes before the slow plan's first, and no other plan ran alongside the slow one (plans
/// used side by side for different values are parameter sensitivity, not a plan that changed).
pub(super) fn replaced_plan(q: &QueryStat) -> Option<(&PlanVariant, &PlanVariant)> {
    let usable: Vec<&PlanVariant> = q.plans.iter().filter(|p| p.executions >= PLAN_MIN_RUNS).collect();
    if usable.len() < 2 {
        return None;
    }
    let spans: Vec<(f64, f64)> = usable.iter().map(|p| plan_span(p)).collect::<Option<_>>()?;
    let (latest_at, (latest_first, latest_last)) =
        spans.iter().copied().enumerate().max_by(|a, b| a.1.1.total_cmp(&b.1.1))?;
    let latest = usable[latest_at];
    let overlaps = spans
        .iter()
        .enumerate()
        .any(|(i, (first, last))| i != latest_at && *first <= latest_last && latest_first <= *last);
    if overlaps || latest.is_forced {
        return None;
    }
    let best = usable
        .iter()
        .zip(&spans)
        .filter(|(p, (_, last))| *last < latest_first && p.executions >= BASELINE_MIN_RUNS)
        .map(|(p, _)| *p)
        .min_by(|a, b| a.mean_time_ms.total_cmp(&b.mean_time_ms))?;
    (!best.is_forced
        && latest.mean_time_ms >= best.mean_time_ms * 2.0
        && latest.mean_time_ms - best.mean_time_ms >= 5.0
        && does_more_work(best, latest) != Some(false))
    .then_some((best, latest))
}

pub fn plan_regression(snapshot: &Snapshot) -> Vec<Finding> {
    let Some(workload) = snapshot.workload.as_ref() else { return Vec::new() };
    app_queries(snapshot)
        .filter_map(|q| {
            let (best, latest) = replaced_plan(q)?;
            let impact = query_impact(snapshot, &[q])?;
            let factor = latest.mean_time_ms / best.mean_time_ms.max(0.001);
            let wasted = (latest.mean_time_ms - best.mean_time_ms) * latest.executions;
            // The same bar as `regression`: the slow plan's extra time, or its own time, is a real part of the workload.
            let (wasted_share, latest_share) = (workload.share_of(wasted, q), workload.share_of(latest.total_time_ms, q));
            if wasted_share < 0.01 && latest_share < 0.03 {
                return None;
            }
            let mut f = finding("plan-regression", &q.id, Category::Queries, subject(SubjectKind::Query, &q.id));
            apply_impact(snapshot, &mut f, &impact);
            let wasted_per_day = workload.per_day_measured(wasted);
            f.impact.time_ms_per_day = wasted_per_day;
            f.impact.score = score_for_share(latest_share.max(wasted_share));
            f.impact.label = format!("{} slower", format::factor(factor));
            // Its time a day since it took over, so a regression that began this morning isn't diluted by the week.
            let since = latest.first_seen.as_deref().and_then(parse_epoch_seconds);
            let end = workload.window_end.as_deref().or(Some(snapshot.taken_at.as_str())).and_then(parse_epoch_seconds);
            let amount = match (since, end) {
                (Some(since), Some(end)) => severity_for_recent_time((end - since) / 3600.0, latest.total_time_ms, 1.0),
                _ => severity_for_time(snapshot, latest.total_time_ms, 1.0),
            };
            f.severity = severity_for_share(latest_share).min(amount);
            if factor >= 3.0 && wasted_share >= 0.05 {
                f.severity = f.severity.max(Severity::Medium.min(amount));
            }
            f.confidence = Confidence::High;
            f.title = format!("{} got {} slower after its plan changed", capitalize(&query_handle(q)), format::factor(factor));
            let when = |at: Option<&str>| at.map(crate::queries::short_when).unwrap_or_default();
            f.summary = format!(
                "Query Store saw this query run with plan {} at {} on average until {}, and with plan {} at {} since {} ({} runs). The plan choice changed, typically after a statistics update, a schema change or a recompile with unusual parameter values.",
                best.id,
                format::duration(best.mean_time_ms),
                when(best.last_seen.as_deref()),
                latest.id,
                format::duration(latest.mean_time_ms),
                when(latest.first_seen.as_deref()),
                format::count(latest.executions)
            );
            f.queries = query_refs(&[q]);
            f.metrics.push(Metric::new("Fast plan", best.mean_time_ms, Unit::Ms));
            f.metrics.push(Metric::new("Current plan", latest.mean_time_ms, Unit::Ms));
            if let Some(extra) = wasted_per_day {
                f.metrics.push(Metric::new("Extra time per day", extra, Unit::Ms));
            }
            f.fix = Some(Fix {
                summary: "Force the fast plan now to stop the bleeding, then find out why the optimizer changed its mind (statistics, parameter values, schema). Unforce once the cause is fixed.".into(),
                sql: Some(format!("EXEC sp_query_store_force_plan @query_id = {}, @plan_id = {};", q.id, best.id)),
                cost: None,
            });
            f.verify = vec!["The query's average duration in Query Store should return to the fast plan's level within an interval or two.".into()];
            f.caveats.push("A forced plan can fail to apply after schema changes; Query Store reports that in sys.query_store_plan.last_force_failure_reason_desc.".into());
            Some(f)
        })
        .collect()
}
