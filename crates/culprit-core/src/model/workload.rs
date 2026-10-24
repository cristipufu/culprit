//! Workload, statement statistics, timeline buckets, and rate/cadence evidence math.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::plan::Plan;

use super::WaitStat;

/// What a workload was read from. The rules that depend on it ask this, never the name in `Workload::source`: a slow
/// query log keeps only the runs it logged, performance_schema's histograms count from the server's start, the plan
/// cache forgets what it evicts. A provider sets it; [`Workload::kind`] reads it.
#[derive(Serialize, Deserialize, TS, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
#[ts(export)]
pub enum WorkloadKind {
    /// SQL Server's Query Store: runtime statistics by interval, so a scan picks its window.
    QueryStore,
    /// SQL Server's plan cache: totals since each plan was cached.
    PlanCache,
    /// PostgreSQL's pg_stat_statements: totals since its last reset.
    StatementStats,
    /// MySQL and MariaDB's performance_schema statement digests: totals since the server started or they were reset.
    PerformanceSchema,
    /// MySQL and MariaDB's slow query log table: the runs it logged, one by one.
    SlowLog,
    /// An application's own log of its statements (a Rails or Django log), for an engine that keeps none.
    QueryLog,
}

#[derive(Serialize, Deserialize, TS, Clone, Debug)]
#[ts(export)]
pub struct Workload {
    /// Where the statements were read from, by name, for sentences ("Query Store covers …").
    pub source: String,
    /// The same, typed: see [`Workload::kind`]. `None` in scans saved before it existed.
    #[serde(default)]
    pub kind: Option<WorkloadKind>,
    /// The source keeps history by time, so a scan can choose its window (Query Store), or records a period of its own
    /// (an application's log): its numbers are of that period, never running totals to subtract. Cumulative sources
    /// (pg_stat_statements, the plan cache) cover "since the last reset" and ignore the requested window.
    #[serde(default)]
    pub windowed: bool,
    /// The application's log the statements were read from, by format ("Rails"), for an engine that keeps no statement
    /// history (SQLite). Its period is what the log's own timestamps span, which may have ended long before the scan;
    /// a log without timestamps covers a period nobody can say. `None` when the database's statistics are the source.
    #[serde(default)]
    pub log: Option<String>,
    pub window_start: Option<String>,
    pub window_end: Option<String>,
    pub window_hours: f64,
    pub total_time_ms: f64,
    pub total_cpu_ms: Option<f64>,
    pub total_executions: f64,
    pub query_count: f64,
    pub queries: Vec<QueryStat>,
    /// Database time across the window, oldest first. `None` when the source keeps no history and Culprit has no
    /// earlier scans of this connection to compare with.
    #[serde(default)]
    pub timeline: Option<Timeline>,
    /// Time the source measured but couldn't put on any statement, so it is in neither `queries` nor `total_time_ms`:
    /// MySQL's catch-all digest row once its digest table is full. That row is server-wide (it doesn't say which
    /// database ran it), so shares are of the attributed time and this says how much more there may be. `None` when
    /// nothing was left out or the source can't say.
    #[serde(default)]
    pub untracked_time_ms: Option<f64>,
    /// For a source whose entries count from different moments (the plan cache counts each plan from its compile): the
    /// sum of every entry's time an hour of its own life, in ms. Shares are then weighed by it ([`Workload::share_of`])
    /// so a plan cached for a month doesn't outweigh one compiled an hour ago by its age alone. `None` elsewhere.
    #[serde(default)]
    pub hourly_time_ms: Option<f64>,
    /// Only runs slower than this were recorded (the slow query log's `long_query_time`), so totals, shares and run
    /// counts are of logged slow runs, not of everything the database ran. `None` when every run is counted.
    #[serde(default)]
    pub min_logged_ms: Option<f64>,
    // Typed measurement context; legacy fields remain readable in older reports.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub measurement: Option<crate::measurement::MeasurementContext>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub history_inputs: Vec<crate::BaselineReference>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub counter_set: Option<CounterSet>,
}

/// Native contributors to distributed cumulative counters. A missing epoch or skipped node cannot establish
/// continuity. Node identity is part of the series even when the SQL connection goes through a load balancer.
#[derive(Serialize, Deserialize, TS, Clone, Debug, PartialEq, Eq)]
#[ts(export)]
pub struct CounterSet {
    pub scope: CounterScope,
    pub members: std::collections::BTreeMap<String, Option<String>>,
    pub complete: bool,
}

#[derive(Serialize, Deserialize, TS, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
#[ts(export)]
pub enum CounterScope {
    Node,
    Cluster,
}

impl CounterSet {
    pub fn epochs_known(&self) -> bool {
        self.complete
            && !self.members.is_empty()
            && self.members.values().all(|at| at.as_deref().and_then(crate::format::epoch_seconds).is_some())
    }
}

/// "7 days", "10 hours", "35 minutes": a window's length in words.
pub fn span_words(hours: f64) -> String {
    if hours >= 47.0 {
        return format!("{} days", (hours / 24.0).round());
    }
    if hours >= 1.5 {
        return format!("{} hours", hours.round());
    }
    let minutes = (hours * 60.0).round().max(1.0);
    if minutes == 1.0 { "1 minute".into() } else { format!("{minutes} minutes") }
}

/// Under this many hours of data, a per-day rate would be an extrapolation: say what was measured instead.
pub const PER_DAY_MIN_HOURS: f64 = 20.0;

/// Under this much data a count isn't a rate at all: a threshold on a rate (runs a day, ms a day) needs at least an
/// hour of it, so minutes of counters can't pass for a busy day. Saying "a day" in a sentence needs
/// [`PER_DAY_MIN_HOURS`].
pub const RATE_MIN_HOURS: f64 = 1.0;

impl Workload {
    /// What the workload was read from. Scans saved before providers said so are told by the source's name: the one
    /// place that reads it.
    pub fn kind(&self) -> Option<WorkloadKind> {
        self.kind.or_else(|| match self.source.as_str() {
            "Query Store" => Some(WorkloadKind::QueryStore),
            "Plan cache" => Some(WorkloadKind::PlanCache),
            "pg_stat_statements" => Some(WorkloadKind::StatementStats),
            "performance_schema" => Some(WorkloadKind::PerformanceSchema),
            "slow query log" => Some(WorkloadKind::SlowLog),
            _ if self.log.is_some() => Some(WorkloadKind::QueryLog),
            _ => None,
        })
    }

    /// Hours an entry of this workload has counted: from its first run (a cached plan's compile) to the window's end,
    /// at least a minute. `None` without both times.
    pub fn entry_hours(&self, first_seen: Option<&str>) -> Option<f64> {
        let first = crate::format::epoch_seconds(first_seen?)?;
        let end = crate::format::epoch_seconds(self.window_end.as_deref()?)?;
        Some(((end - first) / 3600.0).max(1.0 / 60.0))
    }

