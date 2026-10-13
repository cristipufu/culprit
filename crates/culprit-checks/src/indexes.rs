use culprit_core::{
    Capability, Category, CheckInfo, Confidence, Engine, Finding, Fix, IndexStat, Metric, MissingIndex, QueryStat,
    Severity, Snapshot, SubjectKind, Unit, format, strip_direction,
};

use crate::support::*;

const MAX_INCLUDE: usize = 6;

pub fn missing_index_info() -> CheckInfo {
    CheckInfo {
        id: "missing-index".into(),
        title: "Missing indexes".into(),
        category: Category::Indexes,
        engines: vec![Engine::Mssql],
        requires: vec![Capability::MissingIndexes],
        description: "Index suggestions recorded by SQL Server's optimizer, merged with each other and checked against existing indexes.".into(),
    }
}

pub(crate) struct Candidate {
    pub(crate) schema: String,
    pub(crate) table: String,
    pub(crate) equality: Vec<String>,
    pub(crate) inequality: Vec<String>,
    pub(crate) include: Vec<String>,
    pub(crate) seeks: f64,
    pub(crate) scans: f64,
    pub(crate) weight: f64,
    pub(crate) impact_pct: f64,
    pub(crate) query_hashes: Vec<String>,
    /// The optimizer requests merged into this candidate, its own first.
    pub(crate) requests: Vec<MissingIndex>,
}

impl Candidate {
    fn from(m: &MissingIndex) -> Candidate {
        Candidate {
            schema: m.schema.clone(),
            table: m.table.clone(),
            equality: m.equality.clone(),
            inequality: m.inequality.clone(),
            include: m.include.clone(),
            seeks: m.seeks,
            scans: m.scans,
            weight: m.avg_query_cost * (m.avg_impact_pct / 100.0) * (m.seeks + m.scans),
            impact_pct: m.avg_impact_pct,
            query_hashes: m.query_hashes.clone(),
            requests: vec![m.clone()],
        }
    }

    pub(crate) fn keys(&self) -> Vec<String> {
        self.equality.iter().chain(self.inequality.iter()).cloned().collect()
    }

    fn same_table(&self, other: &Candidate) -> bool {
        self.schema.eq_ignore_ascii_case(&other.schema) && self.table.eq_ignore_ascii_case(&other.table)
    }

    /// An index built for `other` also serves `self` when self's equality columns are a subset of other's and the range columns line up.
    fn covered_by(&self, other: &Candidate) -> bool {
        if !self.same_table(other) {
            return false;
        }
        let mine = lower_set(&self.equality);
        let theirs = lower_set(&other.equality);
        if !mine.iter().all(|c| theirs.contains(c)) {
            return false;
        }
        if self.inequality.is_empty() {
            return true;
        }
        mine == theirs
            && self.inequality.len() <= other.inequality.len()
            && self.inequality.iter().zip(&other.inequality).all(|(a, b)| a.eq_ignore_ascii_case(b))
    }

    fn absorb(&mut self, other: Candidate) {
        let keys = lower_set(&self.keys());
        for c in other.include.into_iter().chain(other.inequality) {
            let lc = c.to_ascii_lowercase();
            if !keys.contains(&lc) && !self.include.iter().any(|i| i.eq_ignore_ascii_case(&c)) {
                self.include.push(c);
            }
        }
        self.seeks += other.seeks;
        self.scans += other.scans;
        self.weight += other.weight;
        self.impact_pct = self.impact_pct.max(other.impact_pct);
        self.query_hashes.extend(other.query_hashes);
        self.requests.extend(other.requests);
    }
}

pub(crate) fn merge_candidates(snapshot: &Snapshot) -> Vec<Candidate> {
    let mut all: Vec<Candidate> = snapshot
        .missing_indexes
        .iter()
        .filter(|m| m.seeks + m.scans >= 10.0 && m.avg_impact_pct >= 10.0)
        .map(Candidate::from)
        .collect();
    all.sort_by(|a, b| b.keys().len().cmp(&a.keys().len()).then(b.weight.total_cmp(&a.weight)));
    let mut merged: Vec<Candidate> = Vec::new();
    for c in all {
        match merged.iter_mut().find(|m| c.covered_by(m)) {
            Some(target) => target.absorb(c),
            None => merged.push(c),
        }
    }
    merged
}

/// An existing index whose leading keys already match the suggestion: the fix is to widen it, not add another.
pub(crate) fn existing_match<'a>(snapshot: &'a Snapshot, c: &'a Candidate) -> Option<&'a IndexStat> {
    let keys = c.keys();
    snapshot.indexes_on(&c.schema, &c.table).find(|i| {
        let existing = i.key_names();
        existing.len() >= keys.len()
            && lower_set(&existing[..c.equality.len()].iter().map(|s| s.to_string()).collect::<Vec<_>>())
                == lower_set(&c.equality)
            && keys[c.equality.len()..]
                .iter()
                .zip(&existing[c.equality.len()..])
                .all(|(a, b)| a.eq_ignore_ascii_case(b))
    })
}

pub fn missing_index(snapshot: &Snapshot) -> Vec<Finding> {
    let queries = snapshot.queries();
    merge_candidates(snapshot).into_iter().map(|c| missing_index_finding(snapshot, queries, c)).collect()
}

