//! The `plan-warnings` check: Implicit conversions, spills, missing join predicates and missing statistics in the plans of heavy queries.

use super::*;

pub fn plan_warnings_info() -> CheckInfo {
    CheckInfo {
        id: "plan-warnings".into(),
        title: "Plan warnings".into(),
        category: Category::Queries,
        engines: vec![Engine::Mssql, Engine::Postgres, Engine::Mysql],
        requires: vec![Capability::QueryPlans],
        description: "Implicit conversions, spills, missing join predicates and missing statistics in the plans of heavy queries.".into(),
    }
}

check!(PlanWarningsCheck: plan_warnings_info, plan_warnings);

pub fn plan_warnings(snapshot: &Snapshot) -> Vec<Finding> {
    let mut out = Vec::new();
    for q in app_queries(snapshot) {
        let Some(plan) = &q.plan else { continue };
        let Some(impact) = query_impact(snapshot, &[q]) else {
            continue;
        };
        if impact.share < 0.005 {
            continue;
        }
        for warning in [
            PlanWarning::ImplicitConversion,
            PlanWarning::Spill,
            PlanWarning::NoJoinPredicate,
            PlanWarning::MissingStatistics,
        ] {
            let nodes: Vec<&PlanNode> = plan.nodes().into_iter().filter(|n| n.warnings.contains(&warning)).collect();
            if nodes.is_empty() && !plan.warnings.contains(&warning) {
                continue;
            }
            if let Some(f) = plan_warning_finding(snapshot, q, &impact, warning, nodes.first().copied()) {
                out.push(f);
            }
        }
    }
    merge_same_cause(snapshot, out)
}

/// One root cause (a type mismatch on one column) is one finding, however many queries hit it. It stops them seeking an
/// index only where SQL Server said so of one of them (a "Seek Plan" warning, kept as High confidence); otherwise the
/// conversion slows them, and their share doesn't raise it. Seeking, not using: the plan may still scan the index.
pub(super) fn merge_same_cause(snapshot: &Snapshot, findings: Vec<Finding>) -> Vec<Finding> {
    let mut merged: Vec<Finding> = Vec::new();
    for f in findings {
        match merged.iter_mut().find(|m| m.id == f.id) {
            Some(m) => {
                if f.confidence == Confidence::High && m.confidence != Confidence::High {
                    m.confidence = Confidence::High;
                    m.severity = m.severity.max(f.severity);
                    m.summary = f.summary.clone();
                }
                for r in f.queries {
                    if !m.queries.iter().any(|x| x.id == r.id) {
                        m.queries.push(r);
                    }
                }
            }
            None => merged.push(f),
        }
    }
    for m in merged.iter_mut().filter(|m| m.queries.len() > 1) {
        let queries: Vec<&QueryStat> =
            m.queries.iter().filter_map(|r| snapshot.workload.as_ref()?.query(&r.id)).collect();
        let Some(impact) = query_impact(snapshot, &queries) else { continue };
        apply_query_impact(m, &impact);
        let column = m.subject.name.clone();
        if m.confidence == Confidence::High {
            m.severity = m.severity.max(severity_for_workload_share(snapshot, impact.share));
            m.title = format!("A type mismatch on {column} stops {} queries from seeking an index", queries.len());
        } else {
            m.title = format!("A type mismatch on {column} slows down {} queries", queries.len());
        }
        m.summary.push_str(&format!(
            " The same mismatch appears in {} queries that together take {} of database time.",
            queries.len(),
            format::percent(impact.share)
        ));
        m.metrics.retain(|x| x.label != "Share of DB time" && x.label != "Average duration");
        m.metrics.insert(0, Metric::new("Share of DB time", impact.share, Unit::Ratio));
        m.metrics.insert(1, Metric::new("Queries affected", queries.len() as f64, Unit::Count));
        if let Some(per_day_ms) = impact.per_day_ms {
            m.metrics.insert(2, Metric::new("DB time per day", per_day_ms, Unit::Ms));
        }
    }
    merged
}

