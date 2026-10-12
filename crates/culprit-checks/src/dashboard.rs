//! The per-database dashboard, composed from one report: a takeaway, the numbers behind it and the rows to open for
//! each of fourteen tiles. `now` (sessions right now) comes from a live read and is added with [`with_now`].
//!
//! Every tile says something on every engine: when an engine, login or scan can't show a signal, the tile is
//! `unavailable` and its takeaway says why, in the engine's own terms.

use std::collections::{BTreeMap, HashMap, HashSet};

use culprit_core::insights::{coverage_note, insights};
use culprit_core::signals::{FileIo, FileKind, Replica, ReplicaRole, merge_logs};
use culprit_core::{
    Capability, Dashboard, Engine, Finding, ItemKind, LiveState, QueryStat, Report, ServerInfo, Severity, Signal,
    SignalStatus, SignalUnit, Snapshot, Tile, TileItem, TileKey, TileMetric, TileStatus, WaitScope, Workload, format,
};

use crate::support::{app_queries, query_handle, since_phrase, statement_handle, top_findings_share, window_phrase};

/// Below this many hours, per-day figures would be forecasts from a sliver of data (the UI's `PER_DAY_MIN_HOURS`).
const PER_DAY_MIN_HOURS: f64 = 20.0;

/// The capabilities that hold query statistics.
const WORKLOAD: [Capability; 4] =
    [Capability::QueryStore, Capability::PgStatStatements, Capability::PerformanceSchema, Capability::PlanCache];

/// Findings about the query statistics themselves: a store that is off, full, forgetting or capturing too little.
const STATISTICS_FINDINGS: [&str; 10] = [
    "config:query-store",
    "config:query-store-size",
    "config:query-store-capture",
    "config:query-store-waits",
    "config:pg_stat_statements.track",
    "config:pg_stat_statements.max",
    "config:performance_schema",
    "config:statements_digest",
    "config:performance_schema_digests_size",
    "config:max_digest_length",
];

/// Most urgent first when two tiles share a status.
const PRIORITY: [TileKey; 14] = [
    TileKey::Now,
    TileKey::DbTime,
    TileKey::Findings,
    TileKey::Statistics,
    TileKey::Failures,
    TileKey::Replication,
    TileKey::Deadlocks,
    TileKey::Storage,
    TileKey::Health,
    TileKey::Connections,
    TileKey::Waits,
    TileKey::Plans,
    TileKey::Indexes,
    TileKey::TopQueries,
];

/// The dashboard for one report. The `now` tile says sessions weren't read until [`with_now`] adds a live read.
pub fn dashboard(report: &Report) -> Dashboard {
    let s = &report.snapshot;
    let tiles = vec![
        db_time(s),
        findings(report),
        top_queries(s),
        waits(s),
        not_read(),
        health(s),
        deadlocks(report),
        indexes(report),
        plans(s),
        storage(report),
        connections(report),
        replication(report),
        failures(report),
        statistics(report),
    ];
    let mut d = Dashboard {
        connection: s.connection.clone(),
        engine: s.server.engine,
        product: s.server.product.clone(),
        database: s.server.database.clone(),
        environment: s.environment,
        report_id: s.id.clone(),
        taken_at: s.taken_at.clone(),
        coverage: s.workload.as_ref().map(|w| format!("{}, {}", w.source, window_phrase(w))),
        headline: String::new(),
        attention: Vec::new(),
        tiles,
    };
    summarize(&mut d);
    d
}

/// Put a live read into the dashboard and re-rank what needs a look.
pub fn with_now(d: &mut Dashboard, live: &LiveState) {
    let tile = now(live);
    match d.tiles.iter_mut().find(|t| t.key == TileKey::Now) {
        Some(slot) => *slot = tile,
        None => d.tiles.push(tile),
    }
    summarize(d);
}

/// Sessions couldn't be read: the tile says why and the rest of the dashboard stands.
pub fn without_now(d: &mut Dashboard, reason: &str) {
    let mut tile =
        Tile::new(TileKey::Now, TileStatus::Unavailable, format!("Couldn't read sessions: {}", sentence(reason)));
    tile.detail = Some("Everything else here comes from the latest scan.".into());
    match d.tiles.iter_mut().find(|t| t.key == TileKey::Now) {
        Some(slot) => *slot = tile,
        None => d.tiles.push(tile),
    }
    summarize(d);
}

fn summarize(d: &mut Dashboard) {
    // Without query statistics, turning them on comes first: only a problem happening right now is more urgent.
    let no_workload = d.tiles.iter().any(|t| t.key == TileKey::DbTime && t.status == TileStatus::Unavailable);
    let rank = |k: TileKey| match k {
        TileKey::Statistics if no_workload => 1,
        TileKey::Now => 0,
        _ => PRIORITY.iter().position(|p| *p == k).unwrap_or(PRIORITY.len()) + 1,
    };
    let mut attention: Vec<&Tile> =
        d.tiles.iter().filter(|t| matches!(t.status, TileStatus::Bad | TileStatus::Watch)).collect();
    attention.sort_by(|a, b| b.status.cmp(&a.status).then(rank(a.key).cmp(&rank(b.key))));
    d.attention = attention.iter().map(|t| t.key).collect();
    d.headline = match attention.first() {
        Some(t) => t.takeaway.clone(),
        None => {
            let missing = d.tiles.iter().filter(|t| t.status == TileStatus::Unavailable).count();
            match missing {
                0 => "Nothing needs a look right now.".into(),
                1 => "Nothing needs a look in what this scan can see; one tile has no data here.".into(),
                n => format!("Nothing needs a look in what this scan can see; {n} tiles have no data here."),
            }
        }
    };
}

/// "SQL Server", "PostgreSQL", "MySQL" or "MariaDB": the name people know the server by.
pub fn family(server: &ServerInfo) -> &'static str {
    if server.product.to_ascii_lowercase().contains("mariadb") {
        return "MariaDB";
    }
    match server.engine {
        Engine::Mssql => "SQL Server",
        Engine::Postgres => "PostgreSQL",
        Engine::Mysql => "MySQL",
    }
}

// ───────── database time ─────────

/// Rate of database time in a recent stretch against the stretch before it.
struct Trend {
    recent_hours: f64,
    baseline_hours: f64,
    ratio: f64,
}

/// Compare the last day (windows of three days or more) or the last quarter of the timeline with what came before,
/// as database time per hour. Buckets that straddle the cut count in proportion. `None` with too little to compare.
fn trend(w: &Workload) -> Option<Trend> {
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
    let recent_hours = if span >= 72.0 { 24.0 } else { span / 4.0 };
    let cut = end - recent_hours * 3600.0;
    let (mut recent, mut baseline) = (0.0, 0.0);
    for (i, start) in starts.iter().enumerate() {
        let stop = starts.get(i + 1).copied().unwrap_or(end);
        let width = stop - start;
        let time = t.time_ms.get(i).copied().unwrap_or(0.0);
        if width <= 0.0 {
            continue;
        }
        let after = ((stop - start.max(cut)).max(0.0) / width).min(1.0);
        recent += time * after;
        baseline += time * (1.0 - after);
    }
    let baseline_hours = span - recent_hours;
    // Under a second of database time either side, a ratio is noise.
    if recent + baseline < 1_000.0 || baseline <= 0.0 || baseline_hours <= 0.0 {
        return None;
    }
    Some(Trend { recent_hours, baseline_hours, ratio: (recent / recent_hours) / (baseline / baseline_hours) })
}

/// "the last day", "the last 6 hours".
/// The trend's two spans in words that add up to the window: over 10 hours, "the last 3 hours" and "7 hours", not
/// "3" and "8" from rounding each on its own.
fn trend_spans(t: &Trend) -> (String, String) {
    let (r, b) = (t.recent_hours, t.baseline_hours);
    if r >= 1.5 && b >= 1.5 && r + b < 47.0 {
        let recent = r.round();
        let baseline = ((r + b).round() - recent).max(1.0);
        return (last(recent), span(baseline));
    }
    (last(r), span(b))
}

fn last(hours: f64) -> String {
    if (hours - 24.0).abs() < 0.5 { "the last day".into() } else { format!("the last {}", span(hours)) }
}

fn span(hours: f64) -> String {
    if hours >= 47.0 {
        return format!("{} days", (hours / 24.0).round());
    }
    if hours >= 1.5 {
        return format!("{} hours", hours.round());
    }
    let minutes = (hours * 60.0).round().max(1.0);
    if minutes == 1.0 { "1 minute".into() } else { format!("{minutes} minutes") }
}

/// A query that started using another plan inside the window: its share of database time and when.
struct PlanChange<'a> {
    query: &'a QueryStat,
    plan: String,
    at: String,
    share: f64,
}

