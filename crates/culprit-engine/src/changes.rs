//! "What changed" between two periods: which periods, and where the numbers come from. Query Store answers from its
//! own intervals; sources that only keep running totals (pg_stat_statements, performance_schema) answer from the
//! differences between Culprit's earlier scans, snapped to the scans it has.

use std::collections::{BTreeMap, BTreeSet};

use culprit_core::changes::{CompareOptions, Period, PeriodData, PeriodStats, PlanPeriods, QueryPeriods, epoch};
use culprit_core::{ChangeKind, Changes, Engine, Error, Result, TimelineSource};
use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::trends::{self, Counters, QueryMeta, StepKind};

/// Which two periods to compare. Everything is optional: by default the recent period is the last half of the
/// history (at most a day) and the baseline is the same length right before it.
#[derive(Serialize, Deserialize, TS, Clone, Debug, Default, PartialEq)]
#[ts(export)]
pub struct ChangeRequest {
    /// Compare everything since this moment with the same length of time before it (a deploy, a plan change).
    #[serde(default)]
    pub since: Option<String>,
    #[serde(default)]
    pub recent_start: Option<String>,
    #[serde(default)]
    pub recent_end: Option<String>,
    #[serde(default)]
    pub baseline_start: Option<String>,
    #[serde(default)]
    pub baseline_end: Option<String>,
    /// The recent period as "the last N hours".
    #[serde(default)]
    pub recent_hours: Option<f64>,
    /// Runs a query needs in each period (default 10).
    #[serde(default)]
    pub min_executions: Option<f64>,
    /// How many times slower or faster counts as a change (default 1.5).
    #[serde(default)]
    pub threshold: Option<f64>,
    /// Read cloud metrics (CloudWatch, Azure Monitor) to spot throttling (default: yes, for cloud databases).
    #[serde(default)]
    pub cloud: Option<bool>,
}

impl ChangeRequest {
    /// No period asked for: use the default comparison.
    pub fn is_default(&self) -> bool {
        self.since.is_none()
            && self.recent_start.is_none()
            && self.recent_end.is_none()
            && self.baseline_start.is_none()
            && self.baseline_end.is_none()
            && self.recent_hours.is_none()
    }
}

pub fn iso(seconds: f64) -> String {
    chrono::DateTime::from_timestamp(seconds.floor() as i64, 0)
        .map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
        .unwrap_or_default()
}

fn parse(field: &str, value: &Option<String>) -> Result<Option<f64>> {
    value
        .as_deref()
        .map(|v| {
            epoch(v).ok_or_else(|| {
                Error::invalid(format!("`{field}` isn't a timestamp: {v}"))
                    .with_hint("Use ISO 8601 with a time zone, e.g. 2026-10-11T14:05:00Z")
            })
        })
        .transpose()
}

/// The two periods, with the baseline ending where the recent period starts unless the request says otherwise.
/// `history` is where the source's data begins and ends (epoch seconds).
pub fn resolve(request: &ChangeRequest, history: (f64, f64)) -> Result<(Period, Period)> {
    let (first, now) = history;
    let recent_end = parse("recent_end", &request.recent_end)?.unwrap_or(now).min(now);
    let recent_start = match (parse("since", &request.since)?, parse("recent_start", &request.recent_start)?) {
        (Some(since), _) => since,
        (None, Some(start)) => start,
        (None, None) => {
            let hours = request.recent_hours.filter(|h| *h > 0.0).unwrap_or_else(|| {
                let span = (recent_end - first).max(0.0) / 3600.0;
                (span / 2.0).clamp(0.25, 24.0)
            });
            recent_end - hours * 3600.0
        }
    };
    if recent_start >= recent_end {
        return Err(Error::invalid("The recent period is empty: its start must come before its end"));
    }
    let length = recent_end - recent_start;
    let baseline_end = parse("baseline_end", &request.baseline_end)?.unwrap_or(recent_start).min(recent_start);
    let baseline_start = parse("baseline_start", &request.baseline_start)?.unwrap_or(baseline_end - length);
    if baseline_start >= baseline_end {
        return Err(Error::invalid("The baseline period is empty: its start must come before its end"));
    }
    Ok((
        Period { start: iso(baseline_start), end: iso(baseline_end) },
        Period { start: iso(recent_start), end: iso(recent_end) },
    ))
}

