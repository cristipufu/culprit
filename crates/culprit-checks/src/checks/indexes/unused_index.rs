//! The `unused-index` check: Indexes that are maintained on every write but no query has read since usage counters were reset. One that may be less than a week old (a scan saw it appear, or its table changed before Culprit's first scan) isn't called unused yet.

use super::*;

pub fn unused_index_info() -> CheckInfo {
    CheckInfo {
        id: "unused-index".into(),
        title: "Unused indexes".into(),
        category: Category::Indexes,
        engines: vec![Engine::Mssql, Engine::Postgres, Engine::Mysql],
        requires: vec![Capability::IndexUsage],
        description:
            "Indexes that are maintained on every write but no query has read since usage counters were reset. \
            One that may be less than a week old (a scan saw it appear, or its table changed before Culprit's first \
            scan) isn't called unused yet."
                .into(),
    }
}

check!(UnusedIndexCheck: unused_index_info, unused_index);

pub fn unused_index(snapshot: &Snapshot) -> Vec<Finding> {
    let engine = snapshot.server.engine;
    let days = counter_days(snapshot);
    let mut by_table: Vec<(String, String, Vec<&IndexStat>)> = Vec::new();
    // By table and name: MySQL and SQL Server index names are unique only within a table.
    let redundant: Vec<(&str, &str, &str)> = redundant_indexes(snapshot)
        .iter()
        .map(|(a, _)| (a.schema.as_str(), a.table.as_str(), a.name.as_str()))
        .collect();
    let mysql = snapshot.server.engine == Engine::Mysql;
    // InnoDB refuses to drop the index a foreign key relies on.
    for i in snapshot
        .indexes
        .iter()
        .filter(|i| {
            is_unused(snapshot, i) && !redundant.contains(&(i.schema.as_str(), i.table.as_str(), i.name.as_str()))
        })
        .filter(|i| !(mysql && backs_foreign_key(snapshot, i).is_some()))
    {
        match by_table.iter_mut().find(|(s, t, _)| *s == i.schema && *t == i.table) {
            Some((_, _, list)) => list.push(i),
            None => by_table.push((i.schema.clone(), i.table.clone(), vec![i])),
        }
    }
    by_table
        .into_iter()
        .map(|(schema, table, list)| {
            let table_name = format!("{schema}.{table}");
            let total_size: f64 = list.iter().filter_map(|i| i.size_bytes).sum();
            let writes = table_writes(snapshot, &schema, &table, &list);
            // SQL Server counts the writes that maintained each index (user_updates), not the table's: the number is
            // the index's own. MySQL's per-index count is the table's writes, and PostgreSQL keeps only the table's.
            let index_counted = engine == Engine::Mssql && list.iter().any(|i| i.writes.is_some());
            // An index that may be younger than the counters has gone unread only since it may have been created.
            let youngest = list
                .iter()
                .filter_map(|i| Some((index_age_days(snapshot, i)?, i.created_at_latest.as_deref()?)))
                .filter(|(age, _)| days.is_none_or(|d| *age < d))
                .min_by(|a, b| a.0.total_cmp(&b.0));
            let days = youngest.map(|(age, _)| age).or(days);
            let since = match youngest {
                Some((age, created)) => format!(
                    "in the {} days since {}, before which {} may not have existed",
                    age.round(),
                    format::friendly_date(created, &snapshot.taken_at),
                    if list.len() == 1 { "it" } else { "some of them" }
                ),
                None => since_phrase(snapshot),
            };
            let mut f = finding("unused-index", &table_name, Category::Indexes, subject(SubjectKind::Table, &table_name));
            f.title = match list.as_slice() {
                [one] => format!("{} on {} is never read", one.name, table),
                many => format!("{} indexes on {} are never read", many.len(), table),
            };
            let names = format::list(&list.iter().map(|i| i.name.clone()).collect::<Vec<_>>());
            let written = |w: f64| match (index_counted, list.len()) {
                (true, 1) => format!(", while writes had to update it {} times", format::count(w)),
                (true, _) => format!(", while writes had to update the busiest of them {} times", format::count(w)),
                (false, _) => format!(", while {} was written {} times", table_name, format::count(w)),
            };
            f.summary = match writes {
                // Nothing written either: the cost is the space, not the upkeep.
                Some(0.0) => format!(
                    "No query has read {} {}, and {} either. {} {} of storage and buffer memory.",
                    names,
                    since,
                    match (index_counted, list.len()) {
                        (true, 1) => "no write has had to update it".to_string(),
                        (true, _) => "no write has had to update them".to_string(),
                        (false, _) => format!("{table_name} hasn't been written to"),
                    },
                    if list.len() == 1 { "It takes" } else { "Together they take" },
                    format::bytes(total_size)
                ),
                _ => format!(
                    "No query has read {} {}{}. {} still has to be updated on every insert, and on updates that touch its columns, and it takes {} of storage and buffer memory.",
                    names,
                    since,
                    writes.map(written).unwrap_or_default(),
                    if list.len() == 1 { "It" } else { "Each one" },
                    format::bytes(total_size)
                ),
            };
            // What the index costs: its size, or the upkeep of a busy table, as a rate (a total since a restart months
            // ago says nothing about today).
            let writes_a_day = writes.and_then(|w| counter_rate_per_day(snapshot, w));
            f.severity = if total_size >= 1024.0 * 1024.0 * 1024.0 || writes_a_day.is_some_and(|w| w >= UPKEEP_A_DAY) {
                Severity::Medium
            } else {
                Severity::Low
            };
            f.impact.score = base_score(f.severity) + (total_size / (1024.0 * 1024.0 * 1024.0)).min(10.0);
            f.impact.label = format!("{} unused", format::bytes(total_size));
            f.confidence = match days {
                Some(d) if d >= 30.0 => Confidence::High,
                Some(d) if d >= 7.0 => Confidence::Medium,
                _ => Confidence::Low,
            };
            f.metrics.push(Metric::new("Size", total_size, Unit::Bytes));
            if let Some(w) = writes {
                let label = if index_counted { "Writes that updated the index" } else { "Writes to the table" };
                f.metrics.push(Metric::new(label, w, Unit::Count));
            }
            if let Some(d) = days {
                f.metrics.push(Metric::new("Counting for", d, Unit::Days));
            }
            let sql = list
                .iter()
                .map(|i| match engine {
                    Engine::Mssql => format!(
                        "ALTER INDEX {} ON {} DISABLE;",
                        quote(engine, &i.name),
                        qualified(engine, &i.schema, &i.table)
                    ),
                    Engine::Postgres => drop_index_sql(engine, &i.schema, &i.table, &i.name),
                    Engine::Sqlite => drop_index_sql(engine, &i.schema, &i.table, &i.name),
                    Engine::Mysql => crate::index_health::hide_index_sql(&snapshot.server, i),
                })
                .collect::<Vec<_>>()
                .join("\n");
            f.fix = Some(Fix {
                summary: match engine {
                    Engine::Mssql => "Disable first: the definition stays, nothing reads it, and `ALTER INDEX … REBUILD` brings it back. Drop it after a full business cycle without complaints.".into(),
                    Engine::Postgres => "Drop it concurrently so writes aren't blocked. Save the definition first so you can recreate it.".into(),
                    Engine::Mysql if snapshot.server.is_mariadb() && crate::index_health::mariadb_can_ignore(&snapshot.server) => "Have the optimizer ignore it first: it stays maintained, and `ALTER INDEX … NOT IGNORED` brings it back instantly. Drop it after a full business cycle without complaints.".into(),
                    Engine::Mysql if snapshot.server.is_mariadb() => "Save its definition, then drop it: this MariaDB version can't hide an index (ignored indexes arrived in 10.6).".into(),
                    Engine::Mysql if !culprit_core::version_at_least(&snapshot.server.version, [8, 0, 0]) => "Save its definition, then drop it: MySQL 5.7 can't hide an index (invisible indexes arrived in 8.0).".into(),
                    Engine::Sqlite => "Drop it: SQLite can't hide an index first, so keep its CREATE INDEX statement to put it back if a query slows down.".into(),
                    Engine::Mysql => "Make it invisible first: the optimizer stops using it but it stays maintained, and `ALTER INDEX … VISIBLE` brings it back instantly. Drop it after a full business cycle without complaints.".into(),
                },
                sql: Some(sql),
                cost: Some(format!("Frees {} and makes writes to {} cheaper.", format::bytes(total_size), table_name)),
            });
            f.verify = vec!["Watch the queries on this table for a week after disabling; none should get slower.".into()];
            f.caveats.push("Usage counters reset when the server restarts or fails over, so an index used only by a monthly job can look unused.".into());
            f.caveats.push("Reads on read replicas or secondaries are not counted here. Check them before dropping.".into());
            let fk_backers: Vec<String> = list
                .iter()
                .filter_map(|i| backs_foreign_key(snapshot, i).map(|fk| format!("{} (foreign key {})", i.name, fk.name)))
                .collect();
            if !fk_backers.is_empty() {
                f.caveats.push(format!(
                    "{} also {} a foreign key: without it, deleting or updating parent rows scans this table.",
                    format::list(&fk_backers),
                    if fk_backers.len() == 1 { "supports" } else { "support" }
                ));
            }
            if let Some(defs) = definitions(&list) {
                f.caveats.push(format!("Definitions, to recreate if needed:\n{defs}"));
            }
            f
        })
        .collect()
}

