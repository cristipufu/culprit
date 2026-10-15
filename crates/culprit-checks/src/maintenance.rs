use culprit_core::{
    Capability, Category, CheckInfo, Confidence, Engine, Finding, Fix, Metric, Severity, Snapshot, SubjectKind, Unit,
    format,
};

use crate::support::*;

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

struct Stale {
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
            f.fix = Some(match engine {
                Engine::Mssql => Fix {
                    summary: "Update the statistics now; if this table changes fast, schedule it or enable AUTO_UPDATE_STATISTICS_ASYNC.".into(),
                    sql: Some(engine.dialect().refresh_statistics(&target)),
                    cost: Some("Reads a sample of the table; run it outside peak hours on very large tables.".into()),
                },
                Engine::Postgres => Fix {
                    summary: "Analyze the table now. If autovacuum falls behind on it regularly, lower its analyze threshold for this table only.".into(),
                    sql: Some(format!(
                        "{}\nALTER TABLE {target} SET (autovacuum_analyze_scale_factor = 0.02);",
                        engine.dialect().refresh_statistics(&target)
                    )),
                    cost: Some("ANALYZE reads a sample of the table and does not block reads or writes.".into()),
                },
                Engine::Sqlite => Fix {
                    summary: "Run ANALYZE, so the planner knows each table's size and how selective each index is.".into(),
                    sql: Some(engine.dialect().refresh_statistics(&target)),
                    cost: Some("ANALYZE reads the indexes and briefly takes the write lock to store what it found.".into()),
                },
                Engine::Mysql => Fix {
                    summary: "Recompute the table's statistics.".into(),
                    sql: Some(engine.dialect().refresh_statistics(&target)),
                    cost: None,
                },
            });
            f
        })
        .collect()
}

/// Whether the database says AUTO_UPDATE_STATISTICS is off (unknown counts as on, SQL Server's default).
fn mssql_auto_update_off(snapshot: &Snapshot) -> bool {
    snapshot.setting("is_auto_update_stats_on").is_some_and(|s| matches!(s.value.trim(), "0" | "false" | "False"))
}

/// The changes after which SQL Server itself considers a statistic out of date: 500 + 20% of the rows, or on
/// compatibility level 130 and later the smaller √(1000 × rows). A statistic below it is as fresh as SQL Server keeps it.
fn mssql_update_threshold(snapshot: &Snapshot, rows: f64) -> f64 {
    let classic = 500.0 + 0.2 * rows;
    let level = snapshot.setting("compatibility_level").and_then(|s| s.value.trim().parse::<u32>().ok());
    if level.is_none_or(|l| l >= 130) { classic.min((1000.0 * rows).sqrt()) } else { classic }
}