/// For sources compared through scans, the natural default: since the previous scan, against the gap before it.
pub fn default_scan_periods(history: &[Counters]) -> Option<(Period, Period)> {
    let anchor = history.iter().filter_map(|c| Some((epoch(&c.taken_at)?, c))).max_by(|a, b| a.0.total_cmp(&b.0))?.1;
    let mut times: Vec<f64> =
        history.iter().filter(|c| c.same_series(anchor)).filter_map(|c| epoch(&c.taken_at)).collect();
    times.sort_by(f64::total_cmp);
    let n = times.len();
    (n >= 3).then(|| {
        (
            Period { start: iso(times[n - 3]), end: iso(times[n - 2]) },
            Period { start: iso(times[n - 2]), end: iso(times[n - 1]) },
        )
    })
}

pub fn options(request: &ChangeRequest) -> CompareOptions {
    let mut options = CompareOptions::default();
    if let Some(min) = request.min_executions.filter(|m| *m >= 1.0) {
        options.min_executions = min;
    }
    if let Some(threshold) = request.threshold.filter(|t| *t > 1.0) {
        options.threshold = threshold;
    }
    options
}

/// "orders: Index Seek (ix_orders) → Index Scan (ix_orders)" from Culprit's comparison of the two plans.
pub fn what_differs(a: &culprit_core::Plan, b: &culprit_core::Plan) -> Option<String> {
    let diff = culprit_core::plan_diff(a, b);
    diff.changes
        .iter()
        .find(|c| c.kind == ChangeKind::Access)
        .or_else(|| diff.changes.iter().find(|c| c.kind == ChangeKind::Operator))
        .map(|c| readable_names(&c.title))
}

/// SQL Server names unnamed primary keys `PK__events__3213E83FAE835E88`; say "primary key" instead.
fn readable_names(text: &str) -> String {
    text.split_inclusive(|c: char| !(c.is_alphanumeric() || c == '_'))
        .map(|part| {
            let word = part.trim_end_matches(|c: char| !(c.is_alphanumeric() || c == '_'));
            let rest = &part[word.len()..];
            let generated = word.starts_with("PK__")
                && word.rsplit("__").next().is_some_and(|h| h.len() >= 8 && h.chars().all(|c| c.is_ascii_hexdigit()));
            if generated { format!("primary key{rest}") } else { part.to_string() }
        })
        .collect()
}

/// The scan nearest to `t`.
fn nearest(points: &[(f64, &Counters)], t: f64) -> usize {
    points
        .iter()
        .enumerate()
        .min_by(|a, b| (a.1.0 - t).abs().total_cmp(&(b.1.0 - t).abs()))
        .map(|(i, _)| i)
        .unwrap_or(0)
}

/// What ran between scans `from` and `to` (indexes into `points`): totals, and each query's `[time, runs]` when every
/// step in between could account for it.
struct Span {
    time_ms: f64,
    executions: f64,
    /// Steps whose totals shrank without the source resetting (entries evicted): the totals hold only what the
    /// statements still there grew by.
    unknown_steps: usize,
    queries: BTreeMap<String, Option<[f64; 2]>>,
}

fn span(points: &[(f64, &Counters)], from: usize, to: usize) -> Span {
    let mut out = Span { time_ms: 0.0, executions: 0.0, unknown_steps: 0, queries: BTreeMap::new() };
    for (_, c) in &points[from..=to] {
        for id in c.queries.keys() {
            out.queries.entry(id.clone()).or_insert(Some([0.0, 0.0]));
        }
    }
    for pair in points[from..=to].windows(2) {
        let ((ta, a), (_, b)) = (pair[0], pair[1]);
        // A reset only when the source says so: totals also shrink when entries are evicted, and then the later totals
        // are not "everything since", only what the entries still there grew by.
        let kind = trends::step_kind(ta, a, b);
        match kind {
            StepKind::Reset => {
                out.time_ms += b.total_time_ms;
                out.executions += b.total_executions;
            }
            StepKind::Growth => {
                out.time_ms += b.total_time_ms - a.total_time_ms;
                out.executions += b.total_executions - a.total_executions;
            }
            StepKind::Unknown => {
                out.unknown_steps += 1;
                for id in b.queries.keys() {
                    if let Some(v) = trends::query_step(kind, ta, a, b, id, None) {
                        out.time_ms += v[0];
                        out.executions += v[1];
                    }
                }
            }
        }
        for (id, value) in out.queries.iter_mut() {
            // Sources report first-seen to the second; an entry created in the scan's own second is new.
            let first = b.meta.get(id).and_then(|m| m.first_seen.as_deref()).and_then(epoch);
            let step = match (a.queries.get(id), b.queries.get(id)) {
                (_, Some(_)) => trends::query_step(kind, ta, a, b, id, first),
                // Gone from the later scan: either it stopped or it fell out of the heaviest queries; unknown.
                (Some(_), None) => None,
                (None, None) => Some([0.0, 0.0]),
            };
            *value = match (*value, step) {
                (Some(v), Some(s)) => Some([v[0] + s[0], v[1] + s[1]]),
                _ => None,
            };
        }
    }
    out
}