    /// How much of its raw share a statement's time may claim: 1, except in a source whose entries count from different
    /// moments ([`Workload::hourly_time_ms`]), where it is at most its share of the time an hour. A plan cached for a
    /// month then claims what it costs an hour, not a month's total next to an hour's; one compiled recently keeps its
    /// raw share, which can only understate it.
    pub fn share_weight(&self, q: &QueryStat) -> f64 {
        let (Some(hourly), Some(hours)) = (self.hourly_time_ms, self.entry_hours(q.first_seen.as_deref())) else {
            return 1.0;
        };
        if hourly <= 0.0 || self.total_time_ms <= 0.0 {
            return 1.0;
        }
        (self.total_time_ms / (hours * hourly)).min(1.0)
    }

    /// The share of the workload's time `time_ms` of statement `q` is ([`Workload::share`], weighed by
    /// [`Workload::share_weight`]): what every check grades a statement's share on.
    pub fn share_of(&self, time_ms: f64, q: &QueryStat) -> f64 {
        self.share(time_ms) * self.share_weight(q)
    }

    pub fn per_day(&self, value: f64) -> f64 {
        if self.measurement_hours() <= 0.0 {
            return value;
        }
        value * 24.0 / self.measurement_hours()
    }

    /// `value` per day, only when the window is long enough ([`PER_DAY_MIN_HOURS`]) to say so without extrapolating.
    pub fn per_day_measured(&self, value: f64) -> Option<f64> {
        (self.measurement_hours() >= PER_DAY_MIN_HOURS).then(|| self.per_day(value))
    }

    /// `value` as a daily rate for comparing with a threshold, only over at least [`RATE_MIN_HOURS`] of data.
    pub fn rate_per_day(&self, value: f64) -> Option<f64> {
        (self.measurement_hours() >= RATE_MIN_HOURS).then(|| self.per_day(value))
    }

    /// "the last 7 days" for windowed history, "the 35 minutes since statistics were reset" for cumulative counters,
    /// "the 50 minutes the Rails log covers" for an application's log.
    pub fn window_phrase(&self) -> String {
        if let Some(log) = &self.log {
            return if self.window_unknown() {
                format!("the period the {log} log covers")
            } else {
                format!("the {} the {log} log covers", span_words(self.measurement_hours()))
            };
        }
        if self.is_windowed() {
            if self.window_unknown() {
                "the recorded workload window".into()
            } else {
                format!("the last {}", span_words(self.measurement_hours()))
            }
        } else if self.window_unknown() {
            "the time since statistics were last reset".to_string()
        } else {
            format!("the {} since statistics were reset", span_words(self.measurement_hours()))
        }
    }

    /// How often something ran, honestly: "about 4.2K runs a day" over a day or more of data, otherwise the measured
    /// count and its window ("4 runs over the last 10 hours"), and fewer runs than the window has days as what they
    /// are ("1 run in the last 6 days", not "0.2 runs a day"). [`Workload::how_often_of`] says it for one statement.
    pub fn how_often(&self, runs: f64) -> String {
        match self.per_day_measured(runs) {
            Some(per_day) if per_day >= 1.0 => {
                format!("about {} {} a day", crate::format::count(per_day.round()), runs_noun(per_day.round()))
            }
            Some(_) => format!("{} {} in {}", crate::format::count(runs), runs_noun(runs), self.window_phrase()),
            None => format!("{} {} over {}", crate::format::count(runs), runs_noun(runs), self.window_phrase()),
        }
    }

    /// [`Workload::how_often`] for one statement, from when it ran as well as how many times ([`Workload::cadence`]):
    /// "750 runs on Oct 11, 15:51–15:55 UTC" for a burst, never a rate a day it never ran at. On a source of running
    /// totals, with a day or more of Culprit's own scans in which it ran ([`Workload::scans_of`]), the rate is theirs,
    /// what happens now, higher or lower than the totals': "about 1.7K runs a day over 72 hours of Culprit's scans",
    /// with the average since the totals began beside it when the two differ twofold ("about 1.7K runs a day over 72
    /// hours of Culprit's scans (about 388 a day over the 1277 days since statistics were reset)"), except for a recent
    /// entry, whose totals are mostly those runs, diluted by the time before it began ([`ScanSpan::is_recent`]).
    pub fn how_often_of(&self, q: &QueryStat) -> String {
        if let Some(activity) = self.observed_activity(q) {
            return activity.runs_phrase();
        }
        let cadence = self.cadence(q);
        if let Some(s) = self.scans_of(q).filter(|_| !matches!(cadence, Cadence::Burst { .. })) {
            let now = s.runs_phrase();
            return match self.totals_runs(q, false).filter(|_| !s.is_recent(q) && s.runs_differ(self, q)) {
                Some(all) => format!("{now} ({all} over {})", self.window_phrase()),
                None => now,
            };
        }
        match cadence {
            Cadence::Counted(runs) => self.how_often(runs),
            burst @ Cadence::Burst { runs, .. } => {
                let comma = if runs == 1.0 { "," } else { "" };
                format!(
                    "{} {}{comma} {}",
                    crate::format::count(runs),
                    runs_noun(runs),
                    burst.when().unwrap_or_default()
                )
            }
            Cadence::DailyWithin { per_day, from, to } => {
                format!(
                    "about {} {} a day {}",
                    crate::format::count(per_day.round()),
                    runs_noun(per_day.round()),
                    days_when(&from, to.as_deref())
                )
            }
            Cadence::Daily(per_day) => {
                format!("about {} {} a day", crate::format::count(per_day.round()), runs_noun(per_day.round()))
            }
        }
    }

    /// Runs since the source began counting, said beside the scans' rate: "about 388 a day" ("about 388 runs a day"
    /// with `noun`), or the count where it is under one a day. `None` under a day of window, or one nobody can date.
    fn totals_runs(&self, q: &QueryStat, noun: bool) -> Option<String> {
        let per_day = self.per_day_measured(q.executions).filter(|_| !self.window_unknown())?;
        Some(if per_day >= 1.0 {
            let n = crate::format::count(per_day.round());
            let runs = match (noun, n == "1") {
                (false, _) => "",
                (true, true) => " run",
                (true, false) => " runs",
            };
            format!("about {n}{runs} a day")
        } else {
            format!("{} {}", crate::format::count(q.executions), runs_noun(q.executions))
        })
    }