/// New plans of queries that matter (2% of database time or more), newest plan per query, biggest share first.
/// The same rule as the plan flags on the chart.
fn plan_changes<'a>(s: &'a Snapshot, w: &Workload) -> Vec<PlanChange<'a>> {
    let total = w.total_time_ms.max(1.0);
    let from = w.window_start.as_deref().and_then(format::epoch_seconds);
    let mut out: Vec<PlanChange> = app_queries(s)
        .filter(|q| q.plans.len() >= 2 && q.total_time_ms / total >= 0.02)
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
                share: q.total_time_ms / total,
            })
        })
        .collect();
    out.sort_by(|a, b| b.share.total_cmp(&a.share));
    out
}

fn no_workload(key: TileKey, what: &str) -> Tile {
    let mut t = Tile::new(key, TileStatus::Unavailable, format!("No query history, so {what}."));
    t.detail = Some("Query statistics shows what it takes to turn them on.".into());
    t
}

fn db_time(s: &Snapshot) -> Tile {
    let Some(w) = &s.workload else { return no_workload(TileKey::DbTime, "database time isn't measured") };
    let per_day = w.window_hours >= PER_DAY_MIN_HOURS;
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
    metrics.push(TileMetric::new("window-hours", "Covers", Some(w.window_hours), span(w.window_hours)));

    let changes = plan_changes(s, w);
    let (status, takeaway) = match trend(w) {
        Some(t) if t.ratio >= 1.5 => (
            if t.ratio >= 3.0 { TileStatus::Bad } else { TileStatus::Watch },
            format!(
                "Database time is up {:.1}× in {} compared with the {} before.",
                t.ratio,
                trend_spans(&t).0,
                trend_spans(&t).1
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
        None if !w.windowed && w.timeline.as_ref().is_none_or(|t| t.starts.len() < 2) => Some(format!(
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

fn capitalized(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(c) => c.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

// ───────── findings ─────────

fn severity_status(s: Severity) -> TileStatus {
    match s {
        Severity::High => TileStatus::Bad,
        Severity::Medium => TileStatus::Watch,
        Severity::Low => TileStatus::Info,
    }
}

fn plural(n: usize, one: &str, many: &str) -> String {
    if n == 1 { format!("1 {one}") } else { format!("{n} {many}") }
}

fn findings(report: &Report) -> Tile {
    let active: Vec<&Finding> = report.findings.iter().filter(|f| !f.inactive).collect();
    let count = |sev: Severity| active.iter().filter(|f| f.severity == sev).count();
    let (high, medium, low) = (count(Severity::High), count(Severity::Medium), count(Severity::Low));
    let stopped = report.findings.len() - active.len();
    let span = report.snapshot.workload.as_ref().map(window_phrase);
    let status = if high > 0 {
        TileStatus::Bad
    } else if medium > 0 {
        TileStatus::Watch
    } else if low > 0 {
        TileStatus::Info
    } else {
        TileStatus::Good
    };
    let top: Vec<&Finding> = active.iter().take(3).copied().collect();
    // The same number the findings summary states: the top three's distinct queries, each counted once.
    let touched = top_findings_share(&report.snapshot, active.iter().copied()).unwrap_or(0.0);
    let takeaway = if active.is_empty() {
        match &span {
            Some(span) => format!("Nothing stands out over {span}."),
            None => "Nothing stands out.".into(),
        }
    } else if touched >= 0.05 {
        format!(
            "{}; the top {} touch {} of database time.",
            capitalized(&plural(active.len(), "opportunity", "opportunities")),
            if top.len() == 1 { "one".to_string() } else { top.len().to_string() },
            format::percent(touched)
        )
    } else if high > 0 {
        format!("{}, {high} of them high.", capitalized(&plural(active.len(), "opportunity", "opportunities")))
    } else {
        format!("{}, none of them high.", capitalized(&plural(active.len(), "opportunity", "opportunities")))
    };
    let mut tile = Tile::new(TileKey::Findings, status, takeaway);
    tile.metrics = vec![
        TileMetric::new("high", "High", Some(high as f64), high.to_string()),
        TileMetric::new("medium", "Medium", Some(medium as f64), medium.to_string()),
        TileMetric::new("low", "Low", Some(low as f64), low.to_string()),
    ];
    if stopped > 0 {
        tile.metrics.push(TileMetric::new("stopped", "Stopped running", Some(stopped as f64), stopped.to_string()));
        tile.detail = Some(format!(
            "{} about queries that stopped running, most likely changed by a deploy.",
            capitalized(&plural(stopped, "more is", "more are"))
        ));
    }
    tile.items = top
        .iter()
        .map(|f| {
            let mut item = TileItem::new(ItemKind::Finding, f.id.clone(), f.title.clone());
            item.value = Some(f.impact.label.clone());
            item.share = f.impact.db_time_share;
            item.status = Some(severity_status(f.severity));
            item.finding_id = Some(f.id.clone());
            item
        })
        .collect();
    tile
}

// ───────── top queries ─────────

fn top_queries(s: &Snapshot) -> Tile {
    let Some(w) = &s.workload else { return no_workload(TileKey::TopQueries, "there is no query list") };
    let total = w.total_time_ms;
    let mut queries: Vec<&QueryStat> = app_queries(s).filter(|q| q.total_time_ms > 0.0).collect();
    queries.sort_by(|a, b| b.total_time_ms.total_cmp(&a.total_time_ms));
    if queries.is_empty() || total <= 0.0 {
        let mut t = Tile::new(
            TileKey::TopQueries,
            TileStatus::Info,
            format!("No application queries ran over {}.", window_phrase(w)),
        );
        t.detail = Some(format!("{} recorded {} statements in all.", w.source, format::count(w.query_count)));
        return t;
    }
    let share = |q: &QueryStat| (q.total_time_ms / total).min(1.0);
    let top = &queries[..queries.len().min(5)];
    let first = share(top[0]);
    let three: f64 = top.iter().take(3).map(|q| share(q)).sum();
    let five: f64 = top.iter().map(|q| share(q)).sum();
    let takeaway = if first >= 0.3 {
        format!("One query is {} of database time: {}.", format::percent(first), query_handle(top[0]))
    } else if three >= 0.6 && top.len() >= 3 {
        format!("Three queries make up {} of database time.", format::percent(three))
    } else if top.len() >= 5 {
        format!("Time is spread out: the top five queries are {} of database time.", format::percent(five))
    } else {
        format!(
            "{} {} of database time.",
            capitalized(&plural(top.len(), "query is", "queries are")),
            format::percent(five)
        )
    };
    let mut tile = Tile::new(TileKey::TopQueries, TileStatus::Info, takeaway);
    tile.detail = Some(format!(
        "{} tracked over {}.",
        capitalized(&plural(w.query_count as usize, "query", "queries")),
        window_phrase(w)
    ));
    tile.metrics = vec![
        TileMetric::new("top-share", "Top query", Some(first), format::percent(first)),
        TileMetric::new("top5-share", "Top five", Some(five.min(1.0)), format::percent(five.min(1.0))),
        TileMetric::new("queries", "Queries", Some(w.query_count), format::count(w.query_count)),
    ];
    tile.items = top
        .iter()
        .map(|q| {
            let mut item = TileItem::new(ItemKind::Query, q.id.clone(), query_handle(q));
            item.value = Some(format::percent(share(q)));
            item.share = Some(share(q));
            item.query_id = Some(q.id.clone());
            item
        })
        .collect();
    tile
}

// ───────── waits ─────────

fn waits(s: &Snapshot) -> Tile {
    let report = crate::waits::explain(s).filter(|r| !r.categories.is_empty());
    let Some(r) = report else { return waits_missing(s) };
    let top = &r.categories[0];
    let named = r.categories.iter().find(|c| c.key != "other");
    // Without performance_schema, MySQL and MariaDB time no waits: the slow query log keeps only each statement's lock
    // time, so "100% locks" would be 100% of the little that was measured. Say what is known, against database time.
    if r.scope == WaitScope::Statements && s.server.engine == Engine::Mysql {
        let database_ms = s.workload.as_ref().map(|w| w.total_time_ms).unwrap_or(0.0);
        let share = if database_ms > 0.0 { r.total_ms / database_ms } else { 0.0 };
        let takeaway = format!(
            "{} times no waits without performance_schema; logged statements waited {} on locks ({} of their time).",
            family(&s.server),
            format::duration(r.total_ms),
            format::percent(share)
        );
        let mut tile =
            Tile::new(TileKey::Waits, if share >= 0.25 { TileStatus::Watch } else { TileStatus::Info }, takeaway);
        tile.detail = top.detail.clone();
        tile.metrics = vec![TileMetric::new("total", "Lock time", Some(r.total_ms), format::duration(r.total_ms))];
        tile.items = r
            .categories
            .iter()
            .take(6)
            .map(|c| {
                let mut item = TileItem::new(ItemKind::Wait, c.key.clone(), c.label.clone());
                item.value = Some(format::duration(c.wait_ms));
                item.query_id = c.queries.first().map(|q| q.id.clone());
                item
            })
            .collect();
        return tile;
    }
    let takeaway = if top.key == "other" {
        // "Everything else" leading says nothing; say what is known and how much isn't.
        match named {
            Some(c) => format!(
                "{} take {}; the other {} isn't broken down by {}.",
                c.label,
                format::percent(c.share),
                format::percent(top.share),
                family(&s.server)
            ),
            None => format!("{} doesn't break this waiting down by kind.", family(&s.server)),
        }
    } else {
        match r.categories.get(1).filter(|c| c.share >= 0.05) {
            Some(next) => format!(
                "Most time goes to {} ({}), then {} ({}).",
                lower_label(&top.label),
                format::percent(top.share),
                lower_label(&next.label),
                format::percent(next.share)
            ),
            None => format!("Most time goes to {} ({}).", lower_label(&top.label), format::percent(top.share)),
        }
    };
    let contended: f64 =
        r.categories.iter().filter(|c| matches!(c.key.as_str(), "lock" | "latch" | "memory")).map(|c| c.share).sum();
    let status = if contended >= 0.25 { TileStatus::Watch } else { TileStatus::Info };
    let mut tile = Tile::new(TileKey::Waits, status, takeaway);
    let lead = if top.key == "other" { named.unwrap_or(top) } else { top };
    // Totals since startup cover a different stretch than database time: say which, rather than set them side by side.
    let since_start = r.scope == WaitScope::SinceStartup;
    tile.detail = if since_start {
        Some(format!("{}.", r.source.trim_end_matches('.')))
    } else {
        lead.detail.clone().or_else(|| first_sentence(&lead.meaning))
    };
    let label = if since_start { "Waiting since start" } else { "Waiting" };
    tile.metrics = vec![TileMetric::new("total", label, Some(r.total_ms), format::duration(r.total_ms))];
    tile.items = r
        .categories
        .iter()
        .take(6)
        .map(|c| {
            let mut item = TileItem::new(ItemKind::Wait, c.key.clone(), c.label.clone());
            item.value = Some(format::percent(c.share));
            item.share = Some(c.share);
            item.query_id = c.queries.first().map(|q| q.id.clone());
            item
        })
        .collect();
    tile
}

/// Labels like "CPU" keep their case; "Disk reads" reads as "disk reads" mid-sentence.
fn lower_label(label: &str) -> String {
    let mut words = label.split(' ');
    let first = words.next().unwrap_or_default();
    let first = if first.len() > 1 && first.chars().all(|c| c.is_ascii_uppercase()) {
        first.to_string()
    } else {
        first.to_lowercase()
    };
    std::iter::once(first).chain(words.map(str::to_string)).collect::<Vec<_>>().join(" ")
}

fn first_sentence(text: &str) -> Option<String> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    Some(match text.find(". ") {
        Some(end) => text[..=end].to_string(),
        None => text.to_string(),
    })
}

fn missing_reason(s: &Snapshot, capability: Capability) -> Option<&culprit_core::MissingCapability> {
    s.server.missing.iter().find(|m| m.capability == capability)
}

fn setting<'a>(s: &'a Snapshot, name: &str) -> Option<&'a str> {
    s.settings.iter().find(|x| x.name.eq_ignore_ascii_case(name)).map(|x| x.value.as_str())
}

fn is_off(value: Option<&str>) -> bool {
    value.is_some_and(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "off" | "0" | "false" | "no"))
}

fn waits_missing(s: &Snapshot) -> Tile {
    let family = family(&s.server);
    let mut tile = match s.server.engine {
        Engine::Postgres => {
            let mut t = Tile::new(
                TileKey::Waits,
                TileStatus::Unavailable,
                "PostgreSQL keeps no history of what sessions wait on.",
            );
            if is_off(setting(s, "track_io_timing")) {
                t.detail = Some(
                    "Turn on track_io_timing to measure disk time per statement, or sample what it waits on right now."
                        .into(),
                );
                t.fix_sql = Some("ALTER SYSTEM SET track_io_timing = on; SELECT pg_reload_conf();".into());
            } else {
                t.detail = Some("Sample what it waits on right now to see where time goes.".into());
            }
            t
        }
        Engine::Mysql => match missing_reason(s, Capability::PerformanceSchema)
            .or_else(|| missing_reason(s, Capability::WaitStats))
        {
            Some(m) => {
                let mut t = Tile::new(
                    TileKey::Waits,
                    TileStatus::Unavailable,
                    format!("{family} isn't counting waits: {}", sentence(&m.reason)),
                );
                t.detail = m.fix.clone();
                t
            }
            None => {
                Tile::new(TileKey::Waits, TileStatus::Unavailable, format!("{family} recorded no waits in this scan."))
            }
        },
        Engine::Mssql => match missing_reason(s, Capability::WaitStats) {
            Some(m) => {
                let mut t = Tile::new(
                    TileKey::Waits,
                    TileStatus::Unavailable,
                    format!("Waits aren't readable here: {}", sentence(&m.reason)),
                );
                t.detail = m.fix.clone();
                t
            }
            None => Tile::new(TileKey::Waits, TileStatus::Unavailable, "This scan recorded no waits."),
        },
    };
    if tile.detail.is_none() {
        tile.detail = Some("Sample what it waits on right now instead.".into());
    }
    tile
}

/// A reason as the end of a sentence: first letter lowered, one full stop.
fn sentence(text: &str) -> String {
    let text = text.trim().trim_end_matches('.');
    let mut chars = text.chars();
    let lowered = match (chars.next(), chars.clone().next()) {
        // Keep acronyms and names ("Query Store is OFF", "SQL Server …") as they are.
        (Some(c), Some(n)) if c.is_uppercase() && n.is_lowercase() && !text.starts_with("Query ") => {
            c.to_lowercase().chain(chars).collect()
        }
        _ => text.to_string(),
    };
    format!("{lowered}.")
}

// ───────── health ─────────

/// A signal's value as a person reads it, with its unit.
pub fn signal_value(s: &Signal) -> String {
    match s.unit {
        SignalUnit::Percent => format!("{:.0}%", s.value),
        SignalUnit::PerSecond => format!("{}/s", format::count(s.value)),
        SignalUnit::PerHour => format!("{}/h", format::count(s.value)),
        SignalUnit::Count => format::count(s.value),
        SignalUnit::Bytes => format::bytes(s.value),
        SignalUnit::Milliseconds => format::duration(s.value),
        SignalUnit::Seconds => format::duration(s.value * 1000.0),
    }
}

fn signal_status(s: SignalStatus) -> TileStatus {
    match s {
        SignalStatus::Bad => TileStatus::Bad,
        SignalStatus::Watch => TileStatus::Watch,
        SignalStatus::Good => TileStatus::Good,
        SignalStatus::Info => TileStatus::Info,
    }
}

fn health(s: &Snapshot) -> Tile {
    let Some(p) = &s.pressure else {
        let mut t = Tile::new(TileKey::Health, TileStatus::Unavailable, "This scan recorded no health counters.");
        t.detail = Some(match s.notes.iter().find(|n| n.collector.contains("pressure")) {
            Some(note) => note.message.clone(),
            None => "Scan again to read compiles, memory, cache and connection counters.".into(),
        });
        return t;
    };
    let mut signals: Vec<&Signal> = p.signals.iter().collect();
    signals.sort_by_key(|s| std::cmp::Reverse(s.status));
    let graded = signals.iter().filter(|s| s.status != SignalStatus::Info).count();
    let bad = signals.iter().filter(|s| s.status == SignalStatus::Bad).count();
    let watch = signals.iter().filter(|s| s.status == SignalStatus::Watch).count();
    let mut status = if bad > 0 {
        TileStatus::Bad
    } else if watch > 0 {
        TileStatus::Watch
    } else if graded > 0 {
        TileStatus::Good
    } else {
        TileStatus::Info
    };
    let mut takeaway = match signals.first().filter(|s| matches!(s.status, SignalStatus::Bad | SignalStatus::Watch)) {
        Some(worst) => match &worst.healthy {
            Some(healthy) => format!("{}: {}; {healthy}.", worst.label, signal_value(worst)),
            None => format!("{}: {}.", worst.label, signal_value(worst)),
        },
        None if graded == 1 => "The one graded signal looks healthy.".to_string(),
        None if graded > 0 => format!("All {graded} graded signals look healthy."),
        None => format!(
            "{} recorded; none has a threshold to grade.",
            capitalized(&plural(signals.len(), "signal", "signals"))
        ),
    };
    let mut items: Vec<TileItem> = Vec::new();
    // An old open transaction holds back cleanup on every engine; it earns a row of its own.
    if let Some(tx) = p.oldest_transaction.as_ref().filter(|t| t.seconds >= 300.0) {
        let idle = tx.state.as_deref().is_some_and(|s| s.contains("idle"));
        let tx_status =
            if tx.seconds >= 3600.0 || (idle && tx.seconds >= 600.0) { TileStatus::Watch } else { TileStatus::Info };
        let mut item = TileItem::new(ItemKind::Signal, "oldest-transaction", "Oldest open transaction");
        item.value = Some(format!(
            "{}{}",
            format::duration(tx.seconds * 1000.0),
            tx.state.as_deref().map(|s| format!(" · {s}")).unwrap_or_default()
        ));
        item.status = Some(tx_status);
        if tx_status == TileStatus::Watch && status < TileStatus::Watch {
            status = TileStatus::Watch;
            takeaway = format!(
                "A transaction has been open for {}{}, holding back cleanup.",
                format::duration(tx.seconds * 1000.0),
                if idle { " doing nothing" } else { "" }
            );
        }
        items.push(item);
    }
    items.extend(signals.iter().take(8).map(|s| {
        let mut item = TileItem::new(ItemKind::Signal, s.key.clone(), s.label.clone());
        item.value = Some(signal_value(s));
        item.status = Some(signal_status(s.status));
        item
    }));
    let mut tile = Tile::new(TileKey::Health, status, takeaway);
    let more = (bad + watch).saturating_sub(1);
    tile.detail = if more > 0 {
        Some(format!(
            "{} also {} a look.",
            capitalized(&plural(more, "more signal", "more signals")),
            if more == 1 { "needs" } else { "need" }
        ))
    } else {
        p.uptime_hours.map(|h| format!("Counters since startup cover {}.", span(h)))
    };
    tile.metrics = vec![
        TileMetric::new("bad", "Bad", Some(bad as f64), bad.to_string()),
        TileMetric::new("watch", "Watch", Some(watch as f64), watch.to_string()),
        TileMetric::new("healthy", "Healthy", Some((graded - bad - watch) as f64), (graded - bad - watch).to_string()),
    ];
    tile.items = items;
    tile
}

// ───────── right now ─────────

fn not_read() -> Tile {
    let mut t = Tile::new(TileKey::Now, TileStatus::Info, "Sessions weren't read for this dashboard.");
    t.detail = Some("The app reads them every few seconds; `culprit live` reads them once.".into());
    t
}

fn idle_in_transaction(s: &culprit_core::Session) -> bool {
    s.transaction_ms.is_some() && (s.state.starts_with("idle in transaction") || s.state == "sleeping")
}

fn running(s: &culprit_core::Session) -> bool {
    s.state != "idle" && s.state != "sleeping" && !s.state.starts_with("idle in transaction")
}

/// Sessions right now: blocking first, then an idle open transaction, then what runs. The app words its polled tile the
/// same way (`app/src/lib/dashboard.ts`).
pub fn now(live: &LiveState) -> Tile {
    let active: Vec<&culprit_core::Session> = live.sessions.iter().filter(|s| running(s)).collect();
    let blocked = live.sessions.iter().filter(|s| s.blocked_by.is_some()).count();
    let idle_tx: Vec<&culprit_core::Session> = live.sessions.iter().filter(|s| idle_in_transaction(s)).collect();
    let longest = active
        .iter()
        .filter(|s| s.blocked_by.is_none())
        .max_by(|a, b| a.elapsed_ms.unwrap_or(0.0).total_cmp(&b.elapsed_ms.unwrap_or(0.0)));
    let oldest_idle =
        idle_tx.iter().max_by(|a, b| a.transaction_ms.unwrap_or(0.0).total_cmp(&b.transaction_ms.unwrap_or(0.0)));
    let chains = live.blocking.as_ref().map(|b| b.chains.as_slice()).unwrap_or_default();

    let (status, takeaway) = if let Some(chain) = chains.first() {
        let severe = chain.waiters >= 5 || chain.longest_wait_ms.is_some_and(|ms| ms >= 30_000.0);
        (if severe { TileStatus::Bad } else { TileStatus::Watch }, chain.headline.clone())
    } else if blocked > 0 {
        (
            TileStatus::Watch,
            format!(
                "{} waiting on another session's lock.",
                capitalized(&plural(blocked, "session is", "sessions are"))
            ),
        )
    } else if let Some(s) = oldest_idle.filter(|s| s.transaction_ms.unwrap_or(0.0) >= 60_000.0) {
        (
            TileStatus::Watch,
            format!(
                "Session {} has been idle in an open transaction for {}.",
                s.id,
                format::duration(s.transaction_ms.unwrap_or(0.0))
            ),
        )
    } else if active.is_empty() {
        (TileStatus::Good, "Nothing is running right now.".to_string())
    } else {
        let tail = match longest.and_then(|s| s.elapsed_ms) {
            Some(ms) if ms >= 1_000.0 => format!("; the longest has run {}", format::duration(ms)),
            _ => String::new(),
        };
        (
            TileStatus::Good,
            format!("{} running, none blocked{tail}.", capitalized(&plural(active.len(), "session", "sessions"))),
        )
    };
    let mut tile = Tile::new(TileKey::Now, status, takeaway);
    tile.metrics = vec![
        TileMetric::new("active", "Running", Some(active.len() as f64), active.len().to_string()),
        TileMetric::new("blocked", "Blocked", Some(blocked as f64), blocked.to_string()),
        TileMetric::new(
            "idle-in-transaction",
            "Idle in transaction",
            Some(idle_tx.len() as f64),
            idle_tx.len().to_string(),
        ),
        TileMetric::new("sessions", "Sessions", Some(live.sessions.len() as f64), live.sessions.len().to_string()),
    ];
    let mut items: Vec<TileItem> = chains
        .iter()
        .take(3)
        .map(|c| {
            let mut item = TileItem::new(ItemKind::Session, c.head.clone(), c.headline.clone());
            item.value = Some(format!("{} waiting", c.waiters));
            item.status = Some(TileStatus::Bad);
            item
        })
        .collect();
    let mut longest_first = active.clone();
    longest_first.sort_by(|a, b| b.elapsed_ms.unwrap_or(0.0).total_cmp(&a.elapsed_ms.unwrap_or(0.0)));
    items.extend(longest_first.iter().filter(|s| !chains.iter().any(|c| c.head == s.id)).take(3).map(|s| {
        let label = s
            .sql
            .as_deref()
            .map(statement_handle)
            .unwrap_or_else(|| s.command.clone().unwrap_or_else(|| s.state.clone()));
        let mut item = TileItem::new(ItemKind::Session, s.id.clone(), format!("Session {} · {label}", s.id));
        item.value = s.elapsed_ms.map(format::duration);
        item.status = Some(if s.blocked_by.is_some() { TileStatus::Watch } else { TileStatus::Good });
        item
    }));
    tile.items = items;
    tile.detail = Some(format!("Read {}.", culprit_core::changes::short_time(&live.taken_at)));
    tile
}

// ───────── deadlocks ─────────

fn deadlocks(report: &Report) -> Tile {
    let s = &report.snapshot;
    let family = family(&s.server);
    let Some(d) = &s.deadlocks else {
        let mut t = Tile::new(TileKey::Deadlocks, TileStatus::Unavailable, "This scan didn't read deadlocks.");
        t.detail = Some(match s.notes.iter().find(|n| n.collector == "deadlocks") {
            Some(note) => note.message.clone(),
            None => "Scan again to read the last day of deadlocks.".into(),
        });
        return t;
    };
    let finding_id = report.findings.iter().find(|f| f.check == "deadlocks").map(|f| f.id.clone());
    if !d.deadlocks.is_empty() {
        // Same pattern on the same tables is the same problem.
        let mut groups: BTreeMap<(String, String), Vec<&culprit_core::Deadlock>> = BTreeMap::new();
        for dl in &d.deadlocks {
            groups.entry((dl.pattern.label().to_string(), dl.objects.join(", "))).or_default().push(dl);
        }
        let mut groups: Vec<((String, String), Vec<&culprit_core::Deadlock>)> = groups.into_iter().collect();
        groups.sort_by_key(|g| std::cmp::Reverse(g.1.len()));
        let n = d.deadlocks.len();
        let ((pattern, objects), biggest) = &groups[0];
        let on = if objects.is_empty() { String::new() } else { format!(" on {objects}") };
        let when = last(d.since_hours);
        let takeaway = if n == 1 {
            format!("One deadlock in {when}: {pattern}{on}.")
        } else if biggest.len() == n {
            format!("{n} deadlocks in {when}, all {pattern}{on}.")
        } else if groups.len() == n {
            format!("{n} deadlocks in {when}, each a different kind.")
        } else {
            format!("{n} deadlocks in {when}; {} of them the same {pattern}{on}.", biggest.len())
        };
        let mut tile =
            Tile::new(TileKey::Deadlocks, if n >= 5 { TileStatus::Bad } else { TileStatus::Watch }, takeaway);
        tile.detail = d.note.clone().or_else(|| Some(format!("Read from {}.", d.source)));
        tile.metrics = vec![TileMetric::new("count", "Deadlocks", Some(n as f64), n.to_string())];
        tile.items = groups
            .iter()
            .take(3)
            .map(|((pattern, objects), list)| {
                let label = if objects.is_empty() {
                    capitalized(pattern)
                } else {
                    format!("{} on {objects}", capitalized(pattern))
                };
                let mut item = TileItem::new(ItemKind::Deadlock, list[0].id.clone(), label);
                item.value = Some(format!("{}×", list.len()));
                item.at = list.iter().filter_map(|dl| dl.at.clone()).max();
                item.status = Some(TileStatus::Watch);
                item.finding_id = finding_id.clone();
                item
            })
            .collect();
        return tile;
    }
    if let Some(counted) = d.counted {
        let since = d.counted_since.as_deref().and_then(|from| format::age_between(from, &s.taken_at));
        let over = since.map(|age| format!(" in the {age} since its statistics were reset")).unwrap_or_default();
        let mut tile = if counted > 0.0 {
            Tile::new(
                TileKey::Deadlocks,
                TileStatus::Watch,
                format!(
                    "{family} counted {} deadlock{}{over}; it keeps no details to explain them.",
                    format::count(counted),
                    if counted == 1.0 { "" } else { "s" }
                ),
            )
        } else {
            Tile::new(TileKey::Deadlocks, TileStatus::Good, format!("No deadlocks counted{over}."))
        };
        tile.detail = d.note.clone();
        tile.metrics = vec![TileMetric::new("counted", "Counted", Some(counted), format::count(counted))];
        return tile;
    }
    let mut tile = Tile::new(TileKey::Deadlocks, TileStatus::Good, format!("No deadlocks in {}.", last(d.since_hours)));
    tile.detail = d.note.clone();
    tile.metrics = vec![TileMetric::new("count", "Deadlocks", Some(0.0), "0")];
    tile
}

// ───────── indexes ─────────

fn indexes(report: &Report) -> Tile {
    let advice = crate::advisor::index_advice(report);
    // A query that asks for two indexes still only takes its share of time once.
    let mut seen = HashSet::new();
    let mut share = 0.0;
    for q in advice.candidates.iter().flat_map(|c| &c.queries) {
        if seen.insert(q.id.as_str()) {
            share += q.share.unwrap_or(0.0);
        }
    }
    let share = share.min(1.0);
    let adds = advice.candidates.len();
    let cleanups = advice.cleanups.len();
    let reclaim: f64 = advice.cleanups.iter().filter_map(|c| c.size_bytes).sum();
    let free = if reclaim > 0.0 { format!(" ({})", format::bytes(reclaim)) } else { String::new() };
    let new = |n: usize| if n == 1 { "One new index".to_string() } else { format!("{n} new indexes") };
    let help = if share >= 0.01 {
        format!("would help queries that take {} of database time", format::percent(share))
    } else if !seen.is_empty() {
        format!("would help {}", plural(seen.len(), "query", "queries"))
    } else if adds == 1 {
        "is worth a look".to_string()
    } else {
        "are worth a look".to_string()
    };
    let takeaway = match (adds, cleanups) {
        (0, 0) => "No index changes stand out.".to_string(),
        (0, c) => format!("No new indexes needed; {} can go{free}.", plural(c, "existing index", "existing indexes")),
        (a, 0) => format!("{} {help}.", new(a)),
        (a, c) => format!("{} {help}; {} can go{free}.", new(a), plural(c, "index", "indexes")),
    };
    let status = if adds > 0 && share >= 0.1 {
        TileStatus::Watch
    } else if adds + cleanups > 0 {
        TileStatus::Info
    } else {
        TileStatus::Good
    };
    let mut tile = Tile::new(TileKey::Indexes, status, takeaway);
    tile.detail = advice
        .notes
        .first()
        .cloned()
        .or_else(|| (!advice.counters.is_empty()).then(|| format!("Index usage counted {}.", advice.counters)));
    tile.metrics = vec![
        TileMetric::new("add", "To add", Some(adds as f64), adds.to_string()),
        TileMetric::new("cleanup", "To clean up", Some(cleanups as f64), cleanups.to_string()),
        TileMetric::new("reclaim", "Space back", Some(reclaim), format::bytes(reclaim)),
        TileMetric::new(
            "tables",
            "Tables to look at",
            Some(advice.tables.len() as f64),
            advice.tables.len().to_string(),
        ),
    ];
    let mut items: Vec<TileItem> = advice
        .candidates
        .iter()
        .take(3)
        .map(|c| {
            let mut item = TileItem::new(
                ItemKind::IndexAdd,
                format!("{}.{}({})", c.schema, c.table, c.keys.join(", ")),
                format!("{} ({})", c.table, c.keys.join(", ")),
            );
            item.value = c.db_time_share.map(format::percent);
            item.share = c.db_time_share;
            item.query_id = c.queries.first().map(|q| q.id.clone());
            item.finding_id = c.finding_id.clone();
            item
        })
        .collect();
    items.extend(advice.cleanups.iter().take(2).map(|c| {
        let mut item = TileItem::new(
            ItemKind::IndexCleanup,
            format!("{}.{}.{}", c.schema, c.table, c.index),
            format!("{} on {}", c.index, c.table),
        );
        item.value = c.size_bytes.map(format::bytes);
        item.finding_id = c.finding_id.clone();
        item
    }));
    tile.items = items;
    tile
}

// ───────── plans ─────────

fn plans(s: &Snapshot) -> Tile {
    let Some(w) = &s.workload else { return no_workload(TileKey::Plans, "there are no plans to read") };
    let total = w.total_time_ms.max(1.0);
    let with_plans: Vec<&QueryStat> = app_queries(s).filter(|q| q.plan.is_some()).collect();
    if with_plans.is_empty() {
        let reason = [Capability::QueryPlans, Capability::Showplan, Capability::GenericPlans]
            .into_iter()
            .find_map(|c| missing_reason(s, c))
            .map(|m| sentence(&m.reason));
        let mut t = Tile::new(
            TileKey::Plans,
            TileStatus::Unavailable,
            match reason {
                Some(reason) => format!("No plans were captured: {reason}"),
                None => "No plans were captured in this scan.".to_string(),
            },
        );
        t.detail = Some("Open a query to plan it on demand, or paste a plan into Plans.".into());
        return t;
    }
    struct Group<'a> {
        plans: usize,
        share: f64,
        severity: Severity,
        top: &'a QueryStat,
    }
    let mut groups: HashMap<String, Group> = HashMap::new();
    let mut flagged = 0;
    let mut note = None;
    for q in &with_plans {
        let Some(plan) = &q.plan else { continue };
        let found = match &plan.insights {
            Some(i) => i.items.clone(),
            None => insights(plan),
        };
        if note.is_none() {
            note = plan.insights.as_ref().and_then(|i| i.note.clone()).or_else(|| coverage_note(plan));
        }
        if found.is_empty() {
            continue;
        }
        flagged += 1;
        let share = q.total_time_ms / total;
        let mut rules = HashSet::new();
        for i in &found {
            if !rules.insert(i.rule.clone()) {
                continue;
            }
            let g =
                groups.entry(i.rule.clone()).or_insert(Group { plans: 0, share: 0.0, severity: i.severity, top: q });
            g.plans += 1;
            g.share += share;
            g.severity = g.severity.max(i.severity);
            if q.total_time_ms > g.top.total_time_ms {
                g.top = q;
            }
        }
    }
    let captured = with_plans.len();
    let mut ranked: Vec<(String, Group)> = groups.into_iter().collect();
    ranked.sort_by(|a, b| b.1.severity.cmp(&a.1.severity).then(b.1.share.total_cmp(&a.1.share)).then(a.0.cmp(&b.0)));
    let (status, takeaway) = match ranked.first() {
        None => (
            TileStatus::Good,
            format!("None of the {} shows a known problem.", plural(captured, "captured plan", "captured plans")),
        ),
        Some((rule, g)) => {
            let status = if g.severity == Severity::High || (g.severity == Severity::Medium && g.share >= 0.1) {
                TileStatus::Watch
            } else {
                TileStatus::Info
            };
            let what = short_rule(rule);
            let share = format::percent(g.share.min(1.0));
            let takeaway = if flagged == 1 {
                format!(
                    "One of the {} has something to fix: {what} ({share} of database time).",
                    plural(captured, "captured plan", "captured plans")
                )
            } else if g.plans == 1 {
                format!(
                    "{flagged} of {captured} captured plans have something to fix; the costliest is {what} ({share} of database time)."
                )
            } else {
                format!(
                    "{flagged} of {captured} captured plans have something to fix; {what} shows up in {} ({share} of database time).",
                    plural(g.plans, "plan", "plans")
                )
            };
            (status, takeaway)
        }
    };
    let mut tile = Tile::new(TileKey::Plans, status, takeaway);
    tile.detail = note;
    tile.metrics = vec![
        TileMetric::new("captured", "Captured", Some(captured as f64), captured.to_string()),
        TileMetric::new("flagged", "With problems", Some(flagged as f64), flagged.to_string()),
    ];
    tile.items = ranked
        .iter()
        .take(4)
        .map(|(rule, g)| {
            let mut item = TileItem::new(ItemKind::Rule, rule.clone(), capitalized(&short_rule(rule)));
            item.value = Some(plural(g.plans, "plan", "plans"));
            item.share = Some(g.share.min(1.0));
            item.status = Some(severity_status(g.severity));
            item.query_id = Some(g.top.id.clone());
            item
        })
        .collect();
    tile
}

/// A rule as a few words for a sentence: "a large sort", "key lookups".
fn short_rule(rule: &str) -> String {
    match rule {
        "key-lookups" => "key lookups",
        "scan-filter" => "a scan reading far more rows than it keeps",
        "missing-index-hint" => "a missing index",
        "implicit-conversion" => "an implicit conversion",
        "non-sargable" => "a function around a filtered column",
        "leading-wildcard" => "a leading wildcard",
        "not-in" => "a NOT IN",
        "scalar-udf" => "a scalar function called per row",
        "table-variable" => "a table variable",
        "parallelism-blocked" => "a plan that can't go parallel",
        "parallel-skew" => "uneven parallel threads",
        "eager-spool" => "an eager spool",
        "spill" => "a spill to disk",
        "memory-grant" => "an oversized memory grant",
        "misestimate" => "a row misestimate",
        "nested-loops" => "a nested loop repeating thousands of times",
        "big-sort" => "a large sort",
        "no-join-predicate" => "a join without a condition",
        "missing-statistics" => "missing statistics",
        "optimizer-timeout" => "an optimizer timeout",
        "legacy-ce" => "the legacy cardinality estimator",
        "slow-compile" => "slow planning",
        "many-to-many" => "a many-to-many merge join",
        "remote-query" => "a remote query",
        "heap-fetches" => "heap fetches",
        "lossy-bitmap" => "a lossy bitmap scan",
        "workers-short" => "missing parallel workers",
        "jit" => "JIT compilation",
        "correlated-subquery" => "a correlated subquery",
        "join-buffer" => "a join without an index",
        "temp-table-sort" => "a temporary table and filesort",
        "index-ignored" => "an ignored index",
        other => return other.replace('-', " "),
    }
    .to_string()
}

// ───────── query statistics ─────────

/// The first SQL statement in a fix sentence ("Run CREATE EXTENSION pg_stat_statements; in this database…"): an
/// upper-case verb up to its semicolon. "Set performance_schema = ON in the configuration" is advice, not SQL.
fn fix_statement(fix: &str) -> Option<String> {
    let start = ["CREATE ", "ALTER ", "GRANT ", "SET ", "UPDATE ", "CALL ", "EXEC ", "INSTALL "]
        .iter()
        .filter_map(|verb| {
            let mut from = 0;
            while let Some(at) = fix[from..].find(verb) {
                let i = from + at;
                if i == 0 || !fix.as_bytes()[i - 1].is_ascii_alphanumeric() {
                    return Some(i);
                }
                from = i + verb.len();
            }
            None
        })
        .min()?;
    let end = start + fix[start..].find(';')? + 1;
    Some(fix[start..end].trim().to_string())
}

// ───────── storage, connections, replication, failing queries ─────────

/// The worst finding of one check, for a tile that leads with it.
/// Findings arrive ranked by impact; among equally severe ones the first is the most important. (`max_by_key` keeps
/// the last of equal maximums, hence the reversal.)
fn worst_finding<'a>(report: &'a Report, check: &str) -> Option<&'a Finding> {
    report.findings.iter().filter(|f| f.check == check && !f.inactive).rev().max_by_key(|f| f.severity)
}

