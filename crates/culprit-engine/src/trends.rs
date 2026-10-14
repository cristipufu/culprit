//! Trends for sources that only keep running totals (pg_stat_statements, performance_schema, the plan cache): the
//! difference between two of Culprit's own scans is what ran in between. Each saved report gets a small counters file
//! so building a trend never re-reads whole reports.
//!
//! Totals that shrink aren't always a reset. The source's own marker says when its counters started over (a
//! statistics reset, a restart, a truncate); without it, totals shrink because entries left the source between the two
//! scans (pg_stat_statements evicts the least-used, the plan cache drops and recompiles plans) and took everything they
//! had counted with them. Such a step's total is unknown: it holds only what the statements Culprit tracks grew by, and
//! says so, rather than everything since the last reset in one bucket.

use std::collections::{BTreeMap, HashMap};

use culprit_core::{Plan, QueryTrend, Report, Timeline, TimelineSource, Workload};
use serde::{Deserialize, Serialize};

/// Scans older than this don't shape the trend.
pub const HISTORY_DAYS: f64 = 30.0;
/// The heaviest queries get a trend line; the rest would only grow the report.
const TREND_QUERIES: usize = 40;
const MAX_BUCKETS: f64 = 120.0;
const MIN_BUCKET_SECONDS: f64 = 300.0;

/// A scan's running totals: the workload's and each tracked query's `[time ms, executions]`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Counters {
    pub taken_at: String,
    pub source: String,
    pub database: String,
    /// When the source's counters last started from zero (statistics reset or server start).
    pub window_start: Option<String>,
    pub total_time_ms: f64,
    pub total_executions: f64,
    pub queries: BTreeMap<String, [f64; 2]>,
    /// What "What changed" needs to name a query and notice a new plan without re-reading the report.
    #[serde(default)]
    pub meta: BTreeMap<String, QueryMeta>,
    /// Entries the source has evicted since its counters started (pg_stat_statements_info.dealloc, 14+): when it grew
    /// between two scans, entries left and took their totals with them.
    #[serde(default)]
    pub evictions: Option<f64>,
}

/// The setting a pg_stat_statements scan reports its evictions in.
const EVICTIONS_SETTING: &str = "pg_stat_statements.dealloc";

/// A tracked query as one scan saw it.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct QueryMeta {
    pub text: String,
    /// The captured plan's shape (operators and objects, in order); a different shape is a different plan.
    #[serde(default)]
    pub shape: Option<String>,
    /// Where that plan spends most of its effort, in words: "Seq Scan on orders".
    #[serde(default)]
    pub hot: Option<String>,
    #[serde(default)]
    pub first_seen: Option<String>,
    #[serde(default)]
    pub last_seen: Option<String>,
}

/// A plan's shape and its hottest step in words. Costs and row estimates are left out: they move with statistics
/// while the plan stays the same.
pub fn plan_mark(plan: &Plan) -> (String, String) {
    let shape = plan
        .nodes()
        .iter()
        .map(|n| match (n.object.as_deref(), n.index.as_deref()) {
            (Some(o), Some(i)) => format!("{}({} {})", n.op, short_object(o), i),
            (Some(o), None) => format!("{}({})", n.op, short_object(o)),
            _ => n.op.clone(),
        })
        .collect::<Vec<_>>()
        .join(">");
    let hot = plan.hottest();
    let words = match hot.object.as_deref() {
        Some(o) => format!("{} on {}", hot.op, short_object(o)),
        None => hot.op.clone(),
    };
    (shape, words)
}

/// `[shop].[dbo].[orders]` or `public.orders` → `orders`.
fn short_object(name: &str) -> String {
    name.rsplit('.').next().unwrap_or(name).trim_matches(|c| matches!(c, '[' | ']' | '"' | '`')).to_string()
}

impl Counters {
    /// `None` for windowed sources (Query Store keeps its own history).
    pub fn of(report: &Report) -> Option<Counters> {
        let snapshot = &report.snapshot;
        let workload = snapshot.workload.as_ref().filter(|w| !w.windowed)?;
        Some(Counters::of_workload(workload, &snapshot.server.database, &snapshot.taken_at, evictions(snapshot)))
    }

