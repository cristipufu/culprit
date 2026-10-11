//! "What changed": two periods of a workload side by side, every query judged with the care Query Store deserves.
//!
//! Averages are weighted by executions, a query needs enough runs in both periods to be compared, and a period the
//! source has no data for is never read as zero. The comparison itself is a pure function of what a source measured,
//! so Query Store intervals and Culprit's own scan history go through the same rules and the same wording.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::model::{Engine, TimelineSource};

/// A span of time, UTC ISO 8601, as the source could actually measure it (snapped to Query Store intervals or scans).
#[derive(Serialize, Deserialize, TS, Clone, Debug, Default, PartialEq)]
#[ts(export)]
pub struct Period {
    pub start: String,
    pub end: String,
}

impl Period {
    pub fn hours(&self) -> f64 {
        match (epoch(&self.start), epoch(&self.end)) {
            (Some(a), Some(b)) if b > a => (b - a) / 3600.0,
            _ => 0.0,
        }
    }
}

/// What ran in one period: execution-weighted, so the average is the average run, not the average interval.
#[derive(Serialize, Deserialize, TS, Clone, Debug, Default, PartialEq)]
#[ts(export)]
pub struct PeriodStats {
    pub executions: f64,
    pub total_ms: f64,
    pub mean_ms: f64,
    /// Fastest and slowest single run, where the source keeps them (Query Store does; running totals don't).
    pub min_ms: Option<f64>,
    pub max_ms: Option<f64>,
}

impl PeriodStats {
    pub fn new(executions: f64, total_ms: f64) -> PeriodStats {
        let mean_ms = if executions > 0.0 { total_ms / executions } else { 0.0 };
        PeriodStats { executions, total_ms, mean_ms, min_ms: None, max_ms: None }
    }

    /// Merge two partial aggregates (plans, intervals) of the same period.
    pub fn add(&mut self, other: &PeriodStats) {
        self.executions += other.executions;
        self.total_ms += other.total_ms;
        self.mean_ms = if self.executions > 0.0 { self.total_ms / self.executions } else { 0.0 };
        self.min_ms = min_opt(self.min_ms, other.min_ms);
        self.max_ms = max_opt(self.max_ms, other.max_ms);
    }
}

fn min_opt(a: Option<f64>, b: Option<f64>) -> Option<f64> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

fn max_opt(a: Option<f64>, b: Option<f64>) -> Option<f64> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (a, b) => a.or(b),
    }
}

/// One plan (or, for sources without plan history, one plan shape Culprit captured) in both periods.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct PlanPeriods {
    pub id: String,
    /// Where the plan spends its effort, e.g. "Seq Scan on orders", when known.
    pub label: Option<String>,
    pub baseline: Option<PeriodStats>,
    pub recent: Option<PeriodStats>,
    /// When the plan first ran inside the recent period (UTC ISO 8601).
    pub first_recent: Option<String>,
}

/// What a source measured for one query in each period, before Culprit judges it.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct QueryPeriods {
    pub query_id: String,
    pub text: String,
    pub object_name: Option<String>,
    pub baseline: Option<PeriodStats>,
    pub recent: Option<PeriodStats>,
    pub plans: Vec<PlanPeriods>,
    /// Why a missing period can't be read as "didn't run" (sources that only track their heaviest queries). With a
    /// reason, a query seen in one period only is not comparable rather than new or stopped.
    pub untracked: Option<String>,
}

/// Everything a source measured for a comparison.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct PeriodData {
    pub source: TimelineSource,
    pub baseline: Period,
    pub recent: Period,
    pub baseline_total: PeriodStats,
    pub recent_total: PeriodStats,
    /// Share of the baseline period the source has data for (0–1). Below one half, a query missing from the baseline
    /// may simply not have been recorded, so it is "not comparable" rather than "new".
    pub baseline_coverage: f64,
    pub queries: Vec<QueryPeriods>,
    pub notes: Vec<String>,
    /// Why the source can't compare these periods at all (too few scans, no data inside them), when it can't.
    #[serde(default)]
    pub unavailable: Option<String>,
}

