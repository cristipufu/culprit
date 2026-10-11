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
    let per_day = writes_per_day(snapshot, schema, table)?;
    Some(format!(
        "{schema}.{table} receives about {} writes a day; each one will also update this index.",
        format::count(per_day)
    ))
}

/// Inserts, updates and deletes a day on a table, from the clustered index's update counter (SQL Server) or the
/// table's row counters (Postgres, MySQL), over the days the counters have been accumulating.
pub(crate) fn writes_per_day(snapshot: &Snapshot, schema: &str, table: &str) -> Option<f64> {
    let base = snapshot.indexes_on(schema, table).find(|i| i.is_clustered || i.is_primary_key).and_then(|i| i.writes);
    let row_writes = snapshot
        .table(schema, table)
        .and_then(|t| Some(t.inserts? + t.updates.unwrap_or(0.0) + t.deletes.unwrap_or(0.0)));
    let writes = base.or(row_writes)?;
    let days = counter_days(snapshot).filter(|d| *d >= 0.5)?;
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
            "Indexes that are maintained on every write but no query has read since usage counters were reset.".into(),
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
        .filter(|i| is_unused(i) && !redundant.contains(&i.name.as_str()))
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
            let mut f = finding("unused-index", &table_name, Category::Indexes, subject(SubjectKind::Table, &table_name));
            f.title = match list.as_slice() {
                [one] => format!("{} on {} is never read", one.name, table),
                many => format!("{} indexes on {} are never read", many.len(), table),
            };
            let names = format::list(&list.iter().map(|i| i.name.clone()).collect::<Vec<_>>());
            f.summary = format!(
                "No query has read {} {}{}. {} still has to be updated on every insert, and on updates that touch its columns, and it takes {} of storage and buffer memory.",
                names,
                since_phrase(snapshot),
                writes.map(|w| format!(", while {} was written {} times", table_name, format::count(w))).unwrap_or_default(),
                if list.len() == 1 { "It" } else { "Each one" },
                format::bytes(total_size)
            );
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

pub(crate) fn is_unused(i: &IndexStat) -> bool {
    let is_candidate = !i.is_primary_key && !i.is_unique && !i.is_clustered && !i.is_disabled && i.filter.is_none();
    is_candidate && i.reads == Some(0.0) && i.size_bytes.unwrap_or(0.0) >= 64.0 * 1024.0
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

pub fn unindexed_fk(snapshot: &Snapshot) -> Vec<Finding> {
    snapshot
        .foreign_keys
        .iter()
        .filter(|fk| !fk.columns.is_empty())
        .filter_map(|fk| {
            let rows = snapshot.table(&fk.schema, &fk.table).and_then(|t| t.rows).unwrap_or(0.0);
            if rows < 10_000.0 {
                return None;
            }
            let covered = snapshot.indexes_on(&fk.schema, &fk.table).any(|i| {
                let keys = i.key_names();
                keys.len() >= fk.columns.len()
                    && lower_set(&keys[..fk.columns.len()].iter().map(|s| s.to_string()).collect::<Vec<_>>())
                        == lower_set(&fk.columns)
            });
            if covered {
                return None;
            }
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
            let touching = queries_touching(snapshot, &fk.schema, &fk.table);
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
            f.verify = vec![format!("Deletes from {parent} should no longer scan {child}; re-scan to confirm the finding is gone.")];
            Some(f)
        })
        .collect()
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

fn split_object(object: &str) -> Option<(String, String)> {
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
    let wanted = if equality.is_empty() { &keys[..1] } else { equality };
    snapshot.indexes_on(schema, table).any(|i| {
        let existing = i.key_names();
        existing.len() >= wanted.len()
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
            .filter(|b| !(is_unused(b) && a.reads.is_some_and(|r| r > 0.0)));
        if let Some(b) = keeper {
            out.push((a, b));
        }
    }
    out
}
