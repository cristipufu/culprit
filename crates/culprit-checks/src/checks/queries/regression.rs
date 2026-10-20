//! Checks over the workload timeline: queries that got slower at a point in time.

use crate::evidence::{QueryMeasure, Requirement as Evidence};
use culprit_core::{
    Category, CheckInfo, Confidence, Engine, Finding, Fix, Metric, PlanVariant, QueryStat, QueryTrend, Severity,
    Snapshot, SubjectKind, Timeline, TimelineSource, Unit, format,
};

use crate::queries::{apply_impact, time_noun};
use crate::support::*;

/// The metric a regression carries when a plan change coincides with it: "815 → 685".
pub(crate) const PLAN_CHANGE: &str = "Plan change";

/// "Much slower": the average run takes at least twice as long as before, and a few milliseconds more.
const MIN_FACTOR: f64 = 2.0;
const MIN_EXTRA_MS: f64 = 2.0;
/// A bucket counts as slow at 1.5× the old average; most buckets after the change must be slow, few before it.
const SLOW_BUCKET: f64 = 1.5;
const MIN_RECENT_SLOW: f64 = 0.7;
const MAX_BASELINE_SLOW: f64 = 0.3;
const MIN_BASELINE_RUNS: f64 = 20.0;
const MIN_RECENT_RUNS: f64 = 10.0;

pub fn regression_info() -> CheckInfo {
    CheckInfo {
        id: "regression".into(),
        title: "Queries that got slower".into(),
        category: Category::Queries,
        engines: vec![Engine::Mssql, Engine::Postgres, Engine::Mysql],
        requires: vec![],
        description: "Queries whose runs became much slower at some point in the window, with when it started and whether a plan change coincides. Needs Query Store history or a few earlier scans.".into(),
    }
}

check!(RegressionCheck: regression_info, regression, &[Evidence::Query(QueryMeasure::Timeline)]);

/// Where a query's average duration stepped up and stayed up.
#[derive(Debug, PartialEq)]
pub(crate) struct Shift {
    /// First bucket of the slow period.
    pub start: usize,
    pub before_ms: f64,
    pub after_ms: f64,
    pub after_runs: f64,
    pub after_time_ms: f64,
}

impl Shift {
    fn factor(&self) -> f64 {
        self.after_ms / self.before_ms.max(0.001)
    }

    fn extra_ms(&self) -> f64 {
        (self.after_ms - self.before_ms) * self.after_runs
    }
}

/// The split of the timeline into a normal "before" and a slow "after" that explains the most extra time.
pub(crate) fn shift(trend: &QueryTrend, buckets: usize) -> Option<Shift> {
    let known: Vec<Option<(f64, f64)>> = (0..buckets)
        .map(|i| {
            let time = (*trend.time_ms.get(i)?)?;
            let runs = (*trend.executions.get(i)?)?;
            (runs > 0.0).then_some((time, runs))
        })
        .collect();
    let totals = |range: std::ops::Range<usize>| {
        known[range].iter().flatten().fold((0.0, 0.0, 0usize), |(t, r, n), (bt, br)| (t + bt, r + br, n + 1))
    };
    let mut best: Option<Shift> = None;
    for start in 2..buckets {
        let (before_time, before_runs, before_buckets) = totals(0..start);
        let (after_time, after_runs, after_buckets) = totals(start..buckets);
        if before_buckets < 2 || after_buckets < 2 || before_runs < MIN_BASELINE_RUNS || after_runs < MIN_RECENT_RUNS {
            continue;
        }
        let before_ms = before_time / before_runs;
        let after_ms = after_time / after_runs;
        if after_ms < before_ms * MIN_FACTOR || after_ms - before_ms < MIN_EXTRA_MS {
            continue;
        }
        let slow = |b: &Option<(f64, f64)>| b.is_some_and(|(t, r)| t / r >= before_ms * SLOW_BUCKET);
        if !slow(&known[start]) {
            continue;
        }
        let slow_after = known[start..].iter().filter(|b| slow(b)).count() as f64 / after_buckets as f64;
        let slow_before = known[..start].iter().filter(|b| slow(b)).count() as f64 / before_buckets as f64;
        if slow_after < MIN_RECENT_SLOW || slow_before > MAX_BASELINE_SLOW {
            continue;
        }
        let candidate = Shift { start, before_ms, after_ms, after_runs, after_time_ms: after_time };
        if best.as_ref().is_none_or(|b| candidate.extra_ms() > b.extra_ms()) {
            best = Some(candidate);
        }
    }
    best
}

