//! The `hot-updates` check: Tables where few updates are HOT (heap-only): each other update writes a new entry in every index and leaves cleanup for VACUUM. Says whether pages are too full (fillfactor) or an indexed column changes (PostgreSQL 16+ tells them apart).

use super::*;

pub fn hot_updates_info() -> CheckInfo {
    CheckInfo {
        id: "hot-updates".into(),
        title: "Updates that rewrite every index".into(),
        category: Category::Maintenance,
        engines: vec![Engine::Postgres],
        requires: vec![],
        description: "Tables where few updates are HOT (heap-only): each other update writes a new entry in every index and leaves cleanup for VACUUM. Says whether pages are too full (fillfactor) or an indexed column changes (PostgreSQL 16+ tells them apart).".into(),
    }
}

check!(HotUpdatesCheck: hot_updates_info, hot_updates);

/// Non-HOT updates a day to one table that make hot-updates Medium.
pub(super) const COLD_UPDATES_MEDIUM_PER_DAY: f64 = 1_000_000.0;

pub fn hot_updates(snapshot: &Snapshot) -> Vec<Finding> {
    let mut findings: Vec<Finding> = snapshot
        .tables
        .iter()
        .filter_map(|t| {
            let s = t.storage.as_ref()?;
            let updates = t.updates.filter(|u| *u >= 10_000.0)?;
            let hot = s.hot_updates?;
            let ratio = hot / updates;
            // A table without indexes has no entries to rewrite: a non-HOT update there costs what a HOT one does.
            let indexes = snapshot.indexes_on(&t.schema, &t.name).count();
            (ratio < 0.5 && indexes > 0).then_some((t, s, updates, hot, ratio, indexes))
        })
        .map(|(t, s, updates, hot, ratio, indexes)| {
            let name = t.qualified();
            let mut f = finding("hot-updates", &name, Category::Maintenance, subject(SubjectKind::Table, &name));
            let cold = updates - hot;
            f.title = if hot < 1.0 {
                format!("No update to {} is HOT: each one rewrites every index", t.name)
            } else {
                format!("Only {} of updates to {} are HOT: the rest rewrite every index", format::percent(ratio), t.name)
            };
            let every_index = match indexes {
                1 => "its only index".to_string(),
                2 => "both of its indexes".to_string(),
                n => format!("all {n} of its indexes"),
            };
            let fillfactor = s.fillfactor.unwrap_or(100.0);
            let moved = s.new_page_updates.map(|n| (n / cold.max(1.0)).min(1.0));
            let cause = match moved {
                Some(m) if m >= 0.5 => format!(
                    " {} of them moved to another page because their own was full (fillfactor {fillfactor:.0}).",
                    format::percent(m)
                ),
                Some(m) => format!(
                    " Only {} of them lacked room on the page; the rest changed an indexed column, which rules HOT out.",
                    format::percent(m)
                ),
                None => " Either the pages are full (fillfactor {ff}) or the updates change an indexed column.".replace("{ff}", &format!("{fillfactor:.0}")),
            };
            let how_many = if hot < 1.0 {
                format!("None of {}'s {} updates were", t.name, format::count(updates))
            } else {
                format!("{} of {}'s {} updates weren't", format::count(cold), t.name, format::count(updates))
            };
            f.summary = format!(
                "An update that changes no indexed column and finds room on the same page is HOT: it writes no index entries and its old version is cleaned up without VACUUM. {how_many}, so each wrote a new entry in {every_index}.{cause}"
            );
            // The counts run from the database's last statistics reset, which PostgreSQL 15+ leaves empty until the first
            // one: then their age is unknown (the counters outlive restarts), and so is any rate. A one-time backfill
            // keeps the share low long after it ran, so only a measured rate raises it.
            let hours = s.counter_hours.filter(|h| *h >= culprit_core::RATE_MIN_HOURS);
            let per_day = hours.map(|h| cold / h * 24.0);
            f.severity = since_start_severity(per_day, f64::INFINITY, COLD_UPDATES_MEDIUM_PER_DAY);
            if per_day.is_none_or(|d| d < COLD_UPDATES_MEDIUM_PER_DAY) {
                f.caveats.push(match s.counter_hours {
                    Some(h) => format!(
                        "The counts cover the {} since the statistics were last reset: one large update or backfill in that time keeps the share low long after it ran. Compare how n_tup_hot_upd grows against n_tup_upd over a few days before changing the table.",
                        culprit_core::span_words(h)
                    ),
                    None => "How long these counts have run isn't known (PostgreSQL 15+ records no start until its statistics are first reset, and the counts outlive restarts): one large update or backfill long ago would keep the share low. Compare how n_tup_hot_upd grows against n_tup_upd over a few days before changing the table.".into(),
                });
            }
            severity_score(&mut f);
            f.impact.label = format!("{} HOT", format::percent(ratio));
            f.confidence = if moved.is_some() { Confidence::High } else { Confidence::Medium };
            f.metrics = vec![
                Metric::new("Updates", updates, Unit::Count),
                Metric::new("HOT", ratio, Unit::Ratio),
                Metric::new("Indexes", indexes as f64, Unit::Count),
                Metric::new("Fillfactor", fillfactor, Unit::Count),
            ];
            let room = moved.is_none_or(|m| m >= 0.5) && fillfactor >= 100.0;
            f.fix = Some(Fix {
                summary: if room {
                    "Leave room on each page so updated rows fit next to the old version: a fillfactor of 85–90 is typical for tables updated often. It applies to pages written from now on; existing pages get the room as rows move (pg_repack rewrites the table online if you want it at once). If an index covers a column that changes on every update (updated_at, status, counters), dropping it makes those updates HOT too.".into()
                } else {
                    "The updates change an indexed column. Find the index on a column that changes often (updated_at, status, counters) and drop it if reads don't need it; every update that touches it can then be HOT.".into()
                },
                sql: room.then(|| format!("ALTER TABLE {} SET (fillfactor = 85);", qualified(Engine::Postgres, &t.schema, &t.name))),
                cost: None,
            });
            f.verify = vec!["Re-scan a few days later: the HOT share counts since the last statistics reset, so compare how n_tup_hot_upd grows against n_tup_upd.".into()];
            f
        })
        .collect();
    findings.sort_by(|a, b| b.impact.score.total_cmp(&a.impact.score));
    findings.truncate(5);
    findings
}