    fn of_workload(workload: &Workload, database: &str, taken_at: &str, evictions: Option<f64>) -> Counters {
        Counters {
            taken_at: taken_at.to_string(),
            source: workload.source.clone(),
            database: database.to_string(),
            window_start: workload.window_start.clone(),
            total_time_ms: workload.total_time_ms,
            total_executions: workload.total_executions,
            queries: workload.queries.iter().map(|q| (q.id.clone(), [q.total_time_ms, q.executions])).collect(),
            meta: workload
                .queries
                .iter()
                .map(|q| {
                    let (shape, hot) = q.plan.as_ref().map(plan_mark).unzip();
                    let meta = QueryMeta {
                        text: q.text.chars().take(300).collect(),
                        shape,
                        hot,
                        first_seen: q.first_seen.clone(),
                        last_seen: q.last_seen.clone(),
                    };
                    (q.id.clone(), meta)
                })
                .collect(),
            evictions,
        }
    }
}

/// How many entries the source says it has evicted, when it says (pg_stat_statements 14+).
pub fn evictions(snapshot: &culprit_core::Snapshot) -> Option<f64> {
    snapshot.setting(EVICTIONS_SETTING).and_then(|s| s.value.trim().parse().ok())
}

/// How the step from one scan's running totals (`a`, taken at `ta`) to the next (`b`) reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StepKind {
    /// The totals grew: their difference is what ran in between.
    Growth,
    /// The source's own marker says its counters started over after the earlier scan: the later totals are what ran.
    Reset,
    /// The totals shrank, or entries were evicted, though the source didn't start over: entries left in between and
    /// took their totals with them. What ran is known only for the entries still there.
    Unknown,
}

pub fn step_kind(ta: f64, a: &Counters, b: &Counters) -> StepKind {
    if b.window_start.as_deref().and_then(epoch).is_some_and(|r| r > ta + 1.0) {
        return StepKind::Reset;
    }
    let shrank = b.total_executions < a.total_executions || b.total_time_ms < a.total_time_ms - 0.5;
    let evicted = matches!((a.evictions, b.evictions), (Some(x), Some(y)) if y > x);
    if shrank || evicted { StepKind::Unknown } else { StepKind::Growth }
}

/// What query `id` ran in a step: its difference, everything for a reset or for an entry the source created afresh in
/// between, `None` when neither scan can account for it.
pub fn query_step(
    kind: StepKind,
    ta: f64,
    a: &Counters,
    b: &Counters,
    id: &str,
    first_seen: Option<f64>,
) -> Option<[f64; 2]> {
    let after = *b.queries.get(id)?;
    if kind == StepKind::Reset {
        return Some(after);
    }
    match a.queries.get(id) {
        Some(before) if after[1] >= before[1] && after[0] >= before[0] => {
            Some([after[0] - before[0], after[1] - before[1]])
        }
        // The source dropped and re-created the entry; it counts from zero again.
        Some(_) => Some(after),
        // Not tracked by the earlier scan: known only when the query first ran after it.
        None => first_seen.is_some_and(|f| f + 1.0 > ta).then_some(after),
    }
}

pub fn epoch(text: &str) -> Option<f64> {
    chrono::DateTime::parse_from_rfc3339(text).ok().map(|t| t.timestamp_millis() as f64 / 1000.0)
}

fn iso(seconds: f64) -> String {
    chrono::DateTime::from_timestamp_millis((seconds * 1000.0) as i64)
        .map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
        .unwrap_or_default()
}

/// What ran between two consecutive scans.
struct Step {
    start: f64,
    end: f64,
    time_ms: f64,
    executions: f64,
    /// The totals hold only part of what ran (see [`StepKind::Unknown`]).
    unknown: bool,
    queries: Vec<Option<[f64; 2]>>,
}