/// How Culprit judges one query between the periods.
#[derive(Serialize, Deserialize, TS, Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(rename_all = "kebab-case")]
#[ts(export)]
pub enum ChangeStatus {
    Regressed,
    New,
    Improved,
    Stopped,
    LowSample,
    NotComparable,
    Unchanged,
}

impl ChangeStatus {
    pub fn label(self) -> &'static str {
        match self {
            ChangeStatus::Regressed => "Got slower",
            ChangeStatus::New => "New",
            ChangeStatus::Improved => "Got faster",
            ChangeStatus::Stopped => "Stopped running",
            ChangeStatus::LowSample => "Too few runs",
            ChangeStatus::NotComparable => "Not comparable",
            ChangeStatus::Unchanged => "About the same",
        }
    }
}

/// The plan a query used most changed between the periods.
#[derive(Serialize, Deserialize, TS, Clone, Debug, Default, PartialEq)]
#[ts(export)]
pub struct PlanSwitch {
    pub from: String,
    pub to: String,
    pub from_label: Option<String>,
    pub to_label: Option<String>,
    /// When the new plan first ran in the recent period.
    pub at: Option<String>,
    /// What differs, in a few words ("Index Seek → Index Scan on customers"), when Culprit compared the two plans.
    pub what: Option<String>,
}

#[derive(Serialize, Deserialize, TS, Clone, Debug, PartialEq)]
#[ts(export)]
pub struct QueryChange {
    pub query_id: String,
    pub text: String,
    pub object_name: Option<String>,
    pub status: ChangeStatus,
    pub baseline: Option<PeriodStats>,
    pub recent: Option<PeriodStats>,
    /// Recent average per run over the baseline's, when both periods have enough runs.
    pub ratio: Option<f64>,
    /// Database time the change cost (slower, new) or saved (faster, stopped) over the recent period, in ms.
    pub impact_ms: f64,
    /// Share of the recent period's database time.
    pub recent_share: f64,
    pub plan_switch: Option<PlanSwitch>,
    /// The plan stayed the same in both periods (only said when the source keeps plan history).
    pub same_plan: bool,
    pub headline: String,
    /// Why it couldn't be compared, when it couldn't.
    pub reason: Option<String>,
    /// Changes around then that may explain it (a dropped index on a table it reads, a deploy, throttling), likeliest
    /// first; ids point into `Changes::events`.
    #[serde(default)]
    pub context: Vec<crate::events::EventLink>,
}

