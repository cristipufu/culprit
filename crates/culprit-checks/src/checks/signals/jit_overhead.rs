//! The `jit-overhead` check: Statements where PostgreSQL's JIT compilation (pg_stat_statements jit_* columns, 15+) takes a large part of their run time.

use super::*;
use crate::evidence::{QueryMeasure, Requirement as Evidence};

pub fn jit_overhead_info() -> CheckInfo {
    CheckInfo {
        id: "jit-overhead".into(),
        title: "JIT compilation that takes much of a statement's time".into(),
        category: Category::Queries,
        engines: vec![Engine::Postgres],
        requires: vec![],
        description: "Statements where PostgreSQL's JIT compilation (pg_stat_statements jit_* columns, 15+) takes a large part of their run time.".into(),
    }
}

check!(JitOverheadCheck: jit_overhead_info, jit_overhead, &[Evidence::Query(QueryMeasure::Jit)]);

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
            let setting = threshold.as_ref().map_or_else(
                || "Culprit did not read jit_above_cost.".to_string(),
                |t| format!("jit_above_cost is {t} in Culprit's session."),
            );
            f.summary = format!(
                "PostgreSQL spent {} of its {} per run compiling parts of this statement to machine code ({} in all {}). \
                 JIT is chosen at plan time when the estimated cost exceeds the effective jit_above_cost. {setting} \
                 The application's role, database or session can use another setting, and a cached plan can keep a decision made before this scan. \
                 JIT pays off for long analytical queries; on short runs its compilation can cost more than it saves. These totals alone do not show that the planner overestimated the rows.",
                format::duration(per_run(jit, q.executions)),
                format::duration(q.mean_time_ms),
                format::duration(jit),
                ran_phrase(snapshot, q)
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
                Some(compare) => format!("Once the application's connections use the change, its JIT time per run should drop to zero or near it between two scans: {compare}."),
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
    let role = "-- For the role the application connects as (takes effect on its next connections):\nALTER ROLE <app_role> SET jit = off;";
    let Some(current) = current.and_then(|v| v.trim().parse::<f64>().ok()).filter(|v| v.is_finite() && *v >= 0.0)
    else {
        return Fix {
            summary: "Turn JIT off for the application's role, or raise jit_above_cost in the scope that enables it. This scan does not establish that threshold for the application, so it cannot choose a higher value safely. Check the role, database and session settings before changing it; a role's new connections use the SQL below unless the application overrides it.".into(),
            sql: Some(role.into()),
            cost: Some("This affects every statement that role runs, including analytical queries that may benefit from JIT.".into()),
        };
    };
    let target = (current.max(JIT_ABOVE_COST_DEFAULT) * 10.0).round();
    let change = setting_change(&snapshot.server, "jit_above_cost", &format!("{target:.0}"));
    Fix {
        summary: format!(
            "jit_above_cost is {} in Culprit's session; first check the application's effective threshold. If it uses this value, raise it so JIT starts only for more expensive plans: {} Apply the change in the role, database or session instead if that is where the application's threshold is set. Or turn JIT off for the application's role (the alternative SQL), whose new connections use it unless the application overrides it.",
            format::count(current),
            change.how
        ),
        sql: Some(match change.sql {
            Some(sql) => format!(
                "-- Only if the application uses the threshold Culprit read, with no role, database or session override:\n{sql}\n-- Alternative, for the application's role only:\n-- ALTER ROLE <app_role> SET jit = off;"
            ),
            None => role.to_string(),
        }),
        cost: None,
    }
}
