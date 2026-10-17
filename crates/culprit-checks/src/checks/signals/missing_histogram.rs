//! The `missing-histogram` check: Columns that heavy queries filter on with no index leading on them and no histogram (MySQL 8 COLUMN_STATISTICS, MariaDB mysql.column_stats), so the optimizer assumes a fixed share of rows match.

use super::*;

pub fn missing_histogram_info() -> CheckInfo {
    CheckInfo {
        id: "missing-histogram".into(),
        title: "Filter columns the optimizer has to guess about".into(),
        category: Category::Maintenance,
        engines: vec![Engine::Mysql],
        requires: vec![],
        description: "Columns that heavy queries filter on with no index leading on them and no histogram (MySQL 8 COLUMN_STATISTICS, MariaDB mysql.column_stats), so the optimizer assumes a fixed share of rows match.".into(),
    }
}

check!(MissingHistogramCheck: missing_histogram_info, missing_histogram);

/// The columns a missing-histogram finding names, for de-duplication against index findings.
pub(crate) fn histogram_columns(f: &Finding) -> Vec<String> {
    f.metrics
        .iter()
        .find(|m| m.label == "Columns")
        .and_then(|m| m.text.as_deref())
        .map(|t| t.split(", ").map(str::to_string).collect())
        .unwrap_or_default()
}

/// The table name a plan node reads, without schema or quotes.
pub(super) fn node_table(object: &str) -> String {
    object.rsplit('.').next().unwrap_or(object).trim_matches(|c| matches!(c, '`' | '"' | '[' | ']')).to_string()
}

/// Per table: the columns the optimizer guesses about, the guessed share EXPLAIN showed, and the queries.
pub(super) type Guessed<'a> = (Vec<String>, f64, Vec<&'a QueryStat>);

/// Whether the optimizer has nothing to choose with an estimate for this step: one table read in full, with no index
/// it could use for the filter (no possible key) or for the order (none leads with a column it sorts or groups by).
/// A histogram changes a plan through a choice, a join order or an index; here the scan is the only way in, and a
/// better estimate leaves it as it is.
fn no_choice(snapshot: &Snapshot, q: &QueryStat, plan: &culprit_core::Plan, node: &culprit_core::PlanNode) -> bool {
    let mut tables: Vec<String> =
        plan.nodes().iter().filter_map(|n| n.object.as_deref()).map(|o| node_table(o).to_ascii_lowercase()).collect();
    tables.sort();
    tables.dedup();
    let possible_keys = node.props.get("possible_keys").is_some_and(|k| !k.trim().is_empty());
    let table = node.object.as_deref().map(node_table).unwrap_or_default();
    let lower = q.text.to_ascii_lowercase().replace('`', "");
    let ordered =
        ["order by", "group by"].iter().filter_map(|clause| lower.rfind(clause).map(|at| &lower[at..])).any(|rest| {
            snapshot
                .indexes
                .iter()
                .filter(|i| i.table.eq_ignore_ascii_case(&table))
                .filter_map(|i| i.key_names().first().cloned())
                .any(|column| mentions_word(rest, &column.to_ascii_lowercase()))
        });
    tables.len() == 1 && node.kind == culprit_core::NodeKind::Scan && !possible_keys && !ordered
}

