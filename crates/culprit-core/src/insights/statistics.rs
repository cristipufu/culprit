//! Plan insights about the optimizer's estimates: missing or stale statistics, and steps whose rows were far from
//! the estimate.

use super::*;

pub(super) fn misestimate(cx: &Cx) -> Vec<Draft> {
    // The plan's misestimates by the one shared rule (`PlanNode::misestimates`), judged here again so a plan saved
    // before that rule reads the same as a fresh one.
    let found: HashMap<u32, f64> = cx.plan.root.misestimates().into_iter().collect();
    // The deepest misestimated operators are the causes; everything above inherits their error.
    let causes: Vec<&PlanNode> = cx
        .nodes
        .iter()
        .filter(|n| found.contains_key(&n.id))
        .filter(|n| {
            let mut below = Vec::new();
            n.children.iter().for_each(|c| c.walk(&mut below));
            !below.iter().any(|b| found.contains_key(&b.id))
        })
        // A table variable estimated at one row is the table-variable rule's: no statistics to refresh.
        .filter(|n| !(table_variable_of(n).is_some() && n.est_rows_total() <= 1.5))
        .copied()
        .collect();
    let mut causes = causes;
    causes.sort_by(|a, b| {
        let w = |n: &PlanNode| found[&n.id].abs() * rows_out(n).max(n.est_rows_for_runs()).log10().max(1.0);
        w(b).total_cmp(&w(a))
    });
    causes
        .into_iter()
        .take(2)
        .map(|n| {
            let factor = found[&n.id];
            let est = n.est_rows_for_runs();
            let act = rows_out(n);
            // MySQL's EXPLAIN ANALYZE estimates the filter in a Filter node above the table it reads.
            let table = n.object.clone().or_else(|| (n.op == "Filter").then(|| first_object(n)).flatten());
            let subject = table.clone().unwrap_or_else(|| n.op.clone());
            let mut above = 0.0;
            // A sort or hash above that ran out of memory was sized for the wrong number of rows.
            let mut spill_above = false;
            // What was planned from this estimate: joins, sorts, aggregates, spools, parallelism above it.
            let mut consumers = false;
            let mut cur = cx.parent(n);
            while let Some(p) = cur {
                above += cx.share(p);
                spill_above |= p.warnings.contains(&PlanWarning::Spill);
                consumers |= matches!(
                    p.kind,
                    NodeKind::Join | NodeKind::Sort | NodeKind::Aggregate | NodeKind::Spool | NodeKind::Parallelism
                ) || p.op.contains("Hash");
                cur = cx.parent(p);
            }
            let impact = (cx.subtree(n) + above).min(1.0);
            let mut d = Draft::new("misestimate", InsightArea::Statistics, impact, vec![n.id]);
            d.floor = if factor.abs() >= 100.0 { Severity::Medium } else { Severity::Low };
            if !consumers {
                // Nothing in this plan was sized from it; the statistics still decide other plans on the table.
                d.floor = Severity::Low;
                d.cap = Severity::Low;
            }
            d.title = format!(
                "Expected {} from {}, got {} ({}× {})",
                plural(est, "row", "rows"),
                table_leaf(&subject),
                rows_text(act),
                rows_text(factor.abs()),
                if factor >= 0.0 { "more" } else { "fewer" }
            );
            let cond = conditions(n);
            // Multi-column statistics only describe plain columns; a filter on an expression (id % 4, lower(x)) needs
            // expression statistics instead, so the advice changes.
            let on_expression = cond.split([' ', '(', ')']).any(|t| matches!(t, "%" | "+" | "-" | "*" | "/"))
                || wrapped_column(&cond, cx.engine()).is_some();
            let cols: Vec<String> = if on_expression { Vec::new() } else { filter_columns(&cond) };
            d.detail = format!(
                "{}{}",
                if consumers {
                    format!(
                        "Everything above this operator was planned for the wrong number of rows: join types, memory grants and parallelism{}.",
                        if spill_above { " (here a sort or hash above it ran out of memory as a result)" } else { "" }
                    )
                } else {
                    "Nothing above it in this plan was sized from that estimate, but the same statistics decide every plan that reads this table.".to_string()
                },
                if cols.len() >= 2 {
                    format!(" The filter combines {}; databases assume such columns are independent, which is wrong when they move together.", format::list(&cols))
                } else if on_expression {
                    " The filter compares an expression, not a plain column, so the column's statistics can't predict it.".to_string()
                } else {
                    String::new()
                }
            );
            let mysql_histograms = !cx
                .prop("server_version")
                .is_some_and(|v| !v.contains("MariaDB") && !crate::version_at_least(v, [8, 0, 0]));
            let (fix, sql) = match (cx.engine(), &table) {
                // Table variables have no statistics, and a temporary table's belong to the session that filled it:
                // nothing Culprit could hand over to run would reach them.
                (Engine::Mssql, Some(t)) if table_variable_of(n).is_some() => (
                    format!("{} is a table variable, so it has no statistics: use a temporary table instead, or add OPTION (RECOMPILE) so the row count at run time is used.", table_leaf(t)),
                    None,
                ),
                (Engine::Mssql, Some(t)) if table_leaf(t).starts_with('#') => (
                    format!("{} is a temporary table: update its statistics in the session that fills it, after loading it, or add OPTION (RECOMPILE) to the query that reads it.", table_leaf(t)),
                    None,
                ),
                (Engine::Mssql, Some(t)) => (
                    format!("Refresh the statistics on {} with a full scan{}.", table_leaf(t), if cols.len() >= 2 { ", and create statistics on the filtered columns together" } else { "" }),
                    Some(if cols.len() >= 2 {
                        format!("UPDATE STATISTICS {} WITH FULLSCAN;\nCREATE STATISTICS st_{}_{} ON {} ({});", qualified(Engine::Mssql, t), table_leaf(t).to_ascii_lowercase(), cols.join("_").to_ascii_lowercase(), qualified(Engine::Mssql, t), cols.iter().map(|c| quote(Engine::Mssql, c)).collect::<Vec<_>>().join(", "))
                    } else {
                        format!("UPDATE STATISTICS {} WITH FULLSCAN;", qualified(Engine::Mssql, t))
                    }),
                ),
                (Engine::Postgres, Some(t)) => (
                    format!(
                        "Analyze {}{}.",
                        table_leaf(t),
                        if cols.len() >= 2 {
                            ", and tell PostgreSQL the filtered columns are related with extended statistics"
                        } else if on_expression {
                            "; on PostgreSQL 14+ statistics on the expression itself help (CREATE STATISTICS … ON (expression) FROM table)"
                        } else {
                            ""
                        }
                    ),
                    Some(if cols.len() >= 2 {
                        format!("ANALYZE {};\nCREATE STATISTICS {}_{}_stats (dependencies, mcv) ON {} FROM {};\nANALYZE {};", qualified(Engine::Postgres, t), table_leaf(t).to_ascii_lowercase(), cols.join("_").to_ascii_lowercase(), cols.iter().map(|c| quote(Engine::Postgres, c)).collect::<Vec<_>>().join(", "), qualified(Engine::Postgres, t), qualified(Engine::Postgres, t))
                    } else {
                        Engine::Postgres.dialect().refresh_statistics(&qualified(Engine::Postgres, t))
                    }),
                ),
                // MariaDB collects histograms with its engine-independent statistics; MySQL has them from 8.0.
                (Engine::Mysql, Some(t)) if cx.mariadb() && !cols.is_empty() => (
                    format!("Refresh the statistics on {}, with column statistics (histograms) on {}.", table_leaf(t), format::list(&cols)),
                    Some(format!("ANALYZE TABLE {} PERSISTENT FOR COLUMNS ({}) INDEXES ();", qualified(Engine::Mysql, t), cols.iter().map(|c| quote(Engine::Mysql, c)).collect::<Vec<_>>().join(", "))),
                ),
                (Engine::Mysql, Some(t)) if mysql_histograms && !cols.is_empty() => (
                    format!("Refresh the statistics on {} and add a histogram on {}.", table_leaf(t), format::list(&cols)),
                    Some(format!("ANALYZE TABLE {} UPDATE HISTOGRAM ON {};", qualified(Engine::Mysql, t), cols.iter().map(|c| quote(Engine::Mysql, c)).collect::<Vec<_>>().join(", "))),
                ),
                (Engine::Mysql, Some(t)) => (
                    format!("Refresh the statistics on {}.", table_leaf(t)),
                    Some(Engine::Mysql.dialect().refresh_statistics(&qualified(Engine::Mysql, t))),
                ),
                (Engine::Sqlite, Some(t)) => (
                    format!("Refresh the statistics on {}.", table_leaf(t)),
                    Some(Engine::Sqlite.dialect().refresh_statistics(&qualified(Engine::Sqlite, t))),
                ),
                (_, None) => ("Refresh statistics on the tables below this operator.".into(), None),
            };
            d.fix = fix;
            d.fix_sql = sql;
            d.verify = "Run with actual rows again: the estimate is within about 10× of the actual count.".into();
            d
        })
        .collect()
}

