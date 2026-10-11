//! What changed between scans. Each scan compares itself with the scan before it (tables, columns, indexes, settings,
//! version, restarts, a workload statistics reset, optimizer statistics) and keeps the result next to its report as
//! `<snapshot id>.events.json`, together with the changes the database dated itself. Reading a timeline is then a
//! matter of reading small files; reports saved before this existed are compared once, the first time they are read.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use culprit_core::changes::epoch;
use culprit_core::events::in_range;
use culprit_core::{
    ChangeEvent, Engine, EventAction, EventKind, EventSource, IndexStat, Report, Snapshot, server_label, sql,
};

use crate::{private_fs, reports, trends};

pub const EVENTS_SUFFIX: &str = ".events.json";
/// Reports compared on first read per call; the rest wait for the next read.
const MAX_BACKFILL: usize = 30;
/// Optimizer statistics updates kept per scan (tables the heaviest queries use, most recent first).
const MAX_STATISTICS: usize = 200;
/// Setting changes in one scan beyond this are folded into one event.
const MAX_SETTINGS: usize = 12;
/// Clocks drift by a second or two between readings of the same start time.
const SAME_TIME: f64 = 120.0;

fn path(dir: &Path, id: &str) -> PathBuf {
    dir.join(format!("{id}{EVENTS_SUFFIX}"))
}

fn read(dir: &Path, id: &str) -> Option<Vec<ChangeEvent>> {
    std::fs::read(path(dir, id)).ok().and_then(|b| serde_json::from_slice(&b).ok())
}

fn write(dir: &Path, id: &str, events: &[ChangeEvent]) {
    if let Ok(json) = serde_json::to_vec(events) {
        let _ = private_fs::write_atomic(&path(dir, id), &json);
    }
}

/// Compare a fresh report with the scan before it and keep the result. Never fails a scan: problems leave no file.
pub fn record(dir: &Path, report: &Report, previous_id: Option<&str>, recorded: Vec<ChangeEvent>) {
    let previous = previous_id
        .filter(|id| *id != report.snapshot.id)
        .and_then(|id| reports::load(&dir.join(format!("{id}.json"))).ok());
    let events = diff(previous.as_ref().map(|r| &r.snapshot), &report.snapshot, recorded);
    write(dir, &report.snapshot.id, &events);
}

/// Everything Culprit's scans noticed in `[start, end]` (epoch seconds), oldest first.
pub fn events(dir: &Path, start: f64, end: f64) -> Vec<ChangeEvent> {
    let ids = reports::report_ids(dir);
    let mut out = Vec::new();
    let mut backfilled = 0;
    let mut previous: Option<Snapshot> = None;
    for (i, id) in ids.iter().enumerate() {
        // A scan's file holds what happened since the scan before it (or, for the first, what the database dated).
        let wanted = reports::id_epoch(id).is_some_and(|t| t >= start - 1.0);
        if let Some(events) = read(dir, id) {
            if wanted {
                out.extend(events);
            }
            previous = None;
            continue;
        }
        if !wanted || backfilled >= MAX_BACKFILL {
            previous = None;
            continue;
        }
        backfilled += 1;
        let Ok(report) = reports::load(&dir.join(format!("{id}.json"))) else {
            previous = None;
            continue;
        };
        let before = previous.take().or_else(|| {
            let prior = ids.get(i.checked_sub(1)?)?;
            reports::load(&dir.join(format!("{prior}.json"))).ok().map(|r| r.snapshot)
        });
        let events = diff(before.as_ref(), &report.snapshot, Vec::new());
        write(dir, id, &events);
        out.extend(events);
        previous = Some(report.snapshot);
    }
    in_range(out, start, end)
}

