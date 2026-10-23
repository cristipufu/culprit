//! The `over-indexed` check: Tables with many secondary indexes that are written a lot: every insert updates each index.

use super::*;
use crate::evidence::Requirement as Evidence;

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

check!(OverIndexedCheck: over_indexed_info, over_indexed, &[Evidence::IndexCatalog, Evidence::Tables(crate::evidence::TableMeasure::Writes)]);

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
    // By table and name: MySQL and SQL Server index names are unique only within a table.
    let redundant: Vec<(&str, &str, &str)> = redundant_indexes(snapshot)
        .iter()
        .map(|(a, _)| (a.schema.as_str(), a.table.as_str(), a.name.as_str()))
        .collect();
    let mut out = Vec::new();
    // A partition carries its parent's indexes: one finding per partition would be one cause many times.
    for t in snapshot.tables.iter().filter(|t| t.partition_of.is_none()) {
        let secondary: Vec<&IndexStat> = snapshot
            .indexes_on(&t.schema, &t.name)
            .filter(|i| !i.is_primary_key && !i.is_clustered && i.maintained.unwrap_or(!i.is_disabled))
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
        let measured = counters_measure_a_day(snapshot, days);
        let idle: Vec<&IndexStat> = secondary
            .iter()
            .copied()
            .filter(|i| {
                is_unused(snapshot, i)
                    || redundant.contains(&(i.schema.as_str(), i.table.as_str(), i.name.as_str()))
                    || (i.is_disabled
                        && i.maintained == Some(true)
                        && !i.is_unique
                        && !too_new(snapshot, i)
                        && (snapshot.server.engine == Engine::Postgres || i.reads == Some(0.0)))
            })
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
            Engine::Mysql | Engine::Sqlite => format!(
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
            } else if idle.iter().all(|i| i.is_disabled) {
                "Drop the hidden or invalid indexes that writes still maintain.".into()
            } else {
                "Hide the indexes nothing reads first (reversible), then drop them after a business cycle.".into()
            },
            sql: (!idle.is_empty())
                .then(|| idle.iter().map(|i| hide_index_sql(snapshot, i)).collect::<Vec<_>>().join("\n")),
            cost: None,
        });
        f.verify =
            vec![format!("Write latency on {} drops; the reads that used the hidden indexes stay as fast.", t.name)];
        out.push(f);
    }
    out
}