pub fn regression(snapshot: &Snapshot) -> Vec<Finding> {
    let Some(workload) = &snapshot.workload else { return Vec::new() };
    let Some(timeline) = workload.timeline.as_ref().filter(|t| t.len() >= 4) else { return Vec::new() };
    // A bucket of the slow query log averages the runs it kept, the slow ones: a wider tail (one lock episode) reads as
    // the statement getting slower while its typical run didn't change.
    if timeline.source == TimelineSource::SlowLog && crate::queries::logs_slow_runs_only(snapshot) {
        return Vec::new();
    }
    let mut findings: Vec<Finding> = app_queries(snapshot)
        .filter_map(|q| {
            // Only the buckets that account for the statement (`Timeline::statement_bucket`): never one the source
            // can't account for in full, nor one where it ran more than the whole database did.
            let trend = q.trend.as_ref()?;
            let (time_ms, executions) = (0..timeline.len())
                .map(|i| timeline.statement_bucket(trend, i).map_or((None, None), |(t, r)| (Some(t), Some(r))))
                .unzip();
            let s = shift(&QueryTrend { time_ms, executions }, timeline.len())?;
            regression_finding(snapshot, timeline, q, &s)
        })
        .collect();
    findings.sort_by(|a, b| b.impact.score.total_cmp(&a.impact.score));
    findings.truncate(6);
    findings
}