    /// How a statement's runs spread over the window, so a rate a day is said only for runs that came that way: a
    /// burst (runs from a first to a last under a day apart, well inside the window: a job, an export, a test run) is
    /// when it came, in any window of an hour or more ([`RATE_MIN_HOURS`]); so are runs on a few of a longer window's
    /// days, at most half of them (an eval run on two afternoons), told by the timeline's buckets; fewer runs than the
    /// window has days are a count; runs over a stretch of a day or more that the window holds three times over are a
    /// rate over that stretch (an entry that began counting days ago, a statement that stopped). Otherwise, under a day
    /// of window or in one nobody can date, it is the count. A statement's first and last runs are the engine's own, or
    /// the buckets it ran in where those hold all its runs: pg_stat_statements keeps no last run (Culprit dates one
    /// from its scans, the latest it can have been), so without them it is taken to run to the window's end.
    pub fn cadence(&self, q: &QueryStat) -> Cadence {
        let runs = q.executions;
        let Some(end) = self.window_end.as_deref().and_then(crate::format::epoch_seconds) else {
            return match self.per_day_measured(runs) {
                Some(per_day) if per_day >= 1.0 => Cadence::Daily(per_day),
                _ => Cadence::Counted(runs),
            };
        };
        if self.window_unknown() || self.measurement_hours() < RATE_MIN_HOURS {
            return Cadence::Counted(runs);
        }
        let window = self.measurement_hours() * 3600.0;
        let start = end - window;
        let first = q.first_seen.as_deref().and_then(crate::format::epoch_seconds);
        let last = match self.kind() {
            Some(WorkloadKind::StatementStats) => None,
            _ => q.last_seen.as_deref().and_then(crate::format::epoch_seconds),
        };
        // Times that can't belong to this window's runs (after its end, or a last run an hour before its start: a
        // statement's times from another scan) say nothing of when they came.
        let consistent =
            first.is_none_or(|f| f <= end + 60.0) && last.is_none_or(|l| l <= end + 60.0 && l >= start - 3600.0);
        let (first, last) = if consistent {
            (first.map(|f| f.clamp(start, end)), last.map(|l| l.clamp(start, end)))
        } else {
            (None, None)
        };
        // The buckets it ran in narrow its first and last runs to them (and give pg_stat_statements a last one).
        let ran = self.ran_in(q, start, end);
        let within = |t: Option<f64>, (from, to): (f64, f64), edge: f64| Some(t.map_or(edge, |t| t.clamp(from, to)));
        let (first, last) = match &ran {
            Some(ran) => (within(first, ran[0], ran[0].0), within(last, ran[ran.len() - 1], ran[ran.len() - 1].1)),
            None => (first, last),
        };
        let iso = crate::format::iso_from_epoch;
        if let (Some(first), Some(last)) = (first, last)
            && last >= first
            && last - first < window * 0.5
            && last - first < PER_DAY_MIN_HOURS * 3600.0
        {
            return Cadence::Burst { runs, from: iso(first), to: iso(last), days: Vec::new() };
        }
        if self.measurement_hours() < PER_DAY_MIN_HOURS {
            return Cadence::Counted(runs);
        }
        // Runs on a few days with silence between them: the buckets it ran in cover under half the time from its first
        // run to its last (a statement that began two days ago and runs on is a rate since then, not bursts).
        if let (Some(ran), Some(first), Some(last)) = (&ran, first, last) {
            let days = days_in(ran, first, last);
            let active: f64 = ran.iter().map(|(from, to)| to.min(last) - from.max(first)).filter(|s| *s > 0.0).sum();
            if days.len() >= 2
                && days.len() as f64 * 2.0 <= self.measurement_hours() / 24.0
                && active < (last - first) * 0.5
            {
                return Cadence::Burst { runs, from: iso(first), to: iso(last), days };
            }
        }
        let per_day = self.per_day(runs);
        if per_day < 1.0 {
            return Cadence::Counted(runs);
        }
        // An hour short of the window's end still runs, as far as the source can tell.
        let to = last.filter(|l| end - l > 3600.0);
        let from = first.filter(|f| f - start > 3600.0);
        let span = to.unwrap_or(end) - from.unwrap_or(start);
        if (from.is_some() || to.is_some()) && span >= PER_DAY_MIN_HOURS * 3600.0 && span < window * 0.75 {
            return Cadence::DailyWithin {
                per_day: runs * 86_400.0 / span,
                from: iso(from.unwrap_or(start)),
                to: to.map(iso),
            };
        }
        Cadence::Daily(per_day)
    }

    /// The stretches of the timeline statement `q` ran in (each bucket with runs of it, within `start`..`end`), oldest
    /// first, where the timeline holds all its runs: every bucket known for it, and its runs there adding up to its
    /// count. `None` otherwise (no timeline or trend, one that began after the statement did, a bucket it can't
    /// account for).
    fn ran_in(&self, q: &QueryStat, start: f64, end: f64) -> Option<Vec<(f64, f64)>> {
        let timeline = self.timeline.as_ref()?;
        let trend = q.trend.as_ref()?;
        let (mut counted, mut ran) = (0.0, Vec::new());
        for i in 0..timeline.len() {
            let runs = (*trend.executions.get(i)?)?;
            if runs <= 0.0 {
                continue;
            }
            let (from, to) = timeline.bounds(i);
            let (from, to) = (crate::format::epoch_seconds(from)?, crate::format::epoch_seconds(to)?);
            counted += runs;
            ran.push((from.clamp(start, end), to.clamp(start, end).max(from.clamp(start, end))));
        }
        (!ran.is_empty() && (counted - q.executions).abs() <= (q.executions * 0.01).max(0.5)).then_some(ran)
    }

    /// Runs a day for a statement, where its runs came that way ([`Workload::cadence`]): over Culprit's scans where
    /// they measured it on a source of running totals ([`Workload::scans_of`]), over the window, or over the stretch
    /// it ran in. `None` for a burst, for fewer runs than days, and under a day of window.
    pub fn runs_per_day_of(&self, q: &QueryStat) -> Option<f64> {
        if self.scans_of(q).is_none() && self.observed_activity(q).is_some() {
            return None;
        }
        let cadence = self.cadence(q);
        if !matches!(cadence, Cadence::Burst { .. })
            && let Some(s) = self.scans_of(q)
        {
            return Some(s.per_day(s.runs)).filter(|per_day| *per_day >= 1.0);
        }
        match cadence {
            Cadence::Daily(per_day) | Cadence::DailyWithin { per_day, .. } => Some(per_day),
            Cadence::Burst { .. } | Cadence::Counted(_) => None,
        }
    }

    /// `value`, counted over the window for statement `q` (its runs, its time, a part of either), as a rate a day for
    /// comparing with a threshold, at the pace its runs came ([`Workload::cadence`]), never extrapolated from a burst: a
    /// burst's total is what the day it ran held, and runs on a few days are their total over those days. On a source of
    /// running totals, at the pace Culprit's scans measured its runs where they did ([`Workload::scans_of`]). Runs over
    /// a stretch of the window are rated over that stretch, and steady runs over the window ([`Workload::rate_per_day`]).
    /// `None` under an hour of window ([`RATE_MIN_HOURS`]), where the total is all there is.
    pub fn rate_per_day_of(&self, q: &QueryStat, value: f64) -> Option<f64> {
        let cadence = self.cadence(q);
        if !matches!(cadence, Cadence::Burst { .. })
            && q.executions > 0.0
            && let Some(s) = self.scans_of(q)
        {
            return Some(value * s.per_day(s.runs) / q.executions);
        }
        match cadence {
            Cadence::Burst { days, .. } => Some(value / days.len().max(1) as f64),
            Cadence::DailyWithin { per_day, .. } if q.executions > 0.0 => Some(value * per_day / q.executions),
            _ => self.rate_per_day(value),
        }
    }

