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
            // With AUTO_UPDATE_STATISTICS on, SQL Server refreshes a statistic past its threshold the moment a query
            // compiles with it: one still stale is one no query has used since. Off (or a statistic created with
            // NORECOMPUTE, which the scan doesn't see) it stays stale for everyone.
            let unused_since =
                snapshot.server.engine == Engine::Mssql && !mssql_auto_update_off(snapshot);
            // Heavy reads of the table are the application's reads that matter: a tiny insert, or a monitoring query
            // whose text happens to name it, plans nothing from its statistics worth a sentence. And none is said
            // where no query has compiled with the statistics since they fell behind.
            let hot = !unused_since && heavy_reads(snapshot, &s.schema, &s.table);
            let mut f = finding("stale-stats", &table_name, Category::Maintenance, subject(SubjectKind::Table, &table_name));
            f.title = format!(
                "Statistics on {} are stale: {} rows changed ({} of the table)",
                s.table,
                format::count(s.modifications),
                format::percent(ratio.min(9.99))
            );
            // A statistic no query has compiled with since it fell behind plans nothing: what happens is the next
            // compile's, which refreshes it first (and waits for that, unless AUTO_UPDATE_STATISTICS_ASYNC is on).
            let async_update = snapshot.setting("is_auto_update_stats_async_on").is_some_and(|s| crate::config::is_on(&s.value));
            let effect = if unused_since {
                format!(
                    "With AUTO_UPDATE_STATISTICS on, SQL Server refreshes a statistic this far behind when the next query that uses it compiles{}.",
                    if async_update {
                        "; that query compiles with the old statistics while they refresh in the background (AUTO_UPDATE_STATISTICS_ASYNC is on)"
                    } else {
                        ", and that query waits for the refresh"
                    }
                )
            } else {
                "The optimizer plans queries from those statistics, so its row estimates for this table drift, and with them join choices and memory grants.".to_string()
            };
            f.summary = format!(
                "{} rows of {} ({} rows) changed since its statistics were last updated{}. {effect}{}",
                format::count(s.modifications),
                table_name,
                format::count(s.rows),
                s.last_updated.as_deref().map(|d| format!(" ({})", short_date(d))).unwrap_or_default(),
                if hot { " Heavy queries in this workload read this table." } else { "" }
            );
            f.severity = if hot && ratio >= 0.5 { Severity::Medium } else { Severity::Low };
            if unused_since {
                f.caveats.push("Had a query compiled with it, SQL Server would have refreshed it: no query has used it since it fell behind. That holds unless it was created with NORECOMPUTE, which this scan doesn't see: SQL Server never refreshes those on its own.".into());
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
            let mut fix = engine.spec().refresh_statistics(&target);
            if unused_since {
                fix.summary = format!(
                    "No update is needed before a query uses these statistics: SQL Server refreshes them on that query's compile. Updating them now (below) {}.",
                    if async_update { "gives that first compile fresh statistics" } else { "spares that compile the wait" }
                );
            }
            f.fix = Some(fix);
            f.verify = vec![format!(
                "Scan again after the update{}: the count of rows changed since the statistics on {} were updated starts over from zero, and this finding is gone.",
                if unused_since { " (the fix's SQL, or SQL Server's own on a query's compile)" } else { "" },
                s.table
            )];
            f
        })
        .collect()
}

/// Whether statements that read `schema.table`, not the monitoring tools' and each a material share of database time
/// ([`crate::queries::MIN_SHARE`]), are in the workload.
fn heavy_reads(snapshot: &Snapshot, schema: &str, table: &str) -> bool {
    queries_touching(snapshot, schema, table).into_iter().any(|q| {
        !is_monitoring_query(q)
            && first_verb(main_statement(&q.text)).is_some_and(|(_, verb)| verb == "select")
            && query_impact(snapshot, &[q]).is_some_and(|i| i.share >= crate::queries::MIN_SHARE)
    })
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

#[cfg(test)]
mod tests {
    use super::*;
    use culprit_core::{Setting, StatisticsInfo};

    fn stale_products(settings: &[(&str, &str)]) -> Finding {
        let mut s = crate::waits::snapshot_with(Engine::Mssql);
        s.statistics = vec![StatisticsInfo {
            schema: "shop".into(),
            table: "products".into(),
            name: "st_products".into(),
            last_updated: Some("2026-10-01T00:00:00Z".into()),
            rows: Some(20_000.0),
            rows_sampled: Some(20_000.0),
            modifications: Some(15_000.0),
            auto_created: true,
        }];
        for (name, value) in settings {
            s.settings.retain(|x| x.name != *name);
            s.settings.push(Setting {
                name: (*name).into(),
                value: (*value).into(),
                default_value: None,
                scope: "database".into(),
            });
        }
        stale_stats(&s).into_iter().find(|f| f.id == "stale-stats:shop.products").expect("finding")
    }

    // A staging SQL Server reading: "its row estimates drift, and with them join choices and memory grants" next to "no query has
    // used it since it fell behind", a fix to enable AUTO_UPDATE_STATISTICS_ASYNC (which only lets a compile go on
    // without waiting for the update) and no way to verify.
    #[test]
    fn a_statistic_no_query_has_used_since_says_what_the_next_compile_does() {
        let f = stale_products(&[("is_auto_update_stats_on", "1"), ("is_auto_update_stats_async_on", "0")]);
        assert!(!f.summary.contains("drift"), "{}", f.summary);
        assert!(
            f.summary.ends_with(
                "With AUTO_UPDATE_STATISTICS on, SQL Server refreshes a statistic this far behind when the next query that uses it compiles, and that query waits for the refresh."
            ),
            "{}",
            f.summary
        );
        let fix = f.fix.clone().unwrap();
        assert!(!fix.summary.contains("ASYNC"), "{}", fix.summary);
        assert!(fix.summary.contains("spares that compile the wait"), "{}", fix.summary);
        assert_eq!(fix.sql.as_deref(), Some("UPDATE STATISTICS [shop].[products];"));
        assert_eq!(
            f.verify,
            vec![
                "Scan again after the update (the fix's SQL, or SQL Server's own on a query's compile): the count of rows changed since the statistics on products were updated starts over from zero, and this finding is gone."
            ]
        );
        // Asynchronous updates: the first compile goes on with the old statistics.
        let f = stale_products(&[("is_auto_update_stats_on", "1"), ("is_auto_update_stats_async_on", "1")]);
        assert!(
            f.summary.contains("compiles with the old statistics while they refresh in the background"),
            "{}",
            f.summary
        );
        // Off, every query plans from the stale statistics; the fix schedules the update.
        let f = stale_products(&[("is_auto_update_stats_on", "0")]);
        assert!(f.summary.contains("drift"), "{}", f.summary);
        assert_eq!(
            f.fix.unwrap().summary,
            "Update the statistics now; if this table changes fast, schedule the update in a maintenance job."
        );
        assert!(!f.verify.is_empty());
    }
}