/// A collector's note, when it explains why a tile has nothing to show.
fn collector_note(s: &Snapshot, names: &[&str]) -> Option<String> {
    s.notes.iter().find(|n| names.contains(&n.collector.as_str())).map(|n| n.message.clone())
}

fn file_name(s: &Snapshot, f: &FileIo) -> String {
    match (s.server.engine, f.kind, f.database.as_deref()) {
        (Engine::Mssql, FileKind::Log, Some(db)) => format!("{db} log"),
        (Engine::Mssql, _, Some(db)) if db.eq_ignore_ascii_case("tempdb") => "tempdb".into(),
        (Engine::Postgres, FileKind::Log, _) => "WAL".into(),
        (Engine::Postgres, FileKind::Temp, _) => "temporary files".into(),
        (Engine::Postgres, _, _) => "tables and indexes".into(),
        _ => f.name.clone(),
    }
}

/// A latency people can read: cache-fast storage in microseconds ("3 µs", as the Health panel says), not "0.00 ms".
/// Storage latency in one unit family: microseconds below a millisecond ("40 µs", "630 µs"), so SQL Server's 0.04 ms
/// and MySQL's 8 µs read alike.
fn latency(ms: f64) -> String {
    if ms < 1.0 { format!("{} µs", (ms * 1000.0).round().max(1.0)) } else { format::duration(ms) }
}

