//! The index advisor: one consolidated plan per database, built from a report with the same logic as the index checks
//! (so the advisor and the findings never disagree). Indexes to add, ordered by the database time of the queries that
//! need them; indexes to clean up; and tables whose shape costs reads or writes.

use std::collections::HashSet;

use culprit_core::{
    AdviceQuery, CandidateSource, Capability, CleanupKind, Engine, IndexAdvice, IndexCandidate, IndexCleanup,
    MergedRequest, QueryStat, Report, Snapshot, TableAdvice, TableAdviceKind, format,
};

use crate::index_health::{full_scans, hide_index_sql, overlapping_pairs};
use crate::indexes::{
    asking_queries, backs_foreign_key, existing_match, is_unused, merge_candidates, redundant_indexes, same_keys,
    scan_groups, widen_fix, write_cost, writes_per_day,
};
use crate::support::*;

const MAX_INCLUDE: usize = 6;

fn advice_query(snapshot: &Snapshot, q: &QueryStat) -> AdviceQuery {
    let text: String = q.text.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(240).collect();
    AdviceQuery {
        id: q.id.clone(),
        text,
        share: snapshot.workload.as_ref().map(|w| w.share(q.total_time_ms)),
        runs_per_day: snapshot.workload.as_ref().filter(|w| w.window_hours >= 20.0).map(|w| w.per_day(q.executions)),
    }
}

fn same_table(a_schema: &str, a_table: &str, b_schema: &str, b_table: &str) -> bool {
    a_schema.eq_ignore_ascii_case(b_schema) && a_table.eq_ignore_ascii_case(b_table)
}