fn missing_index_finding(snapshot: &Snapshot, queries: &[QueryStat], mut c: Candidate) -> Finding {
    let keys = c.keys();
    let table_name = format!("{}.{}", c.schema, c.table);
    let key_text = format::list(&keys);
    let mut f = finding(
        "missing-index",
        &format!("{table_name}({})", keys.join(",")),
        Category::Indexes,
        subject(SubjectKind::Table, &table_name),
    );
    let linked = asking_queries(queries, &c);
    let table = snapshot.table(&c.schema, &c.table);
    let rows = table.and_then(|t| t.rows);
    let lookups = c.seeks + c.scans;

    f.title = format!("{} is searched by {} without a supporting index", c.table, key_text);
    let mut summary = format!(
        "SQL Server's optimizer asked for an index on {} ({}) {} times {}. Without it, each of those lookups reads more of {}{} than it needs to. It estimates the index would make them about {:.0}% cheaper.",
        table_name,
        key_text,
        format::count(lookups),
        since_phrase(snapshot),
        table_name,
        rows.map(|r| format!(" ({} rows)", format::count(r))).unwrap_or_default(),
        c.impact_pct
    );
    match query_impact(snapshot, &linked) {
        Some(impact) => {
            summary.push_str(&format!(
                " The {} that asked for it {} for {} of database time.",
                if linked.len() == 1 { "query".to_string() } else { format!("{} queries", linked.len()) },
                if linked.len() == 1 { "accounts" } else { "account" },
                format::percent(impact.share)
            ));
            apply_query_impact(&mut f, &impact);
            f.severity = severity_for_share(impact.share).max(Severity::Medium);
            f.confidence = if c.impact_pct >= 50.0 { Confidence::High } else { Confidence::Medium };
        }
        None => {
            f.severity = if c.impact_pct >= 70.0 && lookups >= 1000.0 { Severity::Medium } else { Severity::Low };
            f.impact.score = (6.0 * (c.weight + 1.0).log10()).clamp(4.0, 40.0);
            f.impact.label = format!("{} lookups", format::count(lookups));
            f.confidence = if lookups >= 1000.0 { Confidence::Medium } else { Confidence::Low };
        }
    }
    // Azure SQL weighs the same index over a longer history than the optimizer's counters, which reset on restart.
    if let Some(r) = crate::tuning::recommended_create(snapshot, &c.schema, &c.table, &keys) {
        summary.push_str(&format!(
            " Azure SQL's automatic tuning recommends the same index{}.",
            r.score.map(|s| format!(", scoring it {s:.0} out of 100")).unwrap_or_default()
        ));
        f.confidence = Confidence::High;
    }
    f.summary = summary;
    f.queries = query_refs(&linked);
    f.metrics.push(Metric::new("Lookups that wanted it", lookups, Unit::Count));
    f.metrics.push(Metric::new("Estimated improvement", c.impact_pct / 100.0, Unit::Ratio));
    if let Some(r) = rows {
        f.metrics.push(Metric::new("Rows in table", r, Unit::Rows));
    }
    if let Some(share) = f.impact.db_time_share {
        f.metrics.insert(0, Metric::new("Share of DB time", share, Unit::Ratio));
    }

    if let Some(note) = drop_large_includes(snapshot, &c.schema, &c.table, &mut c.include) {
        f.caveats.push(note);
    }
    let wide_include = c.include.len() > MAX_INCLUDE;
    if wide_include {
        f.caveats.push(format!(
            "SQL Server also suggested including {} more columns. That makes a large, write-heavy index; the fix keeps the keys only. Add INCLUDE columns only for the columns the hot queries actually select.",
            c.include.len()
        ));
        c.include.clear();
    }

    f.fix = Some(match existing_match(snapshot, &c) {
        Some(existing) if !c.include.is_empty() => widen_fix(snapshot, existing, &c.include),
        Some(existing) => Fix {
            summary: format!(
                "{} already starts with these columns, so the remaining gap is small. Check whether the queries below use it; if not, look at their predicates for type mismatches or functions on the column.",
                existing.name
            ),
            sql: None,
            cost: None,
        },
        None => Fix {
            summary: format!(
                "Create an index on {}{}. Equality columns come first; put the most selective one at the front if you know it.",
                key_text,
                if c.include.is_empty() { String::new() } else { format!(", including {}", format::list(&c.include)) }
            ),
            sql: Some(create_index_sql(snapshot, &c.schema, &c.table, &keys, &c.include)),
            cost: write_cost(snapshot, &c.schema, &c.table),
        },
    });
    f.verify = vec![
        "Create the index, let the workload run, then re-scan: this suggestion should disappear.".into(),
        "The queries listed here should show an Index Seek on the new index and far fewer reads per execution.".into(),
    ];
    f.caveats.push("Missing-index suggestions are per-query and ignore the cost of maintaining the index; review overlapping suggestions together.".into());
    f
}

/// The queries behind a candidate: linked by query_hash (SQL Server 2019+), or whose plan carries the same request.
pub(crate) fn asking_queries<'a>(queries: &'a [QueryStat], c: &Candidate) -> Vec<&'a QueryStat> {
    queries
        .iter()
        .filter(|q| q.query_hash.as_ref().is_some_and(|h| c.query_hashes.iter().any(|x| x.eq_ignore_ascii_case(h))))
        .chain(queries.iter().filter(|q| plan_asks_for(q, c)))
        .fold(Vec::new(), |mut acc, q| {
            if !acc.iter().any(|a: &&QueryStat| a.id == q.id) {
                acc.push(q);
            }
            acc
        })
}

fn plan_asks_for(q: &QueryStat, c: &Candidate) -> bool {
    q.plan.as_ref().is_some_and(|p| {
        p.index_hints.iter().any(|h| {
            let table_ok = h.table.to_ascii_lowercase().replace(['[', ']'], "").ends_with(&format!(
                "{}.{}",
                c.schema.to_ascii_lowercase(),
                c.table.to_ascii_lowercase()
            ));
            table_ok && lower_set(&h.equality) == lower_set(&c.equality)
        })
    })
}