/// The latency a file is waited on for (reads of data, writes of the log) and how many operations it averages.
fn waited_latency(f: &FileIo) -> Option<(f64, f64, &'static str)> {
    if f.kind == FileKind::Log {
        f.avg_write_ms().map(|avg| (avg, f.writes, "write"))
    } else {
        f.avg_read_ms().map(|avg| (avg, f.reads, "read"))
    }
}

/// Average read latency of data and temporary files, and write latency of the log: what queries and commits wait on.
fn storage(report: &Report) -> Tile {
    let s = &report.snapshot;
    let family = family(&s.server);
    let merged = s.pressure.as_ref().map(|p| merge_logs(&p.files)).unwrap_or_default();
    let files: Vec<&FileIo> =
        merged.iter().filter(|f| f.kind != FileKind::Other && waited_latency(f).is_some()).collect();
    if files.is_empty() {
        // An engine fact only when the scan proves it; otherwise just what this scan lacks (an older scan, a login
        // that can't read the views).
        let why = match s.server.engine {
            Engine::Postgres if is_off(setting(s, "track_io_timing")) => {
                "PostgreSQL times reads and writes only with track_io_timing on.".to_string()
            }
            Engine::Mysql if !s.server.has(Capability::PerformanceSchema) => {
                format!("{family} times no file I/O while performance_schema is off.")
            }
            _ => "This scan has no storage timings.".to_string(),
        };
        let mut t = Tile::new(TileKey::Storage, TileStatus::Unavailable, why);
        t.detail = collector_note(s, &["storage", "files", "signals"]);
        return t;
    }
    // Enough operations for an average to mean something: the floor the slow-storage check uses.
    let worst = |log: bool| {
        files
            .iter()
            .filter(|f| (f.kind == FileKind::Log) == log)
            .filter_map(|f| waited_latency(f).filter(|(_, ops, _)| *ops >= 1_000.0).map(|(avg, _, _)| avg))
            .max_by(f64::total_cmp)
    };
    let (read, log) = (worst(false), worst(true));
    let (status, takeaway) = match worst_finding(report, "slow-storage") {
        Some(f) => (severity_status(f.severity), format!("{}.", f.title.trim_end_matches('.'))),
        None => {
            let mut parts = Vec::new();
            if let Some(avg) = read {
                parts.push(format!("reads average {}", latency(avg)));
            }
            if let Some(avg) = log {
                parts.push(format!("log writes {}", latency(avg)));
            }
            if parts.is_empty() {
                (TileStatus::Info, "Too little I/O since startup to judge storage speed.".to_string())
            } else {
                (TileStatus::Good, format!("Storage answers quickly: {}.", parts.join(", ")))
            }
        }
    };
    let mut t = Tile::new(TileKey::Storage, status, takeaway);
    if let Some(avg) = read {
        t.metrics.push(TileMetric::new("read-ms", "Avg read", Some(avg), latency(avg)));
    }
    if let Some(avg) = log {
        t.metrics.push(TileMetric::new("log-write-ms", "Avg log write", Some(avg), latency(avg)));
    }
    // The slowest files, each by what it's waited on for.
    let mut rows: Vec<(f64, TileItem)> = files
        .iter()
        .filter_map(|f| {
            let (avg, ops, what) = waited_latency(f)?;
            (ops >= 1_000.0).then(|| {
                let id = format!("{}:{}", f.database.as_deref().unwrap_or(""), f.name);
                let mut item = TileItem::new(ItemKind::Signal, id, file_name(s, f));
                item.value = Some(format!("{} per {what}", latency(avg)));
                let limit = if f.kind == FileKind::Log { 10.0 } else { 20.0 };
                item.status = Some(if avg >= limit * 2.5 {
                    TileStatus::Bad
                } else if avg >= limit {
                    TileStatus::Watch
                } else {
                    TileStatus::Good
                });
                (avg, item)
            })
        })
        .collect();
    rows.sort_by(|a, b| b.0.total_cmp(&a.0));
    t.items = rows.into_iter().map(|(_, i)| i).take(4).collect();
    // The counters' own window, worded like every other one (a statistics reset on PostgreSQL, else the restart).
    t.detail = Some(format!("Averages {}.", since_phrase(s)));
    t
}