/// Writes a day to a table that make keeping an unread index up to date a Medium problem.
pub(super) const UPKEEP_A_DAY: f64 = 100_000.0;

/// The foreign key whose columns this index leads with, if any: dropping it would leave that foreign key unindexed.
pub(crate) fn backs_foreign_key<'a>(snapshot: &'a Snapshot, index: &IndexStat) -> Option<&'a culprit_core::ForeignKey> {
    let keys: Vec<String> = index.key_names().iter().map(|k| k.to_ascii_lowercase()).collect();
    snapshot.foreign_keys.iter().find(|fk| {
        fk.schema.eq_ignore_ascii_case(&index.schema)
            && fk.table.eq_ignore_ascii_case(&index.table)
            && !fk.columns.is_empty()
            && keys.len() >= fk.columns.len()
            && lower_set(&keys[..fk.columns.len()]) == lower_set(&fk.columns)
    })
}

pub(super) fn table_writes(snapshot: &Snapshot, schema: &str, table: &str, list: &[&IndexStat]) -> Option<f64> {
    if let Some(w) = list.iter().filter_map(|i| i.writes).reduce(f64::max) {
        return Some(w);
    }
    let t = snapshot.table(schema, table)?;
    Some(t.inserts? + t.updates.unwrap_or(0.0) + t.deletes.unwrap_or(0.0))
}