/// What changed from `previous` to `current` (the same database), plus what the database dated itself (`recorded`).
pub fn diff(previous: Option<&Snapshot>, current: &Snapshot, recorded: Vec<ChangeEvent>) -> Vec<ChangeEvent> {
    let now = epoch(&current.taken_at).unwrap_or(0.0);
    let previous = previous.filter(|p| {
        p.server.engine == current.server.engine
            && p.server.database.eq_ignore_ascii_case(&current.server.database)
            && epoch(&p.taken_at).is_some_and(|t| t < now)
    });
    let since = previous.and_then(|p| epoch(&p.taken_at)).unwrap_or(now - trends::HISTORY_DAYS * 86_400.0);
    let mut out = Vec::new();
    if let Some(p) = previous {
        let at = (p.taken_at.as_str(), current.taken_at.as_str());
        schema(p, current, at, &mut out);
        settings(p, current, at, &mut out);
        version(p, current, at, &mut out);
    }
    restart(previous, current, since, &mut out);
    stats_reset(previous, current, since, &mut out);
    statistics(previous, current, since, &mut out);

    // The database's own timestamps make "between two scans" exact where exactly one change can be behind them, and
    // stand on their own where Culprit's comparison found nothing (a procedure altered, a table rebuilt).
    let recorded: Vec<ChangeEvent> =
        recorded.into_iter().filter(|e| e.start() > since && e.start() <= now + 60.0).collect();
    let mut explained = vec![false; recorded.len()];
    for (i, r) in recorded.iter().enumerate() {
        let matches: Vec<usize> = out
            .iter()
            .enumerate()
            .filter(|(_, e)| e.source == EventSource::Scans && explains(r, e))
            .map(|(j, _)| j)
            .collect();
        if matches.is_empty() {
            continue;
        }
        explained[i] = true;
        // One reload applies every setting it changed; one table alter date only dates one change.
        if matches.len() == 1 || r.kind == EventKind::Setting {
            for j in matches {
                let e = &mut out[j];
                // Ids end with the time; keep them unique and stable under the exact one.
                e.id = format!("{}{}", e.id.strip_suffix(e.at.as_str()).unwrap_or(&e.id), r.at);
                e.at = r.at.clone();
                e.after = None;
                e.source = EventSource::Database;
            }
        }
    }
    // A runtime SET of one setting is only news when the scans saw its global value change (sessions set them too).
    out.extend(
        recorded
            .into_iter()
            .zip(explained)
            .filter(|(r, done)| !done && !(r.kind == EventKind::Setting && r.object.is_some()))
            .map(|(r, _)| r),
    );
    let mut seen = BTreeSet::new();
    out.retain(|e| seen.insert(e.id.clone()));
    out.sort_by(|a, b| a.start().total_cmp(&b.start()));
    out
}

fn explains(recorded: &ChangeEvent, e: &ChangeEvent) -> bool {
    let inside = recorded.start() >= e.start() - 1.0 && recorded.start() <= e.end() + 1.0;
    let same_table = recorded.table.as_deref().zip(e.table.as_deref()).is_some_and(|(a, b)| a.eq_ignore_ascii_case(b));
    inside
        && match (recorded.kind, e.kind) {
            // MySQL's CREATE_TIME moves on create and alter alike, so "changed" dates either.
            (EventKind::Table, EventKind::Table) => {
                same_table && (recorded.action == e.action || recorded.action == Some(EventAction::Changed))
            }
            // A table's alter date moves when its columns or indexes change.
            (EventKind::Table, EventKind::Index) => same_table && recorded.action == Some(EventAction::Changed),
            (EventKind::Index, EventKind::Index) => {
                same_table
                    && recorded.action == e.action
                    && recorded
                        .object
                        .as_deref()
                        .zip(e.object.as_deref())
                        .is_some_and(|(a, b)| a.eq_ignore_ascii_case(b))
            }
            (EventKind::Setting, EventKind::Setting) => match (&recorded.object, &e.object) {
                (None, _) => true,
                (Some(a), Some(b)) => a.eq_ignore_ascii_case(b),
                _ => false,
            },
            _ => false,
        }
}

fn key(schema: &str, name: &str) -> String {
    format!("{}.{}", schema.to_ascii_lowercase(), name.to_ascii_lowercase())
}

fn scans(event: ChangeEvent, after: &str) -> ChangeEvent {
    event.between(after)
}