fn regression_finding(snapshot: &Snapshot, timeline: &Timeline, q: &QueryStat, s: &Shift) -> Option<Finding> {
    let (since, _) = timeline.bounds(s.start);
    let since_epoch = parse_epoch_seconds(since)?;
    let end_epoch = parse_epoch_seconds(&timeline.end)?;
    let after_hours = ((end_epoch - since_epoch) / 3600.0).max(1.0 / 60.0);
    let workload_after: f64 = timeline.time_ms[s.start..].iter().sum();
    if workload_after <= 0.0 {
        return None;
    }
    let share_after = (s.after_time_ms / workload_after).clamp(0.0, 1.0);
    let extra_share = (s.extra_ms() / workload_after).clamp(0.0, 1.0);
    if extra_share < 0.01 && share_after < 0.03 {
        return None;
    }
    let plan = plan_change(timeline, q, s);
    let when = when_phrase(since);
    let factor = s.factor();

    let mut f = finding("regression", &q.id, Category::Queries, subject(SubjectKind::Query, &q.id));
    if let Some(impact) = query_impact(snapshot, &[q]) {
        apply_impact(snapshot, &mut f, &impact);
    }
    let time = time_noun(snapshot);
    let extra_per_day = s.extra_ms() * 24.0 / after_hours;
    // A day's worth only when the slow period covers most of one; under that it would be an extrapolation.
    f.impact.time_ms_per_day = (after_hours >= culprit_core::PER_DAY_MIN_HOURS).then_some(extra_per_day);
    f.impact.score = score_for_share(share_after.max(extra_share));
    f.impact.label = format!("{} slower", times(factor));
    // Runs that came in bursts are graded on their total, never extrapolated to a day.
    let amount = severity_for_recent_time(burst_hours(snapshot, q, after_hours), s.after_time_ms, 1.0);
    f.severity = severity_for_share(share_after).min(amount);
    if factor >= 3.0 && extra_share >= 0.05 {
        f.severity = f.severity.max(Severity::Medium.min(amount));
    }
    f.confidence = if timeline.source == TimelineSource::QueryStore { Confidence::High } else { Confidence::Medium };
    // A server that started just before the step runs its first hours on a cold cache: slower, and not for good. Under
    // a day of slow runs since then it is something to watch, not a regression yet.
    let restarted = snapshot
        .server
        .started_at
        .as_deref()
        .and_then(parse_epoch_seconds)
        .filter(|at| (since_epoch - 2.0 * 3600.0..=since_epoch + 3600.0).contains(at));
    if restarted.is_some() && after_hours < 24.0 && plan.is_none() {
        f.severity = Severity::Low;
        f.confidence = Confidence::Low;
        f.caveats.push(format!(
            "{} started {}, just before it got slower: the first hours after a start read from disk what was in memory before. If it stays slow once the cache is warm, it's a real change.",
            snapshot.server.name(),
            snapshot.server.started_at.as_deref().map(when_phrase).unwrap_or_default()
        ));
    }
    f.title = format!(
        "{} got {} slower {when}{}",
        capitalize(&query_handle(q)),
        times(factor),
        if plan.is_some() { ", when its plan changed" } else { "" }
    );
    let extra = if after_hours >= 20.0 {
        format!("about {} of extra {time} a day", format::duration(extra_per_day))
    } else {
        format!("{} of extra {time} in {}", format::duration(s.extra_ms()), hours_phrase(after_hours))
    };
    let mut summary = format!(
        "Until {} it averaged {} per run; since then {}, over {} runs: {extra}.",
        short_when(since),
        format::duration(s.before_ms),
        format::duration(s.after_ms),
        format::count(s.after_runs),
    );
    match &plan {
        Some(change) => summary.push_str(&format!(
            " Plan {} first ran {} and averages {}; plan {} averaged {}.",
            change.new.id,
            change.new.first_seen.as_deref().map(when_phrase).unwrap_or_else(|| when.clone()),
            format::duration(change.new.mean_time_ms),
            change.old.id,
            format::duration(change.old.mean_time_ms)
        )),
        // Several plans, none new around the step: it didn't come with a plan change, which isn't "the plan never changed".
        None if q.plans.len() > 1 => summary.push_str(" No new plan started when it got slower, so look at what else changed then: a deploy that changed this statement or its parameters, a jump in data volume, a statistics update, or new blocking."),
        None => summary.push_str(" This scan does not establish which plan the earlier and later runs used, so a plan change remains possible. Compare plans and parameter values, and check for changed data volume, statistics, blocking or application code around that time."),
    }
    f.summary = summary;
    f.queries = query_refs(&[q]);
    f.metrics.push(Metric::new("Before", s.before_ms, Unit::Ms));
    f.metrics.push(Metric::new("Since then", s.after_ms, Unit::Ms));
    if after_hours >= 20.0 {
        f.metrics.push(Metric::new("Extra time per day", extra_per_day, Unit::Ms));
    } else {
        f.metrics.push(Metric::new("Extra time", s.extra_ms(), Unit::Ms));
    }
    f.metrics.push(Metric::text("Started", short_when(since)));
    if let Some(change) = &plan {
        f.metrics.push(Metric::text(PLAN_CHANGE, format!("{} → {}", change.old.id, change.new.id)));
        f.facts.plan_change =
            Some(culprit_core::FindingPlanChange { from: change.old.id.clone(), to: change.new.id.clone() });
    }
    f.fix = Some(match (&plan, snapshot.server.engine) {
        (Some(change), Engine::Mssql) if numeric(&q.id) && numeric(&change.old.id) => Fix {
            summary: format!(
                "Force plan {} to stop the bleeding, then find out why the optimizer switched (statistics, parameter values, schema). Unforce once the cause is fixed.{}",
                change.old.id,
                crate::tuning::hand_forcing_note(snapshot).map(|n| format!(" {n}")).unwrap_or_default()
            ),
            sql: Some(format!("EXEC sp_query_store_force_plan @query_id = {}, @plan_id = {};", q.id, change.old.id)),
            cost: None,
        },
        (Some(_), _) => Fix {
            summary: "The plan changed when it got slower. Compare the two plans (Queries → Plans), refresh statistics on the tables it reads, and check the parameter values the slow runs use.".into(),
            sql: None,
            cost: None,
        },
        (None, _) => Fix {
            summary: format!(
                "Find what changed {when}: a deploy touching this statement, its parameters or its indexes; data growth in the tables it reads; a statistics update; or blocking (Live shows current waits). Open the query to compare its plan with what you expect."
            ),
            sql: None,
            cost: None,
        },
    });
    f.verify = vec![format!(
        "Re-scan after the fix: this query's trend in Queries should drop back to about {} per run.",
        format::duration(s.before_ms)
    )];
    if timeline.source == TimelineSource::Scans {
        f.caveats.push("Culprit built this trend from its own scans. Each point covers the time between two scans, so the start time is only as precise as how often you scan.".into());
    }
    Some(f)
}

struct PlanChange<'a> {
    new: &'a PlanVariant,
    old: &'a PlanVariant,
}

