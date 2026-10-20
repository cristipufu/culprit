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
            // A recent vacuum and a dead-row estimate do not prove a blocked horizon: writes may have followed it.
            let recent_vacuum = last.is_some_and(|d| d * 24.0 < 1.0);
            let holder = snapshot.pressure.as_ref().and_then(|p| p.oldest_transaction.as_ref());
            let held = trigger.is_some() && holder.is_some_and(|h| !h.catalog_only && h.xid_age.is_some_and(|age| age > 0.0));
            let mut f = finding("dead-rows", &table_name, Category::Maintenance, subject(SubjectKind::Table, &table_name));
            f.title = format!("{} of {} is dead rows", format::percent(ratio), t.name);
            f.summary = format!(
                "{table_name} holds {} dead rows next to {} live ones. Every sequential scan still reads them, indexes still point at them, and the table stays larger than it needs to be. {}{}",
                format::count(dead),
                format::count(live),
                match trigger {
                    Some(_) if held => "A cleanup holder was observed in this scan. It can prevent removal of some dead rows; this count does not establish how many it retains. Check the holder and vacuum progress before changing maintenance.".to_string(),
                    Some(_) if recent_vacuum => "Vacuum ran within the last hour. These rows may have become dead afterwards, or an older snapshot may prevent removal; the scan does not establish which. Compare write activity and the next vacuum before treating this as failed cleanup.".to_string(),
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
                    summary: "Inspect the observed cleanup holder and vacuum progress. Finish a transaction only after checking its owner and purpose; a replication consumer or prepared transaction needs its own recovery procedure. Then measure whether vacuum removes the backlog under representative writes.".into(),
                    sql: None,
                    cost: None,
                }
            } else if recent_vacuum && trigger.is_some() {
                Fix {
                    summary: "Compare representative writes and the next autovacuum with the dead-row estimate. Inspect vacuum progress and any cleanup holders if the backlog persists. Change per-table maintenance only after establishing that its current settings cannot keep up.".into(),
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
                "After safely resolving the cleanup hold and allowing VACUUM to complete, re-scan"
            } else if recent_vacuum && trigger.is_some() {
                "Compare the next completed vacuum under representative writes"
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