    /// What Culprit's own scans measured of statement `q`, on a source that keeps only running totals (the timeline
    /// built from the differences between scans): the steps that account for both the statement and the whole
    /// database, and no more for the statement than the database ran in a step (a counter misread in a saved scan, not
    /// a run). `None` under a day of such steps ([`PER_DAY_MIN_HOURS`]), so "a day" is never an extrapolation.
    pub fn over_scans(&self, q: &QueryStat) -> Option<ScanSpan> {
        let timeline = self.timeline.as_ref().filter(|t| t.source == TimelineSource::Scans && self.is_cumulative())?;
        let trend = q.trend.as_ref()?;
        let at = |s: &str| crate::format::epoch_seconds(s);
        let mut span =
            ScanSpan { hours: 0.0, runs: 0.0, time_ms: 0.0, workload_time_ms: 0.0, workload_runs: 0.0, period: None };
        let (mut first, mut last) = (None, None);
        for i in 0..timeline.len() {
            let (from, to) = timeline.bounds(i);
            let (Some(from), Some(to)) = (at(from), at(to)) else { continue };
            let Some((time, runs)) = timeline.statement_bucket(trend, i).filter(|_| to > from) else { continue };
            let all_time = timeline.time_ms[i];
            span.hours += (to - from) / 3600.0;
            span.runs += runs;
            span.time_ms += time;
            span.workload_time_ms += all_time;
            span.workload_runs += timeline.executions.get(i).copied().unwrap_or(0.0);
            first = first.or(Some(from));
            last = Some(to);
        }
        // An older subset is not the latest N hours. Date it whenever the scans leave gaps or end earlier.
        if let (Some(first), Some(last), Some(end)) = (first, last, at(&timeline.end))
            && (end - last > 1.0 || last - first - span.hours * 3600.0 > 1.0)
        {
            span.period = Some((first, last));
        }
        (span.hours >= PER_DAY_MIN_HOURS).then_some(span)
    }

    /// The latest scan interval's observed calls, separately from a complete workload share. Evictions can make
    /// database totals incomplete while an entry's growing counters still establish a lower bound on its activity.
    /// Never infer an average, share or daily rate from those possibly incomplete differences.
    pub fn observed_activity(&self, q: &QueryStat) -> Option<StatementActivity> {
        let t = self.timeline.as_ref().filter(|t| t.source == TimelineSource::Scans && self.is_cumulative())?;
        let i = t.len().checked_sub(1)?;
        let trend = q.trend.as_ref()?;
        if self.scans_of(q).is_some() && t.statement_bucket(trend, i).is_some() {
            return None;
        }
        let (time_ms, runs) = ((*trend.time_ms.get(i)?)?, (*trend.executions.get(i)?)?);
        if !time_ms.is_finite() || !runs.is_finite() || time_ms < 0.0 || runs <= 0.0 {
            return None;
        }
        // In a complete bucket an entry cannot outrun the database. Unknown buckets have no usable denominator.
        if !t.is_unknown(i) && t.statement_bucket(trend, i).is_none() {
            return None;
        }
        let (from, to) = t.bounds(i);
        let hours = (crate::format::epoch_seconds(to)? - crate::format::epoch_seconds(from)?) / 3600.0;
        (hours > 0.0).then(|| StatementActivity {
            runs,
            time_ms,
            incomplete: t.is_unknown(i),
            over: format!("{} of Culprit's scans {}", span_words(hours), span_when(from, to)),
        })
    }

    /// What Culprit's own scans measured of statement `q` where that is what a person reads of it and what grades it:
    /// a day or more of them ([`Workload::over_scans`]) in which it ran, whether its totals since the source began
    /// counting say more or less. A statement that didn't run in them at all is read by its totals: what it took until
    /// it stopped, or a weekly job's average.
    pub fn scans_of(&self, q: &QueryStat) -> Option<ScanSpan> {
        self.over_scans(q).filter(|s| s.runs > 0.0)
    }

    /// The share of database time statement `q` is stated and graded at, on every surface (titles, impact, rank, the
    /// lists of statements): over Culprit's scans where they measured it ([`Workload::scans_of`]), otherwise its share
    /// of the window's totals, weighed as the source allows ([`Workload::share_of`]).
    pub fn statement_share(&self, q: &QueryStat) -> f64 {
        self.scans_of(q).map_or_else(|| self.share_of(q.total_time_ms, q), |s| s.share())
    }

    /// Statement `q`'s average run, the same way ([`Workload::statement_share`]).
    pub fn statement_mean_ms(&self, q: &QueryStat) -> f64 {
        self.scans_of(q).and_then(|s| s.mean_ms()).unwrap_or(q.mean_time_ms)
    }

    /// Statement `q`'s share of all statement runs, the same way ([`Workload::statement_share`]).
    pub fn statement_runs_share(&self, q: &QueryStat) -> f64 {
        match self.scans_of(q) {
            Some(s) => s.runs_share(),
            None if self.total_executions > 0.0 => (q.executions / self.total_executions).min(1.0),
            None => 0.0,
        }
    }

    /// Statement `q`'s database time a day, the same way ([`Workload::statement_share`]): over the scans, or over a
    /// window of a day or more where its runs came a day at a time ([`Workload::runs_per_day_of`]). `None` for a burst
    /// and under a day of either.
    pub fn statement_time_per_day(&self, q: &QueryStat) -> Option<f64> {
        if matches!(self.cadence(q), Cadence::Burst { .. }) {
            return None;
        }
        match self.scans_of(q) {
            Some(s) => Some(s.per_day(s.time_ms)),
            None => self.runs_per_day_of(q).and(self.per_day_measured(q.total_time_ms)),
        }
    }

    /// What statement `q` costs, as every list of statements says it (a brief's, an export's, the app's): its share, how
    /// often it ran and its average run, over Culprit's scans where they measured it ([`Workload::scans_of`]), with the
    /// totals since the source began counting beside them where those differ twofold.
    pub fn cost_of(&self, q: &QueryStat) -> StatementCost {
        let scans = self.scans_of(q);
        let activity = self.observed_activity(q);
        let (runs, counted) = match &scans {
            Some(s) if !matches!(self.cadence(q), Cadence::Burst { .. }) => (s.runs_phrase(), s.runs),
            _ => (self.how_often_of(q), q.executions),
        };
        StatementCost {
            query: q.id.clone(),
            share: self.statement_share(q),
            runs: activity.as_ref().map_or(runs, StatementActivity::runs_phrase),
            runs_per_day: if activity.is_some() { None } else { self.runs_per_day_of(q) },
            counted: activity.as_ref().map_or(counted, |a| a.runs),
            mean_ms: self.statement_mean_ms(q),
            over: scans.map(|s| s.phrase()),
            totals: scans.and_then(|s| self.totals_beside(q, &s)),
            activity,
        }
    }