/// Fill `workload.timeline` and the heaviest queries' trends from earlier scans of the same database and source.
/// Leaves the workload alone when there is no earlier scan to compare with.
pub fn from_scans(
    workload: &mut Workload,
    database: &str,
    taken_at: &str,
    evictions: Option<f64>,
    history: &[Counters],
) {
    let current = Counters::of_workload(workload, database, taken_at, evictions);
    let Some(now) = epoch(taken_at) else { return };
    let mut points: Vec<(f64, &Counters)> = history
        .iter()
        .filter(|c| c.source == current.source && c.database == current.database)
        .filter_map(|c| Some((epoch(&c.taken_at)?, c)))
        .filter(|(t, _)| *t < now - 1.0 && *t >= now - HISTORY_DAYS * 86_400.0)
        .collect();
    if points.is_empty() {
        return;
    }
    points.sort_by(|a, b| a.0.total_cmp(&b.0));
    points.push((now, &current));

    let mut tracked: Vec<&culprit_core::QueryStat> = workload.queries.iter().collect();
    tracked.sort_by(|a, b| b.total_time_ms.total_cmp(&a.total_time_ms));
    tracked.truncate(TREND_QUERIES);
    let ids: Vec<String> = tracked.iter().map(|q| q.id.clone()).collect();
    let first_seen: HashMap<&str, f64> =
        tracked.iter().filter_map(|q| Some((q.id.as_str(), epoch(q.first_seen.as_deref()?)?))).collect();

    let steps: Vec<Step> = points
        .windows(2)
        .map(|pair| {
            let ((ta, a), (tb, b)) = (pair[0], pair[1]);
            let kind = step_kind(ta, a, b);
            let first = |id: &str| first_seen.get(id).copied();
            let queries = ids.iter().map(|id| query_step(kind, ta, a, b, id, first(id))).collect();
            let (time_ms, executions) = match kind {
                StepKind::Reset => (b.total_time_ms, b.total_executions),
                StepKind::Growth => (b.total_time_ms - a.total_time_ms, b.total_executions - a.total_executions),
                // Only what the entries still there grew by: a lower bound, never everything since the last reset.
                StepKind::Unknown => b
                    .queries
                    .keys()
                    .filter_map(|id| query_step(kind, ta, a, b, id, None))
                    .fold((0.0, 0.0), |(t, e), v| (t + v[0], e + v[1])),
            };
            Step { start: ta, end: tb, time_ms, executions, unknown: kind == StepKind::Unknown, queries }
        })
        .collect();

    let span = now - steps[0].start;
    let min_width = (span / MAX_BUCKETS).max(MIN_BUCKET_SECONDS);
    let merged = merge(steps, min_width, ids.len());

    workload.timeline = Some(Timeline {
        source: TimelineSource::Scans,
        starts: merged.iter().map(|s| iso(s.start)).collect(),
        end: taken_at.to_string(),
        time_ms: merged.iter().map(|s| round(s.time_ms)).collect(),
        executions: merged.iter().map(|s| s.executions.round()).collect(),
        unknown: merged.iter().any(|s| s.unknown).then(|| merged.iter().map(|s| s.unknown).collect()),
    });
    for (i, id) in ids.iter().enumerate() {
        if let Some(q) = workload.queries.iter_mut().find(|q| &q.id == id) {
            q.trend = Some(QueryTrend {
                time_ms: merged.iter().map(|s| s.queries[i].map(|v| round(v[0]))).collect(),
                executions: merged.iter().map(|s| s.queries[i].map(|v| v[1].round())).collect(),
            });
        }
    }
}

/// Scans taken minutes apart make noisy slivers; fold neighbours together until each bucket is at least `min_width`
/// wide. A query's bucket is unknown if any folded step was.
fn merge(steps: Vec<Step>, min_width: f64, queries: usize) -> Vec<Step> {
    let mut out: Vec<Step> = Vec::new();
    for step in steps {
        match out.last_mut() {
            Some(last) if last.end - last.start < min_width => {
                last.end = step.end;
                last.time_ms += step.time_ms;
                last.executions += step.executions;
                last.unknown |= step.unknown;
                for (q, s) in last.queries.iter_mut().zip(step.queries) {
                    *q = match (*q, s) {
                        (Some(a), Some(b)) => Some([a[0] + b[0], a[1] + b[1]]),
                        _ => None,
                    };
                }
            }
            _ => out.push(step),
        }
    }
    // A last sliver joins its neighbour rather than standing alone.
    if out.len() >= 2 && out.last().is_some_and(|l| l.end - l.start < min_width / 2.0) {
        let last = out.pop().expect("len >= 2");
        let prev = out.last_mut().expect("len >= 1");
        prev.end = last.end;
        prev.time_ms += last.time_ms;
        prev.executions += last.executions;
        prev.unknown |= last.unknown;
        for (q, s) in prev.queries.iter_mut().zip(last.queries) {
            *q = match (*q, s) {
                (Some(a), Some(b)) => Some([a[0] + b[0], a[1] + b[1]]),
                _ => None,
            };
        }
    }
    debug_assert!(out.iter().all(|s| s.queries.len() == queries));
    out
}

