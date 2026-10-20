//! The dashboard's database time tile.

use super::*;

/// Rate of database time in a recent stretch against the stretch before it.
pub(super) struct Trend {
    pub(super) recent_hours: f64,
    pub(super) baseline_hours: f64,
    pub(super) ratio: f64,
    /// Database time in the recent stretch.
    pub(super) recent_ms: f64,
}

impl Trend {
    /// The recent stretch carries a real amount of work: about a minute of database time a day. Below it a rise is
    /// a few hundred milliseconds becoming a second, not news.
    pub(super) fn material(&self) -> bool {
        self.recent_ms / self.recent_hours * 24.0 >= 60_000.0
    }
}

/// Compare the last day (windows of three days or more) or the last quarter of the timeline with what came before,
/// as database time per hour. Buckets that straddle the cut count in proportion. `None` with too little to compare.
pub(super) fn trend(w: &Workload) -> Option<Trend> {
    let t = w.timeline.as_ref()?;
    if t.starts.len() < 3 {
        return None;
    }
    let end = format::epoch_seconds(&t.end)?;
    let mut starts: Vec<f64> = t.starts.iter().map(|s| format::epoch_seconds(s)).collect::<Option<_>>()?;
    // The window can begin inside the first bucket; time before it was never counted.
    if let Some(from) = w.window_start.as_deref().and_then(format::epoch_seconds)
        && from > starts[0]
        && from < starts[1]
    {
        starts[0] = from;
    }
    let span = (end - starts[0]) / 3600.0;
    if span < 2.0 {
        return None;
    }
    let mut recent_hours = if span >= 72.0 { 24.0 } else { span / 4.0 };
    let cut = end - recent_hours * 3600.0;
    let mut baseline_hours = span - recent_hours;
    let (mut recent, mut baseline) = (0.0, 0.0);
    for (i, start) in starts.iter().enumerate() {
        let stop = starts.get(i + 1).copied().unwrap_or(end);
        let width = stop - start;
        let time = t.time_ms.get(i).copied().unwrap_or(0.0);
        if width <= 0.0 {
            continue;
        }
        let after = ((stop - start.max(cut)).max(0.0) / width).min(1.0);
        // A bucket holding only a lower bound (a running total that fell without a reset) is left out of both sides,
        // time and hours alike: the comparison is of rates over the time that was fully counted.
        if t.is_unknown(i) {
            recent_hours -= width * after / 3600.0;
            baseline_hours -= width * (1.0 - after) / 3600.0;
            continue;
        }
        recent += time * after;
        baseline += time * (1.0 - after);
    }
    if recent_hours <= 0.0 {
        return None;
    }
    // Under a second of database time either side, a ratio is noise.
    if recent + baseline < 1_000.0 || baseline <= 0.0 || baseline_hours <= 0.0 {
        return None;
    }
    Some(Trend {
        recent_hours,
        baseline_hours,
        ratio: (recent / recent_hours) / (baseline / baseline_hours),
        recent_ms: recent,
    })
}

/// "the last day", "the last 6 hours".
/// The trend's two spans in words that add up to the window: over 10 hours, "the last 3 hours" and "7 hours", not
/// "3" and "8" from rounding each on its own.
pub(super) fn trend_spans(t: &Trend) -> (String, String) {
    let (r, b) = (t.recent_hours, t.baseline_hours);
    if r >= 1.5 && b >= 1.5 && r + b < 47.0 {
        let recent = r.round();
        let baseline = ((r + b).round() - recent).max(1.0);
        return (last(recent), span(baseline));
    }
    (last(r), span(b))
}

/// A query that started using another plan inside the window: its share of database time and when.
pub(super) struct PlanChange<'a> {
    query: &'a QueryStat,
    plan: String,
    at: String,
    share: f64,
}

/// New plans of queries that matter (2% of database time or more), newest plan per query, biggest share first.
/// The same rule as the plan flags on the chart.
pub(super) fn plan_changes<'a>(s: &'a Snapshot, w: &Workload) -> Vec<PlanChange<'a>> {
    let from = w.window_start.as_deref().and_then(format::epoch_seconds);
    // A plan switch of a query that has since stopped running is history, not news. Its share as every surface states
    // it ([`Workload::statement_share`]).
    let mut out: Vec<PlanChange> = app_queries(s)
        .filter(|q| q.plans.len() >= 2 && w.statement_share(q) >= 0.02)
        .filter(|q| !crate::query_is_history(s, q))
        .filter_map(|q| {
            let seen = |p: &culprit_core::PlanVariant| p.first_seen.as_deref().and_then(format::epoch_seconds);
            let first = q.plans.iter().filter_map(seen).fold(f64::INFINITY, f64::min);
            let newest = q
                .plans
                .iter()
                .filter(|p| seen(p).is_some_and(|t| t > first && from.is_none_or(|f| t >= f)))
                .max_by(|a, b| seen(a).unwrap_or(0.0).total_cmp(&seen(b).unwrap_or(0.0)))?;
            Some(PlanChange {
                query: q,
                plan: newest.id.clone(),
                at: newest.first_seen.clone().unwrap_or_default(),
                share: w.statement_share(q),
            })
        })
        .collect();
    out.sort_by(|a, b| b.share.total_cmp(&a.share));
    out
}

