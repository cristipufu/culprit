//! The `planning-time` check: Statements where planning (PostgreSQL, with pg_stat_statements.track_planning on) or compiling (SQL Server Query Store) takes a large part of their time, usually because the plan isn't reused.

use super::*;

pub fn planning_time_info() -> CheckInfo {
    CheckInfo {
        id: "planning-time".into(),
        title: "Queries that spend their time being planned".into(),
        category: Category::Queries,
        engines: vec![Engine::Mssql, Engine::Postgres],
        requires: vec![],
        description: "Statements where planning (PostgreSQL, with pg_stat_statements.track_planning on) or compiling (SQL Server Query Store) takes a large part of their time, usually because the plan isn't reused.".into(),
    }
}

check!(PlanningTimeCheck: planning_time_info, planning_time);

pub fn planning_time(snapshot: &Snapshot) -> Vec<Finding> {
    let pg = snapshot.server.engine == Engine::Postgres;
    let mut findings: Vec<Finding> = app_queries(snapshot)
        .filter_map(|q| {
            let costs = q.costs.as_ref()?;
            let plan_ms = costs.plan_ms.filter(|ms| *ms >= 5_000.0)?;
            let share = plan_ms / (plan_ms + q.total_time_ms);
            (share >= 0.25 && q.executions >= 50.0).then_some((q, plan_ms, costs.plans.unwrap_or(0.0), share))
        })
        .map(|(q, plan_ms, plans, share)| {
            let mut f = finding("planning-time", &q.id, Category::Queries, subject(SubjectKind::Query, &q.id));
            let handle = query_handle(q);
            let word = if pg { "Planning" } else { "Compiling" };
            let takes = if crate::all_stopped(snapshot, &[q]) { "took" } else { "takes" };
            f.title = format!("{word} {takes} {} of the time of {handle}", share_words(share));
            let per_plan = per_run(plan_ms, plans.max(1.0));
            let every_run = plans >= q.executions * 0.9;
            let recompile_hint = !pg && culprit_core::sql::has_mssql_recompile_hint(&q.text);
            let reuse = if every_run {
                "a new plan for every run".to_string()
            } else {
                format!("a plan reused about {} times", format::count(per_run(q.executions, plans.max(1.0))))
            };
            f.summary = format!(
                "Each {} takes {} and the statement then runs for {}: {} of its time goes to {}, {} {} ({}). \
                 Planning gets expensive with many joins, partitions or large statistics targets, and it repeats when the plan isn't reused.",
                if pg { "plan" } else { "compile" },
                format::duration(per_plan),
                format::duration(q.mean_time_ms),
                share_words(share),
                if pg { "planning" } else { "compiling" },
                format::duration(plan_ms),
                ran_phrase(snapshot, q),
                reuse
            );
            if recompile_hint {
                f.summary.push_str(" Its OPTION (RECOMPILE) hint explicitly asks SQL Server for a new plan on every run.");
            }
            f.severity = if share >= 0.5 && window_at_least(snapshot, q, plan_ms, 60_000.0) { Severity::Medium } else { Severity::Low };
            with_query(snapshot, &mut f, q);
            severity_score(&mut f);
            f.impact.label = format!("{} planning", format::percent(share));
            f.confidence = if pg { Confidence::High } else { Confidence::Medium };
            f.metrics = vec![
                Metric::new("Planning share", share, Unit::Ratio),
                Metric::new("Per plan", per_plan, Unit::Ms),
                Metric::new("Plans", plans, Unit::Count),
                Metric::new("Runs", q.executions, Unit::Count),
            ];
            f.fix = Some(if pg {
                Fix {
                    summary: "Prepare it once and reuse it: most drivers do when you bind parameters (pgjdbc prepareThreshold, Npgsql Max Auto Prepare, psycopg prepare=True). Behind PgBouncer in transaction mode, prepared statements need 1.21+ with max_prepared_statements set. If planning stays slow when reused, look at the number of joins and partitions it touches.".into(),
                    sql: None,
                    cost: None,
                }
            } else if recompile_hint {
                Fix {
                    summary: "Test removing OPTION (RECOMPILE) from this statement so SQL Server can reuse a plan. Compare representative parameter values, including those that need different plans: keep the hint if the extra runtime from reuse costs more than the compiles it saves. Forced parameterization does not override this hint.".into(),
                    sql: None,
                    cost: None,
                }
            } else if every_run {
                Fix {
                    summary: "It compiles on every run: look for OPTION (RECOMPILE), a procedure created WITH RECOMPILE, or literal values in the text instead of parameters (sp_executesql with parameters reuses one plan). Forced parameterization reuses plans for statements that differ only in literals, for the whole database: test it first.".into(),
                    sql: Some("-- Database-wide: statements that differ only in literals share a plan\nALTER DATABASE CURRENT SET PARAMETERIZATION FORCED;".into()),
                    cost: None,
                }
            } else {
                Fix {
                    summary: "Plans are reused but each compile is slow: simplify the statement (fewer joins and views), or keep its plan with a Query Store hint or plan forcing so it isn't recompiled after statistics change.".into(),
                    sql: None,
                    cost: None,
                }
            });
            // On running totals the planning time holds every plan from before the change: two scans show it.
            let now = format!("{} over {} runs", format::duration(plan_ms), format::count(q.executions));
            f.verify = vec![match crate::queries::verify_counts_between_scans(snapshot, "planning time and runs", &now) {
                Some(compare) => format!("Between two scans, its planning time per run should drop: {compare}."),
                None => "Re-scan: planning should be a small part of its time.".into(),
            }];
            if recompile_hint {
                f.verify.push("Compare compiles per run and planning time per run before and after, and check that time per run stays acceptable across representative parameter values. The rewritten statement has a new entry.".into());
            }
            if !pg {
                f.caveats.push("Query Store counts compiles over the query's whole life but keeps runtime statistics only for its retention period: Culprit scales the compiles to the period it keeps runs for and spreads them over the window's runs, so the time per run is an estimate.".into());
            }
            f
        })
        .collect();
    findings.sort_by(|a, b| b.impact.score.total_cmp(&a.impact.score));
    findings.truncate(5);
    findings
}
