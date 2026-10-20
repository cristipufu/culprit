//! The `unindexed-fk` check: Foreign key columns with no index starting with them, which makes deletes on the parent and joins from the child slow.

use super::*;
use crate::evidence::Requirement as Evidence;

pub fn unindexed_fk_info() -> CheckInfo {
    CheckInfo {
        id: "unindexed-fk".into(),
        title: "Foreign keys without an index".into(),
        category: Category::Indexes,
        engines: vec![Engine::Mssql, Engine::Postgres, Engine::Mysql, Engine::Sqlite],
        requires: vec![Capability::TableStats],
        description: "Foreign key columns with no index starting with them, which makes deletes on the parent and joins from the child slow.".into(),
    }
}

check!(
    UnindexedFkCheck: unindexed_fk_info, unindexed_fk,
    &[Evidence::IndexCatalog, Evidence::ForeignKeys, Evidence::Unknown]
);

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

pub(super) fn leads_with(i: &IndexStat, columns: &[String]) -> bool {
    let keys = i.key_names();
    keys.len() >= columns.len()
        && lower_set(&keys[..columns.len()].iter().map(|s| s.to_string()).collect::<Vec<_>>()) == lower_set(columns)
}

/// `([customer_id] IS NOT NULL)`, `(a IS NOT NULL AND b IS NOT NULL)`: a filter whose every condition is one of the key
/// columns being NOT NULL.
pub(super) fn only_excludes_null_keys(filter: &str, columns: &[String]) -> bool {
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

/// A child table this big, read in full for each parent row deleted this many times a day, is Medium: about half a
/// second a read of a million rows makes it a minute a day, Medium's amount of time.
const FK_MEDIUM_ROWS: f64 = 1_000_000.0;
const FK_MEDIUM_PARENT_DELETES_A_DAY: f64 = 100.0;

/// An index led by the key's columns that its filter keeps from serving the key.
pub(super) fn filtered_fk_index<'a>(snapshot: &'a Snapshot, fk: &'a culprit_core::ForeignKey) -> Option<&'a IndexStat> {
    snapshot.indexes_on(&fk.schema, &fk.table).find(|i| {
        leads_with(i, &fk.columns) && i.filter.as_deref().is_some_and(|f| !only_excludes_null_keys(f, &fk.columns))
    })
}

/// Whether the database checks the key when a parent row is deleted or re-keyed. SQL Server's NOCHECK CONSTRAINT
/// switches a foreign key off entirely (`sys.foreign_keys.is_disabled`, read with the untrusted constraints): nothing
/// looks for child rows then, so nothing needs an index for it. A key that is only untrusted (added WITH NOCHECK) is
/// still checked on every delete.
pub(crate) fn fk_is_enforced(snapshot: &Snapshot, fk: &culprit_core::ForeignKey) -> bool {
    !snapshot.constraints.iter().any(|c| {
        c.kind == culprit_core::ConstraintKind::ForeignKey
            && c.disabled
            && c.name.eq_ignore_ascii_case(&fk.name)
            && c.schema.eq_ignore_ascii_case(&fk.schema)
            && c.table.eq_ignore_ascii_case(&fk.table)
    })
}

/// The foreign keys the database checks (see [`fk_is_enforced`]) that have columns.
pub(crate) fn enforced_foreign_keys(snapshot: &Snapshot) -> impl Iterator<Item = &culprit_core::ForeignKey> {
    snapshot.foreign_keys.iter().filter(|fk| !fk.columns.is_empty() && fk_is_enforced(snapshot, fk))
}

/// Every foreign key no index can serve, with its child table's rows when known, largest first. A key repeated on the
/// same columns (a duplicate constraint), or whose columns lead another unindexed key on the same table (one index on
/// the longer key serves both), is listed once. A key the database doesn't check isn't listed.
/// Whether `q`'s captured plan shows no index serving `table` by `columns`: a step that reads the table in full looking
/// for rows by the key (its filter names a key column and keeps few of the rows it reads), or an index the engine
/// builds on those columns for itself on every run. The engine's own account, never the text alone; a statement without
/// a captured plan shows nothing. A full read that filters on something else, or on nothing (a report over every row,
/// the build side of a hash join), wouldn't use an index on the key. On SQLite, whose plans name no conditions, a
/// search through another index that isn't unique, by something other than the key, counts too when the statement
/// compares the key with a value: it reads every row that index finds and checks the key on each (`customer_id = ? AND
/// status = ?` searching an index on status).
pub(crate) fn plan_reads_whole(snapshot: &Snapshot, q: &QueryStat, table: &str, columns: &[String]) -> bool {
    let Some(plan) = &q.plan else { return false };
    let leaf = |name: &str| {
        name.rsplit('.')
            .next()
            .unwrap_or(name)
            .trim_matches(|c| matches!(c, '[' | ']' | '"' | '`'))
            .to_ascii_lowercase()
    };
    let table = leaf(table);
    // A key column as a whole word in `text`, and with `valued`, compared with a value (`customer_id" = ?`), not with
    // another column (a join's ON).
    let names = |text: &str, valued: bool| {
        let text = text.to_ascii_lowercase();
        columns.iter().any(|c| {
            let c = c.to_ascii_lowercase();
            text.match_indices(&c).any(|(at, _)| {
                let word = |ch: Option<char>| ch.is_some_and(|ch| ch.is_ascii_alphanumeric() || ch == '_');
                let rest = &text[at + c.len()..];
                if word(text[..at].chars().last()) || word(rest.chars().next()) {
                    return false;
                }
                !valued || {
                    let rest = rest.trim_start_matches(['"', '`', ']']).trim_start();
                    rest.strip_prefix('=').is_some_and(|v| {
                        v.trim_start()
                            .starts_with(|ch: char| matches!(ch, '?' | '$' | '\'' | ':' | '@') || ch.is_ascii_digit())
                    }) || rest.starts_with("in (")
                }
            })
        })
    };
    // SQLite's plans name no conditions: there the statement says what it looks the rows up by.
    let sqlite = plan.engine == culprit_core::Engine::Sqlite;
    let by_key = |n: &culprit_core::PlanNode| {
        let names_key = if sqlite {
            names(&q.text, true)
        } else {
            [n.predicate.as_deref(), n.seek_predicate.as_deref()].into_iter().flatten().any(|f| names(f, false))
        };
        names_key && n.kept_share().is_none_or(|k| k <= culprit_core::plan::INDEX_KEEPS_AT_MOST)
    };
    // An index of the table's in the catalog that holds a key value more than once: a search through it can find many
    // rows. The rowid and primary key (SQLite's plan names them so) find one.
    let not_unique = |index: &str| {
        snapshot
            .indexes
            .iter()
            .find(|i| leaf(&i.table) == table && i.name.eq_ignore_ascii_case(index))
            .is_some_and(|i| !i.is_unique && !i.is_primary_key)
    };
    let searches_past_key = |n: &culprit_core::PlanNode| {
        sqlite
            && n.kind == NodeKind::Seek
            && n.index.as_deref().is_some_and(not_unique)
            && !n.seek_predicate.as_deref().is_some_and(|s| names(s, false))
            && names(&q.text, true)
    };
    plan.nodes().iter().any(|n| {
        n.object.as_deref().is_some_and(|o| leaf(o) == table)
            && ((n.kind == NodeKind::Scan && by_key(n)) || searches_past_key(n))
    }) || culprit_core::insights::engine_built_indexes(plan).iter().any(|b| {
        leaf(&b.table) == table
            && b.keys.len() >= columns.len()
            && b.keys.iter().zip(columns).all(|(k, c)| k.eq_ignore_ascii_case(c))
    })
}

pub(crate) fn unindexed_foreign_keys(snapshot: &Snapshot) -> Vec<(&culprit_core::ForeignKey, Option<f64>)> {
    if crate::evidence::index_catalog(snapshot).is_some() {
        return Vec::new();
    }
    let keys = |fk: &culprit_core::ForeignKey| fk.columns.iter().map(|c| c.to_ascii_lowercase()).collect::<Vec<_>>();
    let same_table = |a: &culprit_core::ForeignKey, b: &culprit_core::ForeignKey| {
        a.schema.eq_ignore_ascii_case(&b.schema) && a.table.eq_ignore_ascii_case(&b.table)
    };
    // A foreign key on a partition is the partitioned table's key, copied: judged once, at the parent.
    let is_partition =
        |fk: &culprit_core::ForeignKey| snapshot.table(&fk.schema, &fk.table).is_some_and(|t| t.partition_of.is_some());
    let candidates: Vec<&culprit_core::ForeignKey> =
        enforced_foreign_keys(snapshot).filter(|fk| !fk_is_indexed(snapshot, fk) && !is_partition(fk)).collect();
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

pub(super) fn unindexed_fk_finding(snapshot: &Snapshot, fk: &culprit_core::ForeignKey, rows: f64) -> Finding {
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
    let touching: Vec<&QueryStat> =
        through_foreign_key(snapshot, fk).into_iter().filter(|q| !crate::query_is_history(snapshot, q)).collect();
    let quiet_parent = parent_never_deletes(snapshot, fk);
    // SQLite checks foreign keys only on connections that turn PRAGMA foreign_keys on, which Culprit can't see.
    let sqlite = snapshot.server.engine == Engine::Sqlite;
    // Severity needs an amount, not the table's size alone: the parent's deletes, each a full read of this table, as a
    // rate over an hour or more of counters (or, below, the time of the statements the plans show slowed).
    let parent_deletes = snapshot.table(&fk.ref_schema, &fk.ref_table).and_then(|t| t.deletes).filter(|d| *d > 0.0);
    let deletes_a_day = parent_deletes.and_then(|d| counter_rate_per_day(snapshot, d));
    let measured = deletes_a_day.is_some_and(|d| d >= FK_MEDIUM_PARENT_DELETES_A_DAY);
    f.severity = if rows >= FK_MEDIUM_ROWS && measured && !sqlite { Severity::Medium } else { Severity::Low };
    if let (Some(deletes), Some(days)) = (parent_deletes.filter(|_| measured), counter_days(snapshot)) {
        f.summary.push_str(&format!(
            " Deletes from {parent} came to {}, each a read of the whole of {child}.",
            how_many(snapshot, deletes, days, "row", "rows")
        ));
    }
    if sqlite {
        f.caveats.push(
            "SQLite looks for matching child rows only on connections that enable PRAGMA foreign_keys; queries that join or filter on the key need the index either way."
                .into(),
        );
    }
    f.impact.score = base_score(f.severity) + (rows.log10() - 4.0).max(0.0) * 2.0;
    f.impact.label = format!("{} rows unindexed", format::count(rows));
    // The statements the missing index slows are this finding's (refine folds their heavy-query findings into it), so
    // their database time is its weight too, graded by the same amount rule. Only those whose plan shows it: naming the
    // key in the text isn't enough, since another index can serve the statement or the column can be another table's.
    let slowed: Vec<&QueryStat> =
        touching.iter().copied().filter(|q| plan_reads_whole(snapshot, q, &fk.table, &fk.columns)).collect();
    if let Some(impact) = query_impact(snapshot, &slowed).filter(|i| i.share >= 0.01) {
        let by_rows = f.impact.score;
        apply_query_impact(&mut f, &impact);
        f.impact.score = f.impact.score.max(by_rows);
        f.severity = f.severity.max(severity_for_impact(snapshot, &slowed, &impact));
        f.summary.push_str(&format!(
            " The statements that go through it take {} of database time.",
            format::percent(impact.share)
        ));
    }
    f.confidence = if quiet_parent.is_some() { Confidence::Low } else { Confidence::High };
    if let Some(note) = &quiet_parent {
        f.summary.push(' ');
        f.summary.push_str(note);
    }
    if let Some(note) = lookup_key_caveat(snapshot, fk, rows) {
        f.caveats.push(note);
    }
    f.metrics.push(Metric::new("Rows in table", rows, Unit::Rows));
    f.metrics.push(Metric::text("Foreign key", fk.name.clone()));
    // The statements it claims: those the plans show slowed when there are any (the share and the headline count these),
    // otherwise the ones that name the key with no captured plan to say otherwise, as evidence of the relation in use.
    let listed = if slowed.is_empty() { &touching } else { &slowed };
    f.queries = query_refs(&listed.iter().copied().take(5).collect::<Vec<_>>());
    f.fix = Some(Fix {
        summary: format!("Index the foreign key column{} on {}.", if fk.columns.len() > 1 { "s" } else { "" }, child),
        sql: Some(create_index_sql(snapshot, &fk.schema, &fk.table, &fk.columns, &[])),
        cost: write_cost(snapshot, &fk.schema, &fk.table),
    });
    f.verify = fk_verify(snapshot, fk, &slowed);
    if let Some(i) = filtered_fk_index(snapshot, fk) {
        f.caveats.push(filtered_caveat(i, &parent));
    }
    f
}

/// The statements that go through a foreign key, as its finding and the index advisor list them: those that use the
/// key (`advisor::uses_foreign_key`: a key column searched, a delete on the parent, a join of the two tables), except
/// one whose captured plan shows the index wouldn't serve it: a plan that reads the child some other way (the build
/// side of a hash join, a filter on another column) or never reads it at all. A plan that reads the child by the key
/// without an index ([`plan_reads_whole`]) is one it serves; a statement with no captured plan may be, and stays.
pub(crate) fn through_foreign_key<'a>(snapshot: &'a Snapshot, fk: &culprit_core::ForeignKey) -> Vec<&'a QueryStat> {
    queries_touching(snapshot, &fk.schema, &fk.table)
        .into_iter()
        .filter(|q| crate::advisor::uses_foreign_key(snapshot.server.engine, &q.text, fk))
        .filter(|q| q.plan.is_none() || plan_reads_whole(snapshot, q, &fk.table, &fk.columns))
        .collect()
}

