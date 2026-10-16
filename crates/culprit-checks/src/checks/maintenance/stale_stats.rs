//! The `stale-stats` check: Tables whose optimizer statistics are far behind their data, which leads to bad row estimates and plans.

use super::*;

pub fn stale_stats_info() -> CheckInfo {
    CheckInfo {
        id: "stale-stats".into(),
        title: "Stale statistics".into(),
        category: Category::Maintenance,
        engines: vec![Engine::Mssql, Engine::Postgres],
        requires: vec![Capability::Statistics],
        description:
            "Tables whose optimizer statistics are far behind their data, which leads to bad row estimates and plans."
                .into(),
    }
}

check!(StaleStatsCheck: stale_stats_info, stale_stats);

pub(super) struct Stale {
    schema: String,
    table: String,
    rows: f64,
    modifications: f64,
    last_updated: Option<String>,
    names: Vec<String>,
}

pub fn stale_stats(snapshot: &Snapshot) -> Vec<Finding> {
    let mut stale = match snapshot.server.engine {
        Engine::Mssql => mssql_stale(snapshot),
        Engine::Postgres => pg_stale(snapshot),
        Engine::Mysql => Vec::new(),
        // SQLite has no change counters to age statistics by; a file without them is `sqlite::statistics`'s finding.
        Engine::Sqlite => Vec::new(),
    };
    stale.sort_by(|a, b| (b.modifications / b.rows.max(1.0)).total_cmp(&(a.modifications / a.rows.max(1.0))));
    stale
        .into_iter()
        .take(8)
        .map(|s| {
            let table_name = format!("{}.{}", s.schema, s.table);
            let ratio = s.modifications / s.rows.max(1.0);
            let hot = !queries_touching(snapshot, &s.schema, &s.table).is_empty();
            // With AUTO_UPDATE_STATISTICS on, SQL Server refreshes a statistic past its threshold the moment a query
            // compiles with it: one still stale is one no query has used since. Off (or a statistic created with
            // NORECOMPUTE, which the scan doesn't see) it stays stale for everyone.
            let unused_since =
                snapshot.server.engine == Engine::Mssql && !mssql_auto_update_off(snapshot);
            let mut f = finding("stale-stats", &table_name, Category::Maintenance, subject(SubjectKind::Table, &table_name));
            f.title = format!(
                "Statistics on {} are stale: {} rows changed ({} of the table)",
                s.table,
                format::count(s.modifications),
                format::percent(ratio.min(9.99))
            );
            f.summary = format!(
                "{} rows of {} ({} rows) changed since its statistics were last updated{}. The optimizer plans queries from those statistics, so its row estimates for this table drift, and with them join choices and memory grants.{}",
                format::count(s.modifications),
                table_name,
                format::count(s.rows),
                s.last_updated.as_deref().map(|d| format!(" ({})", short_date(d))).unwrap_or_default(),
                if hot { " Heavy queries in this workload read this table." } else { "" }
            );
            f.severity = if hot && ratio >= 0.5 && !unused_since { Severity::Medium } else { Severity::Low };
            if unused_since {
                f.caveats.push("AUTO_UPDATE_STATISTICS is on, so SQL Server updates a statistic this far behind as soon as a query compiles with it: no query has used it since it fell behind. It matters only if one starts to (or if it was created with NORECOMPUTE).".into());
            }
            f.impact.score = base_score(f.severity) + ratio.min(2.0) * 3.0;
            f.impact.label = format!("{} changed", format::count(s.modifications));
            f.confidence = Confidence::Medium;
            f.metrics.push(Metric::new("Rows changed since update", s.modifications, Unit::Rows));
            f.metrics.push(Metric::new("Rows in table", s.rows, Unit::Rows));
            if !s.names.is_empty() {
                f.metrics.push(Metric::text("Statistics", format::list(&s.names)));
            }
            let engine = snapshot.server.engine;
            let target = qualified(engine, &s.schema, &s.table);
            f.fix = Some(engine.spec().refresh_statistics(&target));
            f
        })
        .collect()
}

/// Whether the database says AUTO_UPDATE_STATISTICS is off (unknown counts as on, SQL Server's default).
pub(super) fn mssql_auto_update_off(snapshot: &Snapshot) -> bool {
    snapshot.setting("is_auto_update_stats_on").is_some_and(|s| matches!(s.value.trim(), "0" | "false" | "False"))
}

/// The changes after which SQL Server itself considers a statistic out of date: 500 + 20% of the rows, or on
/// compatibility level 130 and later the smaller √(1000 × rows). A statistic below it is as fresh as SQL Server keeps it.
pub(super) fn mssql_update_threshold(snapshot: &Snapshot, rows: f64) -> f64 {
    let classic = 500.0 + 0.2 * rows;
    let level = snapshot.setting("compatibility_level").and_then(|s| s.value.trim().parse::<u32>().ok());
    if level.is_none_or(|l| l >= 130) { classic.min((1000.0 * rows).sqrt()) } else { classic }
}

/// SQL Server statistics past SQL Server's own update threshold, and changed by a fifth of the table. A filtered
/// statistic covers only some rows (`rows` counts those) while its modification counter counts changes anywhere in
/// its column, so the table's row count is the measure.
pub(super) fn mssql_stale(snapshot: &Snapshot) -> Vec<Stale> {
    let mut out: Vec<Stale> = Vec::new();
    for s in &snapshot.statistics {
        let (Some(stat_rows), Some(mods)) = (s.rows, s.modifications) else {
            continue;
        };
        let table_rows = snapshot.table(&s.schema, &s.table).and_then(|t| t.rows).unwrap_or(0.0);
        let rows = stat_rows.max(table_rows);
        let threshold = (rows * 0.2).max(1000.0).max(mssql_update_threshold(snapshot, rows));
        if rows < 10_000.0 || mods < threshold {
            continue;
        }
        match out.iter_mut().find(|x| x.schema == s.schema && x.table == s.table) {
            Some(x) => {
                x.modifications = x.modifications.max(mods);
                x.names.push(s.name.clone());
            }
            None => out.push(Stale {
                schema: s.schema.clone(),
                table: s.table.clone(),
                rows,
                modifications: mods,
                last_updated: s.last_updated.clone(),
                names: vec![s.name.clone()],
            }),
        }
    }
    out
}

pub(super) fn pg_stale(snapshot: &Snapshot) -> Vec<Stale> {
    snapshot
        .tables
        .iter()
        .filter_map(|t| {
            let rows = t.rows?;
            let mods = t.modifications_since_analyze?;
            // Autoanalyze's own rhythm isn't staleness: past twice the table's trigger (its own scale factor, else the
            // server's), and at least a fifth of the table.
            let trigger = autovacuum_trigger(snapshot, t, rows, "analyze").map_or(0.0, |tr| tr * DEAD_MARGIN);
            if rows < 10_000.0 || mods < (rows * 0.2).max(10_000.0).max(trigger) {
                return None;
            }
            Some(Stale {
                schema: t.schema.clone(),
                table: t.name.clone(),
                rows,
                modifications: mods,
                last_updated: t.last_analyze.clone(),
                names: Vec::new(),
            })
        })
        .collect()
}