pub(super) fn missing_statistics(cx: &Cx) -> Vec<Draft> {
    cx.nodes
        .iter()
        // A table variable never has statistics; the table-variable rule says what to do about it.
        .filter(|n| n.warnings.contains(&PlanWarning::MissingStatistics) && table_variable_of(n).is_none())
        .take(1)
        .map(|n| {
            let cols = n.props.get("ColumnsWithNoStatistics").cloned();
            let mut d = Draft::new("missing-statistics", InsightArea::Statistics, cx.subtree(n), vec![n.id]);
            d.cap = Severity::Medium;
            d.title = format!("No statistics on {}, so its row counts are guesses", cols.clone().unwrap_or_else(|| format!("some columns of {}", table_leaf(&what(n)))));
            d.detail = "Without statistics the optimizer uses fixed guesses for how many rows match, which leads to the wrong join type or memory size. SQL Server creates them by itself unless AUTO_CREATE_STATISTICS is off for the database or it can't write to the database (read-only).".into();
            d.fix = "Check the database's settings: if automatic statistics creation is off, turn it back on (it's on by default); if the database is read-only, create the statistics while it is writable (or on the primary, for a readable secondary) so they come with it.".into();
            d.fix_sql = Some("SELECT name, is_auto_create_stats_on, is_read_only\n  FROM sys.databases\n WHERE database_id = DB_ID();\n-- If is_auto_create_stats_on is 0:\n-- ALTER DATABASE CURRENT SET AUTO_CREATE_STATISTICS ON;".into());
            d.verify = "Explain again: the ColumnsWithNoStatistics warning is gone.".into();
            d
        })
        .collect()
}