/// Period data from Culprit's own scans of a source that keeps only running totals.
pub fn from_scans(history: &[Counters], baseline: &Period, recent: &Period) -> PeriodData {
    let mut data = PeriodData {
        source: TimelineSource::Scans,
        baseline: baseline.clone(),
        recent: recent.clone(),
        ..Default::default()
    };
    let cutoff = epoch(&recent.end).unwrap_or(f64::NEG_INFINITY);
    let anchor = history
        .iter()
        .filter_map(|c| Some((epoch(&c.taken_at)?, c)))
        .filter(|(at, _)| *at <= cutoff)
        .max_by(|a, b| a.0.total_cmp(&b.0))
        .map(|(_, c)| c);
    let mut points: Vec<(f64, &Counters)> = history
        .iter()
        .filter(|c| anchor.is_some_and(|a| c.same_series(a)))
        .filter_map(|c| Some((epoch(&c.taken_at)?, c)))
        .collect();
    points.sort_by(|a, b| a.0.total_cmp(&b.0));
    let at = |p: &Period| (epoch(&p.start).unwrap_or(0.0), epoch(&p.end).unwrap_or(0.0));
    let ((b0, _), (r0, r1)) = (at(baseline), at(recent));
    if points.len() < 3 {
        let why = format!(
            "Comparing two periods needs at least three scans of this database; Culprit has {}. Scan again later (each scan marks a point in time).",
            points.len()
        );
        data.notes.push(why.clone());
        data.unavailable = Some(why);
        return data;
    }
    let end = nearest(&points, r1);
    let mut mid = nearest(&points, r0).min(end.saturating_sub(1));
    let mut start = nearest(&points, b0).min(mid.saturating_sub(1));
    if start == mid {
        mid = (start + 1).min(end.saturating_sub(1));
        if mid == start {
            start = mid.saturating_sub(1);
        }
    }
    if !(start < mid && mid < end) {
        let why = "The scans Culprit has don't fall inside both periods; pick a longer range.".to_string();
        data.notes.push(why.clone());
        data.unavailable = Some(why);
        return data;
    }
    data.baseline = Period { start: iso(points[start].0), end: iso(points[mid].0) };
    data.recent = Period { start: iso(points[mid].0), end: iso(points[end].0) };
    let wanted = (r0 - b0).max(1.0);
    data.baseline_coverage = ((points[mid].0 - points[start].0) / wanted).clamp(0.0, 1.0);
    data.notes.push(format!(
        "Periods follow Culprit's scans: {} scans between {} and {}.",
        end - start + 1,
        culprit_core::changes::short_time(&iso(points[start].0)),
        culprit_core::changes::short_time(&iso(points[end].0))
    ));

    let before = span(&points, start, mid);
    let after = span(&points, mid, end);
    if before.unknown_steps + after.unknown_steps > 0 {
        data.notes.push(
            "Between some of these scans the source's totals shrank without a reset (entries evicted or plans dropped), so those stretches count only what the statements still tracked grew by: totals there are a lower bound."
                .to_string(),
        );
    }
    data.baseline_total = PeriodStats::new(before.executions, before.time_ms);
    data.recent_total = PeriodStats::new(after.executions, after.time_ms);

    let meta_at = |i: usize, id: &str| -> Option<&QueryMeta> { points[i].1.meta.get(id) };
    // Scans that recorded no queries at all (statistics just reset, or nothing ran): a new query isn't "untracked", there
    // was simply nothing to compare it with.
    let baseline_empty = points[start..=mid].iter().all(|(_, c)| c.queries.is_empty());
    let latest_meta = |id: &str, from: usize, to: usize| (from..=to).rev().find_map(|i| meta_at(i, id));
    let ids: BTreeSet<String> = before.queries.keys().chain(after.queries.keys()).cloned().collect();
    for id in ids {
        let known = |s: &Span| s.queries.get(&id).copied().flatten();
        let b = known(&before).filter(|v| v[1] > 0.0).map(|v| PeriodStats::new(v[1], v[0]));
        let r = known(&after).filter(|v| v[1] > 0.0).map(|v| PeriodStats::new(v[1], v[0]));
        let unknown_before = before.queries.get(&id).is_none_or(Option::is_none);
        let unknown_after = after.queries.get(&id).is_none_or(Option::is_none);
        let meta = latest_meta(&id, start, end);
        let untracked = match (&b, &r) {
            (None, Some(_)) if unknown_before && baseline_empty => Some(
                "Culprit's scans before then recorded no queries at all (the statistics were empty, for example just reset), so there is nothing to compare it with"
                    .to_string(),
            ),
            (None, Some(_)) if unknown_before => {
                Some("Culprit's earlier scans didn't track it (it wasn't among the heaviest queries then)".to_string())
            }
            (Some(_), None) if unknown_after => Some(
                "it's no longer among the heaviest queries Culprit tracks, and the source doesn't say when it last ran"
                    .to_string(),
            ),
            _ => None,
        };
        // A query the source says last ran before the recent period really stopped; one it says first ran after the
        // baseline really is new (pg_stat_statements' stats_since, Query Store's first execution).
        let stopped = meta.and_then(|m| m.last_seen.as_deref()).and_then(epoch).is_some_and(|t| t < points[mid].0);
        let started =
            meta.and_then(|m| m.first_seen.as_deref()).and_then(epoch).is_some_and(|t| t + 1.0 >= points[mid].0);
        let untracked = if stopped || (started && b.is_none()) { None } else { untracked };

        let shape_before = latest_meta(&id, start, mid).and_then(|m| Some((m.shape.clone()?, m.hot.clone())));
        let shape_after = latest_meta(&id, mid, end).and_then(|m| Some((m.shape.clone()?, m.hot.clone())));
        let first_after = shape_after.as_ref().and_then(|(shape, _)| {
            (mid..=end)
                .find(|&i| meta_at(i, &id).and_then(|m| m.shape.as_ref()) == Some(shape))
                .map(|i| iso(points[i].0))
        });
        let plans = match (shape_before, shape_after) {
            (Some((a, hot_a)), Some((later, hot_b))) if a == later => vec![PlanPeriods {
                id: short_hash(&a),
                label: hot_a.or(hot_b),
                baseline: b.clone(),
                recent: r.clone(),
                first_recent: None,
            }],
            (Some((a, hot_a)), Some((later, hot_b))) => vec![
                PlanPeriods { id: short_hash(&a), label: hot_a, baseline: b.clone(), recent: None, first_recent: None },
                PlanPeriods {
                    id: short_hash(&later),
                    label: hot_b,
                    baseline: None,
                    recent: r.clone(),
                    first_recent: first_after,
                },
            ],
            _ => Vec::new(),
        };
        data.queries.push(QueryPeriods {
            query_id: id.clone(),
            text: meta.map(|m| m.text.clone()).unwrap_or_default(),
            object_name: None,
            baseline: b,
            recent: r,
            plans,
            untracked,
        });
    }
    data
}