/// How to check an unindexed foreign key's fix: where plans show statements reading the child by the key without an
/// index (the share the finding claims), explaining one of them again shows the new index in use, as the engine says
/// it; the next scan's catalog has the key indexed, so the finding is gone (a parent delete then finds the child's
/// rows through it, which no scan shows by itself); and those statements' time a run drops, between two scans on a
/// source of running totals ([`crate::queries::what_changed_between_scans`]).
pub(crate) fn fk_verify(snapshot: &Snapshot, fk: &culprit_core::ForeignKey, slowed: &[&QueryStat]) -> Vec<String> {
    let child = format!("{}.{}", fk.schema, fk.table);
    let parent = format!("{}.{}", fk.ref_schema, fk.ref_table);
    let engine = snapshot.server.engine;
    let name = index_name(&fk.table, &fk.columns);
    let name = if engine == Engine::Mssql { name } else { name.to_ascii_lowercase() };
    let mut out = Vec::new();
    if let Some(first) = slowed.first() {
        // SQLite's EXPLAIN QUERY PLAN says it in its own words (after the table's name or its alias).
        let shown =
            if engine == Engine::Sqlite { format!(" (`SEARCH … USING INDEX {name}`)") } else { String::new() };
        out.push(format!(
            "Explain {} again: {} {child} through {name}{shown} instead of reading all of it.",
            if slowed.len() == 1 { format!("query {}", first.id) } else { "the queries listed".to_string() },
            if slowed.len() == 1 { "it reads" } else { "each reads" }
        ));
    }
    out.push(format!(
        "Re-scan once the index exists: this finding should be gone, and a delete from {parent} then finds the rows of \
         {child} it checks through the index, not by scanning the table."
    ));
    // What the statements behind the share show: their time a run, which on running totals only two scans show.
    if !slowed.is_empty() {
        let one = slowed.len() == 1;
        let (queries, their) = if one { ("the query", "its") } else { ("the queries listed", "their") };
        out.push(
            crate::queries::what_changed_between_scans(snapshot, queries, &format!("{their} time per run should drop"))
                .unwrap_or_else(|| {
                    format!(
                        "Re-scan after the workload has run: {} time per run should drop.",
                        if one { "the query's" } else { "the queries'" }
                    )
                }),
        );
    }
    out
}

