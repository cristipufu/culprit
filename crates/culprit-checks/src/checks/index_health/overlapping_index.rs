//! The `overlapping-index` check: Two indexes that lead with the same column where one is barely read and the other holds all its key columns: queries that use the first can seek the second and filter the rest there.

use super::*;

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

check!(OverlappingIndexCheck: overlapping_index_info, overlapping_index);

/// Indexes that compete for the same leading column, as (rarely read, well used). The well-used one must hold every key
/// column of the other as a key of its own: sharing only the first column says nothing when that column isn't
/// selective (a tenant id in a multi-tenant schema leads almost every index, and one tenant can be most rows), and an
/// included column is no part of the index's search (PostgreSQL may check it only on the table's row). It must hold every
/// row the other does, so no filter, and it must not be the clustered index (InnoDB's PRIMARY too): seeking that reads
/// whole rows, which is what the narrow index spares its queries.
pub(crate) fn overlapping_pairs(snapshot: &Snapshot) -> Vec<(&IndexStat, &IndexStat)> {
    // "Barely read" is a claim about a week of counters, the same as "unused": under it, reads say nothing yet.
    if !crate::indexes::watched_long_enough(snapshot) {
        return Vec::new();
    }
    // By table and name: MySQL and SQL Server index names are unique only within a table.
    let redundant: Vec<(&str, &str, &str)> = redundant_indexes(snapshot)
        .iter()
        .map(|(a, _)| (a.schema.as_str(), a.table.as_str(), a.name.as_str()))
        .collect();
    let mut out: Vec<(&IndexStat, &IndexStat)> = Vec::new();
    for a in &snapshot.indexes {
        let plain = |i: &IndexStat| {
            !i.is_primary_key
                && !i.is_unique
                && !i.is_clustered
                && !i.is_disabled
                && i.filter.is_none()
                && !crate::indexes::is_partition_copy(i)
        };
        // Rarely read says nothing about an index that may have been created this week.
        if !plain(a)
            || redundant.contains(&(a.schema.as_str(), a.table.as_str(), a.name.as_str()))
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