/// Tables, columns and indexes. Skipped when either scan has no table data (a permission or collector problem, not a
/// schema change), or when most tables vanished at once.
fn schema(p: &Snapshot, c: &Snapshot, (after, at): (&str, &str), out: &mut Vec<ChangeEvent>) {
    if p.tables.is_empty() || c.tables.is_empty() {
        return;
    }
    let tables = |s: &Snapshot| -> BTreeMap<String, String> {
        s.tables.iter().map(|t| (key(&t.schema, &t.name), t.qualified())).collect()
    };
    let (ta, tb) = (tables(p), tables(c));
    let gone = ta.keys().filter(|k| !tb.contains_key(*k)).count();
    if ta.len() > 20 && gone * 2 > ta.len() {
        return;
    }
    for (k, name) in &tb {
        if !ta.contains_key(k) {
            out.push(scans(
                ChangeEvent::new(EventKind::Table, EventSource::Scans, name, at, format!("Table {name} was created"))
                    .action(EventAction::Created)
                    .table(name.clone()),
                after,
            ));
        }
    }
    for (k, name) in &ta {
        if !tb.contains_key(k) {
            out.push(scans(
                ChangeEvent::new(EventKind::Table, EventSource::Scans, name, at, format!("Table {name} was dropped"))
                    .action(EventAction::Dropped)
                    .table(name.clone()),
                after,
            ));
        }
    }
    let common: BTreeMap<&String, &String> = tb.iter().filter(|(k, _)| ta.contains_key(*k)).collect();
    if !p.columns.is_empty() && !c.columns.is_empty() {
        columns(p, c, &common, (after, at), out);
    }
    if !p.indexes.is_empty() || !c.indexes.is_empty() {
        indexes(p, c, &common, (after, at), out);
    }
}

fn columns(
    p: &Snapshot,
    c: &Snapshot,
    common: &BTreeMap<&String, &String>,
    (after, at): (&str, &str),
    out: &mut Vec<ChangeEvent>,
) {
    let by_table = |s: &Snapshot| {
        let mut map: BTreeMap<String, BTreeMap<String, (String, String, bool)>> = BTreeMap::new();
        for col in &s.columns {
            map.entry(key(&col.schema, &col.table))
                .or_default()
                .insert(col.name.to_ascii_lowercase(), (col.name.clone(), col.data_type.clone(), col.nullable));
        }
        map
    };
    let (ca, cb) = (by_table(p), by_table(c));
    for (k, table) in common {
        let (Some(a), Some(b)) = (ca.get(*k), cb.get(*k)) else { continue };
        let shape = |t: &str, nullable: bool| format!("{t}{}", if nullable { "" } else { " NOT NULL" });
        let mut changes = Vec::new();
        for (name, (display, ty, nullable)) in b {
            match a.get(name) {
                None => changes.push(format!("added {display} {}", shape(ty, *nullable))),
                Some((_, old, old_null)) if !old.eq_ignore_ascii_case(ty) || old_null != nullable => {
                    changes.push(format!("{display} {} → {}", shape(old, *old_null), shape(ty, *nullable)))
                }
                Some(_) => {}
            }
        }
        for (name, (display, _, _)) in a {
            if !b.contains_key(name) {
                changes.push(format!("dropped {display}"));
            }
        }
        if changes.is_empty() {
            continue;
        }
        let title = if changes.len() == 1 {
            format!("Column change on {table}: {}", changes[0])
        } else {
            format!("{} column changes on {table}", changes.len())
        };
        out.push(scans(
            ChangeEvent::new(EventKind::Table, EventSource::Scans, &format!("{table}.columns"), at, title)
                .action(EventAction::Changed)
                .table(table.to_string())
                .detail(changes.join("; ")),
            after,
        ));
    }
}

/// What makes two indexes the same index, whatever they are called.
fn signature(i: &IndexStat) -> String {
    format!(
        "{}|{}|{}|{}|{}",
        i.kind.to_ascii_lowercase(),
        i.columns.join(",").to_ascii_lowercase(),
        i.include.join(",").to_ascii_lowercase(),
        i.filter.as_deref().unwrap_or_default().to_ascii_lowercase(),
        i.is_unique
    )
}

fn describe(i: &IndexStat) -> String {
    if let Some(definition) = i.definition.as_deref().filter(|d| !d.is_empty()) {
        return definition.to_string();
    }
    let mut text = format!("({})", i.columns.join(", "));
    if !i.include.is_empty() {
        text.push_str(&format!(" INCLUDE ({})", i.include.join(", ")));
    }
    if let Some(filter) = &i.filter {
        text.push_str(&format!(" WHERE {filter}"));
    }
    if i.is_unique {
        text = format!("UNIQUE {text}");
    }
    text
}

