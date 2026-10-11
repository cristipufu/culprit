//! What changed around a database, on one timeline: marks (a deploy, a migration), schema and index changes, code,
//! settings, upgrades, restarts, statistics updates and cloud throttling.
//!
//! Every event says when it happened as precisely as its source can: the database's own timestamp, the moment someone
//! marked, or "between two of Culprit's scans". [`correlate`] then ties events to the queries that got slower or faster
//! around them, so "it got slow" comes with "and this is what changed", ranked by how directly each event touches the
//! query (an index dropped from the table it reads beats a restart).

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::changes::{ChangeStatus, Changes, QueryChange, epoch, short_time};

#[derive(Serialize, Deserialize, TS, Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(rename_all = "kebab-case")]
#[ts(export)]
pub enum EventKind {
    /// Someone put it on the timeline (`culprit mark`, the app, a coding agent): a deploy, a migration, a config push.
    Mark,
    /// An index was created, dropped, disabled or redefined.
    Index,
    /// A table was created or dropped, or its columns changed.
    Table,
    /// A procedure, view, function or trigger was created or altered.
    Code,
    /// A server or database setting changed, or the configuration was reloaded.
    Setting,
    /// The server was upgraded or patched.
    Version,
    /// The server restarted: caches start cold and every plan compiles again.
    Restart,
    /// The workload statistics were reset (pg_stat_statements_reset, a performance_schema truncate).
    StatsReset,
    /// Optimizer statistics on a table were updated (ANALYZE, UPDATE STATISTICS, auto-stats), which can change plans.
    Statistics,
    /// The cloud held the server back: CPU or IO credits ran out, or a tier limit was hit.
    Throttle,
}

impl EventKind {
    pub fn slug(self) -> &'static str {
        match self {
            EventKind::Mark => "mark",
            EventKind::Index => "index",
            EventKind::Table => "table",
            EventKind::Code => "code",
            EventKind::Setting => "setting",
            EventKind::Version => "version",
            EventKind::Restart => "restart",
            EventKind::StatsReset => "stats-reset",
            EventKind::Statistics => "statistics",
            EventKind::Throttle => "throttle",
        }
    }
}

/// Where the event's time comes from, which says how exact it is.
#[derive(Serialize, Deserialize, TS, Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[serde(rename_all = "kebab-case")]
#[ts(export)]
pub enum EventSource {
    /// Someone marked it.
    Mark,
    /// The database recorded when it happened (object alter dates, statistics dates, its start time).
    Database,
    /// Culprit noticed it between two of its scans: it happened after `after` and before `at`.
    Scans,
    /// The cloud provider's metrics (CloudWatch, Azure Monitor).
    Cloud,
}

#[derive(Serialize, Deserialize, TS, Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[serde(rename_all = "kebab-case")]
#[ts(export)]
pub enum EventAction {
    Created,
    Dropped,
    Changed,
}

/// One thing that changed, and when.
#[derive(Serialize, Deserialize, TS, Clone, Debug, PartialEq)]
#[ts(export)]
pub struct ChangeEvent {
    /// Stable id: `<kind>:<subject>:<time>`.
    pub id: String,
    pub kind: EventKind,
    pub source: EventSource,
    pub action: Option<EventAction>,
    /// When it happened (UTC, ISO 8601). For changes noticed between scans, the scan that noticed it; episodes start here.
    pub at: String,
    /// Noticed between scans: it happened after this scan and before `at`.
    pub after: Option<String>,
    /// Episodes (throttling) last until here.
    pub until: Option<String>,
    /// One plain sentence, without the time: "Index ix_orders_status was dropped from public.orders".
    pub title: String,
    /// Before → after, a definition, what it means.
    pub detail: Option<String>,
    /// The table it touches (`schema.table`, as the database names it), so it can be tied to the queries that use it.
    pub table: Option<String>,
    /// The object it is about, when that isn't a table (a procedure, a view, a setting).
    pub object: Option<String>,
    /// Routine upkeep rather than news (statistics updates): shown with the queries it may explain, never flagged alone.
    pub minor: bool,
    /// Marks: who put it there ("cli", "app" or the coding agent's name).
    pub by: Option<String>,
}

