//! Index-set shape: indexes that overlap a better-used one, tables carrying many indexes under heavy writes, and tables
//! read in full over and over. Every engine where the statistics exist.

use culprit_core::{
    Capability, Category, CheckInfo, Confidence, Engine, Finding, Fix, IndexStat, Metric, QueryStat, ServerInfo,
    Severity, Snapshot, SubjectKind, Unit, format,
};

use crate::indexes::{backs_foreign_key, is_unused, redundant_indexes, same_keys, scan_groups, writes_per_day};
use crate::support::*;

pub fn overlapping_index_info() -> CheckInfo {
    CheckInfo {
        id: "overlapping-index".into(),
        title: "Overlapping indexes".into(),
        category: Category::Indexes,
        engines: vec![Engine::Mssql, Engine::Postgres, Engine::Mysql],
        requires: vec![Capability::IndexUsage],
        description: "Two indexes that lead with the same column where one is barely read: queries that use it can \
            almost always use the other."
            .into(),
    }
}

/// Indexes that compete for the same leading column, as (rarely read, well used).
pub(crate) fn overlapping_pairs(snapshot: &Snapshot) -> Vec<(&IndexStat, &IndexStat)> {
    let redundant: Vec<&str> = redundant_indexes(snapshot).iter().map(|(a, _)| a.name.as_str()).collect();
    let mut out: Vec<(&IndexStat, &IndexStat)> = Vec::new();
    for a in &snapshot.indexes {
        let plain = |i: &IndexStat| {
            !i.is_primary_key && !i.is_unique && !i.is_clustered && !i.is_disabled && i.filter.is_none()
        };
        if !plain(a) || redundant.contains(&a.name.as_str()) || is_unused(a) || a.columns.is_empty() {
            continue;
        }
        let Some(reads) = a.reads else { continue };
        let lead = a.key_names()[0].to_ascii_lowercase();
        let better = snapshot
            .indexes_on(&a.schema, &a.table)
            .filter(|b| b.name != a.name && !b.is_disabled && b.kind == a.kind && !same_keys(a, b))
            .filter(|b| b.key_names().first().is_some_and(|k| k.eq_ignore_ascii_case(&lead)))
            .filter(|b| b.reads.is_some_and(|r| r >= 1000.0 && reads * 100.0 <= r))
            .max_by(|x, y| x.reads.unwrap_or(0.0).total_cmp(&y.reads.unwrap_or(0.0)));
        if let Some(b) = better
            && !out.iter().any(|(x, _)| x.name == a.name)
        {
            out.push((a, b));
        }
    }
    out
}

pub fn overlapping_index(snapshot: &Snapshot) -> Vec<Finding> {
    overlapping_pairs(snapshot)
        .into_iter()
        .map(|(a, b)| {
            let table_name = format!("{}.{}", a.schema, a.table);
            let mut f = finding(
                "overlapping-index",
                &format!("{}.{}", table_name, a.name),
                Category::Indexes,
                subject(SubjectKind::Index, format!("{}.{}", table_name, a.name)),
            );
            let lead = a.key_names()[0].to_string();
            let (ra, rb) = (a.reads.unwrap_or(0.0), b.reads.unwrap_or(0.0));
            f.title = format!("{} is barely read next to {}, which also starts with {}", a.name, b.name, lead);
            f.summary = format!(
                "{} ({}) and {} ({}) both lead with {} on {}. {} was read {} times {}, {} {} times. Queries that use the \
                 first can almost always seek on the second, while every write to {} keeps both up to date.",
                a.name,
                a.columns.join(", "),
                b.name,
                b.columns.join(", "),
                lead,
                table_name,
                a.name,
                format::count(ra),
                since_phrase(snapshot),
                b.name,
                format::count(rb),
                table_name
            );
            let size = a.size_bytes.unwrap_or(0.0);
            f.severity = Severity::Low;
            f.impact.score = base_score(f.severity) + (size / (1024.0 * 1024.0 * 1024.0)).min(6.0);
            f.impact.label = format!("{} barely used", format::bytes(size));
            f.confidence = Confidence::Low;
            f.metrics.push(Metric::new(format!("Reads of {}", a.name), ra, Unit::Count));
            f.metrics.push(Metric::new(format!("Reads of {}", b.name), rb, Unit::Count));
            f.metrics.push(Metric::new("Size", size, Unit::Bytes));
            f.fix = Some(Fix {
                summary: format!(
                    "Find the queries that read {} and check their plans against {}; if they seek it just as well, \
                     hide {} first and drop it after a business cycle.",
                    a.name, b.name, a.name
                ),
                sql: Some(hide_index_sql(&snapshot.server, a)),
                cost: Some(format!("Frees {} and one index's worth of work on every write.", format::bytes(size))),
            });
            f.verify = vec![format!(
                "After hiding {}, the queries on {} keep their speed and {}'s reads grow by about {}.",
                a.name,
                a.table,
                b.name,
                format::count(ra)
            )];
            f.caveats.push(format!(
                "{} may cover columns {} lacks; a query that only reads it for those columns would need a key lookup instead.",
                a.name, b.name
            ));
            if let Some(fk) = backs_foreign_key(snapshot, a) {
                f.caveats.push(format!("{} also supports the foreign key {}.", a.name, fk.name));
            }
            f
        })
        .collect()
}

