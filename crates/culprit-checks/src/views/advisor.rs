//! The index advisor: one consolidated plan per database, built from a report with the same logic as the index checks
//! (so the advisor and the findings never disagree). Indexes to add, ordered by the database time of the queries that
//! need them; indexes to clean up; and tables whose shape costs reads or writes.

use std::collections::HashSet;

use culprit_core::{
    AdviceQuery, CandidateSource, Capability, CleanupKind, Engine, IndexAdvice, IndexCandidate, IndexCleanup,
    MergedRequest, QueryStat, Report, Snapshot, TableAdvice, TableAdviceKind, format,
};

use crate::index_health::{hide_index_sql, overlapping_pairs};
use crate::indexes::{
    asking_queries, backs_foreign_key, candidate_fix, is_unused, merge_candidates, redundant_indexes, same_keys,
    scan_groups, widen_fix, write_cost, writes_per_day,
};
use crate::support::*;

const MAX_INCLUDE: usize = 6;

/// An index the engine builds for itself (`culprit_core::insights::engine_built_indexes`), with the statements whose
/// plans build it.
struct EngineBuiltGroup<'a> {
    schema: String,
    table: String,
    /// The table as the plan named it, which the plan-insights finding's id uses.
    plan_table: String,
    keys: Vec<String>,
    queries: Vec<&'a QueryStat>,
}