pub(super) fn plan_warning_finding(
    snapshot: &Snapshot,
    q: &QueryStat,
    impact: &QueryImpact,
    warning: PlanWarning,
    node: Option<&PlanNode>,
) -> Option<Finding> {
    let key = format!("{}:{}", q.id, warning_key(warning));
    let mut f = finding("plan-warnings", &key, Category::Queries, subject(SubjectKind::Query, &q.id));
    apply_impact(snapshot, &mut f, impact);
    f.queries = vec![QueryRef { id: q.id.clone(), plan_node: node.map(|n| n.id), note: Some(warning.label().into()) }];
    f.metrics.push(Metric::new(share_metric_label(snapshot), impact.share, Unit::Ratio));
    f.metrics.push(Metric::new("Average duration", q.mean_time_ms, Unit::Ms));
    let handle = query_handle(q);
    match warning {
        PlanWarning::ImplicitConversion => {
            let node_props = node.into_iter().flat_map(|n| n.props.values());
            let plan_props = q.plan.iter().flat_map(|p| p.props.values());
            let named: Vec<&str> =
                node_props.chain(plan_props).flat_map(|v| v.lines()).filter(|l| l.contains("CONVERT")).collect();
            // Only a column converted to match a value of another type is a mismatch (the rule the plan parser applies;
            // scans saved before it flagged every conversion SQL Server warned about).
            // A plan captured before a column changed type still shows its old conversion (`current_conversions`).
            let still =
                |l: &str| q.plan.as_ref().map_or_else(|| converts_column(l), |p| !current_conversions(p, l).is_empty());
            let lines: Vec<&str> = named.iter().copied().filter(|l| still(l)).collect();
            if !named.is_empty() && lines.is_empty() {
                return None;
            }
            let issue = lines.iter().find(|l| l.contains("Seek Plan")).or(lines.first()).copied();
            let blocks_seek = issue.is_some_and(|l| l.contains("Seek Plan"));
            let converted = issue.and_then(|l| current_conversions(q.plan.as_ref()?, l).into_iter().next());
            let column = converted.as_ref().map(|(c, _)| c.clone());
            let target = converted.map(|(_, t)| t);
            let unicode = target.as_deref().is_some_and(|t| t.to_ascii_lowercase().starts_with("nvarchar"));
            let convert = issue.map(|l| l.split_once(": ").map(|(_, e)| e).unwrap_or(l).to_string());
            // "Can't seek an index" is SQL Server's verdict (a "Seek Plan" warning), not a reading of the conversion: without
            // it the plan may still seek (a range through the conversion), and the warning is about its estimates. With it,
            // the plan may still scan the index, so the index isn't unused: it can't be searched.
            f.title = match &column {
                Some(c) if blocks_seek => format!("A type mismatch on {c} stops {handle} from seeking an index"),
                Some(c) => format!("A type mismatch on {c} slows down {handle}"),
                None => format!("A type mismatch slows down {handle}"),
            };
            f.summary = format!(
                "The database has to convert {} before comparing it with a parameter of a different type{}. {}{}",
                column.as_deref().unwrap_or("a column"),
                convert.map(|c| format!(" (`{}`)", truncate(&c, 140))).unwrap_or_default(),
                if blocks_seek {
                    "SQL Server warns that the conversion keeps it from searching an index, so it reads every row instead."
                } else {
                    "SQL Server warns that the conversion may affect its row estimates, which can lead it to a poorer plan; it doesn't say an index can't be used."
                },
                match &q.parameters {
                    Some(p) if p.to_ascii_lowercase().contains("nvarchar") => format!(" The application declares its parameters as `{}`: Unicode strings compared with a non-Unicode column.", truncate(p, 120)),
                    Some(p) => format!(" The application declares its parameters as `{}`.", truncate(p, 120)),
                    None if unicode => " This often happens when an application sends strings as nvarchar (Unicode) to a varchar column.".to_string(),
                    None => String::new(),
                }
            );
            f.severity = if blocks_seek { severity_for_workload_share(snapshot, impact.share) } else { Severity::Low };
            f.confidence = if blocks_seek { Confidence::High } else { Confidence::Medium };
            f.fix = Some(Fix { summary: mismatch_fix(column.as_deref(), target.as_deref()), sql: None, cost: None });
            if let Some(c) = &column {
                f.id = format!("plan-warnings:implicit-conversion:{c}");
                f.subject = subject(SubjectKind::Table, c);
                f.metrics.push(Metric::text("Column", c.clone()));
            }
        }
        PlanWarning::Spill => {
            f.title = format!("{} runs out of memory and spills to disk", capitalize(&handle));
            f.summary = "A sort or hash in this plan needed more memory than it was granted and wrote the overflow to disk (tempdb or temp files). Spills usually come from a row estimate that was far too low, or a sort the index could have avoided.".into();
            // A spill too small to matter stays Low, as the plan's own `spill` insight rates it.
            f.severity = if q.plan.as_ref().is_some_and(culprit_core::insights::spills_are_minor) {
                Severity::Low
            } else {
                severity_for_workload_share(snapshot, impact.share)
            };
            f.confidence = Confidence::High;
            f.fix = Some(Fix {
                summary: match snapshot.server.engine {
                    Engine::Mssql => "Update statistics on the tables involved; if the estimate is still far off, add an index that delivers rows in the needed order so the sort disappears.".into(),
                    Engine::Postgres => "Raise work_mem for this workload (per role or per session), or add an index that delivers rows in the needed order so the sort disappears.".into(),
                    Engine::Sqlite => "Give the sort an index that returns rows in the order asked for; otherwise PRAGMA temp_store = MEMORY keeps temporary b-trees in memory, and a larger cache_size helps them fit.".into(),
                    Engine::Mysql => "Raise tmp_table_size and max_heap_table_size for this workload so the temporary table stays in memory, or add an index that returns rows already ordered or grouped.".into(),
                },
                sql: None,
                cost: None,
            });
        }
        PlanWarning::NoJoinPredicate => {
            let engine = snapshot.server.name();
            match join_condition(&q.text) {
                JoinCondition::Missing => {
                    f.title = format!("{} joins tables without a join condition", capitalize(&handle));
                    f.summary = "Part of this plan combines every row of one input with every row of another (a cross join), and the statement has no condition joining them: no ON clause and no comparison of one table's column with another's. A missing join condition multiplies the rows the query reads and returns.".into();
                    f.severity = severity_for_workload_share(snapshot, impact.share);
                    f.confidence = Confidence::Medium;
                    f.fix = Some(Fix {
                        summary: "Check the query's joins and add the missing condition.".into(),
                        sql: None,
                        cost: None,
                    });
                }
                JoinCondition::Present => {
                    f.title = format!("{}'s plan joins two inputs without a join condition", capitalize(&handle));
                    f.summary = format!(
                        "Part of this plan combines every row of one input with every row of another, though the statement does join them. {engine} does this when both join columns are compared with the same value (a.x = @v AND a.x = b.x): it filters each side by the value and drops the join condition as redundant. That is harmless while both sides keep few rows."
                    );
                    f.severity = Severity::Low;
                    f.confidence = Confidence::Medium;
                    f.fix = Some(Fix {
                        summary: "Nothing to change while both inputs keep few rows. If the plan multiplies many rows, check the join columns and the filters on them in the plan.".into(),
                        sql: None,
                        cost: None,
                    });
                }
                JoinCondition::CrossJoin => {
                    f.title = format!("{} asks for a cross join", capitalize(&handle));
                    f.summary = "Part of this plan combines every row of one input with every row of another, and the statement asks for it (CROSS JOIN or CROSS APPLY). That multiplies rows: fine for a small list of values, costly for two large tables.".into();
                    f.severity = Severity::Low;
                    f.confidence = Confidence::Medium;
                    f.fix = Some(Fix {
                        summary: "Make sure the cross join is intended and both inputs stay small.".into(),
                        sql: None,
                        cost: None,
                    });
                }
            }
        }
        PlanWarning::MissingStatistics => {
            f.title = format!("{} is planned without statistics on some columns", capitalize(&handle));
            f.summary = "The optimizer had to guess how many rows match because some columns have no statistics. Guesses lead to the wrong join type or memory grant.".into();
            f.severity = Severity::Low;
            f.confidence = Confidence::Medium;
            // The plan's own insight knows whether the columns are an index's keys (statistics exist but are empty:
            // update them) or have none at all (a database setting): one reading for the finding and the plan view.
            let insight = q
                .plan
                .as_ref()
                .and_then(|p| culprit_core::insights::insights(p).into_iter().find(|i| i.rule == "missing-statistics"));
            let stale = q.plan.as_ref().and_then(culprit_core::insights::stale_index_statistics);
            f.fix = Some(match insight {
                Some(i) => {
                    f.summary = i.detail;
                    // An index's keys have statistics: say they're out of date, and what the catalog says of them.
                    if let Some(stale) = &stale {
                        f.title = format!(
                            "{} is planned on out-of-date statistics on {}",
                            capitalize(&handle),
                            format::list(&stale.columns)
                        );
                        if let Some(said) = statistics_now(snapshot, stale) {
                            f.summary = culprit_core::insights::stale_index_statistics_detail(stale, Some(&said));
                        }
                    }
                    Fix { summary: i.fix, sql: i.fix_sql, cost: None }
                }
                None => Fix {
                    summary: "Make sure automatic statistics creation is on, or create statistics on the columns named in the plan warning.".into(),
                    sql: None,
                    cost: None,
                },
            });
        }
        _ => return None,
    }
    f.verify = vec!["Re-scan after the change; the warning should be gone from this query's plan.".into()];
    Some(f)
}