pub fn index_advice(report: &Report) -> IndexAdvice {
    let s = &report.snapshot;
    let engine = s.server.engine;
    let ids: HashSet<&str> = report.findings.iter().map(|f| f.id.as_str()).collect();
    let link = |id: String| ids.contains(id.as_str()).then_some(id);
    let queries = s.queries();
    let mut candidates: Vec<IndexCandidate> = Vec::new();

    // 1. What SQL Server's optimizer asked for, merged.
    for mut c in merge_candidates(s) {
        let keys = c.keys();
        let table_name = format!("{}.{}", c.schema, c.table);
        let asking = asking_queries(queries, &c);
        let share = query_impact(s, &asking).map(|i| i.share);
        let lookups = c.seeks + c.scans;
        let mut reason = format!(
            "SQL Server's optimizer asked for it {} times {} and estimates those lookups would be about {:.0}% cheaper.",
            format::count(lookups),
            since_phrase(s),
            c.impact_pct
        );
        if let Some(share) = share {
            reason.push_str(&format!(
                " The {} behind it {} for {} of database time.",
                if asking.len() == 1 { "query".to_string() } else { format!("{} queries", asking.len()) },
                if asking.len() == 1 { "accounts" } else { "account" },
                format::percent(share)
            ));
        }
        let mut verify = vec![
            "Re-scan after the workload has run: the request disappears.".to_string(),
            "The queries listed seek on the new index and read far fewer pages per run.".to_string(),
        ];
        if let Some(note) = crate::indexes::drop_large_includes(s, &c.schema, &c.table, &mut c.include) {
            verify.push(note);
        }
        if c.include.len() > MAX_INCLUDE {
            verify.push(format!(
                "SQL Server also wanted {} more columns included; the statement keeps the keys only, so add INCLUDE \
                 columns just for what the hot queries select.",
                c.include.len()
            ));
            c.include.clear();
        }
        let existing = existing_match(s, &c);
        let (sql, widens) = match existing {
            Some(e) if !c.include.is_empty() => (widen_fix(s, e, &c.include).sql, Some(e.name.clone())),
            Some(e) => (None, Some(e.name.clone())),
            None => (Some(create_index_sql(s, &c.schema, &c.table, &keys, &c.include)), None),
        };
        let requests = c
            .requests
            .iter()
            .map(|m| MergedRequest {
                equality: m.equality.clone(),
                inequality: m.inequality.clone(),
                include: m.include.clone(),
                lookups: m.seeks + m.scans,
                improvement: m.avg_impact_pct / 100.0,
            })
            .collect();
        candidates.push(IndexCandidate {
            schema: c.schema.clone(),
            table: c.table.clone(),
            keys: keys.clone(),
            include: c.include.clone(),
            source: CandidateSource::Optimizer,
            reason,
            sql,
            widens,
            queries: asking.iter().map(|q| advice_query(s, q)).collect(),
            requests,
            db_time_share: share,
            lookups: Some(lookups),
            estimated_improvement: Some(c.impact_pct / 100.0),
            table_rows: s.table(&c.schema, &c.table).and_then(|t| t.rows),
            writes_per_day: writes_per_day(s, &c.schema, &c.table),
            write_cost: write_cost(s, &c.schema, &c.table),
            verify,
            finding_id: link(format!("missing-index:{table_name}({})", keys.join(","))),
        });
    }

    // 2. Plans that read a whole table to keep a few rows (every engine with captured plans).
    for g in scan_groups(s) {
        let table_name = format!("{}.{}", g.schema, g.table);
        let group_queries: Vec<&QueryStat> = g.queries.iter().map(|(q, _)| *q).collect();
        if let Some(existing) = candidates
            .iter_mut()
            .find(|c| same_table(&c.schema, &c.table, &g.schema, &g.table) && lower_set(&c.keys) == lower_set(&g.keys))
        {
            for q in &group_queries {
                if !existing.queries.iter().any(|x| x.id == q.id) {
                    existing.queries.push(advice_query(s, q));
                }
            }
            continue;
        }
        let share = query_impact(s, &group_queries).map(|i| i.share);
        candidates.push(IndexCandidate {
            schema: g.schema.clone(),
            table: g.table.clone(),
            keys: g.keys.clone(),
            include: Vec::new(),
            source: CandidateSource::PlanScan,
            reason: format!(
                "{} {} every row of {} ({} rows) and keep{} only those matching {}; no index starts with {}.",
                if group_queries.len() == 1 {
                    "One query reads".to_string()
                } else {
                    format!("{} queries read", group_queries.len())
                },
                "",
                table_name,
                format::count(g.rows),
                if group_queries.len() == 1 { "s" } else { "" },
                g.predicate.trim(),
                format::list(&g.keys)
            )
            .replace("  ", " "),
            sql: Some(create_index_sql(s, &g.schema, &g.table, &g.keys, &[])),
            widens: None,
            queries: group_queries.iter().map(|q| advice_query(s, q)).collect(),
            requests: Vec::new(),
            db_time_share: share,
            lookups: None,
            estimated_improvement: None,
            table_rows: Some(g.rows),
            writes_per_day: writes_per_day(s, &g.schema, &g.table),
            write_cost: write_cost(s, &g.schema, &g.table),
            verify: vec![
                format!("Explain one of the queries again: the scan of {} becomes a seek on the new index.", g.table),
                "Re-scan after the workload has run: the queries' share of database time drops.".into(),
            ],
            finding_id: link(format!("filtered-scan:{table_name}({})", g.keys.join(","))),
        });
    }

    // 3. Foreign keys no index leads with.
    for fk in s.foreign_keys.iter().filter(|fk| !fk.columns.is_empty()) {
        let rows = s.table(&fk.schema, &fk.table).and_then(|t| t.rows).unwrap_or(0.0);
        if rows < crate::indexes::FK_OWN_FINDING_ROWS {
            continue;
        }
        let covered = crate::indexes::fk_is_indexed(s, fk);
        let planned = candidates.iter().any(|c| {
            same_table(&c.schema, &c.table, &fk.schema, &fk.table)
                && c.keys.len() >= fk.columns.len()
                && lower_set(&c.keys[..fk.columns.len()]) == lower_set(&fk.columns)
        });
        if covered || planned {
            continue;
        }
        let child = format!("{}.{}", fk.schema, fk.table);
        candidates.push(IndexCandidate {
            schema: fk.schema.clone(),
            table: fk.table.clone(),
            keys: fk.columns.clone(),
            include: Vec::new(),
            source: CandidateSource::ForeignKey,
            reason: format!(
                "{} is the foreign key to {}. Without an index, deleting or re-keying a row in {} reads all {} rows of \
                 {} looking for matches, and joins from {} can't seek.",
                format::list(&fk.columns),
                fk.ref_table,
                fk.ref_table,
                format::count(rows),
                child,
                fk.ref_table
            ),
            sql: Some(create_index_sql(s, &fk.schema, &fk.table, &fk.columns, &[])),
            widens: None,
            queries: queries_touching(s, &fk.schema, &fk.table)
                .into_iter()
                .filter(|q| uses_foreign_key(&q.text, &fk.columns, &fk.ref_table))
                .take(5)
                .map(|q| advice_query(s, q))
                .collect(),
            requests: Vec::new(),
            db_time_share: None,
            lookups: None,
            estimated_improvement: None,
            table_rows: Some(rows),
            writes_per_day: writes_per_day(s, &fk.schema, &fk.table),
            write_cost: write_cost(s, &fk.schema, &fk.table),
            verify: vec![format!("Deletes from {} no longer scan {}.", fk.ref_table, fk.table)],
            finding_id: link(format!("unindexed-fk:{child}({})", fk.columns.join(","))),
        });
    }

    // A table the scan no longer lists was dropped or renamed, and an index for queries that all stopped running fixes
    // nothing now: neither belongs on the list of indexes to add. Whether a query stopped is read from its own history,
    // not from which findings went inactive: a schema finding (an unindexed foreign key) can list a stopped query too.
    let inactive: HashSet<&str> = report.findings.iter().filter(|f| f.inactive).map(|f| f.id.as_str()).collect();
    let stopped =
        |id: &str| s.workload.as_ref().and_then(|w| w.query(id)).is_some_and(|q| crate::query_stopped(s, q).is_some());
    for c in candidates.iter_mut() {
        // A query that stopped running doesn't need the index any more; keep the list to the ones that still run. A
        // foreign key's index is for deletes on the parent, so it stays when the queries naming it all stopped.
        if c.source == CandidateSource::ForeignKey || c.queries.iter().any(|q| !stopped(&q.id)) {
            c.queries.retain(|q| !stopped(&q.id));
        }
    }
    let before = candidates.len();
    candidates.retain(|c| {
        let table_known =
            s.tables.is_empty() || s.tables.iter().any(|t| same_table(&t.schema, &t.name, &c.schema, &c.table));
        let all_stopped = c.finding_id.as_deref().is_some_and(|id| inactive.contains(id))
            || (!c.queries.is_empty() && c.queries.iter().all(|q| stopped(&q.id)));
        table_known && !all_stopped
    });
    let skipped = before - candidates.len();
    if engine == Engine::Mssql {
        for c in candidates.iter_mut() {
            c.reason = crate::without_database(&c.reason, &s.server.database);
        }
    }

    candidates.sort_by(|a, b| {
        b.db_time_share
            .unwrap_or(0.0)
            .total_cmp(&a.db_time_share.unwrap_or(0.0))
            .then(b.lookups.unwrap_or(0.0).total_cmp(&a.lookups.unwrap_or(0.0)))
            .then(b.table_rows.unwrap_or(0.0).total_cmp(&a.table_rows.unwrap_or(0.0)))
    });

    // Cleanups: duplicates and prefixes, overlaps, then indexes nothing reads.
    let mut cleanups: Vec<IndexCleanup> = Vec::new();
    for (a, b) in redundant_indexes(s) {
        let exact = same_keys(a, b);
        let table_name = format!("{}.{}", a.schema, a.table);
        // Neither is read: both go, so say "unused" rather than "keep the other one".
        if is_unused(s, b) && a.reads.is_none_or(|r| r == 0.0) {
            cleanups.push(IndexCleanup {
                kind: CleanupKind::Unused,
                schema: a.schema.clone(),
                table: a.table.clone(),
                index: a.name.clone(),
                keys: a.columns.clone(),
                other: Some(b.name.clone()),
                other_keys: b.columns.clone(),
                size_bytes: a.size_bytes,
                reads: a.reads,
                writes: a.writes,
                reason: format!(
                    "Not read once {}, and {} {} (which isn't read either).",
                    since_phrase(s),
                    if exact { "it has the same keys as" } else { "its keys are the start of" },
                    b.name
                ),
                sql: Some(hide_index_sql(&s.server, a)),
                caveats: Vec::new(),
                finding_id: link(format!("duplicate-index:{table_name}.{}", a.name)),
            });
            continue;
        }
        cleanups.push(IndexCleanup {
            kind: if exact { CleanupKind::Duplicate } else { CleanupKind::Prefix },
            schema: a.schema.clone(),
            table: a.table.clone(),
            index: a.name.clone(),
            keys: a.columns.clone(),
            other: Some(b.name.clone()),
            other_keys: b.columns.clone(),
            size_bytes: a.size_bytes,
            reads: a.reads,
            writes: a.writes,
            reason: if exact {
                format!("Same keys as {}: the same index twice, both maintained on every write.", b.name)
            } else {
                format!("Its keys are the start of {}'s, so any seek it serves {} serves too.", b.name, b.name)
            },
            sql: Some(drop_index_sql(engine, &a.schema, &a.table, &a.name)),
            caveats: if exact {
                Vec::new()
            } else {
                vec![format!("{} is narrower; keep it if a hot query scans it rather than seeking.", a.name)]
            },
            finding_id: link(format!("duplicate-index:{table_name}.{}", a.name)),
        });
    }
    for (a, b) in overlapping_pairs(s) {
        let table_name = format!("{}.{}", a.schema, a.table);
        cleanups.push(IndexCleanup {
            kind: CleanupKind::Overlap,
            schema: a.schema.clone(),
            table: a.table.clone(),
            index: a.name.clone(),
            keys: a.columns.clone(),
            other: Some(b.name.clone()),
            other_keys: b.columns.clone(),
            size_bytes: a.size_bytes,
            reads: a.reads,
            writes: a.writes,
            reason: format!(
                "Read {} times next to {}'s {}; both start with {}, so its queries can almost always seek on {} instead.",
                format::count(a.reads.unwrap_or(0.0)),
                b.name,
                format::count(b.reads.unwrap_or(0.0)),
                a.key_names()[0],
                b.name
            ),
            sql: Some(hide_index_sql(&s.server, a)),
            caveats: vec![format!("Check the plans of the queries that read {} before hiding it.", a.name)],
            finding_id: link(format!("overlapping-index:{table_name}.{}", a.name)),
        });
    }
    let listed: Vec<String> = cleanups.iter().map(|c| c.index.clone()).collect();
    let mysql = engine == Engine::Mysql;
    for i in s
        .indexes
        .iter()
        .filter(|i| is_unused(s, i) && !listed.contains(&i.name))
        .filter(|i| !(mysql && backs_foreign_key(s, i).is_some()))
    {
        let table_name = format!("{}.{}", i.schema, i.table);
        let fk = backs_foreign_key(s, i);
        cleanups.push(IndexCleanup {
            kind: CleanupKind::Unused,
            schema: i.schema.clone(),
            table: i.table.clone(),
            index: i.name.clone(),
            keys: i.columns.clone(),
            other: None,
            other_keys: Vec::new(),
            size_bytes: i.size_bytes,
            reads: i.reads,
            writes: i.writes,
            reason: format!(
                "Not read once {}{}, yet updated with every write to {}.",
                since_phrase(s),
                i.writes
                    .filter(|w| *w > 0.0)
                    .map(|w| format!(" while maintained {} times", format::count(w)))
                    .unwrap_or_default(),
                table_name
            ),
            sql: Some(hide_index_sql(&s.server, i)),
            caveats: fk
                .map(|fk| {
                    vec![format!("It supports the foreign key {}; deletes on the parent scan without it.", fk.name)]
                })
                .unwrap_or_default(),
            finding_id: link(format!("unused-index:{table_name}")),
        });
    }
    cleanups.sort_by(|a, b| b.size_bytes.unwrap_or(0.0).total_cmp(&a.size_bytes.unwrap_or(0.0)));

    // Tables whose shape costs reads or writes.
    let mut tables: Vec<TableAdvice> = Vec::new();
    let planned: Vec<(String, String)> =
        scan_groups(s).into_iter().map(|g| (g.schema.to_ascii_lowercase(), g.table.to_ascii_lowercase())).collect();
    for t in &s.tables {
        let table_name = t.qualified();
        if let Some((scans, rows_read)) = full_scans(s, t)
            && scans >= 50.0
            && rows_read >= 5_000_000.0
            && !planned.contains(&(t.schema.to_ascii_lowercase(), t.name.to_ascii_lowercase()))
        {
            let touching: Vec<&QueryStat> =
                queries_touching(s, &t.schema, &t.name).into_iter().filter(|q| !is_monitoring_query(q)).collect();
            tables.push(TableAdvice {
                kind: TableAdviceKind::ScanHeavy,
                schema: t.schema.clone(),
                table: t.name.clone(),
                reason: format!(
                    "Read in full about {} times a day ({} rows a day without an index). {}",
                    format::count(scans),
                    format::count(rows_read),
                    if touching.is_empty() {
                        "No captured statement names it.".to_string()
                    } else {
                        "Explain the statements below to see which filter has no index.".to_string()
                    }
                ),
                rows: t.rows,
                indexes: Some(s.indexes_on(&t.schema, &t.name).count() as f64),
                writes_per_day: writes_per_day(s, &t.schema, &t.name),
                full_scans_per_day: Some(scans),
                rows_scanned_per_day: Some(rows_read),
                forwarded_per_day: None,
                sql: None,
                queries: touching.iter().take(5).map(|q| advice_query(s, q)).collect(),
                finding_id: link(format!("scan-heavy:{table_name}")),
            });
        }
        if t.is_heap == Some(true)
            && let Some(forwarded) = t.forwarded_fetches.filter(|f| *f > 0.0)
        {
            // A per-day rate only from a day's worth of counters (20 hours): less would be an extrapolation.
            let per_day = counter_days(s).filter(|d| *d * 24.0 >= 20.0).map(|d| forwarded / d);
            tables.push(TableAdvice {
                kind: TableAdviceKind::ForwardedFetches,
                schema: t.schema.clone(),
                table: t.name.clone(),
                reason: format!(
                    "A heap: reads followed a forwarding pointer to another page {} times {}. A clustered index ends that.",
                    format::count(forwarded),
                    since_phrase(s)
                ),
                rows: t.rows,
                indexes: Some(s.indexes_on(&t.schema, &t.name).count() as f64),
                writes_per_day: writes_per_day(s, &t.schema, &t.name),
                full_scans_per_day: None,
                rows_scanned_per_day: None,
                forwarded_per_day: per_day,
                sql: None,
                queries: Vec::new(),
                finding_id: link(format!("heap-table:{table_name}")),
            });
        }
        let secondary =
            s.indexes_on(&t.schema, &t.name).filter(|i| !i.is_primary_key && !i.is_clustered && !i.is_disabled).count();
        if secondary >= 7
            && let Some(writes) = writes_per_day(s, &t.schema, &t.name).filter(|w| *w >= 50_000.0)
        {
            tables.push(TableAdvice {
                kind: TableAdviceKind::OverIndexed,
                schema: t.schema.clone(),
                table: t.name.clone(),
                reason: format!(
                    "{secondary} secondary indexes and about {} writes a day: every insert updates all of them.",
                    format::count(writes)
                ),
                rows: t.rows,
                indexes: Some(secondary as f64),
                writes_per_day: Some(writes),
                full_scans_per_day: None,
                rows_scanned_per_day: None,
                forwarded_per_day: None,
                sql: None,
                queries: Vec::new(),
                finding_id: link(format!("over-indexed:{table_name}")),
            });
        }
    }

    let mut notes = Vec::new();
    match engine {
        Engine::Mssql if !s.server.has(Capability::MissingIndexes) => notes.push(
            "This login can't read SQL Server's missing-index requests (VIEW DATABASE STATE), so candidates come from \
             captured plans and foreign keys only."
                .to_string(),
        ),
        Engine::Postgres => notes.push(
            "PostgreSQL doesn't record missing-index requests. Candidates come from captured plans that read whole \
             tables, foreign keys, and the table scan counters."
                .to_string(),
        ),
        Engine::Mysql if !s.server.has(Capability::PerformanceSchema) => notes.push(format!(
            "{} doesn't record missing-index requests. Candidates come from the plans of logged statements and foreign \
             keys; performance_schema would add rows read without an index.",
            s.server.name()
        )),
        Engine::Mysql => notes.push(format!(
            "{} doesn't record missing-index requests. Candidates come from captured plans, foreign keys, and rows \
             read without an index (performance_schema).",
            s.server.name()
        )),
        _ => {}
    }
    if !s.server.has(Capability::IndexUsage) {
        notes.push("Index usage counters aren't readable, so unused and overlapping indexes can't be judged.".into());
    }
    if !s.server.has(Capability::QueryPlans) {
        notes.push("No plans were captured, so plan-based candidates are missing.".into());
    }
    if let Some(f) = report.findings.iter().find(|f| f.id == "unindexed-fk:schema") {
        let threshold = crate::indexes::FK_OWN_FINDING_ROWS;
        let small = crate::indexes::unindexed_foreign_keys(s)
            .iter()
            .filter(|(_, rows)| rows.is_some_and(|r| r < threshold))
            .count();
        notes.push(format!(
            "{} more foreign keys on tables under {} rows have no usable index; the finding \u{201c}{}\u{201d} lists them with the SQL.",
            small,
            format::count(threshold),
            f.title
        ));
    }
    if skipped > 0 {
        notes.push(if skipped == 1 {
            "Left out 1 candidate for a table that no longer exists or queries that stopped running.".to_string()
        } else {
            format!("Left out {skipped} candidates for tables that no longer exist or queries that stopped running.")
        });
    }

    IndexAdvice {
        connection: s.connection.clone(),
        engine: Some(engine),
        taken_at: s.taken_at.clone(),
        counters: since_phrase(s),
        candidates,
        cleanups,
        tables,
        notes,
    }
}