/// A pressure signal as a tile row: its label, value and grade.
fn signal_item(sig: &Signal) -> TileItem {
    let mut item = TileItem::new(ItemKind::Signal, sig.key.clone(), sig.label.clone());
    item.value = Some(signal_value(sig));
    item.status = Some(signal_status(sig.status));
    item
}

/// New connections per second and connections in use: an application that connects for every request pays for it
/// on every request.
fn connections(report: &Report) -> Tile {
    let s = &report.snapshot;
    let p = s.pressure.as_ref();
    let rate = p.and_then(|p| p.signal("new-connections"));
    let used = p.and_then(|p| p.signal("connections"));
    if rate.is_none() && used.is_none() {
        let why = match s.server.engine {
            Engine::Postgres if !culprit_core::version_at_least(&s.server.version, [14, 0, 0]) => {
                "PostgreSQL counts sessions from version 14; this server doesn't.".to_string()
            }
            _ => "This scan has no connection counters.".to_string(),
        };
        let mut t = Tile::new(TileKey::Connections, TileStatus::Unavailable, why);
        t.detail = collector_note(s, &["connections", "logins", "sessions"]);
        return t;
    }
    let (status, takeaway) = match worst_finding(report, "connection-churn") {
        Some(f) => (severity_status(f.severity), format!("{}.", f.title.trim_end_matches('.'))),
        None => match rate {
            Some(r) if r.value >= 1.0 => (
                TileStatus::Good,
                format!("{} new connections a second, within what a pool keeps up with.", format::count(r.value)),
            ),
            Some(r) => (
                TileStatus::Good,
                format!("Connections are reused: {} new a minute.", format::count((r.value * 60.0).round())),
            ),
            None => {
                (TileStatus::Info, "This scan has no count of new connections; the ones in use are below.".to_string())
            }
        },
    };
    let mut t = Tile::new(TileKey::Connections, status, takeaway);
    // Under one a second, a per-minute count reads better than "0.0".
    if let Some(r) = rate {
        t.metrics.push(if r.value >= 1.0 {
            TileMetric::new("per-second", "New per second", Some(r.value), format::count(r.value))
        } else {
            let per_minute = (r.value * 60.0).round();
            TileMetric::new("per-minute", "New per minute", Some(per_minute), format::count(per_minute))
        });
    }
    if let Some(u) = used {
        let item = signal_item(u);
        // Running out of connections is worse than opening too many.
        if let Some(st @ (TileStatus::Bad | TileStatus::Watch)) = item.status {
            t.status = t.status.max(st);
        }
        t.items.push(item);
    }
    for key in ["session-length", "pooled-reuse", "thread-cache-misses", "sessions-abandoned"] {
        if let Some(sig) = p.and_then(|p| p.signal(key)) {
            t.items.push(signal_item(sig));
        }
    }
    t.items.truncate(4);
    t
}

