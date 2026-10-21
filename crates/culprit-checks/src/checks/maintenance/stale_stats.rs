//! The `stale-stats` check: Tables whose optimizer statistics are far behind their data, which leads to bad row estimates and plans.

use super::*;
use crate::evidence::Requirement as Evidence;

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

check!(StaleStatsCheck: stale_stats_info, stale_stats, &[Evidence::Statistics]);

/// The metric counting the changes since the statistics were updated: SQL Server's modification counter and
/// PostgreSQL's n_mod_since_analyze count changes (a row updated twice is two), not rows.
pub(crate) const CHANGES_SINCE_UPDATE: &str = "Changes since update";

pub(super) struct Stale {
    schema: String,
    table: String,
    rows: f64,
    /// The most changes counted since any of its statistics was updated, and when that one was.
    modifications: f64,
    last_updated: Option<String>,
    names: Vec<String>,
    /// Each statistic past its threshold (SQL Server): its name, its changes and when it was last updated, the most
    /// changed first. Empty where the engine counts per table (PostgreSQL).
    each: Vec<(String, f64, Option<String>)>,
}

/// Saved plans that name a statistic. Their XML describes the captured compilation, which may predate both the
/// current catalog statistics and later compilations of the same query. It does not prove current use or harm.
fn saved_plans_referencing<'a>(
    snapshot: &'a Snapshot,
    schema: &str,
    table: &str,
    names: &[String],
) -> Vec<(&'a culprit_core::QueryStat, String)> {
    app_queries(snapshot)
        .filter(|q| crate::query_stopped(snapshot, q).is_none())
        .filter_map(|q| {
            let plan = q.plan.as_ref()?;
            if only_unique_lookups(snapshot, plan, schema, table) {
                return None;
            }
            let used = plan_statistics(plan).into_iter().find(|s| {
                s.schema.eq_ignore_ascii_case(schema)
                    && s.table.eq_ignore_ascii_case(table)
                    && names.iter().any(|n| n.eq_ignore_ascii_case(&s.name))
            })?;
            Some((q, used.name))
        })
        .collect()
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
            let auto_update = snapshot.server.engine == Engine::Mssql && !mssql_auto_update_off(snapshot);
            let in_use = if auto_update { saved_plans_referencing(snapshot, &s.schema, &s.table, &s.names) } else { Vec::new() };
            let hot = !auto_update && heavy_reads(snapshot, &s.schema, &s.table);
            let mut f = finding("stale-stats", &table_name, Category::Maintenance, subject(SubjectKind::Table, &table_name));
            // The modification counter counts changes (a row updated twice is two), not rows.
            f.title = format!(
                "Statistics on {} are stale: {} changes since they were updated ({} of the table's rows)",
                s.table,
                format::count(s.modifications),
                format::percent(ratio.min(9.99))
            );
            let effect = if auto_update {
                "Automatic statistics updates are enabled or were not reported disabled. A saved plan's statistics list describes its captured compilation; it does not establish which version later compilations or current executions use. Catalog modification counts alone do not demonstrate a slow plan or justify an immediate refresh.".to_string()
            } else {
                "The optimizer uses these statistics for row estimates. Check current estimates against actual rows before choosing maintenance; modification counts alone do not measure query harm.".to_string()
            };
            // The count and the date said together are one statistic's: the most changed one, with the others' beside.
            let since = match s.each.as_slice() {
                [] | [_] => format!(
                    "since its statistics were last updated{}",
                    s.last_updated.as_deref().map(|d| format!(" ({})", short_date(d))).unwrap_or_default()
                ),
                [(name, _, at), rest @ ..] => {
                    let when = |at: &Option<String>| at.as_deref().map(|d| format!(" ({})", short_date(d))).unwrap_or_default();
                    let rest = match rest {
                        [(other, n, other_at)] => {
                            format!(", and {} since {other} was{}", format::count(*n), when(other_at))
                        }
                        _ => format!(
                            "; the {} other statistics listed below are past SQL Server's own threshold too",
                            rest.len()
                        ),
                    };
                    format!("since {name} was last updated{}{rest}", when(at))
                }
            };
            f.summary = format!(
                "{} changes to {} ({} rows) {since}. {effect}{}",
                format::count(s.modifications),
                table_name,
                format::count(s.rows),
                if hot { " Heavy queries in this workload read this table." } else { "" }
            );
            f.severity = if hot && ratio >= 0.5 { Severity::Medium } else { Severity::Low };
            if auto_update {
                f.actionability = culprit_core::Actionability::Information;
                f.title = format!("Statistics on {} have {} recorded changes", s.table, format::count(s.modifications));
                f.caveats.push("Automatic updates depend on the engine's thresholds and settings, including asynchronous updates and per-statistic NORECOMPUTE; this scan does not establish when every statistic will refresh.".into());
            }
            f.impact.score = base_score(f.severity) + ratio.min(2.0) * 3.0;
            f.impact.label = format!("{} changes", format::count(s.modifications));
            f.confidence = Confidence::Medium;
            f.queries = query_refs(&in_use.iter().map(|(q, _)| *q).collect::<Vec<_>>());
            f.metrics.push(Metric::new(CHANGES_SINCE_UPDATE, s.modifications, Unit::Count));
            f.metrics.push(Metric::new("Rows in table", s.rows, Unit::Rows));
            if !s.names.is_empty() {
                f.metrics.push(Metric::text("Statistics", format::list(&s.names)));
            }
            let engine = snapshot.server.engine;
            let target = qualified(engine, &s.schema, &s.table);
            let mut fix = engine.spec().refresh_statistics(&target);
            if auto_update {
                fix = Fix {
                    summary: "Inspect current statistics metadata and a newly compiled plan under representative load if this query is slow. Refresh only when that evidence supports it; the historical plan reference is not a request to update statistics now.".into(),
                    sql: None,
                    cost: None,
                };
            }
            f.fix = Some(fix);
            f.verify = vec![if auto_update {
                "Compare the current catalog update time and modification counter with a newly compiled plan's estimates and measured rows. An older saved XML reference may remain after maintenance, and does not prove that the change failed.".into()
            } else {
                format!("After maintenance and representative workload, compare the changes-since-update counter on {} and fresh plan estimates with actual rows; check measured query time as well.", s.table)
            }];
            f
        })
        .collect()
}