pub fn missing_histogram(snapshot: &Snapshot) -> Vec<Finding> {
    let Some(histograms) = snapshot.histograms.as_ref() else { return Vec::new() };
    let mariadb = is_mariadb(snapshot);
    let has_histogram = |table: &str, column: &str| {
        histograms.iter().any(|h| h.table.eq_ignore_ascii_case(table) && h.column.eq_ignore_ascii_case(column))
    };
    let leads_index = |table: &str, column: &str| {
        snapshot.indexes.iter().any(|i| {
            i.table.eq_ignore_ascii_case(table) && i.key_names().first().is_some_and(|c| c.eq_ignore_ascii_case(column))
        })
    };
    let mut by_table: BTreeMap<(String, String), Guessed> = BTreeMap::new();
    let mut heavy: Vec<&QueryStat> = app_queries(snapshot).filter(|q| q.plan.is_some()).collect();
    heavy.sort_by(|a, b| b.total_time_ms.total_cmp(&a.total_time_ms));
    for q in heavy.into_iter().take(20) {
        let Some(plan) = q.plan.as_ref() else { continue };
        for node in plan.nodes() {
            let (Some(object), Some(predicate)) = (node.object.as_deref(), node.predicate.as_deref()) else { continue };
            let table = node_table(object);
            let Some(stat) = snapshot.tables.iter().find(|t| t.name.eq_ignore_ascii_case(&table)) else { continue };
            if stat.rows.unwrap_or(0.0) < 1_000.0 {
                continue;
            }
            // MySQL prints typed literals as TIMESTAMP'…' and DATE'…'; they are values like any quoted one.
            let predicate = predicate.replace('`', "").replace("TIMESTAMP'", "'").replace("DATE'", "'");
            let columns = predicate_columns(&predicate);
            // The key parts of the index the step reads are counted from the index itself (MySQL dives into it for a
            // range), not guessed.
            let key_parts: Vec<String> = node
                .props
                .get("used_key_parts")
                .map(|k| k.split(',').map(|c| c.trim().to_string()).collect())
                .unwrap_or_default();
            let guessed: Vec<String> = columns
                .equality
                .iter()
                .chain(columns.range.iter())
                .filter(|c| !leads_index(&table, c) && !has_histogram(&table, c))
                .filter(|c| !key_parts.iter().any(|k| k.eq_ignore_ascii_case(c)))
                .cloned()
                .collect();
            if guessed.is_empty() {
                continue;
            }
            // Only a step whose estimate is the optimizer's fixed guess: MySQL's 10% for =, a third for ranges, a ninth
            // for BETWEEN (and their products for several conditions); MariaDB without statistics for the column counts
            // the condition as keeping every row, 100%. Anything else came from somewhere other than a guess.
            let Some(filtered) = node.props.get("filtered").and_then(|v| v.parse::<f64>().ok()) else { continue };
            let guess = if mariadb {
                (filtered - 100.0).abs() < 0.01
            } else {
                culprit_core::insights::mysql_guessed_filtered(
                    filtered,
                    culprit_core::insights::condition_count(&predicate),
                )
            };
            if !guess || no_choice(snapshot, q, plan, node) {
                continue;
            }
            let entry = by_table.entry((stat.schema.clone(), stat.name.clone())).or_default();
            for c in guessed {
                if !entry.0.iter().any(|e| e.eq_ignore_ascii_case(&c)) {
                    entry.0.push(c);
                }
            }
            entry.1 = filtered;
            if !entry.2.iter().any(|e| e.id == q.id) {
                entry.2.push(q);
            }
        }
    }
    let engine = engine_name(snapshot);
    by_table
        .into_iter()
        .map(|((schema, table), (columns, guess, queries))| {
            let name = format!("{schema}.{table}");
            let mut f = finding("missing-histogram", &name, Category::Maintenance, subject(SubjectKind::Table, &name));
            let list = format::list(&columns);
            f.title = format!(
                "{engine} guesses how many {table} rows match {list}: no {}",
                if mariadb { "column statistics" } else { "histogram" }
            );
            let who = match queries.len() {
                1 => format!("{} filters", capitalize(&query_handle(queries[0]))),
                n => format!("{n} of the heaviest queries filter"),
            };
            f.summary = if mariadb {
                // MariaDB doesn't guess a share: without statistics for a column it counts the condition as keeping
                // every row.
                format!(
                    "{who} {table} on {list}, which no index starts with and no statistics describe, so {engine} assumes the condition keeps every row (EXPLAIN shows filtered = {guess:.0}%). When it actually keeps few, common for status flags and skewed values, it picks the wrong join order or access path. Column statistics cost nothing at query time and change only when you run ANALYZE again."
                )
            } else {
                format!(
                    "{who} {table} on {list}, which no index starts with and no histogram describes, so {engine} assumes a fixed share of rows match (10% for =, a third for ranges); EXPLAIN shows filtered = {guess:.2}%, from those guesses. When the real share is far off, common for status flags and skewed values, it picks the wrong join order or access path. A histogram costs nothing at query time and changes only when you run ANALYZE again."
                )
            };
            f.queries = query_refs(&queries);
            let impact = query_impact(snapshot, &queries);
            let share = impact.as_ref().map(|i| i.share).unwrap_or(0.0);
            if let Some(impact) = impact {
                apply_query_impact(&mut f, &impact);
            }
            // A fixed guess the plan shows, behind a real amount of time (the rule every time-based finding follows).
            let time: f64 = queries.iter().map(|q| q.total_time_ms).sum();
            f.severity = if share >= 0.05 && severity_for_time(snapshot, time, share) >= Severity::Medium {
                Severity::Medium
            } else {
                Severity::Low
            };
            severity_score(&mut f);
            f.confidence = Confidence::High;
            f.metrics =
                vec![Metric::text("Columns", columns.join(", ")), Metric::new("EXPLAIN filtered", guess / 100.0, Unit::Ratio)];
            let target = qualified(Engine::Mysql, &schema, &table);
            let quoted = columns.iter().map(|c| quote(Engine::Mysql, c)).collect::<Vec<_>>().join(", ");
            f.fix = Some(if mariadb {
                Fix {
                    summary: "Collect a histogram for these columns. MariaDB uses it when use_stat_tables is PREFERABLY_FOR_QUERIES and optimizer_use_condition_selectivity is 4 or more, the defaults since 10.4. Run it again after large data changes.".into(),
                    sql: Some(format!("ANALYZE TABLE {target} PERSISTENT FOR COLUMNS ({quoted}) INDEXES ();")),
                    cost: Some("Reads the whole table".into()),
                }
            } else {
                Fix {
                    summary: "Build a histogram for these columns. It reads the table once; MySQL doesn't refresh it on its own, so run it again after large data changes.".into(),
                    sql: Some(format!("ANALYZE TABLE {target} UPDATE HISTOGRAM ON {quoted} WITH 64 BUCKETS;")),
                    cost: Some("Reads the table".into()),
                }
            });
            f.verify = vec!["Re-scan: the columns should have histograms, and EXPLAIN ANALYZE should show estimated rows close to actual for that filter.".into()];
            f
        })
        .collect()
}