/// SQL Server INCLUDE columns that hold large objects: (max) strings and binaries, text, ntext, image, xml, json. Including
/// one copies the column's data into the index (most of a table whose rows are documents), so the suggestions keep the
/// keys and let the few rows found go back to the table for it. Returns the sentence that says what was left out.
pub(crate) fn drop_large_includes(
    snapshot: &Snapshot,
    schema: &str,
    table: &str,
    include: &mut Vec<String>,
) -> Option<String> {
    // Only SQL Server suggests INCLUDE columns, and in PostgreSQL `text` is the everyday string type.
    if snapshot.server.engine != Engine::Mssql {
        return None;
    }
    let is_large = |name: &str| {
        snapshot
            .columns
            .iter()
            .find(|c| {
                c.schema.eq_ignore_ascii_case(schema)
                    && c.table.eq_ignore_ascii_case(table)
                    && c.name.eq_ignore_ascii_case(name)
            })
            .is_some_and(|c| {
                let t = c.data_type.to_ascii_lowercase();
                t.contains("(max)") || matches!(t.as_str(), "text" | "ntext" | "image" | "xml" | "json")
            })
    };
    let large: Vec<String> = include.iter().filter(|c| is_large(c)).cloned().collect();
    if large.is_empty() {
        return None;
    }
    include.retain(|c| !is_large(c));
    let size = snapshot
        .table(schema, table)
        .and_then(|t| t.data_bytes)
        .map(|b| format!(" ({schema}.{table} holds {})", format::bytes(b)))
        .unwrap_or_default();
    Some(format!(
        "Left {} out of INCLUDE: {} large values, and including {} would copy that data into the index{size}. The rows the \
         index finds go back to the table for {}, which costs less.",
        format::list(&large),
        if large.len() == 1 { "it holds" } else { "they hold" },
        if large.len() == 1 { "it" } else { "them" },
        if large.len() == 1 { "it" } else { "them" },
    ))
}

pub(crate) fn widen_fix(snapshot: &Snapshot, existing: &IndexStat, include: &[String]) -> Fix {
    let engine = snapshot.server.engine;
    let keys: Vec<String> = existing.columns.clone();
    let mut all_include = existing.include.clone();
    for c in include {
        if !all_include.iter().any(|i| i.eq_ignore_ascii_case(c))
            && !keys.iter().any(|k| strip_direction(k).eq_ignore_ascii_case(c))
        {
            all_include.push(c.clone());
        }
    }
    let cols = keys.iter().map(|k| match k.strip_suffix(" DESC") {
        Some(name) => format!("{} DESC", quote(engine, name)),
        None => quote(engine, k),
    });
    let inc = all_include.iter().map(|c| quote(engine, c)).collect::<Vec<_>>().join(", ");
    let online = if supports_online_index(snapshot) { ", ONLINE = ON" } else { "" };
    let sql = format!(
        "CREATE NONCLUSTERED INDEX {}\n    ON {} ({})\n    INCLUDE ({inc})\n    WITH (DROP_EXISTING = ON{online});",
        quote(engine, &existing.name),
        qualified(engine, &existing.schema, &existing.table),
        cols.collect::<Vec<_>>().join(", ")
    );
    Fix {
        summary: format!(
            "{} already has the right keys but makes every matching row go back to the table for {}. Rebuild it with those columns included so it covers the query.",
            existing.name,
            format::list(include)
        ),
        sql: Some(sql),
        cost: existing.size_bytes.map(|b| {
            format!("The index grows from {}; writes that touch the included columns maintain it.", format::bytes(b))
        }),
    }
}

pub(crate) fn write_cost(snapshot: &Snapshot, schema: &str, table: &str) -> Option<String> {
    let (writes, days) = measured_writes(snapshot, schema, table)?;
    if writes <= 0.0 {
        return Some(format!(
            "{schema}.{table} received no writes, so this index would cost nothing to keep up there."
        ));
    }
    Some(format!(
        "{schema}.{table} receives {}; each one will also update this index.",
        how_many(snapshot, writes, days, "write", "writes")
    ))
}

/// Inserts, updates and deletes on a table since its counters started, and the days they cover (at least half an
/// hour, so a just-restarted server doesn't read as a write storm).
pub(crate) fn measured_writes(snapshot: &Snapshot, schema: &str, table: &str) -> Option<(f64, f64)> {
    let base = snapshot.indexes_on(schema, table).find(|i| i.is_clustered || i.is_primary_key).and_then(|i| i.writes);
    let row_writes = snapshot
        .table(schema, table)
        .and_then(|t| Some(t.inserts? + t.updates.unwrap_or(0.0) + t.deletes.unwrap_or(0.0)));
    let writes = base.or(row_writes)?;
    let days = counter_days(snapshot).filter(|d| *d >= 0.02)?;
    Some((writes, days))
}

/// Inserts, updates and deletes a day on a table, from the clustered index's update counter (SQL Server) or the
/// table's row counters (Postgres, MySQL), over the days the counters have been accumulating.
pub(crate) fn writes_per_day(snapshot: &Snapshot, schema: &str, table: &str) -> Option<f64> {
    let base = snapshot.indexes_on(schema, table).find(|i| i.is_clustered || i.is_primary_key).and_then(|i| i.writes);
    let row_writes = snapshot
        .table(schema, table)
        .and_then(|t| Some(t.inserts? + t.updates.unwrap_or(0.0) + t.deletes.unwrap_or(0.0)));
    let writes = base.or(row_writes)?;
    // A rate a day only over 20 hours of counters: under that it would be an extrapolation.
    let days = counter_days(snapshot).filter(|d| per_day_is_measured(*d))?;
    Some(writes / days)
}

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