/// Indexes on the tables both scans have, by table and name.
fn index_map<'a>(s: &'a Snapshot, common: &BTreeMap<&String, &String>) -> BTreeMap<(String, String), &'a IndexStat> {
    s.indexes
        .iter()
        .filter(|i| common.contains_key(&key(&i.schema, &i.table)))
        .map(|i| ((key(&i.schema, &i.table), i.name.to_ascii_lowercase()), i))
        .collect()
}

fn indexes(
    p: &Snapshot,
    c: &Snapshot,
    common: &BTreeMap<&String, &String>,
    (after, at): (&str, &str),
    out: &mut Vec<ChangeEvent>,
) {
    let (ia, ib) = (index_map(p, common), index_map(c, common));
    let created: Vec<&IndexStat> = ib.iter().filter(|(k, _)| !ia.contains_key(*k)).map(|(_, i)| *i).collect();
    let mut dropped: Vec<&IndexStat> = ia.iter().filter(|(k, _)| !ib.contains_key(*k)).map(|(_, i)| *i).collect();
    let event = |i: &IndexStat, title: String, action: EventAction| {
        let table = i.qualified_table();
        scans(
            ChangeEvent::new(EventKind::Index, EventSource::Scans, &format!("{table}.{}", i.name), at, title)
                .action(action)
                .table(table)
                .object(i.name.clone()),
            after,
        )
    };
    for i in created {
        let table = i.qualified_table();
        // The same definition under a new name: a rename, which doesn't change plans.
        if let Some(pos) = dropped.iter().position(|d| d.qualified_table() == table && signature(d) == signature(i)) {
            let old = dropped.remove(pos);
            out.push(
                event(i, format!("Index {} on {table} was renamed to {}", old.name, i.name), EventAction::Changed)
                    .minor(),
            );
            continue;
        }
        out.push(
            event(i, format!("Index {} was created on {table}", i.name), EventAction::Created).detail(describe(i)),
        );
    }
    for i in dropped {
        let table = i.qualified_table();
        out.push(
            event(i, format!("Index {} was dropped from {table}", i.name), EventAction::Dropped).detail(describe(i)),
        );
    }
    for (k, b) in &ib {
        let Some(a) = ia.get(k) else { continue };
        let table = b.qualified_table();
        if a.is_disabled != b.is_disabled {
            let (verb, action) = if b.is_disabled {
                ("disabled", EventAction::Dropped)
            } else {
                ("enabled again", EventAction::Created)
            };
            out.push(event(b, format!("Index {} on {table} was {verb}", b.name), action));
        } else if signature(a) != signature(b) {
            out.push(
                event(b, format!("Index {} on {table} was redefined", b.name), EventAction::Changed).detail(format!(
                    "{} → {}",
                    describe(a),
                    describe(b)
                )),
            );
        }
    }
}

fn settings(p: &Snapshot, c: &Snapshot, (after, at): (&str, &str), out: &mut Vec<ChangeEvent>) {
    let skip = |name: &str| culprit_checks::drift::NOT_SETTINGS.iter().any(|n| n.eq_ignore_ascii_case(name));
    let changed: Vec<(String, String, String)> = c
        .settings
        .iter()
        .filter(|s| !skip(&s.name))
        .filter_map(|s| {
            let old = p.setting(&s.name)?;
            (!old.value.trim().eq_ignore_ascii_case(s.value.trim()))
                .then(|| (s.name.clone(), old.value.trim().to_string(), s.value.trim().to_string()))
        })
        .collect();
    if changed.len() > MAX_SETTINGS {
        let detail: Vec<String> = changed.iter().take(8).map(|(n, a, b)| format!("{n}: {a} → {b}")).collect();
        out.push(scans(
            ChangeEvent::new(
                EventKind::Setting,
                EventSource::Scans,
                "many",
                at,
                format!("{} settings changed", changed.len()),
            )
            .action(EventAction::Changed)
            .detail(format!("{}; …", detail.join("; "))),
            after,
        ));
        return;
    }
    for (name, a, b) in changed {
        out.push(scans(
            ChangeEvent::new(
                EventKind::Setting,
                EventSource::Scans,
                &name,
                at,
                format!("{name} changed from {a} to {b}"),
            )
            .action(EventAction::Changed)
            .object(name.clone()),
            after,
        ));
    }
}