/// Whether a statement on the child table goes through the foreign key: it names a key column or the parent table.
/// Utility statements (DDL, EXPLAIN, maintenance) never count, even when they mention them.
pub(crate) fn uses_foreign_key(text: &str, columns: &[String], parent: &str) -> bool {
    let lower = text.to_lowercase();
    let first = lower.trim_start().split(|c: char| !c.is_alphanumeric()).next().unwrap_or("");
    if matches!(first, "create" | "alter" | "drop" | "explain" | "vacuum" | "analyze" | "copy" | "truncate" | "reindex")
    {
        return false;
    }
    let parent = parent.rsplit('.').next().unwrap_or(parent).trim_matches(|c| matches!(c, '"' | '`' | '[' | ']'));
    // A read that only returns the key column doesn't search by it: look from FROM on (joins, filters, subqueries).
    let searched = match first {
        "select" | "with" => lower.find(" from ").map_or(lower.as_str(), |at| &lower[at..]),
        _ => lower.as_str(),
    };
    columns.iter().map(String::as_str).chain([parent]).any(|name| has_word(searched, &name.to_lowercase()))
}

/// `word` appears in `text` as a whole identifier, not inside a longer one (`order_id` is not in `suborder_id`).
fn has_word(text: &str, word: &str) -> bool {
    if word.is_empty() {
        return false;
    }
    let ident = |c: char| c.is_alphanumeric() || c == '_';
    text.match_indices(word).any(|(at, _)| {
        let before = text[..at].chars().next_back();
        let after = text[at + word.len()..].chars().next();
        !before.is_some_and(ident) && !after.is_some_and(ident)
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn foreign_key_candidates_list_statements_that_use_the_key() {
        let cols = vec!["order_id".to_string()];
        assert!(super::uses_foreign_key("SELECT * FROM order_items WHERE order_id = $1", &cols, "shop.orders"));
        assert!(super::uses_foreign_key("DELETE FROM orders o USING order_items i WHERE i.x = o.id", &cols, "orders"));
        assert!(!super::uses_foreign_key("SELECT * FROM order_items WHERE product_id = $1", &cols, "shop.orders"));
        assert!(!super::uses_foreign_key("SELECT suborder_id FROM order_items", &cols, "shop.orders_archive"));
        assert!(!super::uses_foreign_key("SELECT id, order_id FROM order_items WHERE sku = $1", &cols, "shop.orders"));
        assert!(super::uses_foreign_key(
            "SELECT i.id FROM orders o JOIN order_items i ON i.order_id = o.id",
            &cols,
            "x"
        ));
        assert!(!super::uses_foreign_key("CREATE INDEX culprit_tmp ON order_items (order_id)", &cols, "orders"));
        assert!(!super::uses_foreign_key(
            "EXPLAIN (ANALYZE) SELECT * FROM order_items WHERE order_id = 1",
            &cols,
            "orders"
        ));
    }

    use super::*;

    fn report(name: &str) -> Report {
        let text =
            std::fs::read_to_string(format!("{}/../../fixtures/{name}", env!("CARGO_MANIFEST_DIR"))).expect("fixture");
        serde_json::from_str(&text).expect("report")
    }

    #[test]
    fn advice_matches_the_findings_on_every_demo() {
        for name in ["mssql-demo.report.json", "pg-demo.report.json", "mysql-demo.report.json"] {
            let r = report(name);
            let advice = index_advice(&r);
            let ids: HashSet<&str> = r.findings.iter().map(|f| f.id.as_str()).collect();
            for c in &advice.candidates {
                assert!(
                    c.sql.is_some() || c.widens.is_some(),
                    "{name}: every candidate has DDL or names the index to widen"
                );
                if let Some(id) = &c.finding_id {
                    assert!(ids.contains(id.as_str()), "{name}: candidate links to a real finding");
                }
            }
            for w in advice.candidates.windows(2) {
                assert!(
                    w[0].db_time_share.unwrap_or(0.0) >= w[1].db_time_share.unwrap_or(0.0),
                    "{name}: ordered by DB time"
                );
            }
            assert!(!advice.candidates.is_empty() || !advice.cleanups.is_empty(), "{name}: the demos have index work");
        }
    }

    #[test]
    fn dropped_tables_and_stopped_queries_get_no_new_index() {
        let mut r = report("mssql-demo.report.json");
        let before = index_advice(&r);
        let first = before.candidates.first().expect("a candidate").clone();
        // The table is gone from the scan: its candidate is left out, and a note says so.
        r.snapshot
            .tables
            .retain(|t| !(t.schema.eq_ignore_ascii_case(&first.schema) && t.name.eq_ignore_ascii_case(&first.table)));
        let after = index_advice(&r);
        assert!(after.candidates.iter().all(|c| !(c.schema == first.schema && c.table == first.table)));
        assert!(after.notes.iter().any(|n| n.contains("no longer exist")), "{:?}", after.notes);

        // Every query behind a candidate stopped running: left out too.
        let mut r = report("mssql-demo.report.json");
        let advice = index_advice(&r);
        let with_finding =
            advice.candidates.iter().find(|c| c.finding_id.is_some()).expect("a linked candidate").clone();
        let id = with_finding.finding_id.clone().unwrap();
        for f in r.findings.iter_mut().filter(|f| f.id == id) {
            f.inactive = true;
        }
        let after = index_advice(&r);
        assert!(after.candidates.iter().all(|c| c.finding_id.as_deref() != Some(id.as_str())));
    }

    #[test]
    fn sql_server_candidates_carry_their_queries_and_requests() {
        let advice = index_advice(&report("mssql-demo.report.json"));
        let optimizer: Vec<&IndexCandidate> =
            advice.candidates.iter().filter(|c| c.source == CandidateSource::Optimizer).collect();
        assert!(!optimizer.is_empty());
        assert!(optimizer.iter().all(|c| !c.requests.is_empty()));
        assert!(advice.notes.is_empty() || advice.notes.iter().all(|n| !n.contains("PostgreSQL")));
    }
}