fn round(ms: f64) -> f64 {
    (ms * 100.0).round() / 100.0
}

/// Earlier scans of the same database and source, newest first, within [`HISTORY_DAYS`] before `now`.
fn earlier<'a>(history: &'a [Counters], source: &str, database: &str, now: f64) -> Vec<(f64, &'a Counters)> {
    let mut points: Vec<(f64, &Counters)> = history
        .iter()
        .filter(|c| c.source == source && c.database == database)
        .filter_map(|c| Some((epoch(&c.taken_at)?, c)))
        .filter(|(t, _)| *t < now - 1.0 && *t >= now - HISTORY_DAYS * 86_400.0)
        .collect();
    points.sort_by(|a, b| b.0.total_cmp(&a.0));
    points
}

/// When each statement last ran, for a source that doesn't say (pg_stat_statements): by the latest scan its run count
/// had grown. It last ran after the scan before that one, so this is the latest it can have run, and the time since is
/// how long it has been quiet at least. A statement no earlier scan tracked, or whose entry started over, is left
/// unknown; one that hasn't run since the oldest scan that tracked it last ran by then.
pub fn last_seen_from_scans(workload: &mut Workload, database: &str, taken_at: &str, history: &[Counters]) {
    if workload.windowed {
        return;
    }
    let Some(now) = epoch(taken_at) else { return };
    let points = earlier(history, &workload.source, database, now);
    if points.is_empty() {
        return;
    }
    let start_now = workload.window_start.clone();
    for q in workload.queries.iter_mut().filter(|q| q.last_seen.is_none()) {
        q.last_seen = last_ran(&points, taken_at, start_now.as_deref(), &q.id, q.executions);
    }
}

fn last_ran(
    points: &[(f64, &Counters)],
    taken_at: &str,
    start_now: Option<&str>,
    id: &str,
    runs: f64,
) -> Option<String> {
    let (mut later_at, mut later_runs, mut later_start) = (taken_at, runs, start_now);
    for (t, c) in points {
        // The counters started over after this scan: the statement ran since then, by the later scan.
        if later_start.and_then(epoch).is_some_and(|r| r > t + 1.0) {
            break;
        }
        let Some([_, before]) = c.queries.get(id).copied() else {
            // Not tracked then: it ran at some point since, unknown when, unless a later scan already said.
            return (later_at != taken_at).then(|| later_at.to_string());
        };
        if before < later_runs - 0.5 {
            return Some(later_at.to_string());
        }
        if before > later_runs + 0.5 {
            // Its entry started over (evicted and created again): unknown.
            return None;
        }
        (later_at, later_runs, later_start) = (c.taken_at.as_str(), before, c.window_start.as_deref());
    }
    (later_at != taken_at).then(|| later_at.to_string())
}

/// pg_stat_statements before PostgreSQL 14 keeps no reset time, and its statistics outlive restarts, so the provider
/// leaves the window unknown. The scan before, of the same database, narrows it: if the statements it tracked kept
/// counting, these are the same counters and started when it said (still unknown if it didn't know); if they started
/// over, they did after it, so its time is the earliest they can have started, and the window at most that long.
pub fn infer_window(workload: &mut Workload, previous: Option<(&Workload, &str)>, taken_at: &str) {
    if !workload.window_unknown() {
        return;
    }
    let Some((before, before_at)) = previous.filter(|(p, _)| !p.windowed && p.source == workload.source) else {
        return;
    };
    let start = if started_over(before, workload) { Some(before_at.to_string()) } else { before.window_start.clone() };
    if let Some(start) = start
        && let (Some(s), Some(n)) = (epoch(&start), epoch(taken_at))
        && n > s
    {
        workload.window_hours = (n - s) / 3600.0;
        workload.window_start = Some(start);
    }
}

