//! Trends for sources that only keep running totals (pg_stat_statements, performance_schema, the plan cache): the
//! difference between two of Culprit's own scans is what ran in between. Each saved report gets a small counters file
//! so building a trend never re-reads whole reports.

use std::collections::{BTreeMap, HashMap};

use culprit_core::{QueryTrend, Report, Timeline, TimelineSource, Workload};
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
}

impl Counters {
    /// `None` for windowed sources (Query Store keeps its own history).
    pub fn of(report: &Report) -> Option<Counters> {
        let snapshot = &report.snapshot;
        let workload = snapshot.workload.as_ref().filter(|w| !w.windowed)?;
        Some(Counters::of_workload(workload, &snapshot.server.database, &snapshot.taken_at))
    }

    fn of_workload(workload: &Workload, database: &str, taken_at: &str) -> Counters {
        Counters {
            taken_at: taken_at.to_string(),
            source: workload.source.clone(),
            database: database.to_string(),
            window_start: workload.window_start.clone(),
            total_time_ms: workload.total_time_ms,
            total_executions: workload.total_executions,
            queries: workload.queries.iter().map(|q| (q.id.clone(), [q.total_time_ms, q.executions])).collect(),
        }
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
    queries: Vec<Option<[f64; 2]>>,
}

/// Fill `workload.timeline` and the heaviest queries' trends from earlier scans of the same database and source.
/// Leaves the workload alone when there is no earlier scan to compare with.
pub fn from_scans(workload: &mut Workload, database: &str, taken_at: &str, history: &[Counters]) {
    let current = Counters::of_workload(workload, database, taken_at);
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
            let reset_at = b.window_start.as_deref().and_then(epoch).filter(|r| *r > ta + 1.0);
            let reset = reset_at.is_some()
                || b.total_executions < a.total_executions
                || b.total_time_ms < a.total_time_ms - 0.5;
            let queries = ids
                .iter()
                .map(|id| {
                    let after = *b.queries.get(id)?;
                    if reset {
                        return Some(after);
                    }
                    match a.queries.get(id) {
                        Some(before) if after[1] >= before[1] && after[0] >= before[0] => {
                            Some([after[0] - before[0], after[1] - before[1]])
                        }
                        // The source dropped and re-created the entry; it counts from zero again.
                        Some(_) => Some(after),
                        // Not tracked by the earlier scan: known only when the query first ran after it.
                        None => first_seen.get(id.as_str()).is_some_and(|f| *f > ta).then_some(after),
                    }
                })
                .collect();
            let (time_ms, executions) = if reset {
                (b.total_time_ms, b.total_executions)
            } else {
                (b.total_time_ms - a.total_time_ms, b.total_executions - a.total_executions)
            };
            Step { start: ta, end: tb, time_ms, executions, queries }
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
            window_start: Some(reset.into()),
            window_end: None,
            window_hours: 1.0,
            total_time_ms: queries.iter().map(|q| q.total_time_ms).sum(),
            total_cpu_ms: None,
            total_executions: queries.iter().map(|q| q.executions).sum(),
            query_count: queries.len() as f64,
            queries,
            timeline: None,
        }
    }

    fn counters(at: &str, reset: &str, queries: &[(&str, f64, f64)]) -> Counters {
        let w = workload(reset, queries.iter().map(|(id, t, r)| query(id, *t, *r)).collect());
        Counters::of_workload(&w, "shop", at)
    }

    #[test]
    fn differences_between_scans_become_buckets() {
        let reset = "2026-10-01T00:00:00Z";
        let history = vec![
            counters("2026-10-10T00:00:00Z", reset, &[("a", 100.0, 10.0), ("b", 50.0, 5.0)]),
            counters("2026-10-10T06:00:00Z", reset, &[("a", 160.0, 16.0), ("b", 50.0, 5.0)]),
        ];
        let mut w = workload(reset, vec![query("a", 400.0, 20.0), query("b", 80.0, 8.0)]);
        from_scans(&mut w, "shop", "2026-10-10T12:00:00Z", &history);
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
        from_scans(&mut w, "shop", "2026-10-10T06:00:00Z", &history);
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
        from_scans(&mut w, "shop", "2026-10-10T02:00:00Z", &history);
        assert_eq!(w.timeline.as_ref().unwrap().len(), 1);
        assert_eq!(w.queries[1].trend.as_ref().unwrap().time_ms, vec![None]);

        let mut alone = workload(reset, vec![query("a", 20.0, 2.0)]);
        from_scans(&mut alone, "shop", "2026-10-10T02:00:00Z", &[other]);
        assert!(alone.timeline.is_none(), "no earlier scan of this database, no trend");
    }

    #[test]
    fn scans_minutes_apart_fold_together() {
        let reset = "2026-10-01T00:00:00Z";
        let history: Vec<Counters> = (0..10)
            .map(|i| counters(&format!("2026-10-10T00:0{i}:00Z"), reset, &[("a", f64::from(i) * 10.0, f64::from(i))]))
            .collect();
        let mut w = workload(reset, vec![query("a", 100.0, 10.0)]);
        from_scans(&mut w, "shop", "2026-10-10T00:10:00Z", &history);
        let t = w.timeline.as_ref().unwrap();
        assert_eq!(t.len(), 2, "ten one-minute steps fold into five-minute buckets");
        assert_eq!(t.time_ms.iter().sum::<f64>(), 100.0);
    }
}