/// MariaDB 10.6+ can tell the optimizer to ignore an index (its INVISIBLE).
pub(crate) fn mariadb_can_ignore(server: &ServerInfo) -> bool {
    let mut parts = server.version.split(['.', '-']).map(|p| p.parse::<u32>().unwrap_or(0));
    let (major, minor) = (parts.next().unwrap_or(0), parts.next().unwrap_or(0));
    (major, minor) >= (10, 6)
}

/// Reversible first step: SQL Server disables, MySQL makes invisible, MariaDB ignores (10.6+), Postgres can only drop
/// (keep the definition).
pub(crate) fn hide_index_sql(server: &ServerInfo, i: &IndexStat) -> String {
    let engine = server.engine;
    if server.name() == "MariaDB" {
        return match mariadb_can_ignore(server) {
            true => format!(
                "ALTER TABLE {} ALTER INDEX {} IGNORED;",
                qualified(engine, &i.schema, &i.table),
                quote(engine, &i.name)
            ),
            false => drop_index_sql(engine, &i.schema, &i.table, &i.name),
        };
    }
    match engine {
        Engine::Mssql => {
            format!("ALTER INDEX {} ON {} DISABLE;", quote(engine, &i.name), qualified(engine, &i.schema, &i.table))
        }
        Engine::Postgres => drop_index_sql(engine, &i.schema, &i.table, &i.name),
        // Invisible indexes arrived in MySQL 8.0; 5.7 can only drop (keep the definition).
        Engine::Mysql if !culprit_core::version_at_least(&server.version, [8, 0, 0]) => {
            drop_index_sql(engine, &i.schema, &i.table, &i.name)
        }
        Engine::Mysql => format!(
            "ALTER TABLE {} ALTER INDEX {} INVISIBLE;",
            qualified(engine, &i.schema, &i.table),
            quote(engine, &i.name)
        ),
    }
}

pub fn over_indexed_info() -> CheckInfo {
    CheckInfo {
        id: "over-indexed".into(),
        title: "Many indexes on write-heavy tables".into(),
        category: Category::Indexes,
        engines: vec![Engine::Mssql, Engine::Postgres, Engine::Mysql],
        requires: vec![Capability::TableStats],
        description: "Tables with many secondary indexes that are written a lot: every insert updates each index."
            .into(),
    }
}

const MANY_INDEXES: usize = 7;

pub fn over_indexed(snapshot: &Snapshot) -> Vec<Finding> {
    let redundant: Vec<&str> = redundant_indexes(snapshot).iter().map(|(a, _)| a.name.as_str()).collect();
    let mut out = Vec::new();
    for t in &snapshot.tables {
        let secondary: Vec<&IndexStat> = snapshot
            .indexes_on(&t.schema, &t.name)
            .filter(|i| !i.is_primary_key && !i.is_clustered && !i.is_disabled)
            .collect();
        if secondary.len() < MANY_INDEXES {
            continue;
        }
        let Some(writes) = writes_per_day(snapshot, &t.schema, &t.name) else { continue };
        if writes < 50_000.0 {
            continue;
        }
        let idle: Vec<&IndexStat> =
            secondary.iter().copied().filter(|i| is_unused(i) || redundant.contains(&i.name.as_str())).collect();
        let table_name = t.qualified();
        let mut f = finding("over-indexed", &table_name, Category::Indexes, subject(SubjectKind::Table, &table_name));
        f.title =
            format!("{} has {} indexes and gets about {} writes a day", t.name, secondary.len(), format::count(writes));
        f.summary = format!(
            "Every insert into {} updates all {} secondary indexes, and every update touching their columns updates them \
             too: about {} index changes a day. {}",
            table_name,
            secondary.len(),
            format::count(writes * secondary.len() as f64),
            match idle.len() {
                0 => "All of them are read, so trim only after checking which queries rely on each.".to_string(),
                1 => format!("{} of them is never read or duplicated by another; start there.", idle[0].name),
                n => format!(
                    "{n} of them ({}) are never read or duplicated by another; start there.",
                    format::list(&idle.iter().map(|i| i.name.clone()).collect::<Vec<_>>())
                ),
            }
        );
        f.severity = if idle.is_empty() { Severity::Low } else { Severity::Medium };
        f.impact.score = base_score(f.severity) + (writes.log10() - 4.0).max(0.0) * 3.0;
        f.impact.label = format!("{} indexes · {} writes/day", secondary.len(), format::count(writes));
        f.confidence = if idle.is_empty() { Confidence::Low } else { Confidence::Medium };
        f.metrics.push(Metric::new("Secondary indexes", secondary.len() as f64, Unit::Count));
        f.metrics.push(Metric::new("Writes per day", writes, Unit::Count));
        f.metrics.push(Metric::new("Never read or duplicated", idle.len() as f64, Unit::Count));
        f.fix = Some(Fix {
            summary: if idle.is_empty() {
                "Review which queries need each index; fold indexes that share a leading column into one.".into()
            } else {
                "Hide the indexes nothing reads first (reversible), then drop them after a business cycle.".into()
            },
            sql: (!idle.is_empty())
                .then(|| idle.iter().map(|i| hide_index_sql(&snapshot.server, i)).collect::<Vec<_>>().join("\n")),
            cost: None,
        });
        f.verify =
            vec![format!("Write latency on {} drops; the reads that used the hidden indexes stay as fast.", t.name)];
        out.push(f);
    }
    out
}