/// What the catalog says now of the statistics a plan warned held nothing usable: when each was last updated, from how
/// many rows, and the changes counted since, with the table's rows when the plan was compiled and now. None when the
/// scan read none of them.
fn statistics_now(snapshot: &Snapshot, stale: &culprit_core::insights::StaleIndexStatistics) -> Option<String> {
    let (schema, table) = match stale.table.rsplit_once('.') {
        Some((schema, table)) => (schema.rsplit('.').next(), table),
        None => (None, stale.table.as_str()),
    };
    let same_table =
        |s: &str, t: &str| t.eq_ignore_ascii_case(table) && schema.is_none_or(|schema| s.eq_ignore_ascii_case(schema));
    let rows = |n: f64| format!("{} {}", format::count(n), if n == 1.0 { "row" } else { "rows" });
    let said: Vec<String> = stale
        .indexes
        .iter()
        .filter_map(|index| {
            let st = snapshot
                .statistics
                .iter()
                .find(|st| same_table(&st.schema, &st.table) && st.name.eq_ignore_ascii_case(index))?;
            let when = st.last_updated.as_deref().map(|at| {
                format!(
                    " were last updated on {}{}",
                    format::friendly_date(at, &snapshot.taken_at),
                    st.rows.map(|n| format!(", when the table had {}", rows(n))).unwrap_or_default()
                )
            })?;
            let since = match st.modifications {
                Some(m) if m > 0.0 => {
                    format!(
                        ", and have counted {} {} since",
                        format::count(m),
                        if m == 1.0 { "change" } else { "changes" }
                    )
                }
                Some(_) => ", with no changes counted since".into(),
                None => String::new(),
            };
            Some(format!("The statistics of {index}{when}{since}"))
        })
        .collect();
    if said.is_empty() {
        return None;
    }
    let now = snapshot.tables.iter().find(|t| same_table(&t.schema, &t.name)).and_then(|t| t.rows);
    let table_rows = match (stale.table_rows, now) {
        (Some(then), Some(now)) => {
            format!("; the table held {} when this plan was compiled, and holds {} now", rows(then), format::count(now))
        }
        (Some(then), None) => format!("; the table held {} when this plan was compiled", rows(then)),
        (None, Some(now)) => format!("; the table holds {} now", rows(now)),
        (None, None) => String::new(),
    };
    Some(format!("{}{table_rows}.", said.join("; ")))
}

