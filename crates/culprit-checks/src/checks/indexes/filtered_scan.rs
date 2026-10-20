//! The `filtered-scan` check: Heavy queries whose plan reads a large table in full and keeps only the rows matching a filter that no index supports.

use super::*;
use crate::evidence::Requirement as Evidence;

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

check!(FilteredScanCheck: filtered_scan_info, filtered_scan, &[Evidence::IndexCatalog, Evidence::Unknown]);

pub(crate) struct ScanGroup<'a> {
    pub(crate) schema: String,
    pub(crate) table: String,
    pub(crate) keys: Vec<String>,
    pub(crate) predicate: String,
    pub(crate) rows: f64,
    pub(crate) queries: Vec<(&'a QueryStat, u32)>,
}

impl ScanGroup<'_> {
    /// Keep compilation-time cardinality apart from the catalog collected for this scan. This value is a basis for
    /// selectivity, not a measurement of every execution's reads (TOP, parameters and repeated scans can differ).
    pub(crate) fn row_basis(&self, snapshot: &Snapshot) -> String {
        let basis = format!("The captured scan uses {} rows as its size basis", format::count(self.rows));
        match snapshot.table(&self.schema, &self.table).and_then(|t| t.rows) {
            Some(rows) => format!(
                "{basis}; this scan's catalog reports {} rows in the table. Captured estimates can predate the current table size.",
                format::count(rows)
            ),
            None => format!(
                "{basis}; the current table row count is unavailable. This is not a measured count of rows read on every execution."
            ),
        }
    }
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
            // A partition a run would prune isn't read; a columnstore is read by segments, never sought.
            if node.facts().partition_pruning.is_some()
                || node.facts().storage.as_ref().is_some_and(|s| s.eq_ignore_ascii_case("ColumnStore"))
            {
                continue;
            }
            let Some((schema, table)) = split_object(object) else { continue };
            let rows = scan_rows(snapshot, node, &schema, &table).unwrap_or(0.0);
            // An estimate is the optimizer's word only where it isn't a default guess: PostgreSQL's for a parameter
            // compared by range, MySQL's fixed 10% for an equality it has no statistics for.
            let guessed = node.facts().selectivity_guess.is_some()
                || (snapshot.server.engine == Engine::Mysql
                    && node.facts().estimated_filtered_percent.is_some_and(|f| {
                        culprit_core::insights::mysql_guessed_filtered(
                            f,
                            culprit_core::insights::condition_count(predicate),
                        )
                    }));
            let estimate_keeps_few = if node.act_rows.is_some() {
                node.kept_share().is_some_and(|kept| kept <= culprit_core::INDEX_KEEPS_AT_MOST)
            } else {
                !guessed
                    && node.est_rows_total() <= rows * node.est_executions.max(1.0) * culprit_core::INDEX_KEEPS_AT_MOST
            };
            // What the runs measured, where they can say, outweighs the estimate either way.
            let keeps_few = measured_keeps_few(snapshot, q, rows).unwrap_or(estimate_keeps_few);
            if rows < 10_000.0 || !keeps_few {
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

/// Rows a plan's scan reads. A scan of a filtered index reads only that index's rows, which the plan's estimate of
/// rows read gives; its table cardinality is the whole table's, and would make a scan of an empty filtered index (a
/// queue's "pending" rows, say) look like a read of every row. Any other scan is taken at the table's size: under a
/// TOP, the estimate of rows read assumes matches come early, which is exactly when a costly scan would hide.
pub(crate) fn scan_rows(snapshot: &Snapshot, node: &culprit_core::PlanNode, schema: &str, table: &str) -> Option<f64> {
    let filtered = node.index.as_deref().is_some_and(|index| {
        let name = index.trim_matches(['[', ']', '"']);
        snapshot.indexes.iter().any(|i| {
            i.schema.eq_ignore_ascii_case(schema)
                && i.table.eq_ignore_ascii_case(table)
                && i.name.eq_ignore_ascii_case(name)
                && i.filter.is_some()
        })
    });
    // Per scan, as the table's size below is: callers count the scans. `rows_read` is already over all of them.
    if filtered {
        return node.rows_read.map(|read| read / node.runs());
    }
    node.table_rows.or_else(|| snapshot.table(schema, table).and_then(|t| t.rows))
}

/// MySQL and MariaDB count rows examined and rows returned per statement. A statement that examines most of the
/// table on every run and returns under a tenth of it filters after reading, whatever the optimizer estimated (MariaDB
/// assumes a condition keeps every row when the column has no statistics). Rows returned measure what the filter kept
/// only when the statement returns every row it keeps (see [`returns_every_kept_row`]): `SELECT COUNT(*) … WHERE
/// created_at > ?` returns one row however many the filter keeps, so there the plan's own estimate decides.
/// `None` where the runs can't say: another engine, or a statement whose rows returned don't measure its filter.
pub(super) fn measured_keeps_few(snapshot: &Snapshot, q: &QueryStat, table_rows: f64) -> Option<bool> {
    if snapshot.server.engine != Engine::Mysql || q.executions <= 0.0 || !returns_every_kept_row(&q.text) {
        return None;
    }
    let examined = q.logical_reads.map(|r| r / q.executions).unwrap_or(0.0);
    let returned = q.rows.map(|r| r / q.executions)?;
    // These totals group runs whose LIMIT values can differ. A literal in a sample or its captured plan establishes
    // no common limit for them, so returned rows cannot establish the filter's selectivity under a LIMIT.
    if culprit_core::sql::has_limit(&q.text, Engine::Mysql) {
        return None;
    }
    (examined >= table_rows * 0.5).then_some(returned <= table_rows * culprit_core::INDEX_KEEPS_AT_MOST)
}

/// Put what the runs measured of each scan's filter on the scan itself ([`culprit_core::plan::MEASURED_KEPT`]), where
/// they can say ([`measured_keeps_few`]): the share of the rows examined that the runs returned. The plan's own rules
/// then judge the scan by it, as `filtered-scan` does, so a scan this check names is never one its plan's viewer is
/// silent about (MySQL's fixed `filtered` guess says nothing; the runs do). A prepare step: it adds what the runs
/// measured, and changes nothing a plan estimated.
pub fn mark_measured_kept(snapshot: &mut Snapshot) {
    if snapshot.server.engine != Engine::Mysql {
        return;
    }
    // Saved snapshots can carry an annotation inferred by an older version. Re-establish it from today's evidence
    // rules, including withdrawing it when a sample LIMIT never described all of the statement's runs.
    fn clear(node: &mut culprit_core::PlanNode) {
        node.facts_mut().measured_kept_share = None;
        node.props.remove(culprit_core::plan::MEASURED_KEPT);
        node.children.iter_mut().for_each(clear);
    }
    for plan in snapshot.workload.iter_mut().flat_map(|w| &mut w.queries).filter_map(|q| q.plan.as_mut()) {
        clear(&mut plan.root);
    }
    let mut marks: Vec<(String, u32, f64)> = Vec::new();
    for q in snapshot.queries() {
        let Some(plan) = &q.plan else { continue };
        let scans: Vec<&culprit_core::PlanNode> =
            plan.nodes().into_iter().filter(|n| n.kind == culprit_core::NodeKind::Scan && n.object.is_some()).collect();
        // One table read whole: rows returned measure that scan's filter only then.
        let [scan] = scans.as_slice() else { continue };
        if scan.act_rows.is_some() || scan.predicate.is_none() {
            continue;
        }
        let Some((schema, table)) = scan.object.as_deref().and_then(split_object) else { continue };
        let Some(rows) = scan_rows(snapshot, scan, &schema, &table) else { continue };
        if measured_keeps_few(snapshot, q, rows).is_none() {
            continue;
        }
        let (Some(examined), Some(returned)) = (q.reads_per_execution(), q.rows_per_execution()) else { continue };
        marks.push((q.id.clone(), scan.id, (returned / examined.max(1.0)).clamp(0.0, 1.0)));
    }
    let Some(workload) = snapshot.workload.as_mut() else { return };
    for (id, node, kept) in marks {
        let Some(plan) = workload.queries.iter_mut().find(|q| q.id == id).and_then(|q| q.plan.as_mut()) else {
            continue;
        };
        fn mark(n: &mut culprit_core::PlanNode, id: u32, kept: f64) {
            if n.id == id {
                n.facts_mut().measured_kept_share = Some(kept);
                n.props.insert(culprit_core::plan::MEASURED_KEPT.into(), kept.to_string());
            }
            n.children.iter_mut().for_each(|c| mark(c, id, kept));
        }
        mark(&mut plan.root, node, kept);
    }
}

/// Whether a statement returns every row its filter keeps, so that rows returned measure the filter: a SELECT from one
/// table, without aggregates, GROUP BY, DISTINCT, TOP or FETCH, HAVING, a set operation or a subquery. Any of those
/// returns fewer rows than the filter keeps, and a join can return fewer or more. A LIMIT prevents the statement's
/// returned-row totals from measuring its filter ([`measured_keeps_few`]).
pub(super) fn returns_every_kept_row(text: &str) -> bool {
    const AGGREGATES: [&str; 14] = [
        "COUNT",
        "SUM",
        "AVG",
        "MIN",
        "MAX",
        "GROUP_CONCAT",
        "JSON_ARRAYAGG",
        "JSON_OBJECTAGG",
        "STD",
        "STDDEV",
        "STDDEV_POP",
        "STDDEV_SAMP",
        "VARIANCE",
        "BIT_OR",
    ];
    let plain = text.replace(['`', '"', '[', ']'], "").to_ascii_uppercase();
    let words: Vec<&str> =
        plain.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_')).filter(|w| !w.is_empty()).collect();
    let calls = |name: &str| {
        plain.match_indices(name).any(|(at, _)| {
            let before = plain[..at].chars().next_back();
            let after = plain[at + name.len()..].trim_start();
            !before.is_some_and(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.') && after.starts_with('(')
        })
    };
    words.first() == Some(&"SELECT")
        && words.iter().filter(|w| **w == "SELECT").count() == 1
        && !words.iter().any(|w| {
            matches!(*w, "GROUP" | "DISTINCT" | "TOP" | "FETCH" | "HAVING" | "UNION" | "INTERSECT" | "EXCEPT" | "JOIN")
        })
        && !AGGREGATES.iter().any(|a| calls(a))
        && culprit_core::sql::tables_in(text, Engine::Mysql).len() == 1
}

/// The sentence for an index led by `first_key` that a statement in the workload created (`CREATE [UNIQUE]
/// [NONCLUSTERED] INDEX [CONCURRENTLY] [IF NOT EXISTS] <name> ON <table> (<first key>, …)`) and the catalog doesn't
/// hold now: it existed, and was dropped (with the table, where `gone`). `None` when the workload shows no such
/// statement, or the index is still there.
fn dropped_index(snapshot: &Snapshot, schema: &str, table: &str, first_key: &str, gone: bool) -> Option<String> {
    let clean = |w: &str| w.replace(['[', ']', '"', '`'], "").to_ascii_lowercase();
    let (schema, table, first_key) = (schema.to_ascii_lowercase(), table.to_ascii_lowercase(), clean(first_key));
    let source = snapshot.workload.as_ref()?.source.clone();
    snapshot.queries().iter().find_map(|q| {
        let text = q.text.split_whitespace().collect::<Vec<_>>().join(" ");
        let (head, columns) = text.split_once('(')?;
        let words: Vec<String> = head.split_whitespace().map(clean).collect();
        let index_at =
            words.iter().position(|w| w == "index").filter(|_| words.first().is_some_and(|w| w == "create"))?;
        let on_at = words.iter().skip(index_at).position(|w| w == "on")? + index_at;
        let name = words.get(on_at - 1).filter(|_| on_at > index_at + 1)?;
        let object = words[on_at + 1..].iter().find(|w| *w != "only")?;
        let (in_schema, in_table) = object.rsplit_once('.').map_or((None, object.as_str()), |(s, t)| (Some(s), t));
        let first = clean(columns.split([',', ')']).next()?.split_whitespace().next()?);
        let same = in_table == table && in_schema.is_none_or(|s| s.rsplit('.').next() == Some(schema.as_str()));
        let kept = snapshot.indexes.iter().any(|i| i.table.eq_ignore_ascii_case(&table) && clean(&i.name) == *name);
        (same && first == first_key && !kept).then(|| {
            let when =
                q.first_seen.as_deref().map(|at| format!(" {}", crate::queries::when_phrase(at))).unwrap_or_default();
            let shown: String = text.chars().take(200).collect();
            if gone {
                format!(" An index on {first_key} did exist: {source} recorded `{shown}`{when}.")
            } else {
                format!(
                    " An index on {first_key} did exist: {source} recorded `{shown}`{when}, and the catalog doesn't \
                     list {name} now, so it was dropped or renamed since."
                )
            }
        })
    })
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

/// A disabled index that would lead with the scan's columns ([`leading_index`]'s rule): the fix is to bring it back,
/// not to build its twin.
pub(crate) fn disabled_leading_index<'a>(snapshot: &'a Snapshot, g: &ScanGroup) -> Option<&'a IndexStat> {
    let cols = predicate_columns(&g.predicate);
    let wanted: &[String] = if cols.equality.is_empty() { g.keys.get(..1)? } else { &cols.equality };
    snapshot.indexes.iter().find(|i| {
        let existing = i.key_names();
        i.schema.eq_ignore_ascii_case(&g.schema)
            && i.table.eq_ignore_ascii_case(&g.table)
            && i.is_disabled
            && existing.len() >= wanted.len()
            && lower_set(&existing[..wanted.len()].iter().map(|s| s.to_string()).collect::<Vec<_>>())
                == lower_set(wanted)
    })
}

/// An index already leads with the equality columns: the scan has another cause (type mismatch, function on the column, stale stats).
pub(super) fn leading_index_exists(
    snapshot: &Snapshot,
    schema: &str,
    table: &str,
    equality: &[String],
    keys: &[String],
) -> bool {
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

pub(super) fn filtered_scan_finding(snapshot: &Snapshot, g: ScanGroup) -> Finding {
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
    f.facts.index_keys = g.keys.clone();
    apply_query_impact(&mut f, &impact);
    // Graded on the scan's part of each statement, by its share of the plan's cost: a 12K-row lookup table scanned by
    // a heavy star join is a sliver of that join's time. A plan without costs (SQLite) gives the statement's.
    let time_ms: f64 = g
        .queries
        .iter()
        .map(|(q, node)| {
            let part = q.plan.as_ref().filter(|p| p.weighed()).and_then(|p| {
                let total = p.total_cost.max(p.root.subtree_cost);
                let n = p.nodes().into_iter().find(|n| n.id == *node)?;
                (total > 0.0).then(|| (n.subtree_cost.max(n.self_cost) / total).clamp(0.0, 1.0))
            });
            q.total_time_ms * part.unwrap_or(1.0)
        })
        .sum();
    let total = snapshot.workload.as_ref().map_or(0.0, |w| w.total_time_ms);
    let scan_share = if total > 0.0 { (time_ms / total).min(impact.share) } else { impact.share };
    f.severity = severity_for_time_of(snapshot, &queries, time_ms, scan_share);
    f.confidence = Confidence::High;
    // Statements that stopped (or ran once) are told in the past, all of it: title and summary decide it once. A table
    // that is gone has no catalog left to say which indexes it had, so only the plans speak for it.
    let history = queries.iter().all(|q| crate::query_is_history(snapshot, q));
    f.title = format!("{} {} read in full to find rows by {}", g.table, if history { "was" } else { "is" }, key_text);
    let disabled = disabled_leading_index(snapshot, &g);
    let fk = snapshot.foreign_keys.iter().find(|fk| {
        fk.schema.eq_ignore_ascii_case(&g.schema)
            && fk.table.eq_ignore_ascii_case(&g.table)
            && !fk.columns.is_empty()
            && fk.columns.iter().all(|c| g.keys.iter().any(|k| k.eq_ignore_ascii_case(c)))
    });
    let one = queries.len() == 1;
    // A table the catalog no longer lists: the plans read it before it was dropped or renamed.
    let gone = table_gone(snapshot, &g.table, &queries);
    if gone {
        f.actionability = culprit_core::Actionability::History;
    }
    let (reads, keeps, accounts) = match (history, one) {
        (true, true) => ("One query read", "kept", "It accounted"),
        (true, false) => ("", "kept", "Together they accounted"),
        (false, true) => ("One query plans to read", "keep", "It accounts"),
        (false, false) => ("", "keep", "Together they account"),
    };
    let reads = match (reads, history) {
        ("", true) => format!("{} queries read", queries.len()),
        ("", false) => format!("{} queries plan to read", queries.len()),
        (said, _) => said.to_string(),
    };
    f.summary = format!(
        "{reads} {} and {keeps} only those matching `{}`. {} {}. {accounts} for {} of database time.{}",
        table_name,
        g.predicate.trim(),
        g.row_basis(snapshot),
        match disabled {
            Some(i) => format!(
                "Only {}, which is disabled, starts with {key_text}, so the database {} no faster way in",
                i.name,
                if history { "had" } else { "has" }
            ),
            None if gone => format!(
                "{} no index starting with {key_text} to seek",
                if one { "The plan it last ran with had" } else { "The plans they last ran with had" }
            ),
            None => format!(
                "No index starts with {key_text}, so the database {} no faster way in",
                if history { "had" } else { "has" }
            ),
        },
        format::percent(impact.share),
        fk.map(|fk| format!(
            " {} is also the foreign key to {}, so deletes there scan this table too.",
            key_text, fk.ref_table
        ))
        .unwrap_or_default()
    );
    if gone {
        f.summary.push_str(&format!(" {table_name} isn't in the database's catalog any more."));
    }
    // An index on the key that the workload shows being created, and the catalog doesn't hold now: it was dropped.
    if let Some(sentence) = g.keys.first().and_then(|key| dropped_index(snapshot, &g.schema, &g.table, key, gone)) {
        f.summary.push_str(&sentence);
    }
    f.queries = g
        .queries
        .iter()
        .map(|(q, node)| culprit_core::QueryRef { id: q.id.clone(), plan_node: Some(*node), note: None })
        .collect();
    f.metrics.push(Metric::new("Share of DB time", impact.share, Unit::Ratio));
    if let Some(per_day_ms) = impact.per_day_ms {
        f.metrics.push(Metric::new("DB time per day", per_day_ms, Unit::Ms));
    }
    f.metrics.push(Metric::new("Captured scan size basis", g.rows, Unit::Rows));
    f.metrics.push(Metric::new("Queries", queries.len() as f64, Unit::Count));
    // A scan's own index request also says which returned columns the index must carry to avoid lookups. Keep those
    // columns when the filtered-scan finding absorbs the plan's request.
    let mut include = Vec::new();
    for hint in queries.iter().filter_map(|q| q.plan.as_ref()).flat_map(|p| &p.index_hints) {
        let keys: Vec<_> = hint.equality.iter().chain(&hint.inequality).cloned().collect();
        if names_table(&hint.table, &g.schema, &g.table) && lower_set(&keys) == lower_set(&g.keys) {
            for column in &hint.include {
                if !g.keys.iter().chain(&include).any(|k| k.eq_ignore_ascii_case(column)) {
                    include.push(column.clone());
                }
            }
        }
    }
    if let Some(note) = drop_large_includes(snapshot, &g.schema, &g.table, &mut include) {
        f.caveats.push(note);
    }
    f.fix = Some(match disabled {
        Some(i) => widen_fix(snapshot, i, &include),
        None => new_index_fix(
            snapshot,
            &g.schema,
            &g.table,
            &g.keys,
            &include,
            &queries,
            format!(
                "Test an index on {} as a way for {} to find matching rows without a full scan.",
                key_text,
                if one { "the query" } else { "these queries" }
            ),
        ),
    });
    f.verify = if gone {
        vec![format!(
            "Nothing to check: {table_name} is gone, and this finding goes once no captured statement reads it."
        )]
    } else {
        vec![
            format!(
                "Obtain a newly compiled plan for {} and check whether {} uses {} to read fewer rows. A scan can remain appropriate for some parameter values.",
                if one { "the query" } else { "one of the queries" },
                g.table,
                disabled.map_or("the new index".to_string(), |i| i.name.clone())
            ),
            // On running totals the share holds every run from before the index: What changed shows the runs since.
            match crate::queries::what_changed_between_scans(
                snapshot,
                if one { "the query" } else { "the queries" },
                if one { "compare its time per run under equivalent load" } else { "compare their time per run under equivalent load" },
            ) {
                Some(between) => format!("{between} Use the scans' read-counter changes to compare reads per new run, accounting for resets; include index storage and write cost."),
                None => "Re-scan after representative runs and compare reads and time per run with equivalent parameters and load. Include index storage and write cost; an old stored plan or a changing share of database time alone does not verify improvement.".into(),
            },
        ]
    };
    f
}