impl ChangeEvent {
    /// An event at an exact time from `source`; `subject` makes the id unique among events of its kind.
    pub fn new(kind: EventKind, source: EventSource, subject: &str, at: &str, title: impl Into<String>) -> ChangeEvent {
        ChangeEvent {
            id: format!("{}:{}:{at}", kind.slug(), subject.to_ascii_lowercase()),
            kind,
            source,
            action: None,
            at: at.to_string(),
            after: None,
            until: None,
            title: title.into(),
            detail: None,
            table: None,
            object: None,
            minor: false,
            by: None,
        }
    }

    pub fn action(mut self, action: EventAction) -> ChangeEvent {
        self.action = Some(action);
        self
    }

    pub fn table(mut self, table: impl Into<String>) -> ChangeEvent {
        self.table = Some(table.into());
        self
    }

    pub fn object(mut self, object: impl Into<String>) -> ChangeEvent {
        self.object = Some(object.into());
        self
    }

    pub fn detail(mut self, detail: impl Into<String>) -> ChangeEvent {
        self.detail = Some(detail.into());
        self
    }

    pub fn minor(mut self) -> ChangeEvent {
        self.minor = true;
        self
    }

    /// Noticed between two scans: it happened after `after`.
    pub fn between(mut self, after: &str) -> ChangeEvent {
        self.after = Some(after.to_string());
        self
    }

    pub fn until(mut self, until: &str) -> ChangeEvent {
        self.until = Some(until.to_string());
        self
    }

    /// Earliest moment it may have happened (epoch seconds).
    pub fn start(&self) -> f64 {
        self.after.as_deref().and_then(epoch).or_else(|| epoch(&self.at)).unwrap_or(0.0)
    }

    /// Latest moment it may have happened, or when the episode ended (epoch seconds).
    pub fn end(&self) -> f64 {
        self.until.as_deref().and_then(epoch).or_else(|| epoch(&self.at)).unwrap_or(0.0)
    }

    /// "at Tue 14:05 UTC", "between the Tue 13:00 and 14:00 UTC scans", "from Tue 14:02 to 15:10 UTC". Agents and the
    /// CLI read UTC; the app formats the same fields in local time.
    pub fn when(&self) -> String {
        match (&self.after, &self.until) {
            (Some(after), _) => {
                format!("between the {} and {} scans", short_time(after), clock(&self.at))
            }
            (None, Some(until)) => format!("from {} to {}", short_time(&self.at), clock(until)),
            (None, None) => format!("at {}", short_time(&self.at)),
        }
    }
}

/// "14:05 UTC".
fn clock(iso: &str) -> String {
    chrono::DateTime::parse_from_rfc3339(iso)
        .map(|t| t.with_timezone(&chrono::Utc).format("%H:%M UTC").to_string())
        .unwrap_or_else(|_| iso.to_string())
}

/// An event that may explain a query's change, and why.
#[derive(Serialize, Deserialize, TS, Clone, Debug, PartialEq)]
#[ts(export)]
pub struct EventLink {
    /// The event's id in `Changes::events`.
    pub event: String,
    /// How it relates to this query, without a time: "this query reads public.orders", "it's this query's procedure".
    pub why: String,
    /// Touches this query directly (its table, its procedure) or lines up with what happened to it; otherwise it only
    /// happened around the same time.
    pub strong: bool,
}

/// Events in `[start, end]` (epoch seconds), oldest first, without duplicates.
pub fn in_range(events: Vec<ChangeEvent>, start: f64, end: f64) -> Vec<ChangeEvent> {
    let mut seen = BTreeSet::new();
    let mut out: Vec<ChangeEvent> =
        events.into_iter().filter(|e| e.end() >= start && e.start() <= end && seen.insert(e.id.clone())).collect();
    out.sort_by(|a, b| a.start().total_cmp(&b.start()).then(a.kind.cmp(&b.kind)));
    out
}