pub fn unused_index(snapshot: &Snapshot) -> Vec<Finding> {
    let engine = snapshot.server.engine;
    let days = counter_days(snapshot);
    let mut by_table: Vec<(String, String, Vec<&IndexStat>)> = Vec::new();
    let redundant: Vec<&str> = redundant_indexes(snapshot).iter().map(|(a, _)| a.name.as_str()).collect();
    let mysql = snapshot.server.engine == Engine::Mysql;
    // InnoDB refuses to drop the index a foreign key relies on.
    for i in snapshot
        .indexes
        .iter()
        .filter(|i| is_unused(snapshot, i) && !redundant.contains(&i.name.as_str()))
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
            f.summary = match writes {
                // Nothing written either: the cost is the space, not the upkeep.
                Some(0.0) => format!(
                    "No query has read {} {}, and {} hasn't been written to either. {} {} of storage and buffer memory.",
                    names,
                    since,
                    table_name,
                    if list.len() == 1 { "It takes" } else { "Together they take" },
                    format::bytes(total_size)
                ),
                _ => format!(
                    "No query has read {} {}{}. {} still has to be updated on every insert, and on updates that touch its columns, and it takes {} of storage and buffer memory.",
                    names,
                    since,
                    writes.map(|w| format!(", while {} was written {} times", table_name, format::count(w))).unwrap_or_default(),
                    if list.len() == 1 { "It" } else { "Each one" },
                    format::bytes(total_size)
                ),
            };
            f.severity = if total_size >= 1024.0 * 1024.0 * 1024.0 || writes.is_some_and(|w| w >= 1e6) {
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
                f.metrics.push(Metric::new("Writes to the table", w, Unit::Count));
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
                    Engine::Mysql => crate::index_health::hide_index_sql(&snapshot.server, i),
                })
                .collect::<Vec<_>>()
                .join("\n");
            f.fix = Some(Fix {
                summary: match engine {
                    Engine::Mssql => "Disable first: the definition stays, nothing reads it, and `ALTER INDEX … REBUILD` brings it back. Drop it after a full business cycle without complaints.".into(),
                    Engine::Postgres => "Drop it concurrently so writes aren't blocked. Save the definition first so you can recreate it.".into(),
                    Engine::Mysql if snapshot.server.name() == "MariaDB" && crate::index_health::mariadb_can_ignore(&snapshot.server) => "Have the optimizer ignore it first: it stays maintained, and `ALTER INDEX … NOT IGNORED` brings it back instantly. Drop it after a full business cycle without complaints.".into(),
                    Engine::Mysql if snapshot.server.name() == "MariaDB" => "Save its definition, then drop it: this MariaDB version can't hide an index (ignored indexes arrived in 10.6).".into(),
                    Engine::Mysql if !culprit_core::version_at_least(&snapshot.server.version, [8, 0, 0]) => "Save its definition, then drop it: MySQL 5.7 can't hide an index (invisible indexes arrived in 8.0).".into(),
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

/// Never read since the counters started, and not one the database needs for something else: a key, a constraint or
/// the table's storage. A filtered (partial) index counts: unread, it is only upkeep on every write it matches. An index
/// that may be younger than a week isn't unused yet: the queries it was made for may not have run, or been deployed.
pub(crate) fn is_unused(snapshot: &Snapshot, i: &IndexStat) -> bool {
    let is_candidate = !i.is_primary_key && !i.is_unique && !i.is_clustered && !i.is_disabled;
    is_candidate && i.reads == Some(0.0) && i.size_bytes.unwrap_or(0.0) >= 64.0 * 1024.0 && !too_new(snapshot, i)
}

/// Days an index must have existed before no reads says anything about it.
const MIN_INDEX_DAYS: f64 = 7.0;

/// Days since the latest it may have been created, when a scan found reason to think it may be new.
fn index_age_days(snapshot: &Snapshot, i: &IndexStat) -> Option<f64> {
    days_between(i.created_at_latest.as_deref()?, &snapshot.taken_at).map(|d| d.max(0.0))
}

/// It may have been created less than a week ago, so its usage counters say little yet.
pub(crate) fn too_new(snapshot: &Snapshot, i: &IndexStat) -> bool {
    index_age_days(snapshot, i).is_some_and(|d| d < MIN_INDEX_DAYS)
}

fn table_writes(snapshot: &Snapshot, schema: &str, table: &str, list: &[&IndexStat]) -> Option<f64> {
    if let Some(w) = list.iter().filter_map(|i| i.writes).reduce(f64::max) {
        return Some(w);
    }
    let t = snapshot.table(schema, table)?;
    Some(t.inserts? + t.updates.unwrap_or(0.0) + t.deletes.unwrap_or(0.0))
}

fn definitions(list: &[&IndexStat]) -> Option<String> {
    let defs: Vec<String> = list.iter().filter_map(|i| i.definition.clone()).collect();
    (!defs.is_empty()).then(|| defs.join(";\n") + ";")
}

pub fn duplicate_index_info() -> CheckInfo {
    CheckInfo {
        id: "duplicate-index".into(),
        title: "Duplicate and redundant indexes".into(),
        category: Category::Indexes,
        engines: vec![Engine::Mssql, Engine::Postgres, Engine::Mysql],
        requires: vec![Capability::TableStats],
        description: "Indexes with the same keys as another index, or whose keys are a leading part of a wider one."
            .into(),
    }
}

pub fn duplicate_index(snapshot: &Snapshot) -> Vec<Finding> {
    let engine = snapshot.server.engine;
    let mut out = Vec::new();
    for (a, b) in redundant_indexes(snapshot) {
        let exact = same_keys(a, b);
        let table_name = format!("{}.{}", a.schema, a.table);
        let mut f = finding(
            "duplicate-index",
            &format!("{}.{}", table_name, a.name),
            Category::Indexes,
            subject(SubjectKind::Index, format!("{}.{}", table_name, a.name)),
        );
        f.title = if exact {
            format!("{} duplicates {} on {}", a.name, b.name, a.table)
        } else {
            format!("{} is covered by the wider {} on {}", a.name, b.name, a.table)
        };
        f.summary = format!(
            "{} has keys ({}) and {} has keys ({}). {} Every write to {} maintains both.",
            a.name,
            a.columns.join(", "),
            b.name,
            b.columns.join(", "),
            if exact {
                "They are the same index twice.".to_string()
            } else {
                format!("Any query that can seek on {} can seek on {} instead.", a.name, b.name)
            },
            table_name
        );
        let size = a.size_bytes.unwrap_or(0.0);
        f.severity = if size >= 512.0 * 1024.0 * 1024.0 { Severity::Medium } else { Severity::Low };
        f.impact.score = base_score(f.severity) + (size / (1024.0 * 1024.0 * 1024.0)).min(8.0);
        f.impact.label = format!("{} redundant", format::bytes(size));
        f.confidence = if exact { Confidence::High } else { Confidence::Medium };
        f.metrics.push(Metric::new("Size", size, Unit::Bytes));
        if let Some(r) = a.reads {
            f.metrics.push(Metric::new(format!("Reads of {}", a.name), r, Unit::Count));
        }
        f.fix = Some(Fix {
            summary: format!("Drop {} and keep {}.", a.name, b.name),
            sql: Some(drop_index_sql(engine, &a.schema, &a.table, &a.name)),
            cost: Some(format!("Frees {}.", format::bytes(size))),
        });
        if !exact {
            f.caveats.push(format!(
                "{} is narrower, so scans of it read fewer pages. Keep it if a hot query scans it rather than seeking.",
                a.name
            ));
        }
        out.push(f);
    }
    out
}

pub(crate) fn same_keys(a: &IndexStat, b: &IndexStat) -> bool {
    a.columns.len() == b.columns.len() && a.columns.iter().zip(&b.columns).all(|(x, y)| x.eq_ignore_ascii_case(y))
}

/// `b` makes `a` redundant when a's keys are a leading prefix of b's keys and everything a includes is available in b.
fn makes_redundant(b: &IndexStat, a: &IndexStat) -> bool {
    if b.columns.len() < a.columns.len() {
        return false;
    }
    let prefix = a.columns.iter().zip(&b.columns).all(|(x, y)| x.eq_ignore_ascii_case(y));
    let available: Vec<String> = b.key_names().iter().map(|s| s.to_string()).chain(b.include.iter().cloned()).collect();
    let includes_ok = a.include.iter().all(|c| available.iter().any(|x| x.eq_ignore_ascii_case(c)));
    prefix && includes_ok
}

/// Between two identical indexes, drop the one that is less used, then the one with the later name, so exactly one finding is produced.
fn prefer_drop(a: &IndexStat, b: &IndexStat) -> bool {
    if b.is_unique || b.is_primary_key || b.is_clustered {
        return true;
    }
    let (ra, rb) = (a.reads.unwrap_or(0.0), b.reads.unwrap_or(0.0));
    if ra != rb {
        return ra < rb;
    }
    a.name > b.name
}

pub fn unindexed_fk_info() -> CheckInfo {
    CheckInfo {
        id: "unindexed-fk".into(),
        title: "Foreign keys without an index".into(),
        category: Category::Indexes,
        engines: vec![Engine::Mssql, Engine::Postgres, Engine::Mysql],
        requires: vec![Capability::TableStats],
        description: "Foreign key columns with no index starting with them, which makes deletes on the parent and joins from the child slow.".into(),
    }
}

/// Whether an index can serve the foreign key: it leads with the key columns, isn't disabled, and covers every row the
/// check can match. A filtered (partial) index can't, unless its filter only leaves out rows whose key is NULL (those
/// never match a parent row): checking a parent delete has to find soft-deleted children too.
pub(crate) fn fk_is_indexed(snapshot: &Snapshot, fk: &culprit_core::ForeignKey) -> bool {
    snapshot.indexes_on(&fk.schema, &fk.table).any(|i| {
        !i.is_disabled
            && leads_with(i, &fk.columns)
            && i.filter.as_deref().is_none_or(|f| only_excludes_null_keys(f, &fk.columns))
    })
}

fn leads_with(i: &IndexStat, columns: &[String]) -> bool {
    let keys = i.key_names();
    keys.len() >= columns.len()
        && lower_set(&keys[..columns.len()].iter().map(|s| s.to_string()).collect::<Vec<_>>()) == lower_set(columns)
}

/// `([customer_id] IS NOT NULL)`, `(a IS NOT NULL AND b IS NOT NULL)`: a filter whose every condition is one of the key
/// columns being NOT NULL.
fn only_excludes_null_keys(filter: &str, columns: &[String]) -> bool {
    let plain: String = filter
        .chars()
        .filter(|c| !matches!(c, '[' | ']' | '"' | '`' | '(' | ')'))
        .collect::<String>()
        .to_ascii_lowercase();
    let words: Vec<&str> = plain.split_whitespace().collect();
    words.split(|w| *w == "and").all(|condition| match condition {
        [column, "is", "not", "null"] => {
            let name = column.rsplit('.').next().unwrap_or(column);
            columns.iter().any(|c| c.eq_ignore_ascii_case(name))
        }
        _ => false,
    })
}

/// Child tables smaller than this get no finding of their own: reading one in full is cheap today.
pub(crate) const FK_OWN_FINDING_ROWS: f64 = 10_000.0;

/// An index led by the key's columns that its filter keeps from serving the key.
fn filtered_fk_index<'a>(snapshot: &'a Snapshot, fk: &'a culprit_core::ForeignKey) -> Option<&'a IndexStat> {
    snapshot.indexes_on(&fk.schema, &fk.table).find(|i| {
        leads_with(i, &fk.columns) && i.filter.as_deref().is_some_and(|f| !only_excludes_null_keys(f, &fk.columns))
    })
}

/// Every foreign key no index can serve, with its child table's rows when known, largest first. A key repeated on the
/// same columns (a duplicate constraint), or whose columns lead another unindexed key on the same table (one index on
/// the longer key serves both), is listed once.
pub(crate) fn unindexed_foreign_keys(snapshot: &Snapshot) -> Vec<(&culprit_core::ForeignKey, Option<f64>)> {
    let keys = |fk: &culprit_core::ForeignKey| fk.columns.iter().map(|c| c.to_ascii_lowercase()).collect::<Vec<_>>();
    let same_table = |a: &culprit_core::ForeignKey, b: &culprit_core::ForeignKey| {
        a.schema.eq_ignore_ascii_case(&b.schema) && a.table.eq_ignore_ascii_case(&b.table)
    };
    let candidates: Vec<&culprit_core::ForeignKey> =
        snapshot.foreign_keys.iter().filter(|fk| !fk.columns.is_empty() && !fk_is_indexed(snapshot, fk)).collect();
    let mut out: Vec<(&culprit_core::ForeignKey, Option<f64>)> = Vec::new();
    for fk in &candidates {
        let (mine, longer) = (keys(fk), |o: &&culprit_core::ForeignKey| {
            let theirs = keys(o);
            same_table(fk, o) && theirs.len() > fk.columns.len() && theirs.starts_with(&keys(fk))
        });
        if candidates.iter().any(longer) || out.iter().any(|(o, _)| same_table(fk, o) && keys(o) == mine) {
            continue;
        }
        out.push((fk, snapshot.table(&fk.schema, &fk.table).and_then(|t| t.rows)));
    }
    out.sort_by(|a, b| {
        b.1.unwrap_or(-1.0)
            .total_cmp(&a.1.unwrap_or(-1.0))
            .then_with(|| (&a.0.schema, &a.0.table, &a.0.name).cmp(&(&b.0.schema, &b.0.table, &b.0.name)))
    });
    out
}

pub fn unindexed_fk(snapshot: &Snapshot) -> Vec<Finding> {
    let unindexed = unindexed_foreign_keys(snapshot);
    let mut out: Vec<Finding> = unindexed
        .iter()
        .filter_map(|(fk, rows)| {
            rows.filter(|r| *r >= FK_OWN_FINDING_ROWS).map(|r| unindexed_fk_finding(snapshot, fk, r))
        })
        .collect();
    out.extend(unindexed_fk_pattern(snapshot, &unindexed));
    out
}

fn unindexed_fk_finding(snapshot: &Snapshot, fk: &culprit_core::ForeignKey, rows: f64) -> Finding {
    let child = format!("{}.{}", fk.schema, fk.table);
    let parent = format!("{}.{}", fk.ref_schema, fk.ref_table);
    let cols = format::list(&fk.columns);
    let mut f = finding(
        "unindexed-fk",
        &format!("{child}({})", fk.columns.join(",")),
        Category::Indexes,
        subject(SubjectKind::Table, &child),
    );
    f.title = format!("{} has no index on {}, its foreign key to {}", fk.table, cols, fk.ref_table);
    f.summary = format!(
        "Deleting a row from {parent}, or changing its key, makes the database look for matching rows in {child} ({} rows) without an index, which means reading the whole table. Queries that load {} rows for one {} row can't seek either.",
        format::count(rows),
        fk.table,
        fk.ref_table
    );
    // Queries that go through the key, still running: a plain insert into the child only validates the key against the
    // parent's primary key, which needs no index here.
    let touching: Vec<&QueryStat> = queries_touching(snapshot, &fk.schema, &fk.table)
        .into_iter()
        .filter(|q| crate::advisor::uses_foreign_key(&q.text, &fk.columns, &fk.ref_table))
        .filter(|q| crate::query_stopped(snapshot, q).is_none())
        .collect();
    f.severity = if rows >= 1_000_000.0 { Severity::Medium } else { Severity::Low };
    f.impact.score = base_score(f.severity) + (rows.log10() - 4.0).max(0.0) * 2.0;
    f.impact.label = format!("{} rows unindexed", format::count(rows));
    f.confidence = Confidence::High;
    f.metrics.push(Metric::new("Rows in table", rows, Unit::Rows));
    f.metrics.push(Metric::text("Foreign key", fk.name.clone()));
    f.queries = query_refs(&touching.into_iter().take(5).collect::<Vec<_>>());
    f.fix = Some(Fix {
        summary: format!("Index the foreign key column{} on {}.", if fk.columns.len() > 1 { "s" } else { "" }, child),
        sql: Some(create_index_sql(snapshot, &fk.schema, &fk.table, &fk.columns, &[])),
        cost: write_cost(snapshot, &fk.schema, &fk.table),
    });
    f.verify =
        vec![format!("Deletes from {parent} should no longer scan {child}; re-scan to confirm the finding is gone.")];
    if let Some(i) = filtered_fk_index(snapshot, fk) {
        f.caveats.push(filtered_caveat(i, &parent));
    }
    f
}

fn filtered_caveat(i: &IndexStat, parent: &str) -> String {
    format!(
        "{} leads with these columns but is filtered ({}), so it can't serve the key: a delete on {parent} has to find \
         the rows the filter leaves out too.",
        i.name,
        i.filter.as_deref().unwrap_or_default()
    )
}

/// One finding for the habit rather than the table: many foreign keys unindexed, mostly on tables too small for a
/// finding of their own. A schema helper or ORM that declares keys without indexes is the usual cause, and fixing it
/// there covers the keys still to come.
fn unindexed_fk_pattern(
    snapshot: &Snapshot,
    unindexed: &[(&culprit_core::ForeignKey, Option<f64>)],
) -> Option<Finding> {
    let total = snapshot.foreign_keys.iter().filter(|fk| !fk.columns.is_empty()).count();
    let small: Vec<(&culprit_core::ForeignKey, f64)> =
        unindexed.iter().filter_map(|(fk, rows)| rows.filter(|r| *r < FK_OWN_FINDING_ROWS).map(|r| (*fk, r))).collect();
    if small.len() < 3 || (unindexed.len() as f64) < total as f64 * 0.25 {
        return None;
    }
    let big = unindexed.iter().filter(|(_, rows)| rows.is_some_and(|r| r >= FK_OWN_FINDING_ROWS)).count();
    let unknown = unindexed.iter().filter(|(_, rows)| rows.is_none()).count();
    let filtered: Vec<&IndexStat> = unindexed.iter().filter_map(|(fk, _)| filtered_fk_index(snapshot, fk)).collect();
    let named = |fk: &culprit_core::ForeignKey| format!("{}({})", fk.table, fk.columns.join(", "));
    let under = format::count(FK_OWN_FINDING_ROWS);
    let mut f =
        finding("unindexed-fk", "schema", Category::Indexes, subject(SubjectKind::Database, &snapshot.server.database));
    f.title = if unindexed.len() == total {
        format!("None of the {total} foreign keys has an index that can serve it")
    } else {
        format!("{} of {} foreign keys have no index that can serve them", unindexed.len(), total)
    };
    let names = small
        .iter()
        .take(4)
        .map(|(fk, rows)| format!("{} ({} rows)", named(fk), format::count(*rows)))
        .collect::<Vec<_>>();
    let elsewhere = [
        match big {
            0 => None,
            1 => Some("1 on a larger table has a finding of its own".to_string()),
            n => Some(format!("{n} on larger tables have findings of their own")),
        },
        match unknown {
            0 => None,
            1 => Some("1 is on a table whose size isn't known (a partitioned table, for one)".to_string()),
            n => Some(format!("{n} are on tables whose size isn't known (partitioned tables, for one)")),
        },
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>();
    f.summary = format!(
        "{}{}; {} {}. Deleting a parent row, or changing its key, checks every child table for matching rows, and \
         without an index that check reads the child table in full: cheap while the tables are small, slower with every \
         row and with every new table declared the same way.{} When this many keys lack an index, the code that declares \
         tables usually creates foreign keys without one: fixing it there covers the keys still to come.",
        if elsewhere.is_empty() {
            format!("All {} are on tables under {under} rows", small.len())
        } else {
            format!("{} of these keys are on tables under {under} rows", small.len())
        },
        if elsewhere.is_empty() { String::new() } else { format!(" ({})", format::list(&elsewhere)) },
        if small.len() <= 4 { "they are" } else { "the largest are" },
        format::list(&names),
        match filtered.len() {
            0 => String::new(),
            1 => format!(
                " {} is led by its key's columns but filtered, so it can't serve the check: a parent delete has to find the \
                 rows the filter leaves out too.",
                filtered[0].name
            ),
            n => format!(
                " {n} of these keys are covered only by a filtered index, which can't serve the check: a parent delete has \
                 to find the rows the filter leaves out too."
            ),
        }
    );
    f.severity = Severity::Low;
    f.impact.score = base_score(f.severity) + (small.len() as f64).log2().min(4.0);
    f.impact.label = format!("{} of {} foreign keys unindexed", unindexed.len(), total);
    f.confidence = Confidence::High;
    f.metrics.push(Metric::new("Foreign keys without a usable index", unindexed.len() as f64, Unit::Count));
    f.metrics.push(Metric::new("Foreign keys", total as f64, Unit::Count));
    if !filtered.is_empty() {
        f.metrics.push(Metric::new("Covered only by a filtered index", filtered.len() as f64, Unit::Count));
    }
    f.fix = Some(Fix {
        summary: format!(
            "Give each foreign key an unfiltered index led by its columns, and make declaring a foreign key declare its \
             index wherever the schema is defined (a migration helper, the ORM's table definitions), so new keys get one \
             too.{}",
            if snapshot.server.engine == Engine::Postgres {
                " Run the statements one at a time (psql -f does): CREATE INDEX CONCURRENTLY can't run inside a transaction."
            } else {
                ""
            }
        ),
        sql: Some(
            small
                .iter()
                .map(|(fk, _)| create_index_sql(snapshot, &fk.schema, &fk.table, &fk.columns, &[]))
                .collect::<Vec<_>>()
                .join("\n\n"),
        ),
        cost: Some(
            "Each index is on a small table and costs little to keep up; check the busiest tables' write rates first."
                .into(),
        ),
    });
    f.verify = vec![
        "Re-scan: this finding is gone; the foreign-key findings for single tables go once their own fixes are applied."
            .into(),
    ];
    f.caveats.push(
        "A key whose parent rows are never deleted or re-keyed (a lookup table, append-only history) doesn't need the \
         index for the check; leave those out if their write cost matters."
            .into(),
    );
    Some(f)
}

pub fn filtered_scan_info() -> CheckInfo {
    CheckInfo {
        id: "filtered-scan".into(),
        title: "Scans an index would avoid".into(),
        category: Category::Indexes,
        engines: vec![Engine::Mssql, Engine::Postgres, Engine::Mysql],
        requires: vec![Capability::QueryPlans],
        description: "Heavy queries whose plan reads a large table in full and keeps only the rows matching a filter that no index supports.".into(),
    }
}

pub(crate) struct ScanGroup<'a> {
    pub(crate) schema: String,
    pub(crate) table: String,
    pub(crate) keys: Vec<String>,
    pub(crate) predicate: String,
    pub(crate) rows: f64,
    pub(crate) queries: Vec<(&'a QueryStat, u32)>,
}

pub fn filtered_scan(snapshot: &Snapshot) -> Vec<Finding> {
    scan_groups(snapshot).into_iter().map(|g| filtered_scan_finding(snapshot, g)).collect()
}

/// Full scans of large tables that keep only rows matching a filter no index leads with, grouped by table and keys.
pub(crate) fn scan_groups(snapshot: &Snapshot) -> Vec<ScanGroup<'_>> {
    let mut groups: Vec<ScanGroup> = Vec::new();
    for q in app_queries(snapshot) {
        let Some(plan) = &q.plan else { continue };
        if query_impact(snapshot, &[q]).is_none_or(|i| i.share < 0.005) {
            continue;
        }
        for node in plan.nodes() {
            if node.kind != culprit_core::NodeKind::Scan {
                continue;
            }
            let (Some(object), Some(predicate)) = (&node.object, &node.predicate) else { continue };
            let Some((schema, table)) = split_object(object) else { continue };
            let rows = node.table_rows.or_else(|| snapshot.table(&schema, &table).and_then(|t| t.rows)).unwrap_or(0.0);
            let estimate_keeps_few = node.est_rows_total() * 10.0 <= rows * node.est_executions.max(1.0);
            if rows < 10_000.0 || !(estimate_keeps_few || measured_keeps_few(snapshot, q, rows)) {
                continue;
            }
            let cols = predicate_columns(predicate);
            let mut keys = cols.equality.clone();
            keys.extend(cols.range.into_iter().take(1));
            if keys.is_empty() || leading_index_exists(snapshot, &schema, &table, &cols.equality, &keys) {
                continue;
            }
            match groups
                .iter_mut()
                .find(|g| g.schema == schema && g.table == table && lower_set(&g.keys) == lower_set(&keys))
            {
                Some(g) => {
                    if !g.queries.iter().any(|(x, _)| x.id == q.id) {
                        g.queries.push((q, node.id));
                    }
                }
                None => groups.push(ScanGroup {
                    schema,
                    table,
                    keys,
                    predicate: predicate.clone(),
                    rows,
                    queries: vec![(q, node.id)],
                }),
            }
        }
    }
    groups
}

/// MySQL and MariaDB count rows examined and rows returned per statement. A statement that examines most of the
/// table on every run and returns under a tenth of it filters after reading, whatever the optimizer estimated (MariaDB
/// assumes a condition keeps every row when the column has no statistics).
fn measured_keeps_few(snapshot: &Snapshot, q: &QueryStat, table_rows: f64) -> bool {
    if snapshot.server.engine != Engine::Mysql || q.executions <= 0.0 {
        return false;
    }
    let examined = q.logical_reads.map(|r| r / q.executions).unwrap_or(0.0);
    let returned = q.rows.map(|r| r / q.executions);
    examined >= table_rows * 0.5 && returned.is_some_and(|r| r * 10.0 <= table_rows)
}

pub(crate) fn split_object(object: &str) -> Option<(String, String)> {
    let clean = object.replace(['[', ']', '"'], "");
    let parts: Vec<&str> = clean.split('.').collect();
    match parts.as_slice() {
        [.., schema, table] => Some((schema.to_string(), table.to_string())),
        [table] => Some(("dbo".to_string(), table.to_string())),
        _ => None,
    }
}

/// An index already leads with the equality columns: the scan has another cause (type mismatch, function on the column, stale stats).
fn leading_index_exists(snapshot: &Snapshot, schema: &str, table: &str, equality: &[String], keys: &[String]) -> bool {
    leading_index(snapshot, schema, table, equality, keys).is_some()
}

/// The index that leads with the equality columns (or, without any, with the first key), in any order.
pub(crate) fn leading_index<'a>(
    snapshot: &'a Snapshot,
    schema: &str,
    table: &str,
    equality: &[String],
    keys: &[String],
) -> Option<&'a IndexStat> {
    let wanted = if equality.is_empty() { keys.get(..1)? } else { equality };
    snapshot.indexes.iter().find(|i| {
        let existing = i.key_names();
        i.schema.eq_ignore_ascii_case(schema)
            && i.table.eq_ignore_ascii_case(table)
            && !i.is_disabled
            && existing.len() >= wanted.len()
            && lower_set(&existing[..wanted.len()].iter().map(|s| s.to_string()).collect::<Vec<_>>())
                == lower_set(wanted)
    })
}

fn filtered_scan_finding(snapshot: &Snapshot, g: ScanGroup) -> Finding {
    let table_name = format!("{}.{}", g.schema, g.table);
    let key_text = format::list(&g.keys);
    let queries: Vec<&QueryStat> = g.queries.iter().map(|(q, _)| *q).collect();
    let impact = query_impact(snapshot, &queries).expect("queries have workload");
    let mut f = finding(
        "filtered-scan",
        &format!("{table_name}({})", g.keys.join(",")),
        Category::Indexes,
        subject(SubjectKind::Table, &table_name),
    );
    apply_query_impact(&mut f, &impact);
    f.severity = severity_for_share(impact.share).max(Severity::Medium);
    f.confidence = Confidence::High;
    f.title = format!("{} is read in full to find rows by {}", g.table, key_text);
    let fk = snapshot.foreign_keys.iter().find(|fk| {
        fk.schema.eq_ignore_ascii_case(&g.schema)
            && fk.table.eq_ignore_ascii_case(&g.table)
            && !fk.columns.is_empty()
            && fk.columns.iter().all(|c| g.keys.iter().any(|k| k.eq_ignore_ascii_case(c)))
    });
    f.summary = format!(
        "{} read every row of {} ({} rows) and keep only those matching `{}`. No index starts with {}, so the database has no faster way in. Together they account for {} of database time.{}",
        if queries.len() == 1 {
            "One query plans to".to_string()
        } else {
            format!("{} queries plan to", queries.len())
        },
        table_name,
        format::count(g.rows),
        g.predicate.trim(),
        key_text,
        format::percent(impact.share),
        fk.map(|fk| format!(
            " {} is also the foreign key to {}, so deletes there scan this table too.",
            key_text, fk.ref_table
        ))
        .unwrap_or_default()
    );
    f.queries = g
        .queries
        .iter()
        .map(|(q, node)| culprit_core::QueryRef { id: q.id.clone(), plan_node: Some(*node), note: None })
        .collect();
    f.metrics.push(Metric::new("Share of DB time", impact.share, Unit::Ratio));
    f.metrics.push(Metric::new("DB time per day", impact.per_day_ms, Unit::Ms));
    f.metrics.push(Metric::new("Rows scanned per run", g.rows, Unit::Rows));
    f.metrics.push(Metric::new("Queries", queries.len() as f64, Unit::Count));
    f.fix = Some(Fix {
        summary: format!("Create an index on {} so these queries seek instead of scanning.", key_text),
        sql: Some(create_index_sql(snapshot, &g.schema, &g.table, &g.keys, &[])),
        cost: write_cost(snapshot, &g.schema, &g.table),
    });
    f.verify = vec![
        format!("Explain one of the queries again: the scan of {} should become an index seek/scan on the new index.", g.table),
        "Re-scan after the workload has run: this finding should be gone and the queries' share of database time lower.".into(),
    ];
    f
}

/// Indexes another index makes redundant, mapped to the index that covers them. Shared by the duplicate and unused checks so an index is reported once.
pub fn redundant_indexes(snapshot: &Snapshot) -> Vec<(&IndexStat, &IndexStat)> {
    let mut out = Vec::new();
    for a in &snapshot.indexes {
        if a.is_primary_key || a.is_unique || a.is_clustered || a.is_disabled || a.columns.is_empty() {
            continue;
        }
        let keeper = snapshot
            .indexes_on(&a.schema, &a.table)
            .filter(|b| b.name != a.name && !b.is_disabled && b.filter == a.filter && b.kind == a.kind)
            .find(|b| makes_redundant(b, a) && (!same_keys(a, b) || prefer_drop(a, b)))
            // A read index isn't redundant to one nothing reads: drop the unread one instead (the unused check says so).
            .filter(|b| !(is_unused(snapshot, b) && a.reads.is_some_and(|r| r > 0.0)));
        if let Some(b) = keeper {
            out.push((a, b));
        }
    }
    out
}