fn lag_text(r: &Replica) -> String {
    r.lag_seconds
        .map(|sec| format!("{} behind", format::duration(sec * 1000.0)))
        .or_else(|| r.lag_bytes.map(|b| format!("{} of log", format::bytes(b))))
        .or_else(|| r.state.clone())
        .unwrap_or_else(|| "in sync".into())
}

/// How far replicas and replication slots are behind, worst first.
fn replication(report: &Report) -> Tile {
    let s = &report.snapshot;
    let family = family(&s.server);
    let replicas = s.pressure.as_ref().map(|p| p.replicas.as_slice()).unwrap_or_default();
    if replicas.is_empty() {
        if let Some(note) = collector_note(s, &["replication"]) {
            let mut t = Tile::new(
                TileKey::Replication,
                TileStatus::Unavailable,
                format!("Couldn't read {family}'s replication state."),
            );
            t.detail = Some(note);
            return t;
        }
        let none = match s.server.engine {
            Engine::Postgres => {
                "No replicas or replication slots: nothing can fall behind or hold WAL back.".to_string()
            }
            Engine::Mssql => "No availability group or geo-replica: nothing can fall behind.".to_string(),
            Engine::Mysql => format!("{family} isn't a replica here and reports none: nothing can fall behind."),
        };
        return Tile::new(TileKey::Replication, TileStatus::Good, none);
    }
    let (status, takeaway) = match worst_finding(report, "replica-lag") {
        Some(f) => (severity_status(f.severity), format!("{}.", f.title.trim_end_matches('.'))),
        None => (
            TileStatus::Good,
            format!(
                "{} {} keeping up.",
                capitalized(&plural(replicas.len(), "copy", "copies")),
                if replicas.len() == 1 { "is" } else { "are" }
            ),
        ),
    };
    let mut t = Tile::new(TileKey::Replication, status, takeaway);
    let mut rows: Vec<&Replica> = replicas.iter().collect();
    rows.sort_by(|a, b| {
        b.lag_seconds
            .unwrap_or(0.0)
            .total_cmp(&a.lag_seconds.unwrap_or(0.0))
            .then(b.lag_bytes.unwrap_or(0.0).total_cmp(&a.lag_bytes.unwrap_or(0.0)))
    });
    t.items = rows
        .into_iter()
        .take(4)
        .map(|r| {
            let role = match r.role {
                ReplicaRole::Replica => "replica",
                ReplicaRole::ThisServer => "this server",
                ReplicaRole::Slot => "slot",
            };
            let mut item = TileItem::new(ItemKind::Signal, r.name.clone(), format!("{} ({role})", r.name));
            item.value = Some(lag_text(r));
            item.status = Some(if r.error.is_some() || r.lag_seconds.is_some_and(|s| s >= 300.0) {
                TileStatus::Bad
            } else if r.lag_seconds.is_some_and(|s| s >= 30.0) || r.state.as_deref() == Some("inactive") {
                TileStatus::Watch
            } else {
                TileStatus::Good
            });
            item
        })
        .collect();
    t
}

