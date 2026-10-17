//! The `jit-overhead` check: Statements where PostgreSQL's JIT compilation (pg_stat_statements jit_* columns, 15+) takes a large part of their run time: usually short queries whose cost the planner overestimates.

use super::*;

pub fn jit_overhead_info() -> CheckInfo {
    CheckInfo {
        id: "jit-overhead".into(),
        title: "JIT compilation that costs more than it saves".into(),
        category: Category::Queries,
        engines: vec![Engine::Postgres],
        requires: vec![],
        description: "Statements where PostgreSQL's JIT compilation (pg_stat_statements jit_* columns, 15+) takes a large part of their run time: usually short queries whose cost the planner overestimates.".into(),
    }
}

check!(JitOverheadCheck: jit_overhead_info, jit_overhead);

pub fn jit_overhead(snapshot: &Snapshot) -> Vec<Finding> {
    let threshold = snapshot.setting("jit_above_cost").map(|s| s.value.clone());
    let mut findings: Vec<Finding> = app_queries(snapshot)
        .filter_map(|q| {
            let jit = q.costs.as_ref()?.jit_ms.filter(|ms| *ms >= 1_000.0)?;
            let share = per_run(jit, q.total_time_ms);
            (share >= 0.25).then_some((q, jit, share))
        })
        .map(|(q, jit, share)| {
            let mut f = finding("jit-overhead", &q.id, Category::Queries, subject(SubjectKind::Query, &q.id));
            let handle = query_handle(q);
            // A statement that stopped running is told in the past.
            let takes = if crate::all_stopped(snapshot, &[q]) { "took" } else { "takes" };
            f.title = if share >= 0.5 {
                format!("JIT compilation {takes} longer than running {handle}")
            } else {
                format!("JIT compilation {takes} {} of the time of {handle}", format::percent(share))
            };
            f.summary = format!(
                "PostgreSQL compiles parts of this statement to machine code before running it, because the planner estimates its cost above jit_above_cost{}. \
                 That takes {} of its {} per run ({} in all {}). JIT pays off for long analytical queries; for a statement that runs in {} the compiling costs more than it saves, usually because the planner overestimates its cost.",
                threshold.as_ref().map(|t| format!(" ({t})")).unwrap_or_default(),
                format::duration(per_run(jit, q.executions)),
                format::duration(q.mean_time_ms),
                format::duration(jit),
                ran_phrase(snapshot, q),
                format::duration(q.mean_time_ms)
            );
            // Medium needs both, as planning time does: most of the statement's time, and a minute a day of it.
            f.severity = if share >= 0.5 && window_at_least(snapshot, q, jit, 60_000.0) { Severity::Medium } else { Severity::Low };
            with_query(snapshot, &mut f, q);
            severity_score(&mut f);
            f.impact.label = format!("{} JIT", format::percent(share));
            f.confidence = Confidence::High;
            f.metrics = vec![
                Metric::new("JIT share", share, Unit::Ratio),
                Metric::new("JIT per run", per_run(jit, q.executions), Unit::Ms),
                Metric::new("Run time", q.mean_time_ms, Unit::Ms),
            ];
            f.fix = Some(jit_fix(snapshot, threshold.as_deref()));
            // On running totals the JIT time holds every run from before the change: two scans show it.
            let now = format!("{} over {} runs", format::duration(jit), format::count(q.executions));
            f.verify = vec![match crate::queries::verify_counts_between_scans(snapshot, "JIT time and runs", &now) {
                Some(compare) => format!("Between two scans, its JIT time per run should drop to zero or near it: {compare}."),
                None => "Re-scan: JIT time per run should drop to zero or near it, and the average run time with it.".into(),
            }];
            f
        })
        .collect();
    findings.sort_by(|a, b| b.impact.score.total_cmp(&a.impact.score));
    findings.truncate(5);
    findings
}

/// The default jit_above_cost, and the threshold the fix raises it to from there: ten times as high.
pub(super) const JIT_ABOVE_COST_DEFAULT: f64 = 100_000.0;

/// Raise jit_above_cost where this server is configured, from what it is now: ten times the default, or ten times the
/// current value when that is already higher, never lower. Or turn JIT off for the application's role.
pub(super) fn jit_fix(snapshot: &Snapshot, current: Option<&str>) -> Fix {
    let current = current.and_then(|v| v.trim().parse::<f64>().ok()).unwrap_or(JIT_ABOVE_COST_DEFAULT);
    let target = (current.max(JIT_ABOVE_COST_DEFAULT) * 10.0).round();
    let change = setting_change(&snapshot.server, "jit_above_cost", &format!("{target:.0}"));
    let role = "-- Or for the application's role only:\n-- ALTER ROLE app_user SET jit = off;";
    Fix {
        summary: format!(
            "Raise the cost threshold so JIT only starts for plans ten times as expensive as now ({} today): {} Or turn JIT off for the application's role. Fixing the row estimate that inflates the cost also helps.",
            format::count(current),
            change.how
        ),
        sql: Some(match change.sql {
            Some(sql) => format!(
                "-- JIT only for plans the planner thinks are 10× more expensive than today's threshold\n{sql}\n{role}"
            ),
            None => role.to_string(),
        }),
        cost: None,
    }
}