#[derive(Serialize, Deserialize, TS, Clone, Debug, PartialEq)]
#[ts(export)]
pub struct Changes {
    pub connection: String,
    pub engine: Engine,
    pub source: TimelineSource,
    pub baseline: Period,
    pub recent: Period,
    pub baseline_total: PeriodStats,
    pub recent_total: PeriodStats,
    pub baseline_coverage: f64,
    pub min_executions: f64,
    /// How much slower or faster (×) counts as a change.
    pub threshold: f64,
    /// One or two sentences about the whole workload.
    pub summary: String,
    /// Changed, new, stopped and not comparable queries, most important first within each status.
    pub queries: Vec<QueryChange>,
    /// Queries that ran about the same in both periods (the heaviest few are listed in `queries`).
    pub unchanged: usize,
    pub notes: Vec<String>,
    /// What changed around the two periods (marks, schema, settings, restarts, throttling), oldest first.
    #[serde(default)]
    pub events: Vec<crate::events::ChangeEvent>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct CompareOptions {
    /// Runs a query needs in each period before its averages are compared.
    pub min_executions: f64,
    /// Ratio of averages that counts as slower (and its inverse as faster).
    pub threshold: f64,
    /// Smallest change per run that matters, in ms (2× slower from 0.01 ms to 0.02 ms is noise).
    pub min_change_ms: f64,
    /// Unchanged queries kept in the list, heaviest first.
    pub keep_unchanged: usize,
}

impl Default for CompareOptions {
    fn default() -> Self {
        CompareOptions { min_executions: 10.0, threshold: 1.5, min_change_ms: 1.0, keep_unchanged: 15 }
    }
}

/// Judge every query between the two periods.
pub fn compare(connection: &str, engine: Engine, data: PeriodData, options: &CompareOptions) -> Changes {
    let min = options.min_executions.max(1.0);
    let covered = data.baseline_coverage >= 0.5;
    let recent_total_ms = data.recent_total.total_ms;
    let mut judged: Vec<QueryChange> = data
        .queries
        .into_iter()
        .filter_map(|q| judge(q, engine, data.source, covered, recent_total_ms, min, options))
        .collect();
    judged.sort_by(|a, b| a.status.cmp(&b.status).then(b.impact_ms.total_cmp(&a.impact_ms)));
    let unchanged = judged.iter().filter(|q| q.status == ChangeStatus::Unchanged).count();
    let mut kept_unchanged = 0;
    judged.retain(|q| {
        if q.status != ChangeStatus::Unchanged {
            return true;
        }
        kept_unchanged += 1;
        kept_unchanged <= options.keep_unchanged
    });

    let mut notes = data.notes;
    // A comparison that couldn't be made has no coverage to describe: its reason leads the summary instead.
    if data.unavailable.is_none() && data.baseline_coverage < 0.999 && data.baseline.hours() > 0.0 {
        notes.push(match data.source {
            TimelineSource::QueryStore => format!(
                "Query Store has data for {} of the baseline period; queries missing from it may not have been recorded.",
                crate::format::percent(data.baseline_coverage)
            ),
            TimelineSource::Scans => format!(
                "Culprit's scans cover {} of the baseline period; it follows the scans it has.",
                crate::format::percent(data.baseline_coverage)
            ),
            TimelineSource::SlowLog => format!(
                "The slow query log covers {} of the baseline period; statements missing from it may not have been logged.",
                crate::format::percent(data.baseline_coverage)
            ),
        });
    }
    // A comparison that couldn't be made says so first, rather than "no queries ran".
    let summary = match &data.unavailable {
        Some(why) => format!("Can't compare yet. {why}"),
        None => summarize(&judged, &data.baseline, &data.recent, &data.baseline_total, &data.recent_total),
    };
    Changes {
        connection: connection.to_string(),
        engine,
        source: data.source,
        baseline: data.baseline,
        recent: data.recent,
        baseline_total: data.baseline_total,
        recent_total: data.recent_total,
        baseline_coverage: data.baseline_coverage,
        min_executions: min,
        threshold: options.threshold,
        summary,
        queries: judged,
        unchanged,
        notes,
        events: Vec::new(),
    }
}

fn judge(
    q: QueryPeriods,
    engine: Engine,
    source: TimelineSource,
    covered: bool,
    recent_total_ms: f64,
    min: f64,
    options: &CompareOptions,
) -> Option<QueryChange> {
    let b = q.baseline.clone().filter(|s| s.executions > 0.0);
    let r = q.recent.clone().filter(|s| s.executions > 0.0);
    let recent_share = r.as_ref().map(|r| share(r.total_ms, recent_total_ms)).unwrap_or(0.0);
    let one_sided = b.is_none() != r.is_none();
    // A handful of runs seen in one period only (a one-off script, a migration) is noise unless it was heavy.
    let side_runs = b.as_ref().or(r.as_ref()).map(|s| s.executions).unwrap_or(0.0);
    if one_sided && side_runs < min && recent_share < 0.01 {
        return None;
    }
    let (status, ratio, impact_ms) = match (&b, &r) {
        (None, None) => return None,
        (None, Some(r)) if one_sided && q.untracked.is_some() => (ChangeStatus::NotComparable, None, r.total_ms),
        (Some(b), None) if one_sided && q.untracked.is_some() && b.executions >= min => {
            (ChangeStatus::NotComparable, None, b.total_ms)
        }
        (None, Some(r)) if !covered => (ChangeStatus::NotComparable, None, r.total_ms),
        (None, Some(r)) if r.executions >= min => (ChangeStatus::New, None, r.total_ms),
        (None, Some(r)) => (ChangeStatus::LowSample, None, r.total_ms),
        (Some(b), None) if b.executions >= min => (ChangeStatus::Stopped, None, b.total_ms),
        // A handful of runs that didn't come back is noise, not news.
        (Some(_), None) => return None,
        (Some(b), Some(r)) if b.executions < min || r.executions < min => (ChangeStatus::LowSample, None, r.total_ms),
        (Some(b), Some(r)) => {
            let ratio = if b.mean_ms > 0.0 { r.mean_ms / b.mean_ms } else { f64::INFINITY };
            let diff = r.mean_ms - b.mean_ms;
            if ratio >= options.threshold && diff >= options.min_change_ms {
                (ChangeStatus::Regressed, Some(ratio), diff * r.executions)
            } else if ratio <= 1.0 / options.threshold && -diff >= options.min_change_ms {
                (ChangeStatus::Improved, Some(ratio), -diff * r.executions)
            } else {
                (ChangeStatus::Unchanged, Some(ratio), r.total_ms)
            }
        }
    };
    let (plan_switch, same_plan) = plan_switch(&q.plans);
    let reason = match status {
        ChangeStatus::NotComparable => Some(q.untracked.clone().unwrap_or_else(|| match source {
            TimelineSource::QueryStore => "Query Store has no data for the baseline period".to_string(),
            TimelineSource::Scans => "Culprit has no scan covering the baseline period".to_string(),
            TimelineSource::SlowLog => "The slow query log has no runs of it in the baseline period".to_string(),
        })),
        _ => None,
    };
    let mut change = QueryChange {
        query_id: q.query_id,
        text: q.text,
        object_name: q.object_name,
        status,
        baseline: b,
        recent: r,
        ratio: ratio.filter(|r| r.is_finite()),
        impact_ms,
        recent_share,
        plan_switch,
        same_plan,
        headline: String::new(),
        reason,
        context: Vec::new(),
    };
    change.headline = headline(&change, engine, source, min);
    Some(change)
}

/// The plan each period used most, and whether that changed. Plans need runs in both periods to say "same plan".
fn plan_switch(plans: &[PlanPeriods]) -> (Option<PlanSwitch>, bool) {
    let busiest = |pick: fn(&PlanPeriods) -> Option<&PeriodStats>| {
        plans
            .iter()
            .filter_map(|p| Some((p, pick(p)?.executions)))
            .filter(|(_, runs)| *runs > 0.0)
            .max_by(|a, b| a.1.total_cmp(&b.1))
            .map(|(p, _)| p)
    };
    let (Some(before), Some(after)) = (busiest(|p| p.baseline.as_ref()), busiest(|p| p.recent.as_ref())) else {
        return (None, false);
    };
    if before.id == after.id {
        return (None, true);
    }
    let switch = PlanSwitch {
        from: before.id.clone(),
        to: after.id.clone(),
        from_label: before.label.clone(),
        to_label: after.label.clone(),
        at: after.first_recent.clone(),
        what: match (&before.label, &after.label) {
            (Some(a), Some(b)) if a != b => Some(format!("{a} → {b}")),
            _ => None,
        },
    };
    (Some(switch), false)
}

fn share(part: f64, whole: f64) -> f64 {
    if whole > 0.0 { (part / whole).clamp(0.0, 1.0) } else { 0.0 }
}

/// "4.1×" for a ratio.
pub fn times(ratio: f64) -> String {
    if ratio >= 10.0 { format!("{ratio:.0}×") } else { format!("{ratio:.1}×") }
}

/// Plain-language sentence for one query. Times are UTC here; the app shows them in local time.
pub fn headline(c: &QueryChange, engine: Engine, source: TimelineSource, min: f64) -> String {
    use crate::format::{count, percent};
    let per_run =
        |b: &PeriodStats, r: &PeriodStats| format!("{} → {} per run", run_time(b.mean_ms), run_time(r.mean_ms));
    let switch_words = |s: &PlanSwitch| -> String {
        let when = s.at.as_deref().map(|t| format!(" on {}", short_time(t))).unwrap_or_default();
        let what = s.what.as_deref().map(|w| format!(" ({w})")).unwrap_or_default();
        match (engine, source) {
            (Engine::Mssql, TimelineSource::QueryStore) => {
                format!("{when}, when SQL Server switched from plan {} to plan {}{what}", s.from, s.to)
            }
            _ => format!("{when}, when the plan Culprit captured for it changed{what}"),
        }
    };
    match (c.status, &c.baseline, &c.recent) {
        (ChangeStatus::Regressed, Some(b), Some(r)) => {
            let ratio = c.ratio.map(times).unwrap_or_else(|| "much".to_string());
            match &c.plan_switch {
                Some(s) => format!("Got {ratio} slower ({}){}.", per_run(b, r), switch_words(s)),
                None if c.same_plan => {
                    let plan =
                        if source == TimelineSource::Scans { "the same estimated plan" } else { "the same plan" };
                    format!(
                        "Got {ratio} slower ({}) with {plan}: more data, blocking or a busier server.",
                        per_run(b, r)
                    )
                }
                None => format!("Got {ratio} slower ({}).", per_run(b, r)),
            }
        }
        (ChangeStatus::Improved, Some(b), Some(r)) => {
            let ratio = c.ratio.map(|x| times(1.0 / x)).unwrap_or_default();
            match &c.plan_switch {
                Some(s) => format!("Got {ratio} faster ({}){}.", per_run(b, r), switch_words(s)),
                None => format!("Got {ratio} faster ({}).", per_run(b, r)),
            }
        }
        (ChangeStatus::New, _, Some(r)) => format!(
            "Started running: {} runs averaging {}, {} of recent database time.",
            count(r.executions),
            run_time(r.mean_ms),
            percent(c.recent_share)
        ),
        (ChangeStatus::Stopped, Some(b), _) => {
            format!("Stopped running: it ran {} before, averaging {}.", times_word(b.executions), run_time(b.mean_ms))
        }
        (ChangeStatus::LowSample, b, r) => format!(
            "Too few runs to compare: {} before and {} after (Culprit needs {} in each).",
            count(b.as_ref().map(|s| s.executions).unwrap_or(0.0)),
            count(r.as_ref().map(|s| s.executions).unwrap_or(0.0)),
            count(min)
        ),
        (ChangeStatus::NotComparable, b, r) => {
            let why = c.reason.as_deref().unwrap_or("there is no data for the other period");
            match (b, r) {
                (Some(b), None) => {
                    format!("Ran {} before; can't tell whether it still runs: {why}.", times_word(b.executions))
                }
                (_, r) => format!(
                    "Ran {} recently, but can't be compared: {why}.",
                    times_word(r.as_ref().map(|s| s.executions).unwrap_or(0.0))
                ),
            }
        }
        (_, Some(b), Some(r)) => format!("About the same ({}).", per_run(b, r)),
        _ => c.status.label().to_string(),
    }
}

fn summarize(
    queries: &[QueryChange],
    baseline: &Period,
    recent: &Period,
    baseline_total: &PeriodStats,
    recent_total: &PeriodStats,
) -> String {
    use crate::format::duration;
    let per_hour = |s: &PeriodStats, p: &Period| if p.hours() > 0.0 { s.total_ms / p.hours() } else { 0.0 };
    let (before, after) = (per_hour(baseline_total, baseline), per_hour(recent_total, recent));
    // Rates per hour are only honest when the periods are at least that long; otherwise say what was measured.
    let short = baseline.hours() < 1.0 || recent.hours() < 1.0;
    let count = |status: ChangeStatus| queries.iter().filter(|q| q.status == status).count();
    let (slower, new, faster) =
        (count(ChangeStatus::Regressed), count(ChangeStatus::New), count(ChangeStatus::Improved));
    let workload = if before > 0.0 && after > 0.0 {
        let ratio = after / before;
        let trend = if ratio >= 1.15 {
            format!("rose {}", times(ratio))
        } else if ratio <= 1.0 / 1.15 {
            format!("fell to {}", crate::format::percent(ratio))
        } else {
            "stayed about the same".to_string()
        };
        if short {
            format!(
                "Database time {trend} for the time covered: {} over {} before, {} over {} since.",
                duration(baseline_total.total_ms),
                span_words(baseline.hours()),
                duration(recent_total.total_ms),
                span_words(recent.hours())
            )
        } else {
            format!("Database time per hour {trend}: {} → {}.", duration(before), duration(after))
        }
    } else if after > 0.0 {
        format!("Database time per hour: {} recently; nothing to compare it with before.", duration(after))
    } else {
        "No queries ran in the recent period.".to_string()
    };
    let plural = |n: usize, one: &str, many: &str| if n == 1 { format!("1 {one}") } else { format!("{n} {many}") };
    let parts: Vec<String> = [
        (slower, "query got slower", "queries got slower"),
        (new, "query is new", "queries are new"),
        (faster, "query got faster", "queries got faster"),
    ]
    .into_iter()
    .filter(|(n, _, _)| *n > 0)
    .map(|(n, one, many)| plural(n, one, many))
    .collect();
    if parts.is_empty() {
        format!("{workload} No query changed by more than the threshold.")
    } else {
        format!("{workload} {}.", join_words(&parts))
    }
}

/// "3 s", "12 min", "5 h".
fn span_words(hours: f64) -> String {
    crate::format::duration(hours * 3_600_000.0)
}

fn join_words(parts: &[String]) -> String {
    match parts {
        [] => String::new(),
        [one] => capitalize(one),
        [rest @ .., last] => format!("{} and {last}", capitalize(&rest.join(", "))),
    }
}

fn capitalize(text: &str) -> String {
    let mut chars = text.chars();
    chars.next().map(|c| c.to_uppercase().collect::<String>() + chars.as_str()).unwrap_or_default()
}

/// A single run's duration with enough precision to compare two of them ("8.2 ms", not "8 ms").
pub fn run_time(ms: f64) -> String {
    if (1.0..10.0).contains(&ms) { format!("{ms:.1} ms") } else { crate::format::duration(ms) }
}

/// "once", "twice", "1,240 times".
fn times_word(n: f64) -> String {
    match n.round() as i64 {
        1 => "once".into(),
        2 => "twice".into(),
        _ => format!("{} times", crate::format::count(n)),
    }
}

/// "Tue 14:05 UTC".
pub fn short_time(iso: &str) -> String {
    chrono::DateTime::parse_from_rfc3339(iso)
        .map(|t| t.with_timezone(&chrono::Utc).format("%a %H:%M UTC").to_string())
        .unwrap_or_else(|_| iso.to_string())
}

/// "Sun Oct 4 18:00 UTC": [`short_time`] with the date, for ranges longer than a day.
pub fn dated_time(iso: &str) -> String {
    chrono::DateTime::parse_from_rfc3339(iso)
        .map(|t| t.with_timezone(&chrono::Utc).format("%a %b %-d %H:%M UTC").to_string())
        .unwrap_or_else(|_| iso.to_string())
}

/// Both ends of a range: weekday and time within a day, with dates once it spans more than one.
pub fn range_times(start: &str, end: &str) -> (String, String) {
    let long = match (epoch(start), epoch(end)) {
        (Some(a), Some(b)) => (b - a).abs() > 20.0 * 3600.0,
        _ => false,
    };
    if long { (dated_time(start), dated_time(end)) } else { (short_time(start), short_time(end)) }
}

/// `2026-10-11 14:05:00` (UTC) for an ISO 8601 time, to embed in SQL. `None` for anything else, so only a time Culprit
/// formatted itself reaches a statement.
pub fn sql_timestamp(iso: &str) -> Option<String> {
    chrono::DateTime::parse_from_rfc3339(iso)
        .ok()
        .map(|t| t.with_timezone(&chrono::Utc).format("%Y-%m-%d %H:%M:%S").to_string())
}

pub fn epoch(text: &str) -> Option<f64> {
    chrono::DateTime::parse_from_rfc3339(text).ok().map(|t| t.timestamp_millis() as f64 / 1000.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges_longer_than_a_day_carry_dates() {
        let (a, b) = range_times("2026-10-04T18:00:00Z", "2026-10-11T18:00:00Z");
        assert_eq!((a.as_str(), b.as_str()), ("Sun Oct 4 18:00 UTC", "Sun Oct 11 18:00 UTC"));
        let (a, b) = range_times("2026-10-11T12:00:00Z", "2026-10-11T18:00:00Z");
        assert_eq!((a.as_str(), b.as_str()), ("Sun 12:00 UTC", "Sun 18:00 UTC"));
    }

    fn stats(runs: f64, mean: f64) -> PeriodStats {
        PeriodStats::new(runs, runs * mean)
    }

    fn data(queries: Vec<QueryPeriods>, coverage: f64) -> PeriodData {
        let mut baseline_total = PeriodStats::default();
        let mut recent_total = PeriodStats::default();
        for q in &queries {
            if let Some(b) = &q.baseline {
                baseline_total.add(b);
            }
            if let Some(r) = &q.recent {
                recent_total.add(r);
            }
        }
        PeriodData {
            source: TimelineSource::QueryStore,
            baseline: Period { start: "2026-10-11T06:00:00Z".into(), end: "2026-10-11T10:00:00Z".into() },
            recent: Period { start: "2026-10-11T10:00:00Z".into(), end: "2026-10-11T14:00:00Z".into() },
            baseline_total,
            recent_total,
            baseline_coverage: coverage,
            queries,
            notes: vec![],
            unavailable: None,
        }
    }

    fn query(id: &str, b: Option<PeriodStats>, r: Option<PeriodStats>) -> QueryPeriods {
        QueryPeriods { query_id: id.into(), text: "SELECT 1".into(), baseline: b, recent: r, ..Default::default() }
    }

    #[test]
    fn statuses_follow_the_rules() {
        let changes = compare(
            "shop",
            Engine::Mssql,
            data(
                vec![
                    query("slower", Some(stats(100.0, 2.0)), Some(stats(120.0, 8.2))),
                    query("faster", Some(stats(100.0, 40.0)), Some(stats(100.0, 16.0))),
                    query("same", Some(stats(100.0, 5.0)), Some(stats(100.0, 5.5))),
                    query("new", None, Some(stats(50.0, 3.0))),
                    query("gone", Some(stats(50.0, 3.0)), None),
                    query("few", Some(stats(3.0, 3.0)), Some(stats(500.0, 30.0))),
                    query("tiny", Some(stats(100.0, 0.01)), Some(stats(100.0, 0.05))),
                    query("blip", Some(stats(2.0, 3.0)), None),
                ],
                1.0,
            ),
            &CompareOptions::default(),
        );
        let status = |id: &str| changes.queries.iter().find(|q| q.query_id == id).map(|q| q.status);
        assert_eq!(status("slower"), Some(ChangeStatus::Regressed));
        assert_eq!(status("faster"), Some(ChangeStatus::Improved));
        assert_eq!(status("same"), Some(ChangeStatus::Unchanged));
        assert_eq!(status("new"), Some(ChangeStatus::New));
        assert_eq!(status("gone"), Some(ChangeStatus::Stopped));
        assert_eq!(status("few"), Some(ChangeStatus::LowSample), "3 runs before is too few");
        assert_eq!(status("tiny"), Some(ChangeStatus::Unchanged), "5× of 0.01 ms is noise");
        assert_eq!(status("blip"), None);
        assert_eq!(changes.queries[0].query_id, "slower", "regressions come first");
        assert_eq!(changes.unchanged, 2);
        assert_eq!(changes.queries[0].headline, "Got 4.1× slower (2.0 ms → 8.2 ms per run).");
        assert!(changes.summary.contains("1 query got slower"), "{}", changes.summary);
    }

    #[test]
    fn a_missing_baseline_is_never_zero() {
        let only_recent = vec![query("new", None, Some(stats(50.0, 3.0)))];
        let changes = compare("shop", Engine::Mssql, data(only_recent, 0.2), &CompareOptions::default());
        assert_eq!(changes.queries[0].status, ChangeStatus::NotComparable);
        assert!(changes.notes[0].contains("20%"), "{:?}", changes.notes);
    }

    #[test]
    fn untracked_queries_are_not_comparable() {
        let mut q = query("seen-late", None, Some(stats(80.0, 3.0)));
        q.untracked = Some("Culprit's earlier scans didn't track it".into());
        let changes = compare("shop", Engine::Postgres, data(vec![q], 1.0), &CompareOptions::default());
        assert_eq!(changes.queries[0].status, ChangeStatus::NotComparable);
        assert!(changes.queries[0].headline.contains("didn't track it"), "{}", changes.queries[0].headline);
    }

    #[test]
    fn short_periods_report_what_was_measured() {
        let mut d = data(vec![query("q", Some(stats(100.0, 1.0)), Some(stats(100.0, 3.0)))], 1.0);
        d.baseline = Period { start: "2026-10-11T10:00:00Z".into(), end: "2026-10-11T10:00:02Z".into() };
        d.recent = Period { start: "2026-10-11T10:00:02Z".into(), end: "2026-10-11T10:00:05Z".into() };
        let changes = compare("shop", Engine::Postgres, d, &CompareOptions::default());
        assert!(changes.summary.contains("for the time covered"), "{}", changes.summary);
        assert!(!changes.summary.contains("per hour"), "{}", changes.summary);
    }

    #[test]
    fn plan_switches_are_named() {
        let mut q = query("q", Some(stats(100.0, 2.0)), Some(stats(100.0, 8.6)));
        q.plans = vec![
            PlanPeriods {
                id: "815".into(),
                label: Some("Index Seek on customers".into()),
                baseline: Some(stats(100.0, 2.0)),
                recent: Some(stats(5.0, 2.0)),
                first_recent: None,
            },
            PlanPeriods {
                id: "685".into(),
                label: Some("Index Scan on customers".into()),
                baseline: None,
                recent: Some(stats(95.0, 9.0)),
                first_recent: Some("2026-10-13T14:05:00Z".into()),
            },
        ];
        let changes = compare("shop", Engine::Mssql, data(vec![q], 1.0), &CompareOptions::default());
        assert_eq!(
            changes.queries[0].headline,
            "Got 4.3× slower (2.0 ms → 8.6 ms per run) on Tue 14:05 UTC, when SQL Server switched from plan 815 to plan 685 (Index Seek on customers → Index Scan on customers)."
        );
        let mut same = query("s", Some(stats(100.0, 2.0)), Some(stats(100.0, 8.0)));
        same.plans = vec![PlanPeriods {
            id: "1".into(),
            baseline: Some(stats(100.0, 2.0)),
            recent: Some(stats(100.0, 8.0)),
            ..Default::default()
        }];
        let changes = compare("shop", Engine::Mssql, data(vec![same], 1.0), &CompareOptions::default());
        assert!(changes.queries[0].same_plan);
        assert!(changes.queries[0].headline.contains("with the same plan"));
    }
}