pub(super) fn db_time(s: &Snapshot) -> Tile {
    let Some(w) = &s.workload else { return no_workload(TileKey::DbTime, "database time isn't measured") };
    let per_day = w.measurement_hours() >= PER_DAY_MIN_HOURS;
    let window = window_phrase(w);
    let mut metrics = Vec::new();
    if per_day {
        let day = w.per_day(w.total_time_ms);
        metrics.push(TileMetric::new("per-day", "Database time per day", Some(day), format::duration(day)));
        let runs = w.per_day(w.total_executions);
        metrics.push(TileMetric::new("runs-per-day", "Runs per day", Some(runs), format::count(runs)));
    } else {
        metrics.push(TileMetric::new(
            "total",
            "Database time",
            Some(w.total_time_ms),
            format::duration(w.total_time_ms),
        ));
        metrics.push(TileMetric::new("runs", "Runs", Some(w.total_executions), format::count(w.total_executions)));
    }
    if let Some(cpu) = w.total_cpu_ms.filter(|_| w.total_time_ms > 0.0) {
        let share = (cpu / w.total_time_ms).min(1.0);
        metrics.push(TileMetric::new("cpu-share", "On CPU", Some(share), format::percent(share)));
    }
    metrics.push(TileMetric::new("window-hours", "Covers", Some(w.measurement_hours()), span(w.measurement_hours())));

    let changes = plan_changes(s, w);
    let (status, takeaway) = match trend(w) {
        Some(t) if t.ratio >= 1.5 && t.material() => (
            if t.ratio >= 3.0 { TileStatus::Bad } else { TileStatus::Watch },
            format!(
                "Database time is up {:.1}× in {} compared with the {} before.",
                t.ratio,
                trend_spans(&t).0,
                trend_spans(&t).1
            ),
        ),
        // Up, but from almost nothing: said as it is, not as steady and not as a warning.
        Some(t) if t.ratio >= 1.5 => (
            TileStatus::Info,
            format!(
                "Database time is up {:.1}× in {}, but small: {} over {window}.",
                t.ratio,
                trend_spans(&t).0,
                format::duration(w.total_time_ms)
            ),
        ),
        Some(t) if t.ratio <= 1.0 / 1.5 => (
            TileStatus::Good,
            format!(
                "Database time is down {} in {} compared with the {} before.",
                format::percent(1.0 - t.ratio),
                trend_spans(&t).0,
                trend_spans(&t).1
            ),
        ),
        Some(_) if per_day => (
            TileStatus::Good,
            format!("Database time is steady at about {} a day.", format::duration(w.per_day(w.total_time_ms))),
        ),
        Some(_) => {
            (TileStatus::Good, format!("Database time is steady: {} over {window}.", format::duration(w.total_time_ms)))
        }
        None if per_day => (
            TileStatus::Info,
            format!("About {} of database time a day over {window}.", format::duration(w.per_day(w.total_time_ms))),
        ),
        None => (
            TileStatus::Info,
            format!("{} of database time over {window}.", capitalized(&format::duration(w.total_time_ms))),
        ),
    };
    let mut tile = Tile::new(TileKey::DbTime, status, takeaway);
    tile.metrics = metrics;
    tile.detail = match changes.first() {
        Some(c) if changes.len() == 1 => Some(format!(
            "{} switched plans in this window ({} of database time).",
            capitalized(&query_handle(c.query)),
            format::percent(c.share)
        )),
        Some(c) => Some(format!(
            "{} queries switched plans in this window; the busiest is {} ({} of database time).",
            changes.len(),
            query_handle(c.query),
            format::percent(c.share)
        )),
        None if !w.is_windowed() && w.timeline.as_ref().is_none_or(|t| t.starts.len() < 2) => Some(format!(
            "{} keeps running totals only, so the trend comes from Culprit's own scans; it appears after the next scan.",
            w.source
        )),
        None => None,
    };
    tile.items = changes
        .iter()
        .take(5)
        .map(|c| {
            let mut item = TileItem::new(ItemKind::PlanChange, c.plan.clone(), query_handle(c.query));
            item.value = Some(format::percent(c.share));
            item.share = Some(c.share);
            item.query_id = Some(c.query.id.clone());
            item.at = Some(c.at.clone());
            item
        })
        .collect();
    tile
}