fn version(p: &Snapshot, c: &Snapshot, (after, at): (&str, &str), out: &mut Vec<ChangeEvent>) {
    if p.server.version == c.server.version {
        return;
    }
    let (a, b) =
        (server_label(&p.server.product, &p.server.version), server_label(&c.server.product, &c.server.version));
    let mut event = ChangeEvent::new(
        EventKind::Version,
        EventSource::Scans,
        &c.server.version,
        at,
        format!("The server's version changed: {a} → {b}"),
    )
    .action(EventAction::Changed);
    // Upgrades restart the server, which dates them.
    match c.server.started_at.as_deref().filter(|s| epoch(s).is_some_and(|t| t > epoch(after).unwrap_or(0.0))) {
        Some(started) => {
            event.at = started.to_string();
            event.source = EventSource::Database;
        }
        None => event = event.between(after),
    }
    out.push(event);
}

fn restart(p: Option<&Snapshot>, c: &Snapshot, since: f64, out: &mut Vec<ChangeEvent>) {
    let Some(started) = c.server.started_at.as_deref() else { return };
    let Some(t) = epoch(started) else { return };
    let moved = match p.and_then(|p| p.server.started_at.as_deref()).and_then(epoch) {
        Some(before) => (t - before).abs() > SAME_TIME,
        None => true,
    };
    if !moved || t <= since {
        return;
    }
    let title = if p.is_some() { "The server restarted" } else { "The server started" };
    out.push(
        ChangeEvent::new(EventKind::Restart, EventSource::Database, "server", started, title)
            .detail("Caches start empty and every plan compiles again, so the first minutes run slower."),
    );
}

/// pg_stat_statements says when it was last reset; other sources' windows are inferred and would raise false alarms.
fn stats_reset(p: Option<&Snapshot>, c: &Snapshot, since: f64, out: &mut Vec<ChangeEvent>) {
    let Some(w) = c.workload.as_ref().filter(|w| !w.windowed && c.server.engine == Engine::Postgres) else { return };
    let Some(reset) = w.window_start.as_deref() else { return };
    let Some(t) = epoch(reset) else { return };
    let before = p.and_then(|p| p.workload.as_ref()).and_then(|w| w.window_start.as_deref()).and_then(epoch);
    let started = c.server.started_at.as_deref().and_then(epoch);
    if t <= since
        || before.is_some_and(|b| (t - b).abs() <= SAME_TIME)
        || started.is_some_and(|s| (t - s).abs() <= SAME_TIME)
    {
        return;
    }
    out.push(
        ChangeEvent::new(
            EventKind::StatsReset,
            EventSource::Database,
            &w.source,
            reset,
            format!("{} was reset", w.source),
        )
        .detail("Query statistics start again from zero; periods before and after it can't be compared through scans."),
    );
}

