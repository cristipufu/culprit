//! Index-set shape: indexes that overlap a better-used one, tables carrying many indexes under heavy writes, and tables
//! read in full over and over. Every engine where the statistics exist.

use culprit_core::{
    Capability, Category, CheckInfo, Confidence, Engine, Finding, Fix, IndexStat, Metric, QueryStat, ServerInfo,
    Severity, Snapshot, SubjectKind, Unit, format,
};

use crate::indexes::{
    backs_foreign_key, is_unused, measured_writes, redundant_indexes, same_keys, scan_groups, too_new,
};
use crate::support::*;

pub fn overlapping_index_info() -> CheckInfo {
    CheckInfo {
        id: "overlapping-index".into(),
        title: "Overlapping indexes".into(),
        category: Category::Indexes,
        engines: vec![Engine::Mssql, Engine::Postgres, Engine::Mysql],
        requires: vec![Capability::IndexUsage],
        description: "Two indexes that lead with the same column where one is barely read and the other holds all \
            its key columns: queries that use the first can seek the second and filter the rest there."
            .into(),
    }
}

/// Indexes that compete for the same leading column, as (rarely read, well used). The well-used one must hold every key
/// column of the other as a key of its own: sharing only the first column says nothing when that column isn't
/// selective (a tenant id in a multi-tenant schema leads almost every index, and one tenant can be most rows), and an
/// included column is no part of the index's search (PostgreSQL may check it only on the table's row). It must hold every
/// row the other does, so no filter, and it must not be the clustered index (InnoDB's PRIMARY too): seeking that reads
/// whole rows, which is what the narrow index spares its queries.
pub(crate) fn overlapping_pairs(snapshot: &Snapshot) -> Vec<(&IndexStat, &IndexStat)> {
    let redundant: Vec<&str> = redundant_indexes(snapshot).iter().map(|(a, _)| a.name.as_str()).collect();
    let mut out: Vec<(&IndexStat, &IndexStat)> = Vec::new();
    for a in &snapshot.indexes {
        let plain = |i: &IndexStat| {
            !i.is_primary_key && !i.is_unique && !i.is_clustered && !i.is_disabled && i.filter.is_none()
        };
        // Rarely read says nothing about an index that may have been created this week.
        if !plain(a)
            || redundant.contains(&a.name.as_str())
            || is_unused(snapshot, a)
            || too_new(snapshot, a)
            || a.columns.is_empty()
        {
            continue;
        }
        let Some(reads) = a.reads else { continue };
        let lead = a.key_names()[0].to_ascii_lowercase();
        let better = snapshot
            .indexes_on(&a.schema, &a.table)
            .filter(|b| {
                b.name != a.name
                    && !b.is_disabled
                    && !b.is_clustered
                    && b.filter == a.filter
                    && b.kind == a.kind
                    && !same_keys(a, b)
            })
            .filter(|b| b.key_names().first().is_some_and(|k| k.eq_ignore_ascii_case(&lead)))
            .filter(|b| {
                let held = b.key_names();
                a.key_names().iter().all(|k| held.iter().any(|h| h.eq_ignore_ascii_case(k)))
            })
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
                "{} ({}) and {} ({}) both lead with {} on {}, and the second has every key column of the first among its keys. {} was \
                 read {} times {}, {} {} times. Queries that use the first can seek the second on {} and filter the rest \
                 inside it, while every write to {} keeps both up to date.",
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
                lead,
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

pub(crate) const MANY_INDEXES: usize = 7;

/// Writes a day that make many secondary indexes a cost worth a finding: statements on SQL Server, rows elsewhere
/// (see [`IndexWrites`]). A statement writes at least a row, so the bar is never looser on SQL Server.
pub(crate) const MANY_WRITES_A_DAY: f64 = 50_000.0;

/// The writes to a table that maintain its secondary indexes, since its counters started, and what the count counts.
pub(crate) struct IndexWrites {
    pub(crate) total: f64,
    pub(crate) days: f64,
    /// SQL Server counts write statements on the clustered index (`user_updates`), whatever rows each touched;
    /// PostgreSQL and MySQL count rows.
    pub(crate) statements: bool,
}

impl IndexWrites {
    /// The unit, one and many: "write statement(s)" on SQL Server, "row(s) written" elsewhere.
    pub(crate) fn unit(&self) -> (&'static str, &'static str) {
        if self.statements { ("write statement", "write statements") } else { ("row written", "rows written") }
    }
}

/// [`measured_writes`] without the updates that maintain no index: PostgreSQL's HOT updates change no indexed column
/// and stay on their page, so they write no index entry at all.
pub(crate) fn index_writes(snapshot: &Snapshot, t: &culprit_core::TableStat) -> Option<IndexWrites> {
    let (total, days) = measured_writes(snapshot, &t.schema, &t.name)?;
    let hot = match snapshot.server.engine {
        Engine::Postgres => t.storage.as_ref().and_then(|s| s.hot_updates).unwrap_or(0.0).min(t.updates.unwrap_or(0.0)),
        _ => 0.0,
    };
    Some(IndexWrites { total: (total - hot).max(0.0), days, statements: snapshot.server.engine == Engine::Mssql })
}

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
        let Some(w) = index_writes(snapshot, t) else { continue };
        let (writes_total, days) = (w.total, w.days);
        // Judged per day; said per day only over 20 hours of counters.
        let writes = writes_total / days;
        if writes < MANY_WRITES_A_DAY {
            continue;
        }
        let (one, many) = w.unit();
        let measured = per_day_is_measured(days);
        let idle: Vec<&IndexStat> = secondary
            .iter()
            .copied()
            .filter(|i| is_unused(snapshot, i) || redundant.contains(&i.name.as_str()))
            .collect();
        let table_name = t.qualified();
        let mut f = finding("over-indexed", &table_name, Category::Indexes, subject(SubjectKind::Table, &table_name));
        f.title = format!(
            "{} has {} indexes and gets {}",
            t.name,
            secondary.len(),
            how_many(snapshot, writes_total, days, one, many)
        );
        let upkeep = match snapshot.server.engine {
            Engine::Mssql => format!(
                "Every insert into {table_name} updates all {} secondary indexes, and every update touching their columns \
                 updates them too: {}, each one doing that for every row it changes.",
                secondary.len(),
                how_many(snapshot, writes_total, days, one, many)
            ),
            Engine::Postgres => format!(
                "Every insert into {table_name} adds an entry to all {} secondary indexes, and so does every update that \
                 isn't HOT (HOT updates change no indexed column and stay on their page): up to {}.",
                secondary.len(),
                how_many(snapshot, writes_total * secondary.len() as f64, days, "index entry", "index entries")
            ),
            Engine::Mysql => format!(
                "Every insert into {table_name} updates all {} secondary indexes, and every update touching their columns \
                 updates them too: up to {}.",
                secondary.len(),
                how_many(snapshot, writes_total * secondary.len() as f64, days, "index change", "index changes")
            ),
        };
        f.summary = format!(
            "{upkeep} {}",
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
        let short = if w.statements { "write statements" } else { "rows written" };
        f.impact.label = if measured {
            format!("{} indexes · {} {short}/day", secondary.len(), format::count(writes))
        } else {
            format!("{} indexes · {} {short}", secondary.len(), format::count(writes_total))
        };
        f.confidence = if idle.is_empty() { Confidence::Low } else { Confidence::Medium };
        f.metrics.push(Metric::new("Secondary indexes", secondary.len() as f64, Unit::Count));
        let label = if w.statements { "Write statements" } else { "Rows written" };
        f.metrics.push(if measured {
            Metric::new(format!("{label} per day"), writes, Unit::Count)
        } else {
            Metric::new(label, writes_total, Unit::Count)
        });
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

/// Full-table reads and rows read without an index since the counters started, and the days that covers. A full read
/// is the table's rows read once: PostgreSQL's `seq_scan` counts a scan once for every process that runs it (each
/// parallel worker and the leader) and again on every rescan, and counts a scan a LIMIT stopped early as a whole one,
/// so the rows it read, divided by the table's rows, is the count to say when it is lower.
pub(crate) fn full_scans_measured(snapshot: &Snapshot, t: &culprit_core::TableStat) -> Option<(f64, f64, f64)> {
    let days = counter_days_for_rate(snapshot)?;
    let rows = t.rows.filter(|r| *r >= 50_000.0)?;
    let scanned = t.seq_rows_read?;
    let full_reads = (scanned / rows).round();
    let scans = t.seq_scans.map_or(full_reads, |n| n.min(full_reads));
    Some((scans, scanned, days))
}

/// The captured statements that read a table in full: their plan scans it, or, without a plan, they read or change
/// rows of it (SELECT, UPDATE, DELETE, MERGE) and name it. A statement that seeks the table through an index, or only
/// inserts into it, isn't behind its full reads, so its time isn't theirs.
pub(crate) fn scanning_queries<'a>(snapshot: &'a Snapshot, schema: &str, table: &str) -> Vec<&'a QueryStat> {
    let qualified = format!("{schema}.{table}").to_ascii_lowercase();
    let names_table = |object: &str| {
        let o = object.to_ascii_lowercase().replace(['[', ']', '"', '`'], "");
        o == qualified || o.ends_with(&format!(".{qualified}")) || o == table.to_ascii_lowercase()
    };
    queries_touching(snapshot, schema, table)
        .into_iter()
        .filter(|q| !is_monitoring_query(q))
        .filter(|q| match &q.plan {
            Some(plan) => plan
                .nodes()
                .iter()
                .any(|n| n.kind == culprit_core::NodeKind::Scan && n.object.as_deref().is_some_and(names_table)),
            None => {
                let verb = q.text.trim_start_matches(|c: char| c.is_whitespace() || c == '(').to_ascii_lowercase();
                ["select", "with", "update", "delete", "merge"].iter().any(|v| verb.starts_with(v))
            }
        })
        .collect()
}

pub fn scan_heavy(snapshot: &Snapshot) -> Vec<Finding> {
    let planned: Vec<(String, String)> = scan_groups(snapshot)
        .into_iter()
        .map(|g| (g.schema.to_ascii_lowercase(), g.table.to_ascii_lowercase()))
        .collect();
    let mut out = Vec::new();
    for t in &snapshot.tables {
        let Some((scans_total, read_total, days)) = full_scans_measured(snapshot, t) else { continue };
        // Judged per day; said per day only over 20 hours of counters, otherwise as counted.
        let (scans, rows_read) = (scans_total / days, read_total / days);
        let measured = per_day_is_measured(days);
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
        let scanning = scanning_queries(snapshot, &t.schema, &t.name);
        let mut f = finding("scan-heavy", &table_name, Category::Indexes, subject(SubjectKind::Table, &table_name));
        let rows_text = if measured {
            format!("{} rows a day", format::count(rows_read))
        } else {
            format!("{} rows", format::count(read_total))
        };
        f.title = format!(
            "{} is read in full {} ({rows_text})",
            t.name,
            how_many(snapshot, scans_total, days, "time", "times")
        );
        f.summary = format!(
            "{} has {} rows, and queries have read all of it {}{}: {rows_text} read without an index. {}",
            table_name,
            format::count(t.rows.unwrap_or(0.0)),
            how_many(snapshot, scans_total, days, "time", "times"),
            if measured { format!(" {}", since_phrase(snapshot)) } else { String::new() },
            if !scanning.is_empty() {
                format!(
                    "{} of the captured statements read it; explain them to see which filter has no index.",
                    scanning.len()
                )
            } else if !touching.is_empty() {
                "The captured statements that name it reach it through an index, so the full reads come from \
                 statements outside the top of the workload."
                    .to_string()
            } else {
                "None of the captured statements names it, so the scans come from statements outside the top of the workload."
                    .to_string()
            }
        );
        // Only the statements that read it in full are behind the scans, and their time is the cost.
        let impact = query_impact(snapshot, &scanning);
        match &impact {
            Some(i) if i.share >= 0.005 => {
                apply_query_impact(&mut f, i);
                let time_ms: f64 = scanning.iter().map(|q| q.total_time_ms).sum();
                f.severity = severity_for_time(snapshot, time_ms, i.share);
            }
            _ => {
                f.severity = if rows_read >= 1e9 { Severity::Medium } else { Severity::Low };
                f.impact.score = base_score(f.severity) + (rows_read.log10() - 6.0).max(0.0) * 3.0;
                f.impact.label = if measured {
                    format!("{} rows scanned/day", format::count(rows_read))
                } else {
                    format!("{} rows scanned", format::count(read_total))
                };
            }
        }
        f.confidence = if scanning.is_empty() { Confidence::Low } else { Confidence::Medium };
        f.queries = query_refs(&scanning.iter().copied().take(5).collect::<Vec<_>>());
        if measured {
            f.metrics.push(Metric::new("Full reads per day", scans, Unit::Count));
            f.metrics.push(Metric::new("Rows read without an index per day", rows_read, Unit::Rows));
        } else {
            f.metrics.push(Metric::new("Full reads", scans_total, Unit::Count));
            f.metrics.push(Metric::new("Rows read without an index", read_total, Unit::Rows));
        }
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