/// Whether running totals started over between two scans: they shrank, and the statements both tracked counted less
/// (an eviction shrinks the totals too, but the entries that stay keep counting).
fn started_over(before: &Workload, after: &Workload) -> bool {
    let shrank = after.total_executions < before.total_executions || after.total_time_ms < before.total_time_ms - 0.5;
    if !shrank {
        return false;
    }
    let common: Vec<(f64, bool)> = before
        .queries
        .iter()
        .filter_map(|b| {
            let a = after.queries.iter().find(|a| a.id == b.id)?;
            Some((b.total_time_ms.max(1.0), a.executions < b.executions))
        })
        .collect();
    let weight: f64 = common.iter().map(|(w, _)| w).sum();
    let fell: f64 = common.iter().filter(|(_, fell)| *fell).map(|(w, _)| w).sum();
    common.is_empty() || fell * 2.0 > weight
}

#[cfg(test)]
mod tests {
    use super::*;
    use culprit_core::QueryStat;

    fn query(id: &str, time: f64, runs: f64) -> QueryStat {
        let mut q: QueryStat = serde_json::from_value(serde_json::json!({
            "id": id, "query_hash": null, "text": "SELECT 1", "parameters": null, "object_name": null,
            "executions": runs, "total_time_ms": time, "mean_time_ms": time / runs.max(1.0), "max_time_ms": null,
            "stddev_time_ms": null, "cpu_time_ms": null, "rows": null, "logical_reads": null, "physical_reads": null,
            "temp_writes": null, "first_seen": null, "last_seen": null, "plans": [], "plan": null, "waits": []
        }))
        .unwrap();
        q.trend = None;
        q
    }

    fn workload(reset: &str, queries: Vec<QueryStat>) -> Workload {
        Workload {
            source: "pg_stat_statements".into(),
            windowed: false,
            window_start: (!reset.is_empty()).then(|| reset.into()),
            window_end: None,
            window_hours: 1.0,
            total_time_ms: queries.iter().map(|q| q.total_time_ms).sum(),
            total_cpu_ms: None,
            total_executions: queries.iter().map(|q| q.executions).sum(),
            query_count: queries.len() as f64,
            queries,
            timeline: None,
            untracked_time_ms: None,
            min_logged_ms: None,
        }
    }

    fn counters(at: &str, reset: &str, queries: &[(&str, f64, f64)]) -> Counters {
        let w = workload(reset, queries.iter().map(|(id, t, r)| query(id, *t, *r)).collect());
        Counters::of_workload(&w, "shop", at, None)
    }

    #[test]
    fn differences_between_scans_become_buckets() {
        let reset = "2026-10-01T00:00:00Z";
        let history = vec![
            counters("2026-10-10T00:00:00Z", reset, &[("a", 100.0, 10.0), ("b", 50.0, 5.0)]),
            counters("2026-10-10T06:00:00Z", reset, &[("a", 160.0, 16.0), ("b", 50.0, 5.0)]),
        ];
        let mut w = workload(reset, vec![query("a", 400.0, 20.0), query("b", 80.0, 8.0)]);
        from_scans(&mut w, "shop", "2026-10-10T12:00:00Z", None, &history);
        let t = w.timeline.as_ref().unwrap();
        assert_eq!(t.source, TimelineSource::Scans);
        assert_eq!(t.starts, vec!["2026-10-10T00:00:00Z", "2026-10-10T06:00:00Z"]);
        assert_eq!(t.time_ms, vec![60.0, 270.0]);
        let a = w.queries[0].trend.as_ref().unwrap();
        assert_eq!(a.time_ms, vec![Some(60.0), Some(240.0)]);
        assert_eq!(a.mean_ms(1), Some(60.0), "four runs averaging 60 ms, up from 10 ms");
    }

    #[test]
    fn a_statistics_reset_counts_from_zero() {
        let history = vec![counters("2026-10-10T00:00:00Z", "2026-10-01T00:00:00Z", &[("a", 1000.0, 100.0)])];
        let mut w = workload("2026-10-10T03:00:00Z", vec![query("a", 30.0, 3.0)]);
        from_scans(&mut w, "shop", "2026-10-10T06:00:00Z", None, &history);
        let t = w.timeline.as_ref().unwrap();
        assert_eq!((t.time_ms[0], t.executions[0]), (30.0, 3.0));
        assert_eq!(w.queries[0].trend.as_ref().unwrap().time_ms, vec![Some(30.0)]);
    }