/// Every index the application's statements make the engine build for itself, by table and keys.
fn engine_built(s: &Snapshot) -> Vec<EngineBuiltGroup<'_>> {
    let mut out: Vec<EngineBuiltGroup> = Vec::new();
    for q in app_queries(s) {
        let Some(plan) = &q.plan else { continue };
        for built in culprit_core::insights::engine_built_indexes(plan) {
            let leaf = built.table.rsplit('.').next().unwrap_or(&built.table);
            // The plan names the table; the snapshot says which schema it's in.
            let Some(table) = s.tables.iter().find(|t| t.name.eq_ignore_ascii_case(leaf)) else { continue };
            match out.iter_mut().find(|g| {
                same_table(&g.schema, &g.table, &table.schema, &table.name)
                    && lower_set(&g.keys) == lower_set(&built.keys)
            }) {
                Some(g) if !g.queries.iter().any(|x| x.id == q.id) => g.queries.push(q),
                Some(_) => {}
                None => out.push(EngineBuiltGroup {
                    schema: table.schema.clone(),
                    table: table.name.clone(),
                    plan_table: built.table.clone(),
                    keys: built.keys.clone(),
                    queries: vec![q],
                }),
            }
        }
    }
    out
}

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
        // An index the table has would serve these queries if they wrote its filter as a literal: only what it can't do
        // then is left to create, filtered like it.
        let parameterized = crate::indexes::parameterized_filter(s, &c, &asking);
        // A table too small for an index to pay: the missing-index finding says there's nothing to create.
        if parameterized.is_none() && crate::indexes::too_small_to_index(s, &c, &asking) {
            continue;
        }
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
        let (index_keys, include, sql, widens) = match &parameterized {
            Some(p) => {
                let remainder = p.remainder(&c.include);
                let Some((sql, widens)) = p.remainder_sql(s, &remainder) else { continue };
                let columns = format::list(&p.columns());
                reason.push_str(&format!(
                    " {} {columns} as a parameter, which keeps SQL Server off {}, filtered to {}; written as a literal, ",
                    if asking.len() == 1 { "The query passes" } else { "The queries pass" },
                    p.index.name,
                    p.literals()
                ));
                verify.splice(
                    0..2,
                    [
                        format!(
                            "Write {} as a literal in the queries first, then re-scan: the request either disappears or asks for this.",
                            p.literals()
                        ),
                        format!(
                            "The queries listed seek on {} and read far fewer pages per run.",
                            widens.as_deref().unwrap_or("the new index")
                        ),
                    ],
                );
                match remainder {
                    crate::indexes::Remainder::Index { keys, include } => {
                        reason.push_str(&format!(
                            "it finds the rows by {}, not by {}. This index is the rest of the request, filtered the same way.",
                            format::list(&keys[..keys.len() - p.missing_range.len()]),
                            format::list(&p.missing_range)
                        ));
                        (keys, include, Some(sql), widens)
                    }
                    crate::indexes::Remainder::Include(missing) => {
                        reason.push_str(&format!(
                            "it serves them, reading {} from the table for each row. Including {} is the rest of the request.",
                            format::list(&missing),
                            if missing.len() == 1 { "it" } else { "them" }
                        ));
                        (p.index.key_names().iter().map(|k| k.to_string()).collect(), missing, Some(sql), widens)
                    }
                    crate::indexes::Remainder::Nothing => continue,
                }
            }
            None => {
                // The same fix the missing-index finding gives: widen or bring back the index with these keys, or
                // create one.
                let fix = candidate_fix(s, &c, &asking);
                // A fix that rebuilds an existing index keeps that index's keys and adds the rest as included columns:
                // the candidate shows what its statement builds.
                let rebuilt = fix.fix.sql.as_ref().and(fix.widens.as_ref()).and_then(|name| {
                    s.indexes.iter().find(|i| i.schema == c.schema && i.table == c.table && &i.name == name)
                });
                match rebuilt {
                    Some(i) => {
                        let held = |col: &String| {
                            i.key_names()
                                .into_iter()
                                .chain(i.include.iter().map(String::as_str))
                                .any(|h| h.eq_ignore_ascii_case(col))
                        };
                        let mut include = i.include.clone();
                        for col in keys.iter().chain(&c.include) {
                            if !held(col) && !include.iter().any(|h| h.eq_ignore_ascii_case(col)) {
                                include.push(col.clone());
                            }
                        }
                        (i.key_names().iter().map(|k| k.to_string()).collect(), include, fix.fix.sql, fix.widens)
                    }
                    None => (keys.clone(), c.include.clone(), fix.fix.sql, fix.widens),
                }
            }
        };
        // A widen rebuilds an existing index under its own name: the queries seek that one. With no query behind the
        // request, there is no list to look at.
        if parameterized.is_none() {
            verify[1] = format!(
                "The queries listed seek on {} and read far fewer pages per run.",
                widens.as_deref().unwrap_or("the new index")
            );
        }
        if asking.is_empty() {
            verify.remove(1);
        }
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
            keys: index_keys,
            include,
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

    // 2. Plans that read a whole table to keep a few rows (every engine with captured plans), as the refined findings
    // have them: a scan the refine stage folded into a missing-index request lends that candidate its queries, and one
    // a rewrite fixes instead (a function or a leading wildcard on the column) gets no index of its own.
    for g in scan_groups(s) {
        let table_name = format!("{}.{}", g.schema, g.table);
        let group_queries: Vec<&QueryStat> = g.queries.iter().map(|(q, _)| *q).collect();
        let finding_id = format!("filtered-scan:{table_name}({})", g.keys.join(","));
        if !ids.contains(finding_id.as_str()) {
            let absorbed_by = report.findings.iter().find(|f| {
                f.check == "missing-index"
                    && g.queries
                        .iter()
                        .all(|(q, node)| f.queries.iter().any(|r| r.id == q.id && r.plan_node == Some(*node)))
            });
            if let Some(existing) =
                absorbed_by.and_then(|f| candidates.iter_mut().find(|c| c.finding_id.as_deref() == Some(&f.id)))
            {
                for q in &group_queries {
                    if !existing.queries.iter().any(|x| x.id == q.id) {
                        existing.queries.push(advice_query(s, q));
                    }
                }
            }
            continue;
        }
        let share = query_impact(s, &group_queries).map(|i| i.share);
        // A disabled index with these keys is brought back rather than built twice.
        let disabled = crate::indexes::disabled_leading_index(s, &g);
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
            sql: match disabled {
                Some(i) => widen_fix(s, i, &[]).sql,
                None => Some(create_index_sql(s, &g.schema, &g.table, &g.keys, &[])),
            },
            widens: disabled.map(|i| i.name.clone()),
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
            finding_id: link(finding_id),
        });
    }

    // 3. Indexes the engine builds for itself on every run because none fits (SQLite's automatic index): its own verdict,
    // like the optimizer's requests, weighed by the statements that make it build one. Never one an index already leads
    // with (the engine had another reason), and merged into a candidate with the same keys.
    for g in engine_built(s) {
        let table_name = format!("{}.{}", g.schema, g.table);
        if g.keys.is_empty() || crate::indexes::leading_index(s, &g.schema, &g.table, &g.keys, &g.keys).is_some() {
            continue;
        }
        if let Some(existing) = candidates.iter_mut().find(|c| {
            same_table(&c.schema, &c.table, &g.schema, &g.table)
                && c.keys.len() >= g.keys.len()
                && lower_set(&c.keys[..g.keys.len()]) == lower_set(&g.keys)
        }) {
            for q in &g.queries {
                if !existing.queries.iter().any(|x| x.id == q.id) {
                    existing.queries.push(advice_query(s, q));
                }
            }
            continue;
        }
        let share = query_impact(s, &g.queries).map(|i| i.share);
        let mut reason = format!(
            "{} builds an index on {} of {} for itself every time {} runs, because none fits, and drops it after. \
             Created once, it saves that work on every run.",
            engine.label(),
            format::list(&g.keys),
            table_name,
            if g.queries.len() == 1 { "one query".to_string() } else { format!("one of {} queries", g.queries.len()) }
        );
        if let Some(share) = share {
            reason.push_str(&format!(
                " {} {} of database time.",
                if g.queries.len() == 1 { "It takes" } else { "They take" },
                format::percent(share)
            ));
        }
        candidates.push(IndexCandidate {
            schema: g.schema.clone(),
            table: g.table.clone(),
            keys: g.keys.clone(),
            include: Vec::new(),
            source: CandidateSource::EngineBuilt,
            reason,
            sql: Some(create_index_sql(s, &g.schema, &g.table, &g.keys, &[])),
            widens: None,
            queries: g.queries.iter().map(|q| advice_query(s, q)).collect(),
            requests: Vec::new(),
            db_time_share: share,
            lookups: None,
            estimated_improvement: None,
            table_rows: s.table(&g.schema, &g.table).and_then(|t| t.rows),
            writes_per_day: writes_per_day(s, &g.schema, &g.table),
            write_cost: write_cost(s, &g.schema, &g.table),
            verify: vec![
                format!(
                    "Explain one of the queries again: no automatic index on {}, and a search using the new one.",
                    g.table
                ),
                "Re-scan after the workload has run: the queries' share of database time drops.".into(),
            ],
            finding_id: link(format!("plan-insights:automatic-index:{}", g.plan_table.to_ascii_lowercase())),
        });
    }

    // 4. Foreign keys no index leads with, among those the database checks.
    for fk in crate::indexes::enforced_foreign_keys(s) {
        let rows = s.table(&fk.schema, &fk.table).and_then(|t| t.rows).unwrap_or(0.0);
        if rows < crate::indexes::FK_OWN_FINDING_ROWS {
            continue;
        }
        if crate::indexes::fk_is_indexed(s, fk) {
            continue;
        }
        let through_key: Vec<&QueryStat> = queries_touching(s, &fk.schema, &fk.table)
            .into_iter()
            .filter(|q| uses_foreign_key(engine, &q.text, fk))
            .collect();
        // Weight comes only from statements whose plan shows no index serving the key (see `plan_reads_whole`).
        let slowed: Vec<&QueryStat> = through_key
            .iter()
            .copied()
            .filter(|q| crate::indexes::plan_reads_whole(s, q, &fk.table, &fk.columns))
            .collect();
        // A candidate that already leads with the key serves it: its statements are this key's too, and so is its weight.
        // Not a filtered one (it can't serve a delete on the parent), nor one whose statements all stopped (it leaves
        // the list, and the key's index would go with it).
        let history = |c: &IndexCandidate| {
            !c.queries.is_empty()
                && c.queries.iter().all(|a| {
                    s.workload.as_ref().and_then(|w| w.query(&a.id)).is_some_and(|q| crate::query_is_history(s, q))
                })
        };
        if let Some(planned) = candidates.iter_mut().find(|c| {
            same_table(&c.schema, &c.table, &fk.schema, &fk.table)
                && c.keys.len() >= fk.columns.len()
                && lower_set(&c.keys[..fk.columns.len()]) == lower_set(&fk.columns)
                && !c.sql.as_deref().is_some_and(|sql| sql.to_ascii_uppercase().contains(" WHERE "))
                && !history(c)
        }) {
            let added = slowed.iter().filter(|q| !planned.queries.iter().any(|x| x.id == q.id)).count();
            if added > 0 {
                for q in &slowed {
                    if !planned.queries.iter().any(|x| x.id == q.id) {
                        planned.queries.push(advice_query(s, q));
                    }
                }
                let all: Vec<&QueryStat> =
                    planned.queries.iter().filter_map(|a| s.workload.as_ref()?.query(&a.id)).collect();
                planned.db_time_share = query_impact(s, &all).map(|i| i.share).or(planned.db_time_share);
                planned.reason.push_str(&format!(
                    " It is also the foreign key to {}, which {} more {} filter or join on{}.",
                    fk.ref_table,
                    added,
                    if added == 1 { "query" } else { "queries" },
                    planned
                        .db_time_share
                        .map(|s| format!(": together they take {} of database time", format::percent(s)))
                        .unwrap_or_default()
                ));
            }
            // The refine stage folds an engine-built index's finding into the key's: link to the one that's left.
            if planned.finding_id.is_none() {
                planned.finding_id = link(format!("unindexed-fk:{}.{}({})", fk.schema, fk.table, fk.columns.join(",")));
            }
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
                 {} looking for matches, and joins from {} can't seek.{}",
                format::list(&fk.columns),
                fk.ref_table,
                fk.ref_table,
                format::count(rows),
                child,
                fk.ref_table,
                crate::indexes::parent_never_deletes(s, fk).map(|n| format!(" {n}")).unwrap_or_default()
            ),
            sql: Some(create_index_sql(s, &fk.schema, &fk.table, &fk.columns, &[])),
            widens: None,
            queries: through_key.iter().take(5).map(|q| advice_query(s, q)).collect(),
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
        |id: &str| s.workload.as_ref().and_then(|w| w.query(id)).is_some_and(|q| crate::query_is_history(s, q));
    for c in candidates.iter_mut() {
        // A query that stopped running, or ran once, doesn't need the index; keep the list to the ones that still run. A
        // foreign key's index is for deletes on the parent, so it stays when the queries naming it all stopped. Their
        // time leaves the candidate's share with them.
        if c.source == CandidateSource::ForeignKey || c.queries.iter().any(|q| !stopped(&q.id)) {
            let gone: Vec<&QueryStat> =
                c.queries.iter().filter(|q| stopped(&q.id)).filter_map(|q| s.workload.as_ref()?.query(&q.id)).collect();
            c.queries.retain(|q| !stopped(&q.id));
            if let (Some(share), Some(gone)) = (c.db_time_share, query_impact(s, &gone)) {
                c.db_time_share = Some((share - gone.share).max(0.0)).filter(|s| *s > 0.0);
            }
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
    // By table and name: MySQL and SQL Server index names are unique only within a table.
    let listed: Vec<(String, String, String)> =
        cleanups.iter().map(|c| (c.schema.clone(), c.table.clone(), c.index.clone())).collect();
    let mysql = engine == Engine::Mysql;
    for i in s
        .indexes
        .iter()
        .filter(|i| is_unused(s, i) && !listed.contains(&(i.schema.clone(), i.table.clone(), i.name.clone())))
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
    // A partition carries its parent's indexes and is read with it: the findings judge the parent, and so does this.
    for t in s.tables.iter().filter(|t| t.partition_of.is_none()) {
        let table_name = t.qualified();
        // Listed where the scan-heavy check found it worth a finding (its bars, and full reads that aren't all from
        // statements that read every row on purpose), with the statements that finding lists, so the two agree.
        if let Some(f) = report.findings.iter().find(|f| f.id == format!("scan-heavy:{table_name}"))
            && let Some((scans_total, read_total, days)) = crate::index_health::full_scans_measured(s, t)
        {
            let (scans, rows_read) = (scans_total / days, read_total / days);
            let listed: Vec<&QueryStat> = f.queries.iter().filter_map(|r| s.workload.as_ref()?.query(&r.id)).collect();
            tables.push(TableAdvice {
                kind: TableAdviceKind::ScanHeavy,
                schema: t.schema.clone(),
                table: t.name.clone(),
                reason: format!(
                    "Read in full {} ({} rows without an index). {}",
                    how_many(s, scans_total, days, "time", "times"),
                    format::count(read_total),
                    match listed.len() {
                        0 => "None of the captured statements shows which filter has no index: the full reads come \
                              from statements outside the top of the workload."
                            .to_string(),
                        1 => "Explain the statement listed with it to see which filter has no index.".to_string(),
                        _ => "Explain the statements listed with it to see which filter has no index.".to_string(),
                    }
                ),
                rows: t.rows,
                indexes: Some(s.indexes_on(&t.schema, &t.name).count() as f64),
                writes_per_day: writes_per_day(s, &t.schema, &t.name),
                full_scans_per_day: per_day_is_measured(days).then_some(scans),
                rows_scanned_per_day: per_day_is_measured(days).then_some(rows_read),
                forwarded_per_day: None,
                sql: None,
                queries: listed.iter().map(|q| advice_query(s, q)).collect(),
                finding_id: Some(f.id.clone()),
            });
        }
        // Listed where the heap-table check found it worth a finding (its size and rate bars), so the two agree.
        if t.is_heap == Some(true)
            && let Some(forwarded) = t.forwarded_fetches.filter(|f| *f > 0.0)
            && report.findings.iter().any(|f| f.id == format!("heap-table:{table_name}"))
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
        // As the over-indexed finding judges it: writes that maintain indexes (PostgreSQL's HOT updates write none), in
        // the engine's unit, a day.
        if secondary >= crate::index_health::MANY_INDEXES
            && let Some(w) = crate::index_health::index_writes(s, t)
            && w.total / w.days >= crate::index_health::MANY_WRITES_A_DAY
        {
            let writes = w.total / w.days;
            tables.push(TableAdvice {
                kind: TableAdviceKind::OverIndexed,
                schema: t.schema.clone(),
                table: t.name.clone(),
                reason: format!(
                    "{secondary} secondary indexes and {}: every insert updates all of them.",
                    how_many(s, w.total, w.days, w.unit().0, w.unit().1)
                ),
                rows: t.rows,
                indexes: Some(secondary as f64),
                // A number a day only over a day's worth of counters.
                writes_per_day: per_day_is_measured(w.days).then_some(writes),
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
    // Where candidates come from on this engine, as the engine says it (no missing-index requests, no history…).
    notes.extend(engine.spec().index_advice_sources(&s.server, s.workload.is_some()));
    if !s.server.has(Capability::IndexUsage) {
        notes.push("Index usage counters aren't readable, so unused and overlapping indexes can't be judged.".into());
    } else if !crate::indexes::watched_long_enough(s) {
        notes.push(format!(
            "Index usage counters have counted {}; an index has to go unread for a week before it's listed to clean up.",
            counter_days(s).map_or_else(|| "for an unknown time".to_string(), |d| format!("for {}", format::duration(d * 86_400_000.0)))
        ));
    }
    if !s.server.has(Capability::QueryPlans) {
        notes.push("No plans were captured, so plan-based candidates are missing.".into());
    }
    let partition_copies = s.indexes.iter().filter(|i| crate::indexes::is_partition_copy(i)).count();
    if partition_copies > 0 {
        notes.push(format!(
            "{} {} on partitions {} left out of the clean-ups: each is a partition's copy of its partitioned table's index, a cold partition reads as unused because queries skip it, and a copy can't be dropped on its own.",
            partition_copies,
            if partition_copies == 1 { "index" } else { "indexes" },
            if partition_copies == 1 { "is" } else { "are" }
        ));
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
            "Left out 1 candidate for a table that no longer exists or queries that stopped running or ran once.".to_string()
        } else {
            format!(
                "Left out {skipped} candidates for tables that no longer exist or queries that stopped running or ran once."
            )
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

/// Whether a statement on the child table goes through the foreign key: it names a key column, deletes from or
/// updates the parent (the database checks the key then), or joins the two tables (a column of one compared with a
/// column of the other). Naming the parent alone isn't enough: `SELECT (SELECT count(*) FROM orders), (SELECT count(*)
/// FROM order_items)` uses no key. Utility statements (DDL, EXPLAIN, maintenance) never count, even when they mention
/// them.
pub(crate) fn uses_foreign_key(engine: Engine, text: &str, fk: &culprit_core::ForeignKey) -> bool {
    let lower = text.to_lowercase();
    let first = lower.trim_start().split(|c: char| !c.is_alphanumeric()).next().unwrap_or("");
    if matches!(first, "create" | "alter" | "drop" | "explain" | "vacuum" | "analyze" | "copy" | "truncate" | "reindex")
    {
        return false;
    }
    // A read that only returns the key column doesn't search by it: look from FROM on (joins, filters, subqueries).
    let searched = match first {
        "select" | "with" => lower.find(" from ").map_or(lower.as_str(), |at| &lower[at..]),
        _ => lower.as_str(),
    };
    if fk.columns.iter().any(|c| has_word(searched, &c.to_lowercase())) {
        return true;
    }
    let leaf = |name: &str| name.rsplit('.').next().unwrap_or(name).to_string();
    let (parent, child) = (fk.ref_table.to_lowercase(), fk.table.to_lowercase());
    let tables = culprit_core::sql::tables_in(text, engine);
    // A delete or an update whose target is the parent.
    let main = crate::support::main_statement(text).trim_start().to_lowercase();
    if (main.starts_with("delete") || main.starts_with("update") || main.starts_with("merge"))
        && tables.first().is_some_and(|t| leaf(t) == parent)
    {
        return true;
    }
    // A column of the parent compared with a column of the child (`i.order_id = o.id`), each qualified by its table or
    // an alias the statement gives it.
    let aliases = culprit_core::sql::table_aliases(text, engine);
    let table_of = |column: &str| -> Option<String> {
        let (qualifier, _) = column.rsplit_once('.')?;
        let qualifier = leaf(qualifier);
        Some(aliases.iter().find(|(alias, _)| *alias == qualifier).map_or(qualifier, |(_, name)| leaf(name)))
    };
    let plain: String = lower.chars().filter(|c| !matches!(c, '"' | '`' | '[' | ']')).collect();
    let name = |c: char| c.is_alphanumeric() || matches!(c, '_' | '.' | '$');
    plain.match_indices('=').any(|(at, _)| {
        let before = plain[..at].trim_end();
        if before.ends_with(['<', '>', '!']) {
            return false;
        }
        let after = plain[at + 1..].trim_start();
        let left = &before[before.rfind(|c: char| !name(c)).map_or(0, |p| p + 1)..];
        let right = &after[..after.find(|c: char| !name(c)).unwrap_or(after.len())];
        match (table_of(left), table_of(right)) {
            (Some(a), Some(b)) => (a == parent && b == child) || (a == child && b == parent),
            _ => false,
        }
    })
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
        let fk = culprit_core::ForeignKey {
            name: "fk_items_order".into(),
            schema: "shop".into(),
            table: "order_items".into(),
            columns: vec!["order_id".into()],
            ref_schema: "shop".into(),
            ref_table: "orders".into(),
            ref_columns: vec!["id".into()],
        };
        let uses = |text: &str| super::uses_foreign_key(culprit_core::Engine::Postgres, text, &fk);
        assert!(uses("SELECT * FROM order_items WHERE order_id = $1"));
        assert!(uses("DELETE FROM orders o USING order_items i WHERE i.x = o.id"));
        assert!(uses("DELETE FROM orders WHERE id = $1"));
        assert!(!uses("SELECT * FROM order_items WHERE product_id = $1"));
        assert!(!uses("SELECT suborder_id FROM order_items"));
        assert!(!uses("SELECT id, order_id FROM order_items WHERE sku = $1"));
        assert!(uses("SELECT i.id FROM orders o JOIN order_items i ON i.order_id = o.id"));
        assert!(uses("SELECT i.id FROM orders o JOIN order_items i ON o.id = i.parent"));
        assert!(!uses("CREATE INDEX culprit_tmp ON order_items (order_id)"));
        assert!(!uses("EXPLAIN (ANALYZE) SELECT * FROM order_items WHERE order_id = 1"));
        // MS-6: a statement that counts both tables, each on its own, goes through no key.
        assert!(!uses(
            "SELECT $1||(SELECT count(*) FROM orders)||$2||(SELECT count(*) FROM order_items)||$3||(SELECT count(*) FROM products)"
        ));
        assert!(!uses("SELECT count(*) FROM order_items i WHERE i.quantity = (SELECT max(o.total) FROM orders o)"));
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