/// Fresh optimizer statistics on the tables the heaviest queries use: routine, but the usual reason a plan changes.
fn statistics(p: Option<&Snapshot>, c: &Snapshot, since: f64, out: &mut Vec<ChangeEvent>) {
    let engine = c.server.engine;
    let mut used: BTreeSet<String> = BTreeSet::new();
    let mut queries: Vec<_> = c.queries().iter().collect();
    queries.sort_by(|a, b| b.total_time_ms.total_cmp(&a.total_time_ms));
    for q in queries.iter().take(100) {
        for t in sql::tables_in(&q.text, engine) {
            used.insert(t.rsplit('.').next().unwrap_or(&t).to_string());
        }
    }
    if used.is_empty() {
        return;
    }
    let latest = |s: &Snapshot| -> BTreeMap<String, (String, String)> {
        let mut map: BTreeMap<String, (String, String)> = BTreeMap::new();
        let mut keep = |schema: &str, table: &str, at: &str| {
            let entry = map.entry(key(schema, table)).or_insert_with(|| (format!("{schema}.{table}"), at.to_string()));
            if epoch(at) > epoch(&entry.1) {
                entry.1 = at.to_string();
            }
        };
        for t in &s.tables {
            if let Some(at) = &t.last_analyze {
                keep(&t.schema, &t.name, at);
            }
        }
        for st in &s.statistics {
            if let Some(at) = &st.last_updated {
                keep(&st.schema, &st.table, at);
            }
        }
        map
    };
    let before = p.map(latest).unwrap_or_default();
    let mut found: Vec<ChangeEvent> = latest(c)
        .into_iter()
        .filter(|(k, _)| used.contains(k.rsplit('.').next().unwrap_or(k)))
        .filter(|(k, (_, at))| epoch(at).is_some_and(|t| t > since) && before.get(k).is_none_or(|(_, b)| b != at))
        .map(|(_, (table, at))| {
            ChangeEvent::new(
                EventKind::Statistics,
                EventSource::Database,
                &table,
                &at,
                format!("Statistics on {table} were updated"),
            )
            .table(table)
            .minor()
        })
        .collect();
    found.sort_by(|a, b| b.at.cmp(&a.at));
    found.truncate(MAX_STATISTICS);
    out.extend(found);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn demo() -> Snapshot {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../fixtures/pg-demo.report.json");
        let report: Report = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        report.snapshot
    }

    #[test]
    fn scans_notice_schema_settings_and_restarts() {
        let mut before = demo();
        before.taken_at = "2026-10-11T10:00:00Z".into();
        before.server.started_at = Some("2026-10-01T00:00:00Z".into());
        let mut after = before.clone();
        after.taken_at = "2026-10-11T12:00:00Z".into();
        after.server.started_at = Some("2026-10-11T11:30:00Z".into());
        let dropped = after.indexes.iter().position(|i| !i.is_primary_key).unwrap();
        let gone = after.indexes.remove(dropped);
        let first_table = after.tables[0].clone();
        after.columns.push(culprit_core::ColumnInfo {
            schema: first_table.schema.clone(),
            table: first_table.name.clone(),
            name: "archived_at".into(),
            data_type: "timestamptz".into(),
            nullable: true,
            default: None,
            generated: false,
            ordinal: 99,
        });
        let setting = after.settings[0].name.clone();
        after.settings[0].value = "something else".into();
        let recorded = vec![
            ChangeEvent::new(
                EventKind::Setting,
                EventSource::Database,
                "reload",
                "2026-10-11T11:00:00Z",
                "Configuration reloaded",
            )
            .action(EventAction::Changed),
        ];
        let events = diff(Some(&before), &after, recorded);
        let find = |kind: EventKind| events.iter().filter(move |e| e.kind == kind);
        let index = find(EventKind::Index).next().expect("the dropped index");
        assert_eq!(index.title, format!("Index {} was dropped from {}", gone.name, gone.qualified_table()));
        assert_eq!(index.after.as_deref(), Some("2026-10-11T10:00:00Z"), "noticed between the two scans");
        assert_eq!(index.action, Some(EventAction::Dropped));
        let column = find(EventKind::Table).next().expect("the added column");
        assert!(column.title.contains("added archived_at timestamptz"), "{}", column.title);
        let set = find(EventKind::Setting).next().expect("the changed setting");
        assert!(set.title.starts_with(&format!("{setting} changed from")), "{}", set.title);
        assert_eq!(set.at, "2026-10-11T11:00:00Z", "the reload dates it");
        assert_eq!(set.source, EventSource::Database);
        assert_eq!(find(EventKind::Setting).count(), 1, "the reload itself is folded into the change it explains");
        let restart = find(EventKind::Restart).next().expect("the restart");
        assert_eq!(restart.at, "2026-10-11T11:30:00Z");
        // Nothing changed: nothing to say.
        let mut again = after.clone();
        again.taken_at = "2026-10-11T13:00:00Z".into();
        assert!(diff(Some(&after), &again, vec![]).iter().all(|e| e.kind == EventKind::Statistics));
    }

    #[test]
    fn missing_table_data_is_not_a_mass_drop() {
        let before = demo();
        let mut after = before.clone();
        after.taken_at = "2099-01-01T00:00:00Z".into();
        after.tables.clear();
        after.indexes.clear();
        assert!(
            diff(Some(&before), &after, vec![]).iter().all(|e| !matches!(e.kind, EventKind::Table | EventKind::Index))
        );
    }
}