/// Tie events to the queries that changed around them, append the strongest one to each query's headline, list the
/// events in `changes.events` and mention them in the summary.
pub fn correlate(changes: &mut Changes, events: Vec<ChangeEvent>) {
    let (Some(b0), Some(r0), Some(r1)) =
        (epoch(&changes.baseline.start), epoch(&changes.recent.start), epoch(&changes.recent.end))
    else {
        return;
    };
    let events = in_range(events, b0, r1);
    let mut used = BTreeSet::new();
    for q in changes.queries.iter_mut() {
        if !matches!(
            q.status,
            ChangeStatus::Regressed | ChangeStatus::Improved | ChangeStatus::New | ChangeStatus::Stopped
        ) {
            continue;
        }
        let tables = crate::sql::tables_in(&q.text, changes.engine);
        // When the query changed, if anything says so exactly: the moment its new plan first ran.
        let exact = q.plan_switch.as_ref().and_then(|s| s.at.as_deref()).and_then(epoch);
        let mut links: Vec<(f64, &ChangeEvent, EventLink)> = events
            .iter()
            .filter_map(|e| link(e, q, &tables, exact, (r0, r1)).map(|(score, l)| (score, e, l)))
            .collect();
        links.sort_by(|a, b| b.0.total_cmp(&a.0));
        links.truncate(3);
        if let Some((_, e, l)) = links.first().filter(|(_, _, l)| l.strong) {
            q.headline = format!("{} Around then: {} {} ({}).", q.headline, e.title, e.when(), l.why);
        }
        for (_, e, _) in &links {
            used.insert(e.id.clone());
        }
        q.context = links.into_iter().map(|(_, _, l)| l).collect();
    }
    let events: Vec<ChangeEvent> = events.into_iter().filter(|e| !e.minor || used.contains(&e.id)).collect();
    if let Some(words) = around_then(&events) {
        changes.summary = format!("{} Around then: {words}.", changes.summary);
    }
    changes.events = events;
}

/// Table names compare by their last part, case-insensitively: `orders`, `public.orders` and `shop.orders` all match.
fn same_table(a: &str, b: &str) -> bool {
    let leaf = |t: &str| t.rsplit('.').next().unwrap_or(t).trim_matches(['"', '`', '[', ']']).to_ascii_lowercase();
    leaf(a) == leaf(b)
}

fn leaf(name: &str) -> &str {
    name.rsplit('.').next().unwrap_or(name)
}