/// SQL Server statistics past SQL Server's own update threshold, and changed by a fifth of the table. A filtered
/// statistic covers only some rows (`rows` counts those) while its modification counter counts changes anywhere in
/// its column, so the table's row count is the measure.
fn mssql_stale(snapshot: &Snapshot) -> Vec<Stale> {
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

fn pg_stale(snapshot: &Snapshot) -> Vec<Stale> {
    snapshot
        .tables
        .iter()
        .filter_map(|t| {
            let rows = t.rows?;
            let mods = t.modifications_since_analyze?;
            if rows < 10_000.0 || mods < (rows * 0.2).max(10_000.0) {
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

fn short_date(iso: &str) -> String {
    iso.get(..10).unwrap_or(iso).to_string()
}

pub fn heap_table_info() -> CheckInfo {
    CheckInfo {
        id: "heap-table".into(),
        title: "Large tables without a clustered index".into(),
        category: Category::Maintenance,
        engines: vec![Engine::Mssql],
        requires: vec![Capability::TableStats],
        description:
            "Heaps grow forwarded records and can't be read in key order; most OLTP tables should be clustered.".into(),
    }
}

pub fn heap_table(snapshot: &Snapshot) -> Vec<Finding> {
    if snapshot.server.engine != Engine::Mssql {
        return Vec::new();
    }
    snapshot
        .tables
        .iter()
        .filter(|t| t.is_heap == Some(true) && t.rows.unwrap_or(0.0) >= 100_000.0)
        .map(|t| {
            let table_name = t.qualified();
            let rows = t.rows.unwrap_or(0.0);
            let mut f = finding("heap-table", &table_name, Category::Maintenance, subject(SubjectKind::Table, &table_name));
            let forwarded = t.forwarded_fetches.filter(|f| *f > 0.0);
            let days = counter_days_for_rate(snapshot);
            let forwarded_per_day = forwarded.and_then(|f| counter_rate_per_day(snapshot, f));
            // Said "a day" only over 20 hours of counters; under that, what was counted and since when.
            let said_per_day = forwarded_per_day.filter(|_| days.is_some_and(per_day_is_measured));
            f.title = match (forwarded_per_day, forwarded, days) {
                (Some(per_day), Some(total), Some(days)) if per_day >= 1000.0 => format!(
                    "Reads of {} follow forwarding pointers {}",
                    t.name,
                    how_many(snapshot, total, days, "time", "times")
                ),
                _ => format!("{} ({} rows) has no clustered index", t.name, format::count(rows)),
            };
            f.summary = format!(
                "{table_name} is stored as a heap. Updates that grow a row leave forwarding pointers behind, deleted space isn't reclaimed well, and every nonclustered index lookup has to go through a row identifier.{}{}",
                forwarded
                    .map(|total| format!(
                        " Reads have followed a forwarding pointer to another page {} times {}{}: each one is an extra random read.",
                        format::count(total),
                        since_phrase(snapshot),
                        said_per_day.map(|d| format!(" (about {} a day)", format::count(d))).unwrap_or_default()
                    ))
                    .unwrap_or_default(),
                if t.has_primary_key { " It does have a primary key, just not a clustered one." } else { " It has no primary key either." }
            );
            // What a heap costs here is the extra reads it causes, not its size: an insert-only log heap costs nothing.
            f.severity =
                if forwarded_per_day.is_some_and(|d| d >= 100_000.0) { Severity::Medium } else { Severity::Low };
            if let Some(d) = said_per_day {
                f.metrics.push(Metric::new("Forwarded reads per day", d, Unit::Count));
            }
            f.impact.score = base_score(f.severity);
            f.impact.label = format!("{} rows", format::count(rows));
            f.confidence = Confidence::Medium;
            f.metrics.push(Metric::new("Rows", rows, Unit::Rows));
            if let Some(b) = t.total_bytes {
                f.metrics.push(Metric::new("Size", b, Unit::Bytes));
            }
            f.fix = Some(Fix {
                summary: "Add a clustered index, usually on the primary key or an ever-increasing column such as an identity or creation time. Plan it as a maintenance operation: it rewrites the table.".into(),
                sql: None,
                cost: t.total_bytes.map(|b| format!("Rewrites about {} of data.", format::bytes(b))),
            });
            f.caveats.push("Staging tables that are bulk-loaded and truncated can be fine as heaps.".into());
            f
        })
        .collect()
}

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

/// Dead rows past this many times a table's autovacuum trigger mean autovacuum is behind on it, not about to run.
const DEAD_MARGIN: f64 = 2.0;

/// When autovacuum vacuums `t`: its own storage parameters, else the server's settings, else PostgreSQL's defaults
/// (50 rows + 20% of the table). `None` when autovacuum is off for it (or for the server).
fn vacuum_trigger(snapshot: &Snapshot, t: &culprit_core::TableStat, live: f64) -> Option<f64> {
    let server = |name: &str| snapshot.setting(name).and_then(|s| s.value.trim().parse::<f64>().ok());
    let own = |name: &str| t.storage.as_ref().and_then(|s| s.option(name));
    let off = t.storage.as_ref().is_some_and(|s| s.autovacuum_off())
        || snapshot.setting("autovacuum").is_some_and(|s| !matches!(s.value.trim(), "on" | "true" | "1"));
    if off {
        return None;
    }
    let threshold =
        own("autovacuum_vacuum_threshold").or_else(|| server("autovacuum_vacuum_threshold")).unwrap_or(50.0);
    let factor =
        own("autovacuum_vacuum_scale_factor").or_else(|| server("autovacuum_vacuum_scale_factor")).unwrap_or(0.2);
    Some(threshold + factor * live)
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
            let mut f = finding("dead-rows", &table_name, Category::Maintenance, subject(SubjectKind::Table, &table_name));
            f.title = format!("{} of {} is dead rows", format::percent(ratio), t.name);
            f.summary = format!(
                "{table_name} holds {} dead rows next to {} live ones. Every sequential scan still reads them, indexes still point at them, and the table stays larger than it needs to be. {}{}",
                format::count(dead),
                format::count(live),
                match trigger {
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
            f.fix = Some(Fix {
                summary: "Vacuum it now, and make autovacuum visit this table sooner. If dead rows come back quickly, look for long-running transactions in Live: they stop vacuum from cleaning up.".into(),
                sql: Some(format!(
                    "VACUUM (ANALYZE) {target};\nALTER TABLE {target} SET (autovacuum_vacuum_scale_factor = 0.02, autovacuum_enabled = true);"
                )),
                cost: Some("VACUUM doesn't block reads or writes; it does add I/O while it runs.".into()),
            });
            Some(f)
        })
        .collect()
}

pub fn xid_age_info() -> CheckInfo {
    CheckInfo {
        id: "xid-age".into(),
        title: "Transaction ID wraparound risk".into(),
        category: Category::Maintenance,
        engines: vec![Engine::Postgres],
        requires: vec![Capability::TableStats],
        description:
            "Transaction ID age approaching the point where PostgreSQL stops accepting writes to protect data.".into(),
    }
}

const WRAPAROUND: f64 = 2_000_000_000.0;

pub fn xid_age(snapshot: &Snapshot) -> Vec<Finding> {
    if snapshot.server.engine != Engine::Postgres {
        return Vec::new();
    }
    let db_age = snapshot.setting("datfrozenxid_age").and_then(|s| s.value.parse::<f64>().ok());
    let oldest = snapshot
        .tables
        .iter()
        .filter(|t| t.xid_age.is_some())
        .max_by(|a, b| a.xid_age.unwrap_or(0.0).total_cmp(&b.xid_age.unwrap_or(0.0)));
    let age = db_age.or(oldest.and_then(|t| t.xid_age)).unwrap_or(0.0);
    // Autovacuum starts freezing a table on its own once its age passes autovacuum_freeze_max_age (200M by default,
    // and often raised on big databases): an age a little past it is freezing in progress. Twice it, or 500M when the
    // setting is unknown or low, means freezing is falling behind; never later than 1.2B, with 2B the hard stop.
    let freeze_max = snapshot.setting("autovacuum_freeze_max_age").and_then(|s| s.value.trim().parse::<f64>().ok());
    let threshold = freeze_max.map_or(500_000_000.0, |m| (m * 2.0).max(500_000_000.0)).min(1_200_000_000.0);
    if age < threshold {
        return Vec::new();
    }
    let remaining = (WRAPAROUND - age).max(0.0);
    let mut f = finding(
        "xid-age",
        &snapshot.server.database,
        Category::Maintenance,
        subject(SubjectKind::Database, &snapshot.server.database),
    );
    f.title = format!("Transaction IDs are {} of the way to wraparound", format::percent(age / WRAPAROUND));
    f.summary = format!(
        "The oldest unfrozen transaction ID in this database is {} transactions old.{} At about 2 billion PostgreSQL stops accepting writes until a full vacuum completes. {} transactions remain.",
        format::count(age),
        freeze_max
            .map(|m| format!(
                " Autovacuum starts freezing a table at autovacuum_freeze_max_age ({}), and this is {:.1} times that: freezing isn't keeping up.",
                format::count(m),
                age / m.max(1.0)
            ))
            .unwrap_or_default(),
        format::count(remaining)
    );
    f.severity = if age >= 1_200_000_000.0 { Severity::High } else { Severity::Medium };
    f.impact.score = if age >= 1_500_000_000.0 {
        95.0
    } else if age >= 1_200_000_000.0 {
        70.0
    } else {
        35.0
    };
    f.impact.label = format!("{} left", format::count(remaining));
    f.confidence = Confidence::High;
    f.metrics.push(Metric::new("Transaction ID age", age, Unit::Count));
    f.metrics.push(Metric::new("Remaining before wraparound", remaining, Unit::Count));
    if let Some(t) = oldest {
        f.metrics.push(Metric::text("Oldest table", t.qualified()));
    }
    f.fix = Some(Fix {
        summary: "Freeze the oldest tables first, and look for what holds vacuum back: long-running or idle-in-transaction sessions, abandoned replication slots, or prepared transactions.".into(),
        sql: Some(match oldest {
            Some(t) => format!("VACUUM (FREEZE, VERBOSE) {};", qualified(Engine::Postgres, &t.schema, &t.name)),
            None => "VACUUM (FREEZE, VERBOSE);".into(),
        }),
        cost: Some("A freeze vacuum reads the whole table; it doesn't block reads or writes.".into()),
    });
    vec![f]
}