/// A full equality on a unique key finds at most one row independently of its statistics. The stored plan's list of
/// statistics alone is no reason to refresh them for that access path.
fn only_unique_lookups(snapshot: &Snapshot, plan: &culprit_core::Plan, schema: &str, table: &str) -> bool {
    let nodes: Vec<_> = plan
        .nodes()
        .into_iter()
        .filter(|n| n.object.as_deref().is_some_and(|o| names_table(o, schema, table)))
        .collect();
    !nodes.is_empty()
        && nodes.iter().all(|n| {
            if n.kind != culprit_core::NodeKind::Seek {
                return false;
            }
            let Some(index) = n.index.as_deref() else { return false };
            let name = index.rsplit('.').next().unwrap_or(index).trim_matches(['[', ']', '"']);
            let equality = predicate_columns(n.seek_predicate.as_deref().unwrap_or_default()).equality;
            snapshot.indexes_on(schema, table).any(|i| {
                i.is_unique
                    && i.name.eq_ignore_ascii_case(name)
                    && !i.columns.is_empty()
                    && i.key_names().iter().all(|c| equality.iter().any(|e| e.eq_ignore_ascii_case(c)))
            })
        })
}

/// Whether statements that read `schema.table`, not the monitoring tools' and each a material share of database time
/// ([`crate::queries::MIN_SHARE`]), are in the workload.
fn heavy_reads(snapshot: &Snapshot, schema: &str, table: &str) -> bool {
    queries_touching(snapshot, schema, table).into_iter().any(|q| {
        !is_monitoring_query(q)
            && first_verb(main_statement(&q.text)).is_some_and(|(_, verb)| verb == "select")
            && query_impact(snapshot, &[q]).is_some_and(|i| {
                i.share >= crate::queries::MIN_SHARE && amount_severity_of(snapshot, &[q], i.share) >= Severity::Medium
            })
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
                x.names.push(s.name.clone());
                x.each.push((s.name.clone(), mods, s.last_updated.clone()));
            }
            None => out.push(Stale {
                schema: s.schema.clone(),
                table: s.table.clone(),
                rows,
                modifications: mods,
                last_updated: s.last_updated.clone(),
                names: vec![s.name.clone()],
                each: vec![(s.name.clone(), mods, s.last_updated.clone())],
            }),
        }
    }
    // The count a finding leads with is the most changed statistic's, and so is the date beside it.
    for x in &mut out {
        x.each.sort_by(|a, b| b.1.total_cmp(&a.1));
        if let Some((_, mods, at)) = x.each.first() {
            (x.modifications, x.last_updated) = (*mods, at.clone());
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
                each: Vec::new(),
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
    fn auto_updates_and_saved_plans_do_not_request_an_immediate_refresh() {
        for asynchronous in ["0", "1"] {
            let f =
                stale_products(&[("is_auto_update_stats_on", "1"), ("is_auto_update_stats_async_on", asynchronous)]);
            assert_eq!(f.actionability, culprit_core::Actionability::Information);
            assert!(f.summary.contains("captured compilation"));
            assert!(f.fix.unwrap().sql.is_none());
            assert!(f.verify[0].contains("newly compiled plan"));
        }
        let f = stale_products(&[("is_auto_update_stats_on", "0")]);
        assert!(f.is_actionable());
        assert!(f.fix.unwrap().sql.is_some());
        assert!(f.summary.contains("Check current estimates against actual rows"));
    }

    // A staging SQL Server reading: "no query has used it since it fell behind" for a statistic a plan that ran 6.3K
    // times in the window lists in its StatisticsInfo. A cached plan reused doesn't compile, so it runs on with the
    // statistic as it was; "No update is needed" was wrong for it.
    #[test]
    fn a_saved_statistics_reference_does_not_prove_current_use() {
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
        let q =
            s.workload.as_mut().unwrap().queries.iter_mut().find(|q| q.id == "6").expect("the demo's read of products");
        q.plan.as_mut().unwrap().root.seek_predicate = Some("[id] > [@P1]".into());
        q.plan.as_mut().expect("a plan").raw = Some(
            r#"<ShowPlanXML><StatisticsInfo LastUpdate="2026-10-01T00:00:00.00" ModificationCount="10" SamplingPercent="100" Statistics="[st_products]" Table="[products]" Schema="[shop]" Database="[culprit_demo]" /></ShowPlanXML>"#.into(),
        );
        let f = stale_stats(&s).into_iter().find(|f| f.id == "stale-stats:shop.products").expect("finding");
        assert!(f.summary.contains("captured compilation"), "{}", f.summary);
        assert!(f.queries.iter().any(|r| r.id == "6"));
        assert_eq!(f.actionability, culprit_core::Actionability::Information);
        assert!(f.fix.unwrap().sql.is_none());
        // A unique equality lookup's cardinality doesn't depend on the statistic named in its stored XML.
        let q = s.workload.as_mut().unwrap().queries.iter_mut().find(|q| q.id == "6").unwrap();
        let plan = q.plan.as_mut().unwrap();
        plan.root.kind = culprit_core::NodeKind::Seek;
        plan.root.object = Some("shop.products".into());
        plan.root.index = Some("[pk_products]".into());
        plan.root.seek_predicate = Some("[shop].[products].[id]=[@P1]".into());
        plan.root.children.clear();
        let index = s.indexes.iter_mut().find(|i| i.schema == "shop" && i.table == "products").unwrap();
        index.name = "pk_products".into();
        index.columns = vec!["id".into()];
        index.is_unique = true;
        let f = stale_stats(&s).into_iter().find(|f| f.id == "stale-stats:shop.products").unwrap();
        assert!(f.queries.is_empty());
        assert!(f.fix.unwrap().sql.is_none());
    }

    // A staging SQL Server reading: "26K rows changed (2026-09-08)" for two statistics, the count and the date of one of
    // them, and the other updated on Oct 13 with 3.9K changes.
    #[test]
    fn the_count_and_the_date_are_one_statistics() {
        let mut s = crate::waits::snapshot_with(Engine::Mssql);
        let stat = |name: &str, at: &str, mods: f64| StatisticsInfo {
            schema: "shop".into(),
            table: "products".into(),
            name: name.into(),
            last_updated: Some(at.into()),
            rows: Some(20_000.0),
            rows_sampled: Some(20_000.0),
            modifications: Some(mods),
            auto_created: true,
        };
        s.statistics =
            vec![stat("st_a", "2026-10-13T09:00:00Z", 8_000.0), stat("st_b", "2026-09-08T02:00:00Z", 26_000.0)];
        let f = stale_stats(&s).into_iter().find(|f| f.id == "stale-stats:shop.products").expect("finding");
        assert!(
            f.summary.starts_with("26K changes to shop.products (20K rows) since st_b was last updated (2026-09-08), and 8.0K since st_a was (2026-10-13)."),
            "{}",
            f.summary
        );
        assert!(f.title.contains("26K recorded changes"), "{}", f.title);
        assert_eq!(f.impact.label, "26K changes");
    }
}