/// The sentence that says a key's parent table has deleted nothing, when PostgreSQL or MySQL counted its deletes for
/// at least a week ([`watched_long_enough`]): the check that needs the index runs only when a parent row is deleted or
/// its key changes, so then the index is for joins, not for the check. `None` when it deleted rows, or nobody knows.
pub(crate) fn parent_never_deletes(snapshot: &Snapshot, fk: &culprit_core::ForeignKey) -> Option<String> {
    let parent = snapshot.table(&fk.ref_schema, &fk.ref_table)?;
    if parent.deletes != Some(0.0) || !watched_long_enough(snapshot) {
        return None;
    }
    Some(format!(
        "{}.{} has had no deletes {}{}, though: the database looks for matching rows only when a parent row is deleted \
         or its key changes, so the index would mostly serve joins.",
        fk.ref_schema,
        fk.ref_table,
        since_phrase(snapshot),
        if parent.updates == Some(0.0) { " and no updates" } else { "" }
    ))
}

/// A parent table so small (a lookup table) that each of its keys matches a large share of the child's rows: an index
/// on the key makes a parent delete cheap, but a read of one key's rows would still scan.
pub(super) fn lookup_key_caveat(snapshot: &Snapshot, fk: &culprit_core::ForeignKey, rows: f64) -> Option<String> {
    let parent_rows = snapshot.table(&fk.ref_schema, &fk.ref_table)?.rows.filter(|r| *r >= 1.0)?;
    if parent_rows > 1.0 / culprit_core::INDEX_KEEPS_AT_MOST {
        return None;
    }
    Some(format!(
        "{}.{} has only {} rows, so each of its keys matches about {} rows of {}.{}: the index makes deleting a parent row \
         cheap, but a query that reads one key's rows would still read most of the table.",
        fk.ref_schema,
        fk.ref_table,
        format::count(parent_rows),
        format::count(rows / parent_rows),
        fk.schema,
        fk.table
    ))
}

pub(super) fn filtered_caveat(i: &IndexStat, parent: &str) -> String {
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
pub(super) fn unindexed_fk_pattern(
    snapshot: &Snapshot,
    unindexed: &[(&culprit_core::ForeignKey, Option<f64>)],
) -> Option<Finding> {
    let total = enforced_foreign_keys(snapshot).count();
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
    if snapshot.server.engine == Engine::Sqlite {
        f.caveats.push(
            "SQLite looks for matching child rows only on connections that enable PRAGMA foreign_keys; queries that join or filter on the keys need the indexes either way."
                .into(),
        );
    }
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