/// How likely `e` explains `q`'s change (0–1, before timing), and why. `None` when it doesn't concern the query.
fn link(
    e: &ChangeEvent,
    q: &QueryChange,
    tables: &[String],
    exact: Option<f64>,
    recent: (f64, f64),
) -> Option<(f64, EventLink)> {
    use ChangeStatus::{Improved, New, Regressed, Stopped};
    let touched = e.table.as_deref().and_then(|t| tables.iter().find(|q| same_table(q, t)).map(|_| t));
    let own_object = e.object.as_deref().zip(q.object_name.as_deref()).is_some_and(|(a, b)| same_table(a, b));
    let reads = |t: &str| format!("this query uses {t}");
    let (score, why, strong) = match (e.kind, q.status) {
        (EventKind::Index, status) => {
            let table = touched?;
            let score = match (e.action, status) {
                (Some(EventAction::Dropped), Regressed) | (Some(EventAction::Created), Improved) => 0.95,
                (Some(EventAction::Created), Regressed) | (Some(EventAction::Changed), _) => 0.7,
                (Some(EventAction::Dropped), Improved) => 0.3,
                _ => 0.5,
            };
            (score, reads(table), score >= 0.7)
        }
        // A table's definition changing (columns, its indexes) outranks fresh statistics, which often follow from it.
        (EventKind::Table, status) => {
            let table = touched?;
            let score = match (e.action, status) {
                (Some(EventAction::Created), New) => 0.9,
                (Some(EventAction::Created), _) => 0.5,
                (Some(EventAction::Dropped), Stopped) => 0.9,
                (_, Regressed | Improved) => 0.9,
                _ => 0.6,
            };
            (score, reads(table), score >= 0.7)
        }
        (EventKind::Code, _) if own_object => (0.95, "it's this query's procedure or function".to_string(), true),
        (EventKind::Code, _) => match touched {
            // A trigger runs with every write to its table.
            Some(table) => (0.7, format!("a trigger on {table} runs with this query"), true),
            None => {
                let object = e.object.as_deref().filter(|o| tables.iter().any(|t| same_table(t, o)))?;
                (0.8, format!("this query reads {}", leaf(object)), true)
            }
        },
        // Fresh statistics are the usual reason a plan changes; without a plan change they rarely matter.
        (EventKind::Statistics, Regressed | Improved) if q.plan_switch.is_some() => {
            let table = touched?;
            (0.85, format!("statistics on {table} feed this query's plan, which changed"), true)
        }
        (EventKind::Statistics, _) => return None,
        (EventKind::Mark, New | Stopped) => {
            (0.8, "queries that start or stop usually ship with a deploy".to_string(), true)
        }
        (EventKind::Mark, _) => (0.6, "marked on the timeline".to_string(), true),
        (EventKind::Setting, Regressed | Improved) => (0.5, "settings apply to every query".to_string(), false),
        (EventKind::Version, Regressed | Improved) => {
            (0.6, "a new version can plan queries differently".to_string(), true)
        }
        (EventKind::Restart, Regressed) => {
            (0.4, "after a restart caches are cold and plans compile again".to_string(), false)
        }
        // Throttling slows queries without changing their plans; it has to overlap the recent period to matter.
        (EventKind::Throttle, Regressed) if q.plan_switch.is_none() => {
            if e.end() < recent.0 || e.start() > recent.1 {
                return None;
            }
            (0.75, "the cloud held the server back while this query ran slower".to_string(), true)
        }
        _ => return None,
    };
    if e.kind == EventKind::Throttle {
        return Some((score, EventLink { event: e.id.clone(), why, strong }));
    }
    // Something that happened after the query had already changed can't have caused it.
    if exact.is_some_and(|t| e.start() > t + 60.0) {
        return None;
    }
    // Closer to the change is likelier, fading over hours. Without an exact moment, anything in the recent period is as
    // close as the boundary between the periods.
    let distance = match exact {
        Some(t) => (t.clamp(e.start(), e.end()) - t).abs(),
        None if e.end() >= recent.0 => 0.0,
        None => recent.0 - e.end(),
    };
    let timing = 1.0 / (1.0 + distance / 3600.0 / 3.0);
    Some((score * timing, EventLink { event: e.id.clone(), why, strong }))
}

/// "1 mark (\"deploy v1.4.2\"), 2 index changes and a restart".
fn around_then(events: &[ChangeEvent]) -> Option<String> {
    let count = |kind: EventKind| events.iter().filter(|e| e.kind == kind).count();
    let plural = |n: usize, one: &str, many: &str| match n {
        0 => None,
        1 => Some(one.to_string()),
        n => Some(format!("{n} {many}")),
    };
    let marks: Vec<&ChangeEvent> = events.iter().filter(|e| e.kind == EventKind::Mark).collect();
    let mark_words = match marks.as_slice() {
        [] => None,
        [one] => Some(format!("a mark (\"{}\")", clip(&one.title, 60))),
        many => Some(format!("{} marks", many.len())),
    };
    let parts: Vec<String> = [
        mark_words,
        plural(count(EventKind::Index), "an index change", "index changes"),
        plural(count(EventKind::Table), "a table change", "table changes"),
        plural(count(EventKind::Code), "a procedure or view change", "procedure or view changes"),
        plural(count(EventKind::Setting), "a setting change", "setting changes"),
        plural(count(EventKind::Version), "an upgrade", "upgrades"),
        plural(count(EventKind::Restart), "a restart", "restarts"),
        plural(count(EventKind::StatsReset), "a statistics reset", "statistics resets"),
        plural(count(EventKind::Statistics), "a statistics update", "statistics updates"),
        plural(count(EventKind::Throttle), "cloud throttling", "throttling episodes"),
    ]
    .into_iter()
    .flatten()
    .collect();
    match parts.as_slice() {
        [] => None,
        [one] => Some(one.clone()),
        [rest @ .., last] => Some(format!("{} and {last}", rest.join(", "))),
    }
}