pub fn scan_heavy_info() -> CheckInfo {
    CheckInfo {
        id: "scan-heavy".into(),
        title: "Tables read in full over and over".into(),
        category: Category::Indexes,
        engines: vec![Engine::Postgres, Engine::Mysql],
        requires: vec![Capability::TableStats],
        description: "Large tables that queries read in full many times a day, from the table scan counters.".into(),
    }
}

/// Full-table reads per day and rows read without an index per day, from Postgres' seq_scan/seq_tup_read or MySQL's
/// rows read with no index.
pub(crate) fn full_scans(snapshot: &Snapshot, t: &culprit_core::TableStat) -> Option<(f64, f64)> {
    let days = counter_days(snapshot).filter(|d| *d >= 0.02)?;
    let rows = t.rows.filter(|r| *r >= 50_000.0)?;
    let scanned = t.seq_rows_read?;
    let scans = t.seq_scans.unwrap_or(scanned / rows);
    Some((scans / days, scanned / days))
}

pub fn scan_heavy(snapshot: &Snapshot) -> Vec<Finding> {
    let planned: Vec<(String, String)> = scan_groups(snapshot)
        .into_iter()
        .map(|g| (g.schema.to_ascii_lowercase(), g.table.to_ascii_lowercase()))
        .collect();
    let mut out = Vec::new();
    for t in &snapshot.tables {
        let Some((scans, rows_read)) = full_scans(snapshot, t) else { continue };
        if scans < 50.0 || rows_read < 5_000_000.0 {
            continue;
        }
        // A captured plan already names the filter and the index to add: the filtered-scan finding says it better.
        if planned.contains(&(t.schema.to_ascii_lowercase(), t.name.to_ascii_lowercase())) {
            continue;
        }
        let table_name = t.qualified();
        let touching: Vec<&QueryStat> =
            queries_touching(snapshot, &t.schema, &t.name).into_iter().filter(|q| !is_monitoring_query(q)).collect();
        let mut f = finding("scan-heavy", &table_name, Category::Indexes, subject(SubjectKind::Table, &table_name));
        f.title = format!(
            "{} is read in full about {} times a day ({} rows)",
            t.name,
            format::count(scans),
            format::count(rows_read)
        );
        f.summary = format!(
            "{} has {} rows, and queries have read all of it about {} times a day {}: {} rows a day read without an \
             index. {}",
            table_name,
            format::count(t.rows.unwrap_or(0.0)),
            format::count(scans),
            since_phrase(snapshot),
            format::count(rows_read),
            if touching.is_empty() {
                "None of the captured statements names it, so the scans come from statements outside the top of the workload."
                    .to_string()
            } else {
                format!(
                    "{} of the captured statements read it; explain them to see which filter has no index.",
                    touching.len()
                )
            }
        );
        let impact = query_impact(snapshot, &touching);
        match &impact {
            Some(i) if i.share >= 0.005 => {
                apply_query_impact(&mut f, i);
                f.severity = severity_for_share(i.share).max(Severity::Medium);
            }
            _ => {
                f.severity = if rows_read >= 1e9 { Severity::Medium } else { Severity::Low };
                f.impact.score = base_score(f.severity) + (rows_read.log10() - 6.0).max(0.0) * 3.0;
                f.impact.label = format!("{} rows scanned/day", format::count(rows_read));
            }
        }
        f.confidence = if touching.is_empty() { Confidence::Low } else { Confidence::Medium };
        f.queries = query_refs(&touching.iter().copied().take(5).collect::<Vec<_>>());
        f.metrics.push(Metric::new("Full reads per day", scans, Unit::Count));
        f.metrics.push(Metric::new("Rows read without an index per day", rows_read, Unit::Rows));
        if let Some(rows) = t.rows {
            f.metrics.push(Metric::new("Rows in table", rows, Unit::Rows));
        }
        f.fix = Some(Fix {
            summary: match snapshot.server.engine {
                Engine::Postgres => "Explain the statements below (Culprit can show a generic plan) and index the \
                    columns in their WHERE clause; small lookup tables are fine to scan."
                    .into(),
                _ => "Explain the statements below and index the columns in their WHERE clause; small lookup tables \
                    are fine to scan."
                    .into(),
            },
            sql: None,
            cost: crate::indexes::write_cost(snapshot, &t.schema, &t.name),
        });
        f.verify =
            vec![format!("Re-scan after a day: full reads of {} per day drop, and its index scans rise.", t.name)];
        f.caveats.push(
            "Counters accumulate since statistics were last reset; one nightly report can make up much of them.".into(),
        );
        out.push(f);
    }
    out
}
