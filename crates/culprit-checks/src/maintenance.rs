use culprit_core::{
    Capability, Category, CheckInfo, Confidence, Engine, Finding, Fix, Metric, Severity, Snapshot, StatisticsInfo,
    SubjectKind, Unit, format,
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
        Engine::Mssql => mssql_stale(&snapshot.statistics),
        Engine::Postgres => pg_stale(snapshot),
    };
    stale.sort_by(|a, b| (b.modifications / b.rows.max(1.0)).total_cmp(&(a.modifications / a.rows.max(1.0))));
    stale
        .into_iter()
        .take(8)
        .map(|s| {
            let table_name = format!("{}.{}", s.schema, s.table);
            let ratio = s.modifications / s.rows.max(1.0);
            let hot = !queries_touching(snapshot, &s.schema, &s.table).is_empty();
            let mut f = finding("stale-stats", &table_name, Category::Maintenance, subject(SubjectKind::Table, &table_name));
            f.title = format!("Statistics on {} are {} behind its data", s.table, format::percent(ratio.min(9.99)));
            f.summary = format!(
                "{} rows of {} ({} rows) changed since its statistics were last updated{}. The optimizer plans queries from those statistics, so its row estimates for this table drift, and with them join choices and memory grants.{}",
                format::count(s.modifications),
                table_name,
                format::count(s.rows),
                s.last_updated.as_deref().map(|d| format!(" ({})", short_date(d))).unwrap_or_default(),
                if hot { " Heavy queries in this workload read this table." } else { "" }
            );
            f.severity = if hot && ratio >= 0.5 { Severity::Medium } else { Severity::Low };
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
                    sql: Some(format!("UPDATE STATISTICS {target};")),
                    cost: Some("Reads a sample of the table; run it outside peak hours on very large tables.".into()),
                },
                Engine::Postgres => Fix {
                    summary: "Analyze the table now. If autovacuum falls behind on it regularly, lower its analyze threshold for this table only.".into(),
                    sql: Some(format!(
                        "ANALYZE {target};\nALTER TABLE {target} SET (autovacuum_analyze_scale_factor = 0.02);"
                    )),
                    cost: Some("ANALYZE reads a sample of the table and does not block reads or writes.".into()),
                },
            });
            f
        })
        .collect()
}

fn mssql_stale(stats: &[StatisticsInfo]) -> Vec<Stale> {
    let mut out: Vec<Stale> = Vec::new();
    for s in stats {
        let (Some(rows), Some(mods)) = (s.rows, s.modifications) else {
            continue;
        };
        let threshold = (rows * 0.2).max(1000.0);
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
            f.title = format!("{} ({} rows) has no clustered index", t.name, format::count(rows));
            f.summary = format!(
                "{table_name} is stored as a heap. Updates that grow a row leave forwarding pointers behind, deleted space isn't reclaimed well, and every nonclustered index lookup has to go through a row identifier.{}",
                if t.has_primary_key { " It does have a primary key, just not a clustered one." } else { " It has no primary key either." }
            );
            f.severity = if rows >= 1_000_000.0 { Severity::Medium } else { Severity::Low };
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
            if dead < 10_000.0 || dead < live * 0.2 {
                return None;
            }
            let table_name = t.qualified();
            let ratio = dead / (live + dead).max(1.0);
            let mut f = finding("dead-rows", &table_name, Category::Maintenance, subject(SubjectKind::Table, &table_name));
            f.title = format!("{} of {} is dead rows", format::percent(ratio), t.name);
            f.summary = format!(
                "{table_name} holds {} dead rows next to {} live ones. Every sequential scan still reads them, indexes still point at them, and the table stays larger than it needs to be.{}",
                format::count(dead),
                format::count(live),
                t.last_vacuum
                    .as_deref()
                    .map(|d| format!(" It was last vacuumed {}.", short_date(d)))
                    .unwrap_or_else(|| " It has never been vacuumed since statistics were reset.".into())
            );
            f.severity = if ratio >= 0.5 && dead >= 1_000_000.0 { Severity::Medium } else { Severity::Low };
            f.impact.score = base_score(f.severity) + ratio * 10.0;
            f.impact.label = format!("{} dead rows", format::count(dead));
            f.confidence = Confidence::High;
            f.metrics.push(Metric::new("Dead rows", dead, Unit::Rows));
            f.metrics.push(Metric::new("Live rows", live, Unit::Rows));
            f.metrics.push(Metric::new("Dead share", ratio, Unit::Ratio));
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
        description: "Transaction ID age approaching the point where Postgres stops accepting writes to protect data."
            .into(),
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
    if age < 500_000_000.0 {
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
        "The oldest unfrozen transaction ID in this database is {} transactions old. At about 2 billion Postgres stops accepting writes until a full vacuum completes. {} transactions remain.",
        format::count(age),
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