fn clip(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    format!("{}…", text.chars().take(max - 1).collect::<String>())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::changes::{CompareOptions, Period, PeriodData, PeriodStats, PlanPeriods, QueryPeriods, compare};
    use crate::model::{Engine, TimelineSource};

    fn stats(runs: f64, mean: f64) -> PeriodStats {
        PeriodStats::new(runs, runs * mean)
    }

    fn changes(queries: Vec<QueryPeriods>) -> Changes {
        let mut recent_total = PeriodStats::default();
        let mut baseline_total = PeriodStats::default();
        for q in &queries {
            q.baseline.iter().for_each(|s| baseline_total.add(s));
            q.recent.iter().for_each(|s| recent_total.add(s));
        }
        let data = PeriodData {
            source: TimelineSource::Scans,
            baseline: Period { start: "2026-10-11T08:00:00Z".into(), end: "2026-10-11T12:00:00Z".into() },
            recent: Period { start: "2026-10-11T12:00:00Z".into(), end: "2026-10-11T16:00:00Z".into() },
            baseline_total,
            recent_total,
            baseline_coverage: 1.0,
            queries,
            notes: vec![],
            unavailable: None,
        };
        compare("shop", Engine::Postgres, data, &CompareOptions::default())
    }

    fn query(id: &str, text: &str, b: Option<PeriodStats>, r: Option<PeriodStats>) -> QueryPeriods {
        QueryPeriods { query_id: id.into(), text: text.into(), baseline: b, recent: r, ..Default::default() }
    }

    fn dropped(table: &str, at: &str) -> ChangeEvent {
        ChangeEvent::new(
            EventKind::Index,
            EventSource::Scans,
            &format!("{table}.ix"),
            at,
            format!("Index ix was dropped from {table}"),
        )
        .action(EventAction::Dropped)
        .table(table)
        .between("2026-10-11T11:00:00Z")
    }

    #[test]
    fn an_index_dropped_from_the_table_a_query_reads_explains_it() {
        let mut c = changes(vec![
            query(
                "orders",
                "SELECT * FROM public.orders o WHERE o.status = $1",
                Some(stats(100.0, 2.0)),
                Some(stats(100.0, 9.0)),
            ),
            query("users", "SELECT * FROM users WHERE id = $1", Some(stats(100.0, 2.0)), Some(stats(100.0, 9.0))),
        ]);
        let events = vec![
            dropped("public.orders", "2026-10-11T12:00:00Z"),
            ChangeEvent::new(
                EventKind::Statistics,
                EventSource::Database,
                "public.users",
                "2026-10-11T11:30:00Z",
                "Statistics on public.users were updated",
            )
            .table("public.users")
            .minor(),
            ChangeEvent::new(EventKind::Mark, EventSource::Mark, "deploy", "2026-10-11T02:00:00Z", "deploy v1.4.1"),
        ];
        correlate(&mut c, events);
        let orders = c.queries.iter().find(|q| q.query_id == "orders").unwrap();
        assert_eq!(orders.context.len(), 1);
        assert!(orders.context[0].strong);
        assert!(orders.headline.contains("Around then: Index ix was dropped from public.orders between the Sun 11:00 UTC and 12:00 UTC scans (this query uses public.orders)."), "{}", orders.headline);
        let users = c.queries.iter().find(|q| q.query_id == "users").unwrap();
        assert!(users.context.is_empty(), "statistics without a plan change don't explain it");
        // The mark is outside both periods; the unused statistics update is routine, so neither is listed.
        assert_eq!(c.events.len(), 1, "{:?}", c.events);
        assert!(c.summary.ends_with("Around then: an index change."), "{}", c.summary);
    }

    #[test]
    fn throttling_explains_slower_runs_with_the_same_plan_only() {
        let mut same = query("same", "SELECT 1 FROM t", Some(stats(100.0, 2.0)), Some(stats(100.0, 9.0)));
        same.plans = vec![PlanPeriods {
            id: "a".into(),
            baseline: Some(stats(100.0, 2.0)),
            recent: Some(stats(100.0, 9.0)),
            ..Default::default()
        }];
        let mut switched = query("switched", "SELECT 2 FROM t", Some(stats(100.0, 2.0)), Some(stats(100.0, 9.0)));
        switched.plans = vec![
            PlanPeriods { id: "a".into(), baseline: Some(stats(100.0, 2.0)), ..Default::default() },
            PlanPeriods {
                id: "b".into(),
                recent: Some(stats(100.0, 9.0)),
                first_recent: Some("2026-10-11T12:30:00Z".into()),
                ..Default::default()
            },
        ];
        let mut c = changes(vec![same, switched]);
        let throttle = ChangeEvent::new(
            EventKind::Throttle,
            EventSource::Cloud,
            "cpu-credits",
            "2026-10-11T13:00:00Z",
            "CPU credits ran out",
        )
        .until("2026-10-11T14:10:00Z");
        correlate(&mut c, vec![throttle]);
        let same = c.queries.iter().find(|q| q.query_id == "same").unwrap();
        assert_eq!(same.context.len(), 1);
        assert!(same.headline.contains("CPU credits ran out from Sun 13:00 UTC to 14:10 UTC"), "{}", same.headline);
        let switched = c.queries.iter().find(|q| q.query_id == "switched").unwrap();
        assert!(switched.context.is_empty(), "a plan change is the better explanation");
    }

    #[test]
    fn events_after_the_change_dont_explain_it() {
        let mut q = query("q", "SELECT * FROM orders", Some(stats(100.0, 2.0)), Some(stats(100.0, 9.0)));
        q.plans = vec![
            PlanPeriods { id: "a".into(), baseline: Some(stats(100.0, 2.0)), ..Default::default() },
            PlanPeriods {
                id: "b".into(),
                recent: Some(stats(100.0, 9.0)),
                first_recent: Some("2026-10-11T12:30:00Z".into()),
                ..Default::default()
            },
        ];
        let mut c = changes(vec![q]);
        correlate(
            &mut c,
            vec![ChangeEvent::new(EventKind::Mark, EventSource::Mark, "late", "2026-10-11T15:00:00Z", "rollback")],
        );
        assert!(c.queries[0].context.is_empty(), "the plan changed at 12:30, before the mark");
        assert_eq!(c.events.len(), 1, "it is still listed for the period");
    }

    #[test]
    fn new_queries_line_up_with_deploys_and_procedures_with_their_queries() {
        let mut proc = query("proc", "EXEC dbo.GetOrders @id", Some(stats(100.0, 2.0)), Some(stats(100.0, 9.0)));
        proc.object_name = Some("dbo.GetOrders".into());
        let mut c = changes(vec![query("new", "SELECT * FROM invoices", None, Some(stats(400.0, 3.0))), proc]);
        let events = vec![
            ChangeEvent::new(EventKind::Mark, EventSource::Mark, "deploy", "2026-10-11T11:50:00Z", "deploy v1.4.2"),
            ChangeEvent::new(
                EventKind::Code,
                EventSource::Database,
                "dbo.getorders",
                "2026-10-11T11:55:00Z",
                "Procedure dbo.GetOrders was altered",
            )
            .object("dbo.GetOrders")
            .action(EventAction::Changed),
        ];
        correlate(&mut c, events);
        let new = c.queries.iter().find(|q| q.query_id == "new").unwrap();
        assert_eq!(new.context[0].event, "mark:deploy:2026-10-11T11:50:00Z");
        let proc = c.queries.iter().find(|q| q.query_id == "proc").unwrap();
        assert!(proc.context[0].why.contains("procedure"), "{:?}", proc.context);
        assert!(c.summary.contains("a mark (\"deploy v1.4.2\") and a procedure or view change"), "{}", c.summary);
    }
}