/// A plan that first ran around the start of the slow period and is slower than an earlier one.
fn plan_change<'a>(timeline: &Timeline, q: &'a QueryStat, s: &Shift) -> Option<PlanChange<'a>> {
    if q.plans.len() < 2 {
        return None;
    }
    let (start, end) = timeline.bounds(s.start);
    let (start, end) = (parse_epoch_seconds(start)?, parse_epoch_seconds(end)?);
    let previous = s.start.checked_sub(1).and_then(|i| parse_epoch_seconds(timeline.bounds(i).0)).unwrap_or(start);
    let new = q
        .plans
        .iter()
        .filter(|p| p.executions >= 1.0)
        .filter(|p| p.first_seen.as_deref().and_then(parse_epoch_seconds).is_some_and(|t| t >= previous && t <= end))
        .max_by(|a, b| a.executions.total_cmp(&b.executions))?;
    let old = q
        .plans
        .iter()
        .filter(|p| p.id != new.id && p.executions >= 5.0)
        .filter(|p| p.first_seen.as_deref().and_then(parse_epoch_seconds).is_none_or(|t| t < start))
        .min_by(|a, b| a.mean_time_ms.total_cmp(&b.mean_time_ms))?;
    (new.mean_time_ms >= old.mean_time_ms * SLOW_BUCKET).then_some(PlanChange { new, old })
}

/// "3.4×": how many times slower, as people say it.
fn times(factor: f64) -> String {
    if factor >= 10.0 { format!("{}×", factor.round()) } else { format!("{:.1}×", factor).replace(".0×", "×") }
}

fn numeric(id: &str) -> bool {
    !id.is_empty() && id.chars().all(|c| c.is_ascii_digit())
}

const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];

/// "Oct 9, 14:00 UTC" from an ISO 8601 UTC timestamp.
pub(crate) fn short_when(iso: &str) -> String {
    let month = iso.get(5..7).and_then(|m| m.parse::<usize>().ok()).filter(|m| (1..=12).contains(m));
    let day = iso.get(8..10).and_then(|d| d.parse::<u32>().ok());
    match (month, day, iso.get(11..16)) {
        (Some(m), Some(d), Some(time)) => format!("{} {d}, {time} UTC", MONTHS[m - 1]),
        _ => iso.to_string(),
    }
}

pub(crate) fn when_phrase(iso: &str) -> String {
    let short = short_when(iso);
    match short.split_once(", ") {
        Some((date, time)) => format!("on {date} at {time}"),
        None => format!("at {short}"),
    }
}

fn hours_phrase(hours: f64) -> String {
    if hours >= 1.5 {
        format!("{} hours", hours.round())
    } else {
        format!("{} minutes", (hours * 60.0).round().max(1.0))
    }
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(first) => first.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn trend(means: &[f64], runs: f64) -> QueryTrend {
        QueryTrend {
            time_ms: means.iter().map(|m| Some(m * runs)).collect(),
            executions: means.iter().map(|_| Some(runs)).collect(),
        }
    }

    #[test]
    fn finds_where_a_query_stepped_up() {
        let t = trend(&[10.0, 11.0, 9.0, 10.0, 31.0, 29.0, 30.0], 20.0);
        let s = shift(&t, 7).unwrap();
        assert_eq!(s.start, 4);
        assert!((s.before_ms - 10.0).abs() < 1e-9 && (s.after_ms - 30.0).abs() < 1e-9);
    }

    #[test]
    fn ignores_noise_spikes_and_small_changes() {
        assert!(shift(&trend(&[10.0, 10.0, 60.0, 10.0, 10.0, 11.0], 20.0), 6).is_none(), "one spike");
        assert!(shift(&trend(&[10.0, 10.0, 10.0, 15.0, 15.0, 15.0], 20.0), 6).is_none(), "1.5× is not a regression");
        assert!(shift(&trend(&[0.1, 0.1, 0.1, 0.5, 0.5, 0.5], 20.0), 6).is_none(), "fractions of a millisecond");
        assert!(shift(&trend(&[10.0, 10.0, 10.0, 30.0, 30.0], 2.0), 5).is_none(), "too few runs to tell");
    }

    #[test]
    fn unknown_buckets_are_skipped() {
        let mut t = trend(&[10.0, 10.0, 10.0, 30.0, 30.0, 30.0], 20.0);
        t.time_ms[1] = None;
        t.executions[1] = None;
        assert_eq!(shift(&t, 6).unwrap().start, 3);
    }

    #[test]
    fn dates_read_naturally() {
        assert_eq!(short_when("2026-10-09T14:00:00Z"), "Oct 9, 14:00 UTC");
        assert_eq!(when_phrase("2026-10-09T14:00:00Z"), "on Oct 9 at 14:00 UTC");
        assert_eq!((times(3.42), times(2.0), times(12.4)), ("3.4×".into(), "2×".into(), "12×".into()));
    }
}