pub(super) fn index_ignored(cx: &Cx) -> Vec<Draft> {
    if cx.engine() != Engine::Mysql {
        return Vec::new();
    }
    let mut out = Vec::new();
    for n in cx.nodes.iter().filter(|n| n.op == "Table scan" && index_could_pay(cx, n)) {
        let keys = possible_keys_for_filter(cx, n);
        if keys.is_empty() {
            continue;
        }
        let kept = filter_keeps(cx, n).unwrap_or(1.0);
        let mut d = Draft::new("index-ignored", InsightArea::Statistics, cx.share(n), vec![n.id]);
        d.cap = Severity::Medium;
        d.title = format!("MySQL could use {} but reads all of {} instead", keys.join(", "), table_leaf(&what(n)));
        d.detail = format!(
            "{} {} of {} rows, yet MySQL judged {} not worth it; that usually means stale statistics or a type mismatch in the comparison.",
            if n.act_rows.is_some() { "The filter kept" } else { "MySQL expects the filter to keep" },
            format::percent(kept),
            format::count(rows_read(n)),
            if keys.len() == 1 { "the index" } else { "those indexes" }
        );
        d.fix = "Refresh the table's statistics and compare the column with a value of its own type; if it still scans, compare with FORCE INDEX to see which is really faster.".into();
        d.fix_sql = n.object.as_ref().map(|t| Engine::Mysql.dialect().refresh_statistics(&qualified(Engine::Mysql, t)));
        d.verify = "Explain again: access_type is ref or range on the index.".into();
        out.push(d);
    }
    out
}