pub(super) fn warning_key(w: PlanWarning) -> &'static str {
    match w {
        PlanWarning::ImplicitConversion => "implicit-conversion",
        PlanWarning::Spill => "spill",
        PlanWarning::NoJoinPredicate => "no-join-predicate",
        PlanWarning::MissingStatistics => "missing-statistics",
        PlanWarning::MemoryGrant => "memory-grant",
        PlanWarning::Misestimate => "misestimate",
        PlanWarning::ResidualPredicate => "residual-predicate",
    }
}

pub(super) fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    s.chars().take(max).collect::<String>() + "…"
}

/// What a statement's text says about the join a plan made without a join predicate.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum JoinCondition {
    /// No ON clause, and no comparison of one table's column with another's.
    Missing,
    /// It joins its tables on a condition: the engine dropped it (both sides compared with the same value).
    Present,
    /// It asks for every combination.
    CrossJoin,
}

pub(super) fn join_condition(text: &str) -> JoinCondition {
    let t = format!(" {} ", squeeze(text));
    if t.contains(" cross join ") || t.contains(" cross apply ") {
        return JoinCondition::CrossJoin;
    }
    let joins = t.matches(" join ").count();
    let ons = t.matches(" on ").count() + t.matches(" using(").count() + t.matches(" using ").count();
    if (joins > 0 && ons >= joins) || compares_two_tables(&t) {
        return JoinCondition::Present;
    }
    JoinCondition::Missing
}

/// A comparison of one table's column with another's (`o.customer_id = c.id`): an old-style join in WHERE.
pub(super) fn compares_two_tables(squeezed: &str) -> bool {
    let name = |c: char| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '$' | '@');
    let qualifier = |side: &str| -> Option<String> {
        let (q, column) = side.rsplit_once('.')?;
        let q = q.rsplit('.').next()?;
        let starts = |s: &str| s.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_');
        (starts(q) && starts(column)).then(|| q.to_string())
    };
    squeezed.match_indices('=').any(|(at, _)| {
        let before = squeezed[..at].trim_end_matches(['<', '>', '!', ' ']);
        if squeezed[..at].ends_with(['<', '>', '!']) {
            return false;
        }
        let left: String =
            before.chars().rev().take_while(|c| name(*c)).collect::<Vec<_>>().into_iter().rev().collect();
        let right: String = squeezed[at + 1..].trim_start().chars().take_while(|c| name(*c)).collect();
        matches!((qualifier(&left), qualifier(&right)), (Some(a), Some(b)) if a != b)
    })
}
