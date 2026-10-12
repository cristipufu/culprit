use culprit_core::{
    Capability, Category, CheckInfo, Confidence, Engine, Finding, Fix, Metric, NodeKind, PlanNode, PlanWarning,
    QueryRef, QueryStat, Severity, Snapshot, SubjectKind, Unit, format,
};

use crate::support::*;

const HEAVY_SHARE: f64 = 0.05;

pub fn heavy_query_info() -> CheckInfo {
    CheckInfo {
        id: "heavy-query".into(),
        title: "Queries that dominate database time".into(),
        category: Category::Queries,
        engines: vec![Engine::Mssql, Engine::Postgres, Engine::Mysql],
        requires: vec![],
        description: "Single queries that take a large share of database time, with what their plan spends it on."
            .into(),
    }
}

pub fn heavy_query(snapshot: &Snapshot) -> Vec<Finding> {
    let Some(workload) = &snapshot.workload else {
        return Vec::new();
    };
    let mut queries: Vec<&QueryStat> = workload
        .queries
        .iter()
        .filter(|q| !is_monitoring_query(q) && workload.share(q.total_time_ms) >= HEAVY_SHARE)
        .collect();
    queries.sort_by(|a, b| b.total_time_ms.total_cmp(&a.total_time_ms));
    queries.into_iter().take(6).map(|q| heavy_query_finding(snapshot, q)).collect()
}

fn heavy_query_finding(snapshot: &Snapshot, q: &QueryStat) -> Finding {
    let impact = query_impact(snapshot, &[q]).expect("workload present");
    let handle = query_handle(q);
    let mut f = finding("heavy-query", &q.id, Category::Queries, subject(SubjectKind::Query, &q.id));
    apply_query_impact(&mut f, &impact);
    f.severity = severity_for_share(impact.share);
    f.confidence = Confidence::High;
    let per_day_runs = per_day(snapshot, q.executions);
    let reads_per_row = match (q.reads_per_execution(), q.rows_per_execution()) {
        (Some(reads), Some(rows)) => Some(reads / rows.max(1.0)),
        _ => None,
    };
    let hot = q.plan.as_ref().map(|p| (p, p.hottest()));
    f.title = match reads_per_row {
        Some(r) if r >= 1000.0 && snapshot.server.engine == Engine::Mysql => {
            format!("{} examines {} rows for every row it returns", capitalize(&handle), format::count(r))
        }
        Some(r) if r >= 1000.0 => {
            format!("{} reads {} pages for every row it returns", capitalize(&handle), format::count(r))
        }
        _ => format!("{} takes {} of database time", capitalize(&handle), format::percent(impact.share)),
    };
    let mut summary = format!(
        "It ran {} at {} on average: {} of all database time in {}.",
        runs_phrase(snapshot, q.executions),
        format::duration(q.mean_time_ms),
        format::percent(impact.share),
        snapshot.workload.as_ref().map(window_phrase).unwrap_or_default()
    );
    if let Some((plan, node)) = hot {
        let pct = if plan.total_cost > 0.0 { node.self_cost / plan.total_cost } else { 0.0 };
        summary.push_str(&format!(" {} of its estimated cost is {}.", format::percent(pct), describe_node(node)));
    }
    f.summary = summary;
    f.queries = vec![QueryRef { id: q.id.clone(), plan_node: hot.map(|(_, n)| n.id), note: None }];
    f.metrics.push(Metric::new("Share of DB time", impact.share, Unit::Ratio));
    f.metrics.push(Metric::new("Runs per day", per_day_runs, Unit::Count));
    f.metrics.push(Metric::new("Average duration", q.mean_time_ms, Unit::Ms));
    if let Some(r) = q.reads_per_execution() {
        let label =
            if snapshot.server.engine == Engine::Mysql { "Rows examined per run" } else { "Pages read per run" };
        f.metrics.push(Metric::new(label, r, Unit::Pages));
    }
    if let Some(r) = q.rows_per_execution() {
        f.metrics.push(Metric::new("Rows returned per run", r, Unit::Rows));
    }
    f.fix = Some(Fix { summary: heavy_fix_text(hot.map(|(_, n)| n), per_day_runs, q), sql: None, cost: None });
    f.verify = vec![
        "After the change ships, re-scan and compare this query's share of database time and reads per run.".into(),
    ];
    f
}