    #[test]
    fn untracked_queries_are_unknown_and_other_databases_ignored() {
        let reset = "2026-10-01T00:00:00Z";
        let mut other = counters("2026-10-10T00:00:00Z", reset, &[]);
        other.database = "billing".into();
        let history = vec![other.clone(), counters("2026-10-10T01:00:00Z", reset, &[("a", 10.0, 1.0)])];
        let mut w = workload(reset, vec![query("a", 20.0, 2.0), query("b", 5.0, 1.0)]);
        from_scans(&mut w, "shop", "2026-10-10T02:00:00Z", None, &history);
        assert_eq!(w.timeline.as_ref().unwrap().len(), 1);
        assert_eq!(w.queries[1].trend.as_ref().unwrap().time_ms, vec![None]);

        let mut alone = workload(reset, vec![query("a", 20.0, 2.0)]);
        from_scans(&mut alone, "shop", "2026-10-10T02:00:00Z", None, &[other]);
        assert!(alone.timeline.is_none(), "no earlier scan of this database, no trend");
    }

    #[test]
    fn scans_minutes_apart_fold_together() {
        let reset = "2026-10-01T00:00:00Z";
        let history: Vec<Counters> = (0..10)
            .map(|i| counters(&format!("2026-10-10T00:0{i}:00Z"), reset, &[("a", f64::from(i) * 10.0, f64::from(i))]))
            .collect();
        let mut w = workload(reset, vec![query("a", 100.0, 10.0)]);
        from_scans(&mut w, "shop", "2026-10-10T00:10:00Z", None, &history);
        let t = w.timeline.as_ref().unwrap();
        assert_eq!(t.len(), 2, "ten one-minute steps fold into five-minute buckets");
        assert_eq!(t.time_ms.iter().sum::<f64>(), 100.0);
        assert_eq!(t.unknown, None);
    }

    #[test]
    fn totals_that_shrink_without_a_reset_are_unknown_not_everything_since_the_reset() {
        // Weeks of totals, then pg_stat_statements evicts a heavy entry ("old") between two scans: the totals shrink,
        // but its reset time hasn't moved. The bucket isn't "everything since the reset".
        let reset = "2026-09-01T00:00:00Z";
        let history = vec![
            counters("2026-10-10T00:00:00Z", reset, &[("a", 1_000.0, 100.0), ("old", 900_000.0, 50_000.0)]),
            counters("2026-10-10T06:00:00Z", reset, &[("a", 1_600.0, 160.0), ("old", 905_000.0, 50_300.0)]),
        ];
        let mut w = workload(reset, vec![query("a", 2_000.0, 200.0), query("new", 300.0, 30.0)]);
        w.total_time_ms = 2_300.0 + 400_000.0;
        w.total_executions = 230.0 + 20_000.0;
        from_scans(&mut w, "shop", "2026-10-10T12:00:00Z", None, &history);
        let t = w.timeline.as_ref().unwrap();
        assert_eq!(t.unknown, Some(vec![false, true]));
        // The first step grew as usual; the second holds only what "a" grew by (the new entry's start is unknown).
        assert_eq!(t.time_ms[1], 400.0);
        assert!(t.time_ms[1] < 10_000.0, "not weeks of totals in one bucket");
        assert_eq!(w.queries[0].trend.as_ref().unwrap().time_ms, vec![Some(600.0), Some(400.0)]);

        // Evictions counted by the source flag a step even when the totals still grew.
        let mut a = counters("2026-10-10T00:00:00Z", reset, &[("a", 100.0, 10.0)]);
        let mut b = counters("2026-10-10T06:00:00Z", reset, &[("a", 200.0, 20.0)]);
        (a.evictions, b.evictions) = (Some(3.0), Some(7.0));
        assert_eq!(step_kind(epoch(&a.taken_at).unwrap(), &a, &b), StepKind::Unknown);
        b.evictions = Some(3.0);
        assert_eq!(step_kind(epoch(&a.taken_at).unwrap(), &a, &b), StepKind::Growth);
    }