/// Statements whose runs fail or are cancelled (timeouts), from the counts the engine keeps per statement.
fn failures(report: &Report) -> Tile {
    let s = &report.snapshot;
    let family = family(&s.server);
    let Some(w) = &s.workload else { return no_workload(TileKey::Failures, "failed runs aren't counted") };
    let counted: Vec<(&QueryStat, f64)> =
        app_queries(s).filter_map(|q| q.costs.as_ref()?.unfinished().map(|u| (q, u))).collect();
    if counted.is_empty() {
        let why = match s.server.engine {
            Engine::Postgres => {
                "PostgreSQL doesn't count failed runs per statement; its log does (log_min_error_statement)."
                    .to_string()
            }
            _ if w.source == "slow query log" => format!(
                "The slow query log keeps runs that finished; {family} counts errors per statement only in performance_schema."
            ),
            _ => format!("{family} reported no failed or cancelled runs per statement in this scan."),
        };
        return Tile::new(TileKey::Failures, TileStatus::Unavailable, why);
    }
    let mut failing: Vec<(&QueryStat, f64, f64)> = counted
        .into_iter()
        .filter(|(_, u)| *u > 0.0)
        .map(|(q, u)| (q, u, if q.executions > 0.0 { (u / q.executions).min(1.0) } else { 1.0 }))
        .collect();
    failing.sort_by(|a, b| b.1.total_cmp(&a.1));
    let total: f64 = failing.iter().map(|f| f.1).sum();
    let window = window_phrase(w);
    let (status, takeaway) = match worst_finding(report, "failing-query") {
        Some(f) => (severity_status(f.severity), format!("{}.", f.title.trim_end_matches('.'))),
        None if failing.is_empty() => (TileStatus::Good, format!("No statement failed or timed out over {window}.")),
        None => (
            TileStatus::Info,
            format!(
                "{} didn't finish over {window}, too few to matter.",
                capitalized(&plural(total.round() as usize, "run", "runs"))
            ),
        ),
    };
    let mut t = Tile::new(TileKey::Failures, status, takeaway);
    t.metrics.push(TileMetric::new("statements", "Statements", Some(failing.len() as f64), failing.len().to_string()));
    t.metrics.push(TileMetric::new("runs", "Unfinished runs", Some(total), format::count(total)));
    t.items = failing
        .iter()
        .take(4)
        .map(|(q, unfinished, rate)| {
            let mut item = TileItem::new(ItemKind::Query, q.id.clone(), query_handle(q));
            item.query_id = Some(q.id.clone());
            // Fewer than five unfinished runs is too few to grade (a grey dot), so the value says how few.
            item.value = Some(if *unfinished < 5.0 {
                format!("{} of {} runs", format::count(*unfinished), format::count(q.executions.max(*unfinished)))
            } else if *rate >= 0.999 {
                "every run".into()
            } else {
                format!("{} of runs", format::percent(*rate))
            });
            item.share = Some(*rate);
            let id = format!("failing-query:{}", q.id);
            item.finding_id = report.findings.iter().find(|f| f.id == id).map(|f| f.id.clone());
            item.status = Some(if *unfinished < 5.0 {
                TileStatus::Info
            } else if *rate >= 0.1 {
                TileStatus::Bad
            } else if *rate >= 0.01 {
                TileStatus::Watch
            } else {
                TileStatus::Info
            });
            item
        })
        .collect();
    t
}