fn short_hash(text: &str) -> String {
    // FNV-1a: stable across runs and platforms, enough to tell plan shapes apart in a list.
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in text.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{:06x}", hash & 0xff_ffff)
}

/// Explain plan switches the source can show both plans for (Query Store), most important first.
pub async fn describe_switches(changes: &mut Changes, provider: &dyn culprit_core::Provider, limit: usize) {
    if changes.engine != Engine::Mssql || changes.source != TimelineSource::QueryStore {
        return;
    }
    let mut described = 0;
    for change in changes.queries.iter_mut() {
        if described >= limit {
            break;
        }
        let Some(switch) = change.plan_switch.as_mut() else { continue };
        if switch.what.is_some() {
            continue;
        }
        described += 1;
        let (Ok(Some(a)), Ok(Some(b))) = (
            provider.stored_plan(&change.query_id, &switch.from).await,
            provider.stored_plan(&change.query_id, &switch.to).await,
        ) else {
            continue;
        };
        switch.from_label = a.hottest().map(|n| n.op.clone());
        switch.to_label = b.hottest().map(|n| n.op.clone());
        switch.what = what_differs(&a, &b);
        change.headline =
            culprit_core::changes::headline(change, changes.engine, changes.source, changes.min_executions);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use culprit_core::ChangeStatus;

    #[test]
    fn periods_default_to_the_last_half_and_its_neighbour() {
        let now = epoch("2026-10-11T12:00:00Z").unwrap();
        let (b, r) = resolve(&ChangeRequest::default(), (now - 10.0 * 3600.0, now)).unwrap();
        assert_eq!((r.start.as_str(), r.end.as_str()), ("2026-10-11T07:00:00Z", "2026-10-11T12:00:00Z"));
        assert_eq!((b.start.as_str(), b.end.as_str()), ("2026-10-11T02:00:00Z", "2026-10-11T07:00:00Z"));
        let since = ChangeRequest { since: Some("2026-10-11T10:00:00Z".into()), ..Default::default() };
        let (b, r) = resolve(&since, (now - 30.0 * 86_400.0, now)).unwrap();
        assert_eq!((b.start.as_str(), r.start.as_str()), ("2026-10-11T08:00:00Z", "2026-10-11T10:00:00Z"));
        let nonsense = ChangeRequest { since: Some("yesterday".into()), ..Default::default() };
        assert!(resolve(&nonsense, (0.0, now)).is_err());
    }

    fn counters(at: &str, total: [f64; 2], queries: &[(&str, f64, f64, Option<&str>)]) -> Counters {
        Counters {
            reference: None,
            measurement: None,
            taken_at: at.into(),
            source: "pg_stat_statements".into(),
            database: "shop".into(),
            window_start: Some("2026-10-01T00:00:00Z".into()),
            total_time_ms: total[0],
            total_executions: total[1],
            queries: queries.iter().map(|(id, t, r, _)| (id.to_string(), [*t, *r])).collect(),
            meta: queries
                .iter()
                .map(|(id, _, _, shape)| {
                    let meta = QueryMeta {
                        text: format!("SELECT {id}"),
                        shape: shape.map(str::to_string),
                        hot: shape.map(str::to_string),
                        ..Default::default()
                    };
                    (id.to_string(), meta)
                })
                .collect(),
            evictions: None,
        }
    }

    #[test]
    fn totals_that_shrink_without_a_reset_are_a_lower_bound_not_a_restart() {
        // Weeks of totals, then entries evicted between the second and third scans: the totals shrink, the reset time
        // doesn't move. The recent period is what the statements still there grew by, not everything since the reset.
        let history = vec![
            counters("2026-10-11T00:00:00Z", [900_000.0, 50_000.0], &[("a", 100.0, 10.0, None)]),
            counters("2026-10-11T06:00:00Z", [900_300.0, 50_030.0], &[("a", 400.0, 40.0, None)]),
            counters("2026-10-11T12:00:00Z", [5_000.0, 500.0], &[("a", 700.0, 70.0, None)]),
        ];
        let (baseline, recent) = default_scan_periods(&history).unwrap();
        let data = from_scans(&history, &baseline, &recent);
        assert_eq!(data.recent_total.mean_ms, 10.0);
        assert!(data.recent_total.executions < 100.0, "{:?}", data.recent_total);
        assert!(data.notes.iter().any(|n| n.contains("lower bound")), "{:?}", data.notes);
    }

    #[test]
    fn scans_give_the_same_judgement_as_query_store() {
        let history = vec![
            counters(
                "2026-10-11T00:00:00Z",
                [1000.0, 200.0],
                &[("a", 100.0, 100.0, Some("Index Scan on orders")), ("b", 50.0, 50.0, None)],
            ),
            counters(
                "2026-10-11T06:00:00Z",
                [1300.0, 400.0],
                &[("a", 300.0, 200.0, Some("Index Scan on orders")), ("b", 100.0, 100.0, None)],
            ),
            counters(
                "2026-10-11T12:00:00Z",
                [3500.0, 560.0],
                &[("a", 2300.0, 300.0, Some("Seq Scan on orders")), ("c", 40.0, 20.0, None)],
            ),
        ];
        let (baseline, recent) = default_scan_periods(&history).unwrap();
        let data = from_scans(&history, &baseline, &recent);
        assert!((data.baseline_coverage - 1.0).abs() < 1e-9);
        let changes = culprit_core::changes::compare("shop", Engine::Postgres, data, &CompareOptions::default());
        let a = changes.queries.iter().find(|q| q.query_id == "a").unwrap();
        assert_eq!(a.status, ChangeStatus::Regressed, "2 ms → 20 ms per run");
        let switch = a.plan_switch.as_ref().expect("the captured plan changed");
        assert_eq!(switch.what.as_deref(), Some("Index Scan on orders → Seq Scan on orders"));
        assert!(a.headline.contains("when the plan Culprit captured for it changed"), "{}", a.headline);
        // b vanished from the heaviest queries: can't tell it stopped. c appeared without a first-seen time, so how much
        // of it ran recently is unknown: it isn't listed rather than guessed.
        let status = |id: &str| changes.queries.iter().find(|q| q.query_id == id).map(|q| q.status);
        assert_eq!(status("b"), Some(ChangeStatus::NotComparable));
        assert_eq!(status("c"), None);
    }

    #[test]
    fn generated_key_names_read_as_primary_key() {
        assert_eq!(
            readable_names(
                "events: Key Lookup (PK__events__3213E83FAE835E88) → Clustered Index Scan (PK__events__3213E83FAE835E88)"
            ),
            "events: Key Lookup (primary key) → Clustered Index Scan (primary key)"
        );
        assert_eq!(readable_names("orders: Index Seek (ix_orders_status)"), "orders: Index Seek (ix_orders_status)");
    }

    #[test]
    fn a_statement_the_source_says_started_after_the_baseline_is_new() {
        let mut later = counters(
            "2026-10-11T12:00:00Z",
            [3_000.0, 600.0],
            &[("a", 300.0, 300.0, None), ("n", 2_640.0, 200.0, None)],
        );
        later.meta.get_mut("n").unwrap().first_seen = Some("2026-10-11T09:30:00Z".into());
        let history = vec![
            counters("2026-10-11T00:00:00Z", [100.0, 100.0], &[("a", 100.0, 100.0, None)]),
            counters("2026-10-11T06:00:00Z", [200.0, 200.0], &[("a", 200.0, 200.0, None)]),
            later,
        ];
        let (baseline, recent) = default_scan_periods(&history).unwrap();
        let data = from_scans(&history, &baseline, &recent);
        let n = data.queries.iter().find(|q| q.query_id == "n").expect("n ran recently");
        assert_eq!(n.untracked, None, "first seen after the baseline: new, not untracked");
        let changes = culprit_core::changes::compare("shop", Engine::Postgres, data, &CompareOptions::default());
        let status = changes.queries.iter().find(|q| q.query_id == "n").map(|q| q.status);
        assert_eq!(status, Some(ChangeStatus::New));
    }

    #[test]
    fn empty_earlier_scans_are_not_called_untracked() {
        // Statistics were reset before the first scans: they saw nothing, then a query appeared.
        let mut later = counters("2026-10-11T12:00:00Z", [500.0, 50.0], &[("a", 500.0, 50.0, None)]);
        later.window_start = Some("2026-10-11T07:00:00Z".into());
        let history = vec![
            counters("2026-10-11T00:00:00Z", [0.0, 0.0], &[]),
            counters("2026-10-11T06:00:00Z", [0.0, 0.0], &[]),
            later,
        ];
        let (baseline, recent) = default_scan_periods(&history).unwrap();
        let data = from_scans(&history, &baseline, &recent);
        let a = data.queries.iter().find(|q| q.query_id == "a").expect("a ran recently");
        let why = a.untracked.as_deref().unwrap_or_default();
        assert!(why.contains("recorded no queries at all"), "{why}");
        assert!(!why.contains("heaviest"), "{why}");
    }

    #[test]
    fn too_few_scans_say_so() {
        let history = vec![counters("2026-10-11T00:00:00Z", [1.0, 1.0], &[])];
        let p = Period { start: "2026-10-11T00:00:00Z".into(), end: "2026-10-11T06:00:00Z".into() };
        let data = from_scans(&history, &p, &p);
        assert!(data.unavailable.as_deref().is_some_and(|w| w.contains("at least three scans")));
        let changes = culprit_core::changes::compare("shop", Engine::Postgres, data, &CompareOptions::default());
        assert!(changes.summary.starts_with("Can't compare yet."), "{}", changes.summary);
        assert!(!changes.summary.contains("No queries ran"));
    }
}