    /// The totals since the source began counting, where they say something else than Culprit's scans `s` (twice or
    /// half as much): "19% of database time, about 214 runs a day and 1.4 ms on average over the 1295 days since
    /// statistics were reset", each figure only where it differs. `None` for a recent entry ([`ScanSpan::is_recent`]),
    /// whose totals are those runs diluted by the time before it began, and where nothing differs.
    pub fn totals_beside(&self, q: &QueryStat, s: &ScanSpan) -> Option<String> {
        if s.is_recent(q) {
            return None;
        }
        let mut parts = Vec::new();
        if s.share_differs(self, q) {
            parts.push(format!("{} of database time", crate::format::percent(self.share_of(q.total_time_ms, q))));
        }
        // A burst's runs are when it ran, never a rate a day, on either side.
        if s.runs_differ(self, q)
            && !self.cadence(q).is_burst()
            && let Some(runs) = self.totals_runs(q, true)
        {
            parts.push(runs);
        }
        if s.mean_differs(q) {
            parts.push(format!("{} on average", crate::format::duration(q.mean_time_ms)));
        }
        (!parts.is_empty()).then(|| format!("{} over {}", crate::format::list(&parts), self.window_phrase()))
    }

    /// The source's running totals started at a time nobody can say (pg_stat_statements before PostgreSQL 14 keeps no
    /// reset time, and its statistics outlive restarts), or its log has no timestamps: `window_hours` is 0 and no rate
    /// can be derived from it.
    pub fn window_unknown(&self) -> bool {
        if let Some(context) = &self.measurement {
            return context.period.hours().is_none();
        }
        (self.is_cumulative() || self.log.is_some()) && self.window_start.is_none() && self.measurement_hours() <= 0.0
    }

    pub fn share(&self, time_ms: f64) -> f64 {
        if self.total_time_ms <= 0.0 {
            return 0.0;
        }
        (time_ms / self.total_time_ms).clamp(0.0, 1.0)
    }

    pub fn query(&self, id: &str) -> Option<&QueryStat> {
        self.queries.iter().find(|q| q.id == id)
    }
}

/// "run" or "runs", for a count.
fn runs_noun(runs: f64) -> &'static str {
    if (runs - 1.0).abs() < f64::EPSILON { "run" } else { "runs" }
}

/// How a statement's runs spread over its workload's window ([`Workload::cadence`]): what decides whether "a day" may
/// be said of them. Times are UTC, ISO 8601.
#[derive(Clone, Debug, PartialEq)]
pub enum Cadence {
    /// Runs a day over the window.
    Daily(f64),
    /// Runs a day over the stretch it ran in, from `from` to `to` (`None`: to the window's end).
    DailyWithin { per_day: f64, from: String, to: Option<String> },
    /// Runs that came in bursts well inside the window, never a rate a day: from `from` to `to` (its first run and its
    /// last), under a day apart; or, where `days` names them (UTC dates, `YYYY-MM-DD`), on those few of the window's
    /// days (an eval run on two afternoons, a batch on three nights).
    Burst { runs: f64, from: String, to: String, days: Vec<String> },
    /// Runs counted over the window: fewer than it has days, or a window too short (or undated) for a rate.
    Counted(f64),
}

impl Cadence {
    /// When the runs came, where that is part of how often they ran: "on Oct 11, 15:51–15:55 UTC" for a burst, "on Oct
    /// 11 and Oct 17" for runs on a few days, "since Oct 13" or "from Oct 11 to Oct 13" for a rate over a stretch of the
    /// window.
    pub fn when(&self) -> Option<String> {
        match self {
            // One run between two times is a point somewhere in them, not a run that lasted that long.
            Cadence::Burst { runs, from, to, days } if days.is_empty() && *runs <= 1.0 => Some(one_run_when(from, to)),
            Cadence::Burst { from, to, days, .. } if days.is_empty() => Some(span_when(from, to)),
            Cadence::Burst { days, .. } => Some(on_days(days)),
            Cadence::DailyWithin { from, to, .. } => Some(days_when(from, to.as_deref())),
            Cadence::Daily(_) | Cadence::Counted(_) => None,
        }
    }

    /// Whether the runs came in bursts ([`Cadence::Burst`]): said by when they came, graded on their total.
    pub fn is_burst(&self) -> bool {
        matches!(self, Cadence::Burst { .. })
    }
}

/// The UTC dates (`YYYY-MM-DD`) the stretches `ran` touch (epoch seconds, oldest first), the first narrowed to start at
/// `first` and the last to end at `last`: the days a statement ran on.
fn days_in(ran: &[(f64, f64)], first: f64, last: f64) -> Vec<String> {
    let mut days = std::collections::BTreeSet::new();
    for (i, &(from, to)) in ran.iter().enumerate() {
        let from = if i == 0 { first.max(from).min(to) } else { from };
        let to = if i + 1 == ran.len() { last.min(to).max(from) } else { to };
        // A stretch ends where the next begins: one ending at midnight ran on the day before.
        let to = if to > from { to - 1.0 } else { to };
        let mut day = (from / 86_400.0).floor();
        while day <= (to / 86_400.0).floor() {
            days.insert(crate::format::iso_from_epoch(day * 86_400.0)[..10].to_string());
            day += 1.0;
        }
    }
    days.into_iter().collect()
}

/// The days runs came on (UTC dates, `YYYY-MM-DD`): "on Oct 11 and Oct 17", "on Oct 11, Oct 13 and Oct 17", and past
/// three "on 4 days between Oct 11 and Oct 17".
fn on_days(days: &[String]) -> String {
    let day = |iso: &str| crate::format::friendly_date(iso, iso);
    match days {
        [] => String::new(),
        [one] => format!("on {}", day(one)),
        [first @ .., last] if days.len() <= 3 => {
            format!("on {} and {}", first.iter().map(|d| day(d)).collect::<Vec<_>>().join(", "), day(last))
        }
        [first, .., last] => format!("on {} days between {} and {}", days.len(), day(first), day(last)),
    }
}

/// What Culprit's own scans measured of one statement ([`Workload::over_scans`]).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ScanSpan {
    /// Hours the steps between scans cover.
    pub hours: f64,
    pub runs: f64,
    pub time_ms: f64,
    /// The whole database's time in the same steps.
    pub workload_time_ms: f64,
    /// The whole database's runs in the same steps.
    pub workload_runs: f64,
    /// Dates for a subset that has gaps or does not reach the latest scan.
    pub period: Option<(f64, f64)>,
}

impl ScanSpan {
    /// The statement's share of the database's runs in these steps.
    pub fn runs_share(&self) -> f64 {
        if self.workload_runs <= 0.0 { 0.0 } else { (self.runs / self.workload_runs).clamp(0.0, 1.0) }
    }