fn heavy_fix_text(hot: Option<&PlanNode>, per_day_runs: f64, q: &QueryStat) -> String {
    if let Some(node) = hot {
        if node.kind == NodeKind::Scan && node.predicate.is_some() {
            return format!(
                "The plan scans {} and filters afterwards. An index on the filtered columns turns that into a seek; open the plan to see the predicate.",
                node.object.as_deref().unwrap_or("the table")
            );
        }
        if node.kind == NodeKind::Lookup {
            return "Most of the work is looking rows up in the table after finding them in an index. Including the selected columns in that index (a covering index) removes the lookups.".into();
        }
        if node.kind == NodeKind::Sort {
            return "Most of the work is sorting. An index whose key order matches the ORDER BY lets the database read rows already sorted.".into();
        }
    }
    if per_day_runs >= 100_000.0 && q.mean_time_ms < 5.0 {
        return "Each run is fast; the cost is in how often it runs. Cache the result or batch the calls in the application.".into();
    }
    "Find this statement in the application (copy the brief for your coding agent) and check whether it reads more rows or columns than it needs.".into()
}

fn describe_node(node: &PlanNode) -> String {
    let what = node.op.to_ascii_lowercase();
    // "an index scan", "a seq scan".
    let a = if what.starts_with(['a', 'e', 'i', 'o', 'u']) { "an" } else { "a" };
    match (&node.object, node.rows_read.or(node.table_rows)) {
        (Some(object), Some(rows)) => format!("{a} {what} of {object} that reads {} rows", format::count(rows)),
        (Some(object), None) => format!("{a} {what} of {object}"),
        _ => format!("{a} {what}"),
    }
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(first) => first.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

pub fn chatty_query_info() -> CheckInfo {
    CheckInfo {
        id: "chatty-query".into(),
        title: "Chatty queries (N+1)".into(),
        category: Category::Queries,
        engines: vec![Engine::Mssql, Engine::Postgres, Engine::Mysql],
        requires: vec![],
        description: "Tiny queries executed so often that the round trips cost more than the work, typically a loop in application code.".into(),
    }
}

pub fn chatty_query(snapshot: &Snapshot) -> Vec<Finding> {
    let Some(workload) = &snapshot.workload else {
        return Vec::new();
    };
    if workload.total_executions <= 0.0 {
        return Vec::new();
    }
    workload
        .queries
        .iter()
        .filter(|q| !is_monitoring_query(q))
        .filter(|q| {
            let exec_share = q.executions / workload.total_executions;
            let per_day_runs = workload.per_day(q.executions);
            let small = q.rows_per_execution().is_some_and(|r| r <= 2.0) && q.mean_time_ms < 5.0;
            small && exec_share >= 0.08 && per_day_runs >= 20_000.0
        })
        .take(4)
        .map(|q| {
            let exec_share = q.executions / workload.total_executions;
            let per_day_runs = workload.per_day(q.executions);
            let impact = query_impact(snapshot, &[q]).expect("workload present");
            let mut f = finding("chatty-query", &q.id, Category::Queries, subject(SubjectKind::Query, &q.id));
            let rows = q.rows_per_execution().unwrap_or(1.0);
            let handle = capitalize(&query_handle(q));
            let runs = runs_phrase(snapshot, q.executions);
            let lower = q.text.trim_start_matches(|c: char| c.is_whitespace() || c == '(').to_ascii_lowercase();
            let writes = ["insert", "update", "delete", "merge"].iter().any(|v| lower.starts_with(v));
            let share = format::percent(impact.share);
            let exec = format::percent(exec_share);
            // Three shapes look alike in the counters and need different fixes: a poll that mostly finds nothing, a
            // write repeated row by row (or a heartbeat), and a read inside a loop (N+1).
            let (title, summary, fix, verify) = if rows < 0.5 {
                (
                    format!("{handle} runs {runs} and most runs find nothing"),
                    format!(
                        "This statement is {exec} of every statement the database executes, and on average a run {} {} row{}: the app is polling, asking on a timer whether there is work. Each run is cheap ({share} of database time in all), but every instance of the app asks on its own, so the cost grows with the fleet, not with the work.",
                        if writes { "changes" } else { "returns" },
                        format::count(rows),
                        if rows == 1.0 { "" } else { "s" },
                    ),
                    "Ask less often when the last run found nothing (back off to a few seconds), let one instance poll for the fleet, or have the writer wake the poller (a queue, a notification, a channel). Keep the poll on a small index, for example one filtered to the pending rows.",
                    "After the change, this statement's runs per day should drop by an order of magnitude while the work it finds stays the same.",
                )
            } else if writes {
                (
                    format!("{handle} runs {runs}, one row each time"),
                    format!(
                        "This statement is {exec} of every statement the database executes, and each run changes about {} row{}. Writes that small and that frequent are usually a loop writing row by row, or a heartbeat; each one is a round trip and a log flush ({share} of database time in all).",
                        format::count(rows),
                        if rows <= 1.0 { "" } else { "s" },
                    ),
                    "Write many rows per statement (a multi-row VALUES, a table-valued parameter, or a bulk insert) inside one transaction; for a heartbeat or lease, renew less often.",
                    "After the change, this statement's runs per day should drop by orders of magnitude.",
                )
            } else {
                (
                    format!("{handle} runs {runs}, one row at a time"),
                    format!(
                        "This statement is {exec} of every statement the database executes, and each run returns about {} row{}. That pattern usually comes from a loop in application code (N+1): one query per item instead of one query for all items. The database time is small ({share}), but each call costs a network round trip and a connection.",
                        format::count(rows),
                        if rows <= 1.0 { "" } else { "s" },
                    ),
                    "Load the rows in one query (WHERE key IN (…) or a join) before the loop, or cache them. ORMs usually have an eager-loading option for this.",
                    "After the change, this statement's executions per day should drop by orders of magnitude.",
                )
            };
            f.title = title;
            f.summary = summary;
            f.severity = if exec_share >= 0.25 { Severity::Medium } else { Severity::Low };
            f.impact.score = (score_for_share(impact.share) + exec_share * 40.0).min(60.0);
            f.impact.db_time_share = Some(impact.share);
            f.impact.time_ms_per_day = Some(impact.per_day_ms);
            f.impact.label = format!("{} of executions", format::percent(exec_share));
            f.confidence = Confidence::Medium;
            f.queries = query_refs(&[q]);
            f.metrics.push(Metric::new("Runs per day", per_day_runs, Unit::Count));
            f.metrics.push(Metric::new("Share of executions", exec_share, Unit::Ratio));
            f.metrics.push(Metric::new("Average duration", q.mean_time_ms, Unit::Ms));
            f.fix = Some(Fix { summary: fix.into(), sql: None, cost: None });
            f.verify = vec![verify.into()];
            f.caveats.push("Some high-frequency single-row lookups are legitimate (for example, by primary key on a hot path). Check the calling code before batching.".into());
            f
        })
        .collect()
}

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

/// One root cause (a type mismatch on one column) is one finding, however many queries hit it.
fn merge_same_cause(snapshot: &Snapshot, findings: Vec<Finding>) -> Vec<Finding> {
    let mut merged: Vec<Finding> = Vec::new();
    for f in findings {
        match merged.iter_mut().find(|m| m.id == f.id) {
            Some(m) => {
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
        m.severity = m.severity.max(severity_for_share(impact.share));
        let column = m.subject.name.clone();
        m.title = format!("A type mismatch on {column} stops {} queries from using an index", queries.len());
        m.summary.push_str(&format!(
            " The same mismatch appears in {} queries that together take {} of database time.",
            queries.len(),
            format::percent(impact.share)
        ));
        m.metrics.retain(|x| x.label != "Share of DB time" && x.label != "Average duration");
        m.metrics.insert(0, Metric::new("Share of DB time", impact.share, Unit::Ratio));
        m.metrics.insert(1, Metric::new("Queries affected", queries.len() as f64, Unit::Count));
        m.metrics.insert(2, Metric::new("DB time per day", impact.per_day_ms, Unit::Ms));
    }
    merged
}

fn plan_warning_finding(
    snapshot: &Snapshot,
    q: &QueryStat,
    impact: &QueryImpact,
    warning: PlanWarning,
    node: Option<&PlanNode>,
) -> Option<Finding> {
    let key = format!("{}:{}", q.id, warning_key(warning));
    let mut f = finding("plan-warnings", &key, Category::Queries, subject(SubjectKind::Query, &q.id));
    apply_query_impact(&mut f, impact);
    f.queries = vec![QueryRef { id: q.id.clone(), plan_node: node.map(|n| n.id), note: Some(warning.label().into()) }];
    f.metrics.push(Metric::new("Share of DB time", impact.share, Unit::Ratio));
    f.metrics.push(Metric::new("Average duration", q.mean_time_ms, Unit::Ms));
    let handle = query_handle(q);
    match warning {
        PlanWarning::ImplicitConversion => {
            let node_props = node.into_iter().flat_map(|n| n.props.values());
            let plan_props = q.plan.iter().flat_map(|p| p.props.values());
            // A conversion of the optimizer's own values (a VALUES list's Union1006, Expr1002) or of a parameter happens
            // once per value, not per row of a table: it can't stop an index from being used.
            let all: Vec<&str> = node_props
                .chain(plan_props)
                .flat_map(|v| v.lines())
                .filter(|l| l.contains("CONVERT_IMPLICIT"))
                .collect();
            let lines: Vec<&str> = all.iter().copied().filter(|l| !converts_internal_value(l)).collect();
            if !all.is_empty() && lines.is_empty() {
                return None;
            }
            let issue = lines.iter().find(|l| l.contains("Seek Plan")).or(lines.first()).copied();
            let blocks_seek = issue.is_some_and(|l| l.contains("Seek Plan"));
            let column = issue.and_then(convert_column);
            let convert = issue.map(|l| l.split_once(": ").map(|(_, e)| e).unwrap_or(l).to_string());
            f.title = match &column {
                Some(c) => format!("A type mismatch on {c} stops {handle} from using an index"),
                None => format!("A type mismatch slows down {handle}"),
            };
            f.summary = format!(
                "The database has to convert {} on every row before comparing it with a parameter of a different type{}. A converted column can't be searched with an index, so it reads every row instead.{}",
                column.as_deref().unwrap_or("a column"),
                convert.map(|c| format!(" (`{}`)", truncate(&c, 140))).unwrap_or_default(),
                match &q.parameters {
                    Some(p) if p.to_ascii_lowercase().contains("nvarchar") => format!(" The application declares its parameters as `{}`: Unicode strings compared with a non-Unicode column.", truncate(p, 120)),
                    Some(p) => format!(" The application declares its parameters as `{}`.", truncate(p, 120)),
                    None => " This often happens when an application sends strings as nvarchar (Unicode) to a varchar column.".to_string(),
                }
            );
            f.severity =
                if blocks_seek { severity_for_share(impact.share).max(Severity::Medium) } else { Severity::Low };
            f.confidence = if blocks_seek { Confidence::High } else { Confidence::Medium };
            f.fix = Some(Fix {
                summary: "Send the parameter with the column's exact type. In Node.js (mssql/tedious) declare the parameter as VarChar instead of the default NVarChar; in .NET set SqlDbType.VarChar; with an ORM, map the column as non-Unicode.".into(),
                sql: None,
                cost: None,
            });
            if let Some(c) = &column {
                f.id = format!("plan-warnings:implicit-conversion:{c}");
                f.subject = subject(SubjectKind::Table, c);
                f.metrics.push(Metric::text("Column", c.clone()));
            }
        }
        PlanWarning::Spill => {
            f.title = format!("{} runs out of memory and spills to disk", capitalize(&handle));
            f.summary = "A sort or hash in this plan needed more memory than it was granted and wrote the overflow to disk (tempdb or temp files). Spills usually come from a row estimate that was far too low, or a sort the index could have avoided.".into();
            f.severity = severity_for_share(impact.share);
            f.confidence = Confidence::High;
            f.fix = Some(Fix {
                summary: match snapshot.server.engine {
                    Engine::Mssql => "Update statistics on the tables involved; if the estimate is still far off, add an index that delivers rows in the needed order so the sort disappears.".into(),
                    Engine::Postgres => "Raise work_mem for this workload (per role or per session), or add an index that delivers rows in the needed order so the sort disappears.".into(),
                    Engine::Mysql => "Raise tmp_table_size and max_heap_table_size for this workload so the temporary table stays in memory, or add an index that returns rows already ordered or grouped.".into(),
                },
                sql: None,
                cost: None,
            });
        }
        PlanWarning::NoJoinPredicate => {
            f.title = format!("{} joins tables without a join condition", capitalize(&handle));
            f.summary = "Part of this plan combines every row of one input with every row of another (a cross join). That's almost always a missing ON condition in the query.".into();
            f.severity = severity_for_share(impact.share).max(Severity::Medium);
            f.confidence = Confidence::Medium;
            f.fix = Some(Fix {
                summary: "Check the query's joins and add the missing condition.".into(),
                sql: None,
                cost: None,
            });
        }
        PlanWarning::MissingStatistics => {
            f.title = format!("{} is planned without statistics on some columns", capitalize(&handle));
            f.summary = "The optimizer had to guess how many rows match because some columns have no statistics. Guesses lead to the wrong join type or memory grant.".into();
            f.severity = Severity::Low;
            f.confidence = Confidence::Medium;
            f.fix = Some(Fix {
                summary: "Make sure automatic statistics creation is on, or create statistics on the columns named in the plan warning.".into(),
                sql: None,
                cost: None,
            });
        }
        _ => return None,
    }
    f.verify = vec!["Re-scan after the change; the warning should be gone from this query's plan.".into()];
    Some(f)
}

fn warning_key(w: PlanWarning) -> &'static str {
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

/// Whether every CONVERT_IMPLICIT in `text` converts an expression the optimizer made up (`[Union1006]`, `[Expr1002]`)
/// or a parameter, rather than a table's column.
fn converts_internal_value(text: &str) -> bool {
    let mut any = false;
    for (at, _) in text.match_indices("CONVERT_IMPLICIT(") {
        any = true;
        let rest = &text[at + "CONVERT_IMPLICIT(".len()..];
        let Some(after_type) = rest.find("),").map(|i| &rest[i + 2..]).or_else(|| rest.split_once(',').map(|(_, r)| r))
        else {
            return false;
        };
        let expr = after_type.split(',').next().unwrap_or("").trim();
        let name = expr.trim_matches(|c| c == '[' || c == ']');
        let internal = !name.contains('.') && {
            let digits = name.trim_start_matches(|c: char| c.is_ascii_alphabetic());
            digits.len() >= 3 && digits.len() < name.len() && digits.chars().all(|c| c.is_ascii_digit())
        };
        if !(internal || name.starts_with('@')) {
            return false;
        }
    }
    any
}

/// Pull "Users.Email" out of `CONVERT_IMPLICIT(nvarchar(256),[db].[dbo].[Users].[Email],0)`.
fn convert_column(text: &str) -> Option<String> {
    let start = text.find("CONVERT_IMPLICIT(")? + "CONVERT_IMPLICIT(".len();
    let rest = &text[start..];
    let after_type = rest.find("),").map(|i| &rest[i + 2..]).or_else(|| rest.split_once(',').map(|(_, r)| r))?;
    let expr = after_type.split(',').next()?;
    let parts: Vec<&str> = expr.split('.').map(|p| p.trim_matches(|c| c == '[' || c == ']')).collect();
    match parts.as_slice() {
        [.., table, column] if !column.is_empty() && !column.starts_with('@') => Some(format!("{table}.{column}")),
        [column] if !column.is_empty() && !column.starts_with('@') => Some(column.to_string()),
        _ => None,
    }
}

#[cfg(test)]
mod convert_tests {
    #[test]
    fn conversions_of_the_optimizers_own_values_are_not_column_mismatches() {
        assert!(super::converts_internal_value("CONVERT_IMPLICIT(varchar(36),[Union1006],0)"));
        assert!(super::converts_internal_value("Cardinality Estimate: CONVERT_IMPLICIT(varchar(8),[Expr1002],0)"));
        assert!(super::converts_internal_value("CONVERT_IMPLICIT(varchar(64),[@par2],0)"));
        assert!(!super::converts_internal_value(
            "CONVERT_IMPLICIT(nvarchar(255),[db].[shop].[jobs].[run_key],0)=[@par4]"
        ));
        assert!(!super::converts_internal_value("CONVERT_IMPLICIT(nvarchar(20),[status],0)"));
        assert!(!super::converts_internal_value("no conversion here"));
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    s.chars().take(max).collect::<String>() + "…"
}

pub fn plan_regression_info() -> CheckInfo {
    CheckInfo {
        id: "plan-regression".into(),
        title: "Plan regressions".into(),
        category: Category::Queries,
        engines: vec![Engine::Mssql],
        requires: vec![Capability::QueryStore],
        description: "Queries whose current plan is much slower than a plan they used before.".into(),
    }
}

pub fn plan_regression(snapshot: &Snapshot) -> Vec<Finding> {
    app_queries(snapshot)
        .filter_map(|q| {
            let usable: Vec<_> = q.plans.iter().filter(|p| p.executions >= 5.0).collect();
            if usable.len() < 2 {
                return None;
            }
            let latest = usable.iter().max_by(|a, b| a.last_seen.cmp(&b.last_seen))?;
            let best = usable.iter().min_by(|a, b| a.mean_time_ms.total_cmp(&b.mean_time_ms))?;
            if latest.id == best.id || latest.mean_time_ms < best.mean_time_ms * 2.0 || latest.mean_time_ms - best.mean_time_ms < 5.0 {
                return None;
            }
            let impact = query_impact(snapshot, &[q])?;
            let factor = latest.mean_time_ms / best.mean_time_ms.max(0.001);
            let mut f = finding("plan-regression", &q.id, Category::Queries, subject(SubjectKind::Query, &q.id));
            apply_query_impact(&mut f, &impact);
            let wasted = (latest.mean_time_ms - best.mean_time_ms) * latest.executions;
            f.impact.time_ms_per_day = Some(per_day(snapshot, wasted));
            f.impact.label = format!("{} slower", format::factor(factor));
            f.severity = severity_for_share(impact.share).max(Severity::Medium);
            f.confidence = Confidence::High;
            f.title = format!("{} got {} slower after its plan changed", capitalize(&query_handle(q)), format::factor(factor));
            f.summary = format!(
                "Query Store saw this query run with plan {} at {} on average, and now with plan {} at {}. The plan choice changed, typically after a statistics update, a schema change or a recompile with unusual parameter values.",
                best.id,
                format::duration(best.mean_time_ms),
                latest.id,
                format::duration(latest.mean_time_ms)
            );
            f.queries = query_refs(&[q]);
            f.metrics.push(Metric::new("Fast plan", best.mean_time_ms, Unit::Ms));
            f.metrics.push(Metric::new("Current plan", latest.mean_time_ms, Unit::Ms));
            f.metrics.push(Metric::new("Extra time per day", per_day(snapshot, wasted), Unit::Ms));
            f.fix = Some(Fix {
                summary: "Force the fast plan now to stop the bleeding, then find out why the optimizer changed its mind (statistics, parameter values, schema). Unforce once the cause is fixed.".into(),
                sql: Some(format!("EXEC sp_query_store_force_plan @query_id = {}, @plan_id = {};", q.id, best.id)),
                cost: None,
            });
            f.verify = vec!["The query's average duration in Query Store should return to the fast plan's level within an interval or two.".into()];
            f.caveats.push("A forced plan can fail to apply after schema changes; Query Store reports that in sys.query_store_plan.last_force_failure_reason_desc.".into());
            Some(f)
        })
        .collect()
}

pub fn unstable_query_info() -> CheckInfo {
    CheckInfo {
        id: "unstable-query".into(),
        title: "Unstable queries".into(),
        category: Category::Queries,
        engines: vec![Engine::Mssql, Engine::Postgres, Engine::Mysql],
        requires: vec![],
        description: "Queries whose duration swings wildly between runs with the same plan, a sign of parameter sensitivity or blocking.".into(),
    }
}

pub fn unstable_query(snapshot: &Snapshot) -> Vec<Finding> {
    app_queries(snapshot)
        .filter_map(|q| {
            let stddev = q.stddev_time_ms?;
            let max = q.max_time_ms?;
            if q.executions < 50.0 || q.mean_time_ms <= 0.0 || q.plans.len() > 1 {
                return None;
            }
            let cv = stddev / q.mean_time_ms;
            if cv < 3.0 || max < q.mean_time_ms * 50.0 || max < 1000.0 {
                return None;
            }
            let impact = query_impact(snapshot, &[q])?;
            if impact.share < 0.01 {
                return None;
            }
            let mut f = finding("unstable-query", &q.id, Category::Queries, subject(SubjectKind::Query, &q.id));
            apply_query_impact(&mut f, &impact);
            f.severity = severity_for_share(impact.share);
            f.confidence = Confidence::Medium;
            f.title = format!(
                "{} usually takes {} but sometimes {}",
                capitalize(&query_handle(q)),
                format::duration(q.mean_time_ms),
                format::duration(max)
            );
            f.summary = "Same query, same plan, very different run times. Either some parameter values match far more rows than the plan was built for (parameter sensitivity), or some runs wait on locks. Check the waits for this query, then the parameter values of the slow runs.".into();
            f.queries = query_refs(&[q]);
            f.metrics.push(Metric::new("Average", q.mean_time_ms, Unit::Ms));
            f.metrics.push(Metric::new("Slowest", max, Unit::Ms));
            f.metrics.push(Metric::new("Std deviation", stddev, Unit::Ms));
            // The same ladders as `parameter-sensitive`: the cause is usually the same, only the evidence is coarser.
            f.fix = Some(match snapshot.server.engine {
                Engine::Mssql => crate::sensitivity::mssql_fix(q, None),
                Engine::Postgres => crate::sensitivity::postgres_fix(),
                Engine::Mysql => crate::sensitivity::mysql_fix(0.0, snapshot.server.name() == "MariaDB"),
            });
            if snapshot.server.engine == Engine::Postgres {
                f.caveats.push("PostgreSQL keeps one minimum, average, maximum and standard deviation per statement, so Culprit can't tell which values were slow or how many calls were.".into());
            }
            Some(f)
        })
        .collect()
}

pub fn temp_spill_info() -> CheckInfo {
    CheckInfo {
        id: "temp-spill".into(),
        title: "Queries writing temp files".into(),
        category: Category::Queries,
        engines: vec![Engine::Postgres, Engine::Mysql],
        requires: vec![],
        description: "Statements whose sorts, hashes or temporary tables outgrow memory and spill to disk.".into(),
    }
}

pub fn temp_spill(snapshot: &Snapshot) -> Vec<Finding> {
    match snapshot.server.engine {
        Engine::Postgres => {}
        Engine::Mysql => return mysql_disk_temp_tables(snapshot),
        Engine::Mssql => return Vec::new(),
    }
    let work_mem = snapshot.setting("work_mem").map(|s| s.value.clone()).unwrap_or_else(|| "4MB".into());
    app_queries(snapshot)
        .filter(|q| q.temp_writes.is_some_and(|t| t > 0.0))
        .filter_map(|q| {
            let impact = query_impact(snapshot, &[q])?;
            if impact.share < 0.01 {
                return None;
            }
            let per_run = q.temp_writes? * 8192.0 / q.executions.max(1.0);
            let mut f = finding("temp-spill", &q.id, Category::Queries, subject(SubjectKind::Query, &q.id));
            apply_query_impact(&mut f, &impact);
            f.severity = severity_for_share(impact.share);
            f.confidence = Confidence::High;
            f.title = format!("{} writes {} of temp files per run", capitalize(&query_handle(q)), format::bytes(per_run));
            f.summary = format!(
                "A sort or hash in this statement needs more than work_mem ({work_mem}) and spills to disk on every run. Disk spills are many times slower than in-memory sorts."
            );
            f.queries = query_refs(&[q]);
            f.metrics.push(Metric::new("Temp written per run", per_run, Unit::Bytes));
            f.metrics.push(Metric::new("Average duration", q.mean_time_ms, Unit::Ms));
            f.fix = Some(Fix {
                summary: "Give this workload more sort memory, scoped to the role that runs it rather than server-wide, or add an index that returns rows already ordered.".into(),
                sql: Some(format!(
                    "ALTER ROLE current_user SET work_mem = '{}';",
                    suggested_work_mem(per_run)
                )),
                cost: Some("work_mem is per sort/hash node per connection; size it with your connection count in mind.".into()),
            });
            Some(f)
        })
        .collect()
}

/// MySQL counts on-disk temporary tables per digest: a GROUP BY, DISTINCT or ORDER BY whose temporary table outgrew memory.
fn mysql_disk_temp_tables(snapshot: &Snapshot) -> Vec<Finding> {
    let limit = |name: &str| snapshot.setting(name).and_then(|s| s.value.parse::<f64>().ok());
    let memory_limit = match (limit("tmp_table_size"), limit("max_heap_table_size")) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    };
    app_queries(snapshot)
        .filter(|q| q.temp_writes.is_some_and(|t| t > 0.0))
        .filter_map(|q| {
            let impact = query_impact(snapshot, &[q])?;
            if impact.share < 0.01 {
                return None;
            }
            let share_of_runs = (q.temp_writes? / q.executions.max(1.0)).min(1.0);
            let mut f = finding("temp-spill", &q.id, Category::Queries, subject(SubjectKind::Query, &q.id));
            apply_query_impact(&mut f, &impact);
            f.severity = severity_for_share(impact.share);
            f.confidence = Confidence::High;
            f.title = format!(
                "{} builds an on-disk temporary table on {} of its runs",
                capitalize(&query_handle(q)),
                format::percent(share_of_runs)
            );
            f.summary = format!(
                "The temporary table behind a GROUP BY, DISTINCT or ORDER BY in this statement outgrows the in-memory limit{} and moves to disk, which is many times slower.",
                memory_limit.map(|b| format!(" ({})", format::bytes(b))).unwrap_or_default()
            );
            f.queries = query_refs(&[q]);
            f.metrics.push(Metric::new("Runs that spilled", share_of_runs, Unit::Ratio));
            f.metrics.push(Metric::new("Average duration", q.mean_time_ms, Unit::Ms));
            f.fix = Some(Fix {
                summary: "Add an index that delivers rows already grouped or ordered so no temporary table is needed, or raise the in-memory limit (both variables; the smaller one wins).".into(),
                sql: Some("SET GLOBAL tmp_table_size = 64 * 1024 * 1024;\nSET GLOBAL max_heap_table_size = 64 * 1024 * 1024;".into()),
                cost: Some("The limit is per temporary table per connection; size it with your connection count in mind.".into()),
            });
            Some(f)
        })
        .collect()
}

fn suggested_work_mem(per_run_bytes: f64) -> String {
    let mb = (per_run_bytes / (1024.0 * 1024.0) * 2.0).ceil().clamp(16.0, 512.0);
    let rounded = 2f64.powf(mb.log2().ceil());
    format!("{rounded:.0}MB")
}

#[cfg(test)]
mod wording_tests {
    use super::*;
    use culprit_core::{NodeKind, Properties};

    #[test]
    fn plan_steps_take_the_right_article() {
        let mut n = PlanNode {
            id: 1,
            op: "Index Scan".into(),
            kind: NodeKind::Scan,
            detail: None,
            object: Some("orders".into()),
            index: None,
            seek_predicate: None,
            predicate: None,
            est_rows: 1.0,
            est_executions: 1.0,
            act_rows: None,
            act_executions: None,
            rows_read: None,
            table_rows: None,
            subtree_cost: 1.0,
            self_cost: 1.0,
            act_time_ms: None,
            warnings: vec![],
            props: Properties::new(),
            children: vec![],
            runtime: None,
        };
        assert_eq!(describe_node(&n), "an index scan of orders");
        n.op = "Seq Scan".into();
        assert_eq!(describe_node(&n), "a seq scan of orders");
    }
}
