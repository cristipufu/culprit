//! The `filtered-scan` check: Heavy queries whose plan reads a large table in full and keeps only the rows matching a filter that no index supports.

use super::*;

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

check!(FilteredScanCheck: filtered_scan_info, filtered_scan);

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
            // A partition a run would prune isn't read; a columnstore is read by segments, never sought.
            if node.props.contains_key(culprit_core::plan::NOT_READ_IN_FULL)
                || node.props.get("Storage").is_some_and(|s| s.eq_ignore_ascii_case("ColumnStore"))
            {
                continue;
            }
            let Some((schema, table)) = split_object(object) else { continue };
            let rows = scan_rows(snapshot, node, &schema, &table).unwrap_or(0.0);
            // An estimate is the optimizer's word only where it isn't a default guess: PostgreSQL's for a parameter
            // compared by range, MySQL's fixed 10% for an equality it has no statistics for.
            let guessed = node.props.contains_key(culprit_core::plan::GUESSED_SELECTIVITY)
                || (snapshot.server.engine == Engine::Mysql
                    && node.props.get("filtered").and_then(|f| f.parse::<f64>().ok()).is_some_and(|f| {
                        culprit_core::insights::mysql_guessed_filtered(
                            f,
                            culprit_core::insights::condition_count(predicate),
                        )
                    }));
            let estimate_keeps_few = !guessed
                && node.est_rows_total() <= rows * node.est_executions.max(1.0) * culprit_core::INDEX_KEEPS_AT_MOST;
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
    if filtered {
        return node.rows_read;
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
    (examined >= table_rows * 0.5).then_some(returned <= table_rows * culprit_core::INDEX_KEEPS_AT_MOST)
}

/// Whether a statement returns every row its filter keeps, so that rows returned measure the filter: a SELECT from one
/// table, without aggregates, GROUP BY, DISTINCT, LIMIT (or TOP, FETCH), HAVING, a set operation or a subquery. Any of
/// those returns fewer rows than the filter keeps, and a join can return fewer or more.
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
            matches!(
                *w,
                "GROUP" | "DISTINCT" | "LIMIT" | "TOP" | "FETCH" | "HAVING" | "UNION" | "INTERSECT" | "EXCEPT" | "JOIN"
            )
        })
        && !AGGREGATES.iter().any(|a| calls(a))
        && culprit_core::sql::tables_in(text, Engine::Mysql).len() == 1
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
    f.severity = severity_for_time(snapshot, time_ms, scan_share);
    f.confidence = Confidence::High;
    f.title = format!("{} is read in full to find rows by {}", g.table, key_text);
    let disabled = disabled_leading_index(snapshot, &g);
    let fk = snapshot.foreign_keys.iter().find(|fk| {
        fk.schema.eq_ignore_ascii_case(&g.schema)
            && fk.table.eq_ignore_ascii_case(&g.table)
            && !fk.columns.is_empty()
            && fk.columns.iter().all(|c| g.keys.iter().any(|k| k.eq_ignore_ascii_case(c)))
    });
    f.summary = format!(
        "{} read every row of {} ({} rows) and keep only those matching `{}`. {}, so the database has no faster way in. Together they account for {} of database time.{}",
        if queries.len() == 1 {
            "One query plans to".to_string()
        } else {
            format!("{} queries plan to", queries.len())
        },
        table_name,
        format::count(g.rows),
        g.predicate.trim(),
        match disabled {
            Some(i) => format!("Only {}, which is disabled, starts with {}", i.name, key_text),
            None => format!("No index starts with {key_text}"),
        },
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
    if let Some(per_day_ms) = impact.per_day_ms {
        f.metrics.push(Metric::new("DB time per day", per_day_ms, Unit::Ms));
    }
    f.metrics.push(Metric::new("Rows scanned per run", g.rows, Unit::Rows));
    f.metrics.push(Metric::new("Queries", queries.len() as f64, Unit::Count));
    f.fix = Some(match disabled {
        Some(i) => widen_fix(snapshot, i, &[]),
        None => Fix {
            summary: format!("Create an index on {} so these queries seek instead of scanning.", key_text),
            sql: Some(create_index_sql(snapshot, &g.schema, &g.table, &g.keys, &[])),
            cost: write_cost(snapshot, &g.schema, &g.table),
        },
    });
    f.verify = vec![
        format!(
            "Explain one of the queries again: the scan of {} should become an index seek/scan on {}.",
            g.table,
            disabled.map_or("the new index".to_string(), |i| i.name.clone())
        ),
        "Re-scan after the workload has run: this finding should be gone and the queries' share of database time lower.".into(),
    ];
    f
}