    /// `value`, counted over these steps, a day.
    pub fn per_day(&self, value: f64) -> f64 {
        value * 24.0 / self.hours
    }

    /// The statement's share of the database's time in these steps.
    pub fn share(&self) -> f64 {
        if self.workload_time_ms <= 0.0 { 0.0 } else { (self.time_ms / self.workload_time_ms).clamp(0.0, 1.0) }
    }

    /// "72 hours of Culprit's scans": the steps, as sentences name them (they need not be every scan's).
    pub fn phrase(&self) -> String {
        let dates = self
            .period
            .map(|(from, to)| {
                format!(" {}", span_when(&crate::format::iso_from_epoch(from), &crate::format::iso_from_epoch(to)))
            })
            .unwrap_or_default();
        format!("{} of Culprit's scans{dates}", self.length())
    }

    /// How long the steps cover: hours under four days (89 hours are not "4 days"), days to a tenth after.
    pub fn length(&self) -> String {
        if self.hours < 96.0 {
            return format!("{} hours", self.hours.round());
        }
        let tenths = format!("{:.1}", (self.hours / 2.4).floor() / 10.0);
        format!("{} days", tenths.strip_suffix(".0").unwrap_or(&tenths))
    }

    /// The statement's average run in these steps. `None` without a run.
    pub fn mean_ms(&self) -> Option<f64> {
        (self.runs > 0.0).then(|| self.time_ms / self.runs)
    }

    /// "about 1.7K runs a day over 72 hours of Culprit's scans", or the count under one a day ("3 runs over 72 hours
    /// of Culprit's scans").
    pub fn runs_phrase(&self) -> String {
        let per_day = self.per_day(self.runs);
        if per_day >= 1.0 {
            format!(
                "about {} {} a day over {}",
                crate::format::count(per_day.round()),
                runs_noun(per_day.round()),
                self.phrase()
            )
        } else {
            format!("{} {} over {}", crate::format::count(self.runs), runs_noun(self.runs), self.phrase())
        }
    }

    /// Whether statement `q`'s entry is recent: at least half of the runs its totals hold came in these steps, so the
    /// rest of the window before it began counting dilutes every average of them (an entry the application started
    /// sending days ago, or one evicted and created again).
    pub fn is_recent(&self, q: &QueryStat) -> bool {
        self.runs >= q.executions * 0.5
    }

    /// Whether these steps' runs a day are twice or half statement `q`'s since the source began counting. False when
    /// those can't be said a day (under a day of window, or one nobody can date).
    pub fn runs_differ(&self, workload: &Workload, q: &QueryStat) -> bool {
        let all = workload.per_day_measured(q.executions).filter(|_| !workload.window_unknown());
        all.is_some_and(|all| apart(self.per_day(self.runs), all))
    }

    /// Whether the statement's share of database time in these steps is twice or half its share of the totals.
    pub fn share_differs(&self, workload: &Workload, q: &QueryStat) -> bool {
        apart(self.share(), workload.share_of(q.total_time_ms, q))
    }

    /// Whether its average run in these steps is twice or half its average since the source began counting.
    pub fn mean_differs(&self, q: &QueryStat) -> bool {
        self.mean_ms().is_some_and(|now| apart(now, q.mean_time_ms))
    }

    /// Whether the statement's share of the database's runs in these steps is twice or half its share of the totals'.
    pub fn runs_share_differs(&self, workload: &Workload, q: &QueryStat) -> bool {
        workload.total_executions > 0.0 && apart(self.runs_share(), q.executions / workload.total_executions)
    }
}

/// Two figures twice or half as much as each other: what a sentence gives both of.
fn apart(now: f64, all: f64) -> bool {
    now >= all * 2.0 || now * 2.0 <= all
}

/// What a statement costs, as the lists of statements say it ([`Workload::cost_of`]).
#[derive(Serialize, Deserialize, TS, Clone, Debug, PartialEq)]
#[ts(export)]
pub struct StatementCost {
    /// The statement's id in the workload.
    pub query: String,
    /// Its share of database time ([`Workload::statement_share`]).
    pub share: f64,
    /// How often it ran: "about 1.7K runs a day over 72 hours of Culprit's scans", or [`Workload::how_often_of`].
    pub runs: String,
    /// Its runs a day where its runs came that way ([`Workload::runs_per_day_of`]).
    pub runs_per_day: Option<f64>,
    /// The runs `runs` counts where it says no rate a day (observed activity, a burst, a few runs): over Culprit's scans where they
    /// measured it, otherwise over the window.
    pub counted: f64,
    /// Its average run ([`Workload::statement_mean_ms`]).
    pub mean_ms: f64,
    /// What these figures are over where Culprit's scans measured them: "72 hours of Culprit's scans". `None`: the
    /// workload's window.
    pub over: Option<String>,
    /// The totals since the source began counting, where they differ twofold ([`Workload::totals_beside`]).
    pub totals: Option<String>,
    /// More recent observed calls whose interval cannot supply a complete workload share. The share and average
    /// above retain their own period; this count is never divided by the lifetime reset window.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub activity: Option<StatementActivity>,
}

#[derive(Serialize, Deserialize, TS, Clone, Debug, PartialEq)]
#[ts(export)]
pub struct StatementActivity {
    pub runs: f64,
    pub time_ms: f64,
    /// Evictions mean these differences are lower bounds, not complete counts or a basis for an average.
    pub incomplete: bool,
    pub over: String,
}

impl StatementActivity {
    pub fn runs_phrase(&self) -> String {
        format!(
            "{}{} {} observed over {}",
            if self.incomplete { "at least " } else { "" },
            crate::format::count(self.runs),
            runs_noun(self.runs),
            self.over
        )
    }
}

/// When runs came, from `from` to `to` (ISO 8601): "on Oct 11 at 13:58 UTC" for one moment, "on Oct 11, 15:51–15:55
/// UTC" within a day, "between Oct 11, 22:10 and Oct 12, 03:10 UTC" across midnight.
pub fn span_when(from: &str, to: &str) -> String {
    let (Some(a), Some(b)) = (crate::format::epoch_seconds(from), crate::format::epoch_seconds(to)) else {
        return format!("between {from} and {to}");
    };
    let (a, b) = (crate::format::iso_from_epoch(a), crate::format::iso_from_epoch(b));
    let day = |iso: &str| crate::format::friendly_date(iso, iso);
    let time = |iso: &str| iso.get(11..16).unwrap_or_default().to_string();
    if time(&a) == time(&b) && day(&a) == day(&b) {
        format!("on {} at {} UTC", day(&a), time(&a))
    } else if day(&a) == day(&b) {
        format!("on {}, {}–{} UTC", day(&a), time(&a), time(&b))
    } else {
        format!("between {}, {} and {}, {} UTC", day(&a), time(&a), day(&b), time(&b))
    }
}