    #[test]
    fn last_run_comes_from_the_scans_that_saw_the_count_grow() {
        let reset = "2026-10-01T00:00:00Z";
        let history = vec![
            counters("2026-10-10T00:00:00Z", reset, &[("a", 10.0, 1.0), ("b", 50.0, 5.0), ("c", 9.0, 9.0)]),
            counters("2026-10-10T06:00:00Z", reset, &[("a", 20.0, 2.0), ("b", 50.0, 5.0), ("c", 9.0, 9.0)]),
            counters("2026-10-10T12:00:00Z", reset, &[("a", 20.0, 2.0), ("b", 50.0, 5.0)]),
        ];
        let mut w = workload(
            reset,
            vec![query("a", 20.0, 2.0), query("b", 60.0, 6.0), query("c", 9.0, 9.0), query("d", 1.0, 1.0)],
        );
        last_seen_from_scans(&mut w, "shop", "2026-10-10T18:00:00Z", &history);
        let seen = |i: usize| w.queries[i].last_seen.clone();
        // "a" last grew between the 00:00 and 06:00 scans.
        assert_eq!(seen(0).as_deref(), Some("2026-10-10T06:00:00Z"));
        // "b" grew since the last scan.
        assert_eq!(seen(1).as_deref(), Some("2026-10-10T18:00:00Z"));
        // "c" wasn't tracked by the 12:00 scan: it ran since 06:00, unknown when.
        assert_eq!(seen(2), None);
        // "d" was never tracked.
        assert_eq!(seen(3), None);

        // A statistics reset after a scan stops the comparison there: it ran after the reset, by the next scan.
        let history = vec![
            counters("2026-10-10T00:00:00Z", "2026-10-01T00:00:00Z", &[("a", 500.0, 50.0)]),
            counters("2026-10-10T06:00:00Z", "2026-10-10T03:00:00Z", &[("a", 20.0, 2.0)]),
        ];
        let mut w = workload("2026-10-10T03:00:00Z", vec![query("a", 20.0, 2.0)]);
        last_seen_from_scans(&mut w, "shop", "2026-10-10T12:00:00Z", &history);
        assert_eq!(w.queries[0].last_seen.as_deref(), Some("2026-10-10T06:00:00Z"));
    }

    #[test]
    fn an_unknown_window_takes_the_previous_scans_while_the_counters_keep_counting() {
        // PostgreSQL 13 with pg_stat_statements.save on: the provider can't say when the counters started.
        let mut unknown = workload("", vec![query("a", 200.0, 20.0)]);
        unknown.window_hours = 0.0;
        assert!(unknown.window_unknown());
        let before = workload("2026-09-01T00:00:00Z", vec![query("a", 100.0, 10.0)]);
        let mut w = unknown.clone();
        infer_window(&mut w, Some((&before, "2026-10-10T00:00:00Z")), "2026-10-11T00:00:00Z");
        assert_eq!(w.window_start.as_deref(), Some("2026-09-01T00:00:00Z"));
        assert!((w.window_hours - 40.0 * 24.0).abs() < 1e-6);

        // The counters started over after the previous scan: the window is at most the time since it.
        let mut reset = workload("", vec![query("a", 5.0, 1.0)]);
        reset.window_hours = 0.0;
        infer_window(&mut reset, Some((&before, "2026-10-10T00:00:00Z")), "2026-10-11T00:00:00Z");
        assert_eq!(reset.window_start.as_deref(), Some("2026-10-10T00:00:00Z"));
        assert!((reset.window_hours - 24.0).abs() < 1e-6);

        // An eviction shrinks the totals too, but what stays keeps counting: the same counters.
        let mut evicted_before = before.clone();
        evicted_before.queries.push(query("gone", 10_000.0, 1_000.0));
        evicted_before.total_time_ms += 10_000.0;
        evicted_before.total_executions += 1_000.0;
        let mut w = unknown.clone();
        infer_window(&mut w, Some((&evicted_before, "2026-10-10T00:00:00Z")), "2026-10-11T00:00:00Z");
        assert_eq!(w.window_start.as_deref(), Some("2026-09-01T00:00:00Z"));

        // No earlier scan: still unknown.
        let mut alone = unknown.clone();
        infer_window(&mut alone, None, "2026-10-11T00:00:00Z");
        assert!(alone.window_unknown());
    }
}