fn statistics(report: &Report) -> Tile {
    let s = &report.snapshot;
    let locked = report.locked.len();
    let stats_findings: Vec<&Finding> =
        report.findings.iter().filter(|f| STATISTICS_FINDINGS.contains(&f.id.as_str())).collect();
    let mut tile = match &s.workload {
        None => {
            let missing = s.server.missing.iter().find(|m| WORKLOAD.contains(&m.capability));
            let mut t = Tile::new(
                TileKey::Statistics,
                TileStatus::Bad,
                match missing {
                    Some(m) => format!("No query history: {}", sentence(&m.reason)),
                    None => "No query history in this scan.".into(),
                },
            );
            t.detail = Some(
                "Until they're on, Culprit checks tables, indexes and settings only: no query list, plans or trends."
                    .into(),
            );
            t.fix_sql = missing.and_then(|m| m.fix.as_deref()).and_then(fix_statement);
            t.items = s
                .server
                .missing
                .iter()
                .filter(|m| WORKLOAD.contains(&m.capability))
                .map(|m| {
                    let mut item =
                        TileItem::new(ItemKind::Capability, capability_id(m.capability), m.capability.label());
                    item.value = Some("off".into());
                    item.status = Some(TileStatus::Bad);
                    item
                })
                .collect();
            t
        }
        Some(w) => {
            let planned = app_queries(s).filter(|q| q.plan.is_some()).count();
            let window = window_phrase(w);
            let mut status = TileStatus::Good;
            let mut takeaway = format!(
                "{} covers {window}: {}, {planned} with plans.",
                w.source,
                plural(w.query_count as usize, "query", "queries")
            );
            if let (Some(used), Some(max)) = (
                setting(s, "query_store_options.current_storage_size_mb").and_then(|v| v.parse::<f64>().ok()),
                setting(s, "query_store_options.max_storage_size_mb").and_then(|v| v.parse::<f64>().ok()),
            ) && max > 0.0
                && used / max >= 0.9
            {
                status = TileStatus::Watch;
                takeaway =
                    format!("Query Store is {} full; it stops recording when it fills.", format::percent(used / max));
            }
            if !w.windowed && w.window_hours < 1.0 {
                status = status.max(TileStatus::Watch);
                takeaway =
                    format!("{} covers only {window}, so the numbers are thin until it has seen more.", w.source);
            }
            if let Some(worst) = stats_findings.iter().rev().max_by_key(|f| f.severity) {
                status =
                    status.max(if worst.severity >= Severity::Medium { TileStatus::Watch } else { TileStatus::Info });
                if status >= TileStatus::Watch && worst.severity >= Severity::Medium {
                    takeaway = format!("{}.", worst.title.trim_end_matches('.'));
                }
            }
            let mut t = Tile::new(TileKey::Statistics, status, takeaway);
            // The slow query log only has statements over long_query_time: say so where its numbers are summarized.
            if w.source == "slow query log" {
                t.detail = s.notes.iter().find(|n| n.collector == "workload").map(|n| n.message.clone());
            }
            t.metrics = vec![
                TileMetric::new("queries", "Queries", Some(w.query_count), format::count(w.query_count)),
                TileMetric::new("plans", "With plans", Some(planned as f64), planned.to_string()),
                TileMetric::new("window-hours", "Covers", Some(w.window_hours), span(w.window_hours)),
            ];
            t.items = stats_findings
                .iter()
                .map(|f| {
                    let mut item = TileItem::new(ItemKind::Finding, f.id.clone(), f.title.clone());
                    item.status = Some(severity_status(f.severity));
                    item.finding_id = Some(f.id.clone());
                    item
                })
                .collect();
            t.fix_sql = stats_findings.first().and_then(|f| f.fix.as_ref()).and_then(|f| f.sql.clone());
            t
        }
    };
    tile.metrics.push(TileMetric::new("locked", "Locked checks", Some(locked as f64), locked.to_string()));
    if locked > 0 && tile.detail.is_none() {
        let needs: Vec<String> = {
            let mut caps: Vec<&str> = report.locked.iter().flat_map(|l| l.missing.iter().map(|c| c.label())).collect();
            caps.sort_unstable();
            caps.dedup();
            caps.into_iter().map(str::to_string).collect()
        };
        tile.detail = Some(format!(
            "{} locked: {} need{} {}.",
            capitalized(&plural(locked, "check is", "checks are")),
            format::list(&report.locked.iter().map(|l| l.check.title.clone()).take(3).collect::<Vec<_>>()),
            if locked == 1 { "s" } else { "" },
            format::list(&needs)
        ));
    }
    tile
}

/// The capability's stable id, as it serializes.
fn capability_id(c: Capability) -> String {
    match c {
        Capability::QueryStore => "query-store",
        Capability::PlanCache => "plan-cache",
        Capability::PgStatStatements => "pg-stat-statements",
        Capability::PerformanceSchema => "performance-schema",
        Capability::QueryPlans => "query-plans",
        Capability::GenericPlans => "generic-plans",
        Capability::TableStats => "table-stats",
        Capability::IndexUsage => "index-usage",
        Capability::MissingIndexes => "missing-indexes",
        Capability::Statistics => "statistics",
        Capability::WaitStats => "wait-stats",
        Capability::ResourceUsage => "resource-usage",
        Capability::Sessions => "sessions",
        Capability::Showplan => "showplan",
        Capability::SlowLog => "slow-log",
    }
    .to_string()
}

#[cfg(test)]
mod tests {
    #[test]
    fn trend_spans_add_up_to_the_window() {
        let t = Trend { recent_hours: 10.25 / 4.0, baseline_hours: 10.25 * 3.0 / 4.0, ratio: 2.0 };
        assert_eq!(trend_spans(&t), ("the last 3 hours".to_string(), "7 hours".to_string()));
        let day = Trend { recent_hours: 24.0, baseline_hours: 48.0, ratio: 2.0 };
        assert_eq!(trend_spans(&day), ("the last day".to_string(), "2 days".to_string()));
    }

    use super::*;

    #[test]
    fn fix_statements_come_out_of_sentences() {
        assert_eq!(
            fix_statement("Run CREATE EXTENSION pg_stat_statements; in this database. No restart is needed.")
                .as_deref(),
            Some("CREATE EXTENSION pg_stat_statements;")
        );
        assert_eq!(
            fix_statement("ALTER DATABASE CURRENT SET QUERY_STORE = ON;").as_deref(),
            Some("ALTER DATABASE CURRENT SET QUERY_STORE = ON;")
        );
        assert_eq!(fix_statement("Ask an administrator."), None);
        assert_eq!(fix_statement("Set performance_schema = ON in the server configuration and restart."), None);
    }

    #[test]
    fn reasons_read_as_sentence_ends() {
        assert_eq!(sentence("Query Store is OFF on this database."), "Query Store is OFF on this database.");
        assert_eq!(sentence("The login can't read it"), "the login can't read it.");
        assert_eq!(sentence("SQL Server 2016 or newer"), "SQL Server 2016 or newer.");
    }

    #[test]
    fn the_first_of_equally_severe_findings_leads() {
        use culprit_core::{Category, SubjectKind};
        let mut report = crate::analyze(crate::waits::snapshot_with(Engine::Mssql));
        let make = |key: &str| {
            let mut f = crate::support::finding(
                "failing-query",
                key,
                Category::Queries,
                crate::support::subject(SubjectKind::Query, key),
            );
            f.severity = Severity::High;
            f.title = format!("{key} fails");
            f
        };
        report.findings.retain(|f| f.check != "failing-query");
        report.findings.extend([make("first"), make("second")]);
        assert_eq!(worst_finding(&report, "failing-query").map(|f| f.title.as_str()), Some("first fails"));
    }

    #[test]
    fn labels_keep_acronyms() {
        assert_eq!(lower_label("CPU"), "CPU");
        assert_eq!(lower_label("Disk reads"), "disk reads");
    }
}