/// When one run came, from the bounds that hold it: "on Oct 11 at 13:58 UTC" where they meet, else "sometime between
/// 08:41 and 09:28 UTC on Oct 11" ("sometime between Oct 11, 23:10 and Oct 12, 00:40 UTC" across midnight).
fn one_run_when(from: &str, to: &str) -> String {
    let (Some(a), Some(b)) = (crate::format::epoch_seconds(from), crate::format::epoch_seconds(to)) else {
        return format!("sometime between {from} and {to}");
    };
    let (a, b) = (crate::format::iso_from_epoch(a), crate::format::iso_from_epoch(b));
    let day = |iso: &str| crate::format::friendly_date(iso, iso);
    let time = |iso: &str| iso.get(11..16).unwrap_or_default().to_string();
    if time(&a) == time(&b) && day(&a) == day(&b) {
        format!("on {} at {} UTC", day(&a), time(&a))
    } else if day(&a) == day(&b) {
        format!("sometime between {} and {} UTC on {}", time(&a), time(&b), day(&a))
    } else {
        format!("sometime between {}, {} and {}, {} UTC", day(&a), time(&a), day(&b), time(&b))
    }
}

/// "since Oct 13" for runs to the window's end, "from Oct 11 to Oct 13" otherwise.
fn days_when(from: &str, to: Option<&str>) -> String {
    let day = |iso: &str| crate::format::friendly_date(iso, iso);
    match to {
        Some(to) => format!("from {} to {}", day(from), day(to)),
        None => format!("since {}", day(from)),
    }
}

#[derive(Serialize, Deserialize, TS, Clone, Debug)]
#[ts(export)]
pub struct QueryStat {
    #[serde(default)]
    pub requests: Option<crate::operations::RequestEvidence>,
    /// A CTE write's unambiguous base table, resolved by shared SQL analysis. Absent when unknown or unnecessary.
    #[serde(default)]
    pub resolved_write_target: Option<String>,
    pub id: String,
    pub query_hash: Option<String>,
    pub text: String,
    /// Parameter declarations sent with the statement, e.g. `@P1 nvarchar(4000)`; they explain implicit conversions.
    pub parameters: Option<String>,
    pub object_name: Option<String>,
    pub executions: f64,
    pub total_time_ms: f64,
    pub mean_time_ms: f64,
    pub max_time_ms: Option<f64>,
    pub stddev_time_ms: Option<f64>,
    pub cpu_time_ms: Option<f64>,
    pub rows: Option<f64>,
    pub logical_reads: Option<f64>,
    pub physical_reads: Option<f64>,
    pub temp_writes: Option<f64>,
    pub first_seen: Option<String>,
    pub last_seen: Option<String>,
    pub plans: Vec<PlanVariant>,
    pub plan: Option<Plan>,
    pub waits: Vec<WaitStat>,
    /// This query's part of each bucket of the workload's timeline (only the heaviest queries carry one).
    #[serde(default)]
    pub trend: Option<QueryTrend>,
    /// How run times spread beyond the average, as far as the engine keeps it.
    #[serde(default)]
    pub spread: Option<Spread>,
    /// Failed runs, planning, JIT, log volume and result size, as far as the engine keeps them.
    #[serde(default)]
    pub costs: Option<crate::signals::QueryCosts>,
    /// Rows the statement changed (inserted, updated or deleted), where the engine counts them apart from the rows it
    /// returned (`rows`): MySQL and MariaDB's performance_schema. `None` where the engine doesn't separate them.
    #[serde(default)]
    pub rows_affected: Option<f64>,
    /// Which of `plans` the attached `plan` is (Query Store's plan_id): the plan still in use (the heaviest of the
    /// variants no newer variant replaced), not merely the heaviest over the window. `None` when the source keeps one
    /// plan per statement.
    #[serde(default)]
    pub plan_id: Option<String>,
    // Where retained plan statistics come from when they have a different lifetime from statement statistics.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub plan_history: Option<PlanHistory>,
}

/// How a statement's run times spread. Every engine keeps something different: SQL Server and Postgres the fastest
/// run, MySQL percentiles and a run-time histogram (8.0.19+). Missing values are `None`, never zero.
#[derive(Serialize, Deserialize, TS, Clone, Debug, Default, PartialEq)]
#[ts(export)]
pub struct Spread {
    pub min_ms: Option<f64>,
    pub p50_ms: Option<f64>,
    pub p95_ms: Option<f64>,
    pub p99_ms: Option<f64>,
    /// `[low_ms, high_ms, runs]` per bucket, fastest first (MySQL).
    #[serde(default)]
    pub histogram: Vec<[f64; 3]>,
    /// Time spent waiting for locks across all runs (MySQL).
    #[serde(default)]
    pub lock_ms: Option<f64>,
}

impl Spread {
    /// Percentiles never above the slowest run: MySQL reports them as bucket upper edges, which can overshoot it.
    pub fn clamp_to(&mut self, max_ms: Option<f64>) {
        let Some(max) = max_ms.filter(|m| m.is_finite() && *m > 0.0) else { return };
        for p in [&mut self.p50_ms, &mut self.p95_ms, &mut self.p99_ms] {
            if let Some(v) = p.as_mut() {
                *v = v.min(max);
            }
        }
    }
}

/// Where a timeline's buckets come from.
#[derive(Serialize, Deserialize, TS, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "kebab-case")]
#[ts(export)]
pub enum TimelineSource {
    /// Query Store's runtime-stats intervals, folded into equal buckets.
    #[default]
    QueryStore,
    /// Differences between Culprit's own saved scans of a source that only keeps running totals.
    Scans,
    /// Runs logged one by one (MySQL and MariaDB's slow query log table), bucketed by when they ran.
    SlowLog,
}

/// Database time and executions per bucket. Buckets are equal for Query Store and fall between scans otherwise.
#[derive(Serialize, Deserialize, TS, Clone, Debug, Default)]
#[ts(export)]
pub struct Timeline {
    pub source: TimelineSource,
    /// Start of each bucket (UTC, ISO 8601). Bucket `i` ends where bucket `i + 1` starts; the last one ends at `end`.
    pub starts: Vec<String>,
    pub end: String,
    pub time_ms: Vec<f64>,
    pub executions: Vec<f64>,
    /// Buckets the source can't account for in full, by index: its running totals shrank between two scans without a
    /// reset (entries evicted from pg_stat_statements, plans dropped from the cache), so `time_ms` and `executions`
    /// there hold only the growth of the statements Culprit tracks, a lower bound. `None` when every bucket is known.
    #[serde(default)]
    pub unknown: Option<Vec<bool>>,
}

impl Timeline {
    pub fn len(&self) -> usize {
        self.starts.len()
    }

    pub fn is_empty(&self) -> bool {
        self.starts.is_empty()
    }

