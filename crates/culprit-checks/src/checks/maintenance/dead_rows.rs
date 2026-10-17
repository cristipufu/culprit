//! The `dead-rows` check: Tables carrying many dead tuples, which bloat storage and slow scans.

use super::*;

pub fn dead_rows_info() -> CheckInfo {
    CheckInfo {
        id: "dead-rows".into(),
        title: "Dead rows and lagging autovacuum".into(),
        category: Category::Maintenance,
        engines: vec![Engine::Postgres],
        requires: vec![Capability::TableStats],
        description: "Tables carrying many dead tuples, which bloat storage and slow scans.".into(),
    }
}

check!(DeadRowsCheck: dead_rows_info, dead_rows);

/// When autovacuum vacuums `t`: its own storage parameters, else the server's settings, else PostgreSQL's defaults
/// (50 rows + 20% of the table). `None` when autovacuum is off for it (or for the server).
pub(super) fn vacuum_trigger(snapshot: &Snapshot, t: &culprit_core::TableStat, live: f64) -> Option<f64> {
    autovacuum_trigger(snapshot, t, live, "vacuum")
}

/// An autovacuum setting as it applies to `t`: its own storage option, else the server's.
pub(super) fn autovacuum_setting(snapshot: &Snapshot, t: &culprit_core::TableStat, name: &str) -> Option<f64> {
    t.storage
        .as_ref()
        .and_then(|s| s.option(name))
        .or_else(|| snapshot.setting(name).and_then(|s| s.value.trim().parse::<f64>().ok()))
}

pub fn dead_rows(snapshot: &Snapshot) -> Vec<Finding> {
    if snapshot.server.engine != Engine::Postgres {
        return Vec::new();
    }
    snapshot
        .tables
        .iter()
        .filter_map(|t| {
            let live = t.rows?;
            let dead = t.dead_rows?;
            // A busy table reaches its trigger (by default 20% of it dead) between every two autovacuum runs: that is
            // autovacuum's rhythm. Past twice the trigger it is behind; with autovacuum off for the table, any real
            // pile of dead rows stays.
            let trigger = vacuum_trigger(snapshot, t, live);
            let limit = trigger.map_or(live * 0.2, |tr| tr * DEAD_MARGIN);
            if dead < 10_000.0 || dead < limit {
                return None;
            }
            let table_name = t.qualified();
            let ratio = dead / (live + dead).max(1.0);
            let last = t.last_vacuum.as_deref().and_then(|d| days_between(d, &snapshot.taken_at)).filter(|d| *d >= 0.0);
            // Vacuumed within the hour and the dead rows stayed: vacuum ran but couldn't remove them, because something
            // older holds the cleanup horizon (a long transaction, a replication slot, a prepared transaction).
            let held = trigger.is_some() && last.is_some_and(|d| d * 24.0 < 1.0);
            let mut f = finding("dead-rows", &table_name, Category::Maintenance, subject(SubjectKind::Table, &table_name));
            f.title = format!("{} of {} is dead rows", format::percent(ratio), t.name);
            f.summary = format!(
                "{table_name} holds {} dead rows next to {} live ones. Every sequential scan still reads them, indexes still point at them, and the table stays larger than it needs to be. {}{}",
                format::count(dead),
                format::count(live),
                match trigger {
                    Some(_) if held => "Vacuum ran within the hour and they stayed: something older holds the cleanup horizon back (a long-running transaction, a replication slot or a prepared transaction), so vacuum can't remove them yet.".to_string(),
                    Some(tr) => format!(
                        "Autovacuum should visit it at about {} dead rows; it has {:.1} times that, so it is behind.",
                        format::count(tr),
                        dead / tr.max(1.0)
                    ),
                    None => "Autovacuum is off for it, so nothing cleans them up.".into(),
                },
                match (t.last_vacuum.as_deref(), last) {
                    (Some(_), Some(days)) => format!(" It was last vacuumed {} ago.", format::duration(days * 86_400_000.0)),
                    (Some(d), None) => format!(" It was last vacuumed {}.", short_date(d)),
                    (None, _) => " It has never been vacuumed since statistics were reset.".into(),
                }
            );
            f.severity = if ratio >= 0.5 && dead >= 1_000_000.0 { Severity::Medium } else { Severity::Low };
            f.impact.score = base_score(f.severity) + ratio * 10.0;
            f.impact.label = format!("{} dead rows", format::count(dead));
            f.confidence = Confidence::High;
            f.metrics.push(Metric::new("Dead rows", dead, Unit::Rows));
            f.metrics.push(Metric::new("Live rows", live, Unit::Rows));
            f.metrics.push(Metric::new("Dead share", ratio, Unit::Ratio));
            if let Some(tr) = trigger {
                f.metrics.push(Metric::new("Autovacuum trigger", tr, Unit::Rows));
            }
            let target = qualified(Engine::Postgres, &t.schema, &t.name);
            // Sooner, never later: a table already set below 0.02 keeps its own factor.
            let factor = autovacuum_setting(snapshot, t, "autovacuum_vacuum_scale_factor").unwrap_or(0.2);
            let options = match (factor > 0.02, trigger.is_none()) {
                (true, true) => Some("autovacuum_vacuum_scale_factor = 0.02, autovacuum_enabled = true"),
                (true, false) => Some("autovacuum_vacuum_scale_factor = 0.02"),
                (false, true) => Some("autovacuum_enabled = true"),
                (false, false) => None,
            };
            f.fix = Some(if held {
                Fix {
                    summary: "Find what holds cleanup back and end it (Live shows long-running and idle-in-transaction sessions; the held-cleanup finding names the holder), then vacuum: until then vacuum can't remove these rows.".into(),
                    sql: None,
                    cost: None,
                }
            } else {
                Fix {
                    summary: "Vacuum it now, and make autovacuum visit this table sooner. If dead rows come back quickly, look for long-running transactions in Live: they stop vacuum from cleaning up.".into(),
                    sql: Some(match options {
                        Some(o) => format!("VACUUM (ANALYZE) {target};\nALTER TABLE {target} SET ({o});"),
                        None => format!("VACUUM (ANALYZE) {target};"),
                    }),
                    cost: Some("VACUUM doesn't block reads or writes; it does add I/O while it runs.".into()),
                }
            });
            let when = if held {
                "After ending the cleanup holder and running VACUUM (ANALYZE), re-scan"
            } else {
                "Re-scan after VACUUM (ANALYZE)"
            };
            f.verify = vec![format!(
                "{when}: the dead-row count should fall below the table's autovacuum trigger, the last analyze time should be newer, and changes since analyze should fall. Check again after representative writes to confirm autovacuum keeps the dead-row count near its trigger rather than letting the backlog return."
            )];
            Some(f)
        })
        .collect()
}
