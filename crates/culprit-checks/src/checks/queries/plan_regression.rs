//! The `plan-regression` check: Queries whose current plan is much slower than a plan they used before.

use super::*;
use crate::evidence::{QueryMeasure, Requirement as Evidence};

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

check!(PlanRegressionCheck: plan_regression_info, plan_regression, &[Evidence::Query(QueryMeasure::PlanVariants)]);

/// A plan with this many runs or more counts as one the query used.
pub(super) const PLAN_MIN_RUNS: f64 = culprit_core::PLAN_VARIANT_MIN_RUNS;
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
    let usable: Vec<&PlanVariant> = q.workload_plans().iter().filter(|p| p.executions >= PLAN_MIN_RUNS).collect();
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
            f.facts.plan_change=Some(culprit_core::FindingPlanChange {from:best.id.clone(),to:latest.id.clone()});
            f.facts.plan_baseline = latest.last_seen.as_ref().map(|last| culprit_core::PlanBaseline {
                query: q.id.clone(), mean_ms: best.mean_time_ms, observed_until: last.clone(),
            });
            apply_impact(snapshot, &mut f, &impact);
            // Its time a day since it took over, so a regression that began this morning isn't diluted by the week.
            let since = latest.first_seen.as_deref().and_then(parse_epoch_seconds);
            let end = workload.window_end.as_deref().or(Some(snapshot.taken_at.as_str())).and_then(parse_epoch_seconds);
            // The extra time a day over the hours the slow plan ran, from its first run to its last (to the window's end
            // while it still runs), and only over a day of them: a burst of three minutes is no rate a day.
            let stopped = crate::query_stopped(snapshot, q).is_some();
            let until = if stopped { latest.last_seen.as_deref().and_then(parse_epoch_seconds) } else { end };
            let wasted_per_day = match (since, until) {
                (Some(since), Some(until)) if (until - since) / 3600.0 >= culprit_core::PER_DAY_MIN_HOURS => {
                    Some(wasted * 86_400.0 / (until - since))
                }
                _ => None,
            };
            f.impact.time_ms_per_day = wasted_per_day;
            f.impact.score = score_for_share(latest_share.max(wasted_share));
            f.impact.label = format!("{} slower", format::factor(factor));
            let amount = match (since, end) {
                // Runs that came in bursts are graded on their total, never extrapolated to a day.
                (Some(since), Some(end)) => severity_for_recent_time(
                    burst_hours(snapshot, q, (end - since) / 3600.0),
                    latest.total_time_ms,
                    1.0,
                ),
                _ => severity_for_time_of(snapshot, &[q], latest.total_time_ms, 1.0),
            };
            f.severity = severity_for_share(latest_share).min(amount);
            if factor >= 3.0 && wasted_share >= 0.05 {
                f.severity = f.severity.max(Severity::Medium.min(amount));
            }
            f.confidence = Confidence::High;
            f.title = format!("{} got {} slower after its plan changed", capitalize(&query_handle(q)), format::factor(factor));
            let when = |at: Option<&str>| at.map(crate::queries::short_when).unwrap_or_default();
            // A statement that stopped ran the slow plan for a while, not "since".
            let slow_when = match (stopped, latest.first_seen.as_deref(), latest.last_seen.as_deref()) {
                (true, Some(first), Some(last)) => culprit_core::span_when(first, last),
                _ => format!("since {}", when(latest.first_seen.as_deref())),
            };
            f.summary = format!(
                "Query Store saw this query run with plan {} at {} on average until {}, and with plan {} at {} {slow_when} ({} runs). The plan choice changed, typically after a statistics update, a schema change or a recompile with unusual parameter values.",
                best.id,
                format::duration(best.mean_time_ms),
                when(best.last_seen.as_deref()),
                latest.id,
                format::duration(latest.mean_time_ms),
                format::count(latest.executions)
            );
            f.queries = query_refs(&[q]);
            f.metrics.push(Metric::new("Fast plan", best.mean_time_ms, Unit::Ms));
            // A statement that stopped has no current plan: the slow one is the last it ran with.
            let slow_label = if stopped || table_gone(snapshot, q).is_some() { "Last plan" } else { "Current plan" };
            f.metrics.push(Metric::new(slow_label, latest.mean_time_ms, Unit::Ms));
            match wasted_per_day {
                Some(extra) => f.metrics.push(Metric::new("Extra time per day", extra, Unit::Ms)),
                None => f.metrics.push(Metric::new("Extra time", wasted, Unit::Ms)),
            }
            // A plan forced onto a table that is gone can't apply: there is nothing to force.
            if let Some(gone) = table_gone(snapshot, q) {
                f.actionability = culprit_core::Actionability::History;
                f.fix = Some(Fix {
                    summary: format!(
                        "{gone} is no longer in the catalog, so the statement can't run as it was, and plan {} can't be forced onto it: a forced plan that names a table or an index that is gone fails to apply. Nothing to force; if a statement on the table that replaced it is slow, its own findings say so.",
                        best.id
                    ),
                    sql: None,
                    cost: None,
                });
                f.verify = vec!["Nothing to check unless the statement runs again.".into()];
                return Some(f);
            }
            // Where SQL Server corrects regressions itself, the fix says what it does and what forcing by hand adds.
            let automatic = crate::tuning::hand_forcing_note(snapshot)
                .map(|note| format!(" {note} It hasn't forced plan {} for this one so far.", best.id))
                .unwrap_or_default();
            f.fix = Some(Fix {
                summary: format!("Force the fast plan now to stop the bleeding, then find out why the optimizer changed its mind (statistics, parameter values, schema). Unforce once the cause is fixed.{automatic}"),
                sql: Some(format!("EXEC sp_query_store_force_plan @query_id = {}, @plan_id = {};", q.id, best.id)),
                cost: None,
            });
            f.verify = vec!["The query's average duration in Query Store should return to the fast plan's level within an interval or two.".into()];
            f.caveats.push("A forced plan can fail to apply after schema changes; Query Store reports that in sys.query_store_plan.last_force_failure_reason_desc.".into());
            Some(f)
        })
        .collect()
}

/// A table the statement's plan reads that the catalog no longer has (dropped or renamed since), by its name as the
/// plan writes it. `None` when the scan read no tables at all (the catalog wasn't readable), for temporary tables and
/// table variables, and for a name the plan doesn't qualify with its schema.
fn table_gone(snapshot: &Snapshot, q: &QueryStat) -> Option<String> {
    if snapshot.tables.is_empty() {
        return None;
    }
    q.plan
        .as_ref()?
        .nodes()
        .into_iter()
        .filter(|n| matches!(n.kind, NodeKind::Scan | NodeKind::Seek | NodeKind::Lookup))
        .find_map(|n| {
            let object = n.object.as_deref().filter(|o| o.contains('.'))?;
            let (schema, table) = crate::indexes::split_object(object)?;
            if table.starts_with(['#', '@']) {
                return None;
            }
            snapshot.table(&schema, &table).is_none().then(|| format!("{schema}.{table}"))
        })
}