    /// Start and end of bucket `i`.
    pub fn bounds(&self, i: usize) -> (&str, &str) {
        let end = self.starts.get(i + 1).unwrap_or(&self.end);
        (self.starts[i].as_str(), end.as_str())
    }

    /// Whether bucket `i` holds only part of what ran in it (see [`Timeline::unknown`]).
    pub fn is_unknown(&self, i: usize) -> bool {
        self.unknown.as_ref().and_then(|u| u.get(i).copied()).unwrap_or(false)
    }

    /// What a statement ran in bucket `i`, as `(time ms, runs)`, where the bucket accounts for it: `None` where the
    /// statement wasn't tracked, in a bucket the source can't account for in full ([`Timeline::unknown`]), and where
    /// the statement ran more than the whole database did (a counter misread in a saved scan, not a run). Every reading
    /// of a statement's trend (its rate over Culprit's scans, a regression, the app's charts) takes its buckets from
    /// here; the app mirrors it (`statementBucket` in `app/src/components/Trends.tsx`).
    pub fn statement_bucket(&self, trend: &QueryTrend, i: usize) -> Option<(f64, f64)> {
        let time = (*trend.time_ms.get(i)?)?;
        let runs = (*trend.executions.get(i)?)?;
        let (all_time, all_runs) = (*self.time_ms.get(i)?, *self.executions.get(i)?);
        (!self.is_unknown(i) && time <= all_time + 0.5 && runs <= all_runs + 0.5).then_some((time, runs))
    }
}

/// One query's database time and executions in each bucket of the workload timeline. `None` marks a bucket the
/// source can't account for (an earlier scan didn't track this query, or its counters were reset).
#[derive(Serialize, Deserialize, TS, Clone, Debug, Default)]
#[ts(export)]
pub struct QueryTrend {
    pub time_ms: Vec<Option<f64>>,
    pub executions: Vec<Option<f64>>,
}

impl QueryTrend {
    /// Average duration in bucket `i`, when the query ran in it.
    pub fn mean_ms(&self, i: usize) -> Option<f64> {
        let time = (*self.time_ms.get(i)?)?;
        let runs = (*self.executions.get(i)?)?;
        (runs > 0.0).then(|| time / runs)
    }
}

impl QueryStat {
    /// The measured runs of the attached plan, where the source identifies its variants. A statement's average over
    /// all its plans says nothing about whether this particular plan runs cheaply.
    pub fn captured_plan_variant(&self) -> Option<&PlanVariant> {
        let id = self.plan_id.as_ref()?;
        self.workload_plans().iter().find(|p| &p.id == id)
    }

    /// What its runs took, where an average would hide it: its one run's time, or two runs' times, the longest first,
    /// where one took twice the other or more (an average of 5.2 s for runs of 10.5 s and 0.3 ms). `None` for more
    /// runs, for two alike, and where the engine keeps no longest run.
    pub fn few_runs(&self) -> Option<Vec<f64>> {
        if self.executions == 1.0 {
            return Some(vec![self.total_time_ms]);
        }
        if self.executions != 2.0 || self.total_time_ms <= 0.0 {
            return None;
        }
        let longest = self.max_time_ms?.clamp(0.0, self.total_time_ms);
        let other = self.total_time_ms - longest;
        (longest >= other * 2.0).then(|| vec![longest, other])
    }

    /// [`QueryStat::few_runs`] in words, for a sentence that says what its runs took in place of their average: "24.3
    /// s" for one run, "10.5 s and 0.3 ms" for two.
    pub fn few_runs_took(&self) -> Option<String> {
        let runs = self.few_runs()?;
        Some(runs.iter().map(|ms| crate::format::duration(*ms)).collect::<Vec<_>>().join(" and "))
    }

    pub fn rows_per_execution(&self) -> Option<f64> {
        let rows = self.rows?;
        (self.executions > 0.0).then(|| rows / self.executions)
    }

    /// Whether the statement changes rows (INSERT, UPDATE, DELETE, MERGE, REPLACE), from its text.
    pub fn writes_rows(&self) -> bool {
        let lower = crate::sql::without_parameter_list(&self.text)
            .trim_start_matches(|c: char| c.is_whitespace() || c == '(')
            .to_ascii_lowercase();
        ["insert", "update", "delete", "merge", "replace"].iter().any(|v| lower.starts_with(v))
    }

    /// Rows a run handled: for a write, the rows it changed where the engine counts them apart from rows returned
    /// (MySQL sends a write's client no rows, so `rows` is 0 for every INSERT); otherwise the rows it returned.
    pub fn rows_handled_per_execution(&self) -> Option<f64> {
        let rows = if self.writes_rows() { self.rows_affected.or(self.rows)? } else { self.rows? };
        (self.executions > 0.0).then(|| rows / self.executions)
    }

    pub fn reads_per_execution(&self) -> Option<f64> {
        let reads = self.logical_reads?;
        (self.executions > 0.0).then(|| reads / self.executions)
    }
}

#[derive(Serialize, Deserialize, TS, Clone, Debug)]
#[ts(export)]
pub struct PlanVariant {
    pub id: String,
    pub executions: f64,
    pub total_time_ms: f64,
    pub mean_time_ms: f64,
    pub first_seen: Option<String>,
    pub last_seen: Option<String>,
    pub is_forced: bool,
    #[serde(default)]
    pub min_time_ms: Option<f64>,
    #[serde(default)]
    pub max_time_ms: Option<f64>,
    #[serde(default)]
    pub stddev_time_ms: Option<f64>,
    /// The parameter values this plan was compiled for, as the engine reports them (SQL Server showplan).
    #[serde(default)]
    pub compiled: Option<String>,
    /// SQL Server 2022 parameter-sensitive plans: the variant query this plan belongs to.
    #[serde(default)]
    pub variant_query: Option<String>,
    /// CPU time and logical reads across its runs, where the source keeps them per plan (Query Store): whether a
    /// slower plan does more work, or only waited longer while it ran.
    #[serde(default)]
    pub cpu_time_ms: Option<f64>,
    #[serde(default)]
    pub logical_reads: Option<f64>,
}

#[derive(Serialize, Deserialize, TS, Clone, Debug)]
#[ts(export)]
pub struct PlanHistory {
    pub source: String,
    pub kind: PlanHistoryKind,
    pub note: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub captured_plan_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub captured_plan: Option<Plan>,
}

#[derive(Serialize, Deserialize, TS, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
#[ts(export)]
pub enum PlanHistoryKind {
    Workload,
    /// Independent retained cache entries, with their own periods, resets and evictions.
    Cache,
}

impl QueryStat {
    pub fn plan_history_matches_workload(&self) -> bool {
        self.plan_history.as_ref().is_none_or(|history| history.kind == PlanHistoryKind::Workload)
    }
    /// Variants whose periods and counters can be compared with this statement's workload statistics.
    pub fn workload_plans(&self) -> &[PlanVariant] {
        if self.plan_history_matches_workload() { &self.plans } else { &[] }
    }
}
