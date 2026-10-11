//! Parameter sensitivity: a query that is fast for most calls and slow for some, usually because a plan compiled for
//! one value is reused for very different ones (SQL Server, Postgres generic plans) or because some values touch far
//! more rows (any engine). Each engine shows it through what it keeps: Query Store's plans used side by side, and
//! MySQL's run-time histogram. A single plan with wildly varying run times (all engines, including Postgres's
//! min/max/stddev) is `unstable-query`, which shares the fix ladders below.

use culprit_core::{
    Category, CheckInfo, Confidence, Engine, Finding, Fix, Metric, PlanVariant, QueryStat, Snapshot, SubjectKind, Unit,
    format,
};

use crate::support::*;

/// Enough calls that "most" and "some" mean something.
const MIN_RUNS: f64 = 50.0;
/// A plan has to carry a real share of the calls to count as one of "the" plans.
const MIN_PLAN_SHARE: f64 = 0.01;
/// How much slower the slow side must be, and by how much in absolute terms.
const MIN_FACTOR: f64 = 5.0;
const MIN_GAP_MS: f64 = 5.0;
/// The slowest runs must stand far out from the typical one.
const MIN_TAIL_FACTOR: f64 = 10.0;
const MIN_TAIL_MS: f64 = 200.0;

pub fn parameter_sensitive_info() -> CheckInfo {
    CheckInfo {
        id: "parameter-sensitive".into(),
        title: "Queries that are fast for most calls and slow for some".into(),
        category: Category::Queries,
        engines: vec![Engine::Mssql, Engine::Mysql],
        requires: vec![],
        description: "Statements whose run time depends heavily on the values they run with: two plans used side by side, one fast and one slow (SQL Server Query Store), or a long tail in the run-time histogram (MySQL). Single-plan variance on any engine is `unstable-query`.".into(),
    }
}

/// How the slow calls show up in the data.
#[derive(Debug)]
enum Shape<'a> {
    /// Two plans of one query: one fast, one slow (Query Store).
    Plans { fast: &'a PlanVariant, slow: &'a PlanVariant },
    /// A latency histogram with a long tail (MySQL).
    Tail { typical_ms: f64, p99_ms: f64, slow_share: Option<f64> },
}

fn plans_shape(q: &QueryStat) -> Option<Shape<'_>> {
    let busy: Vec<&PlanVariant> =
        q.plans.iter().filter(|p| p.executions >= 10.0 && p.executions >= q.executions * MIN_PLAN_SHARE).collect();
    let fast = busy.iter().min_by(|a, b| a.mean_time_ms.total_cmp(&b.mean_time_ms))?;
    let slow = busy.iter().max_by(|a, b| a.mean_time_ms.total_cmp(&b.mean_time_ms))?;
    // Both plans in use at the same time: values pick the plan. A fast plan that was replaced is a regression instead.
    let overlap = match (
        fast.last_seen.as_deref(),
        slow.first_seen.as_deref(),
        slow.last_seen.as_deref(),
        fast.first_seen.as_deref(),
    ) {
        (Some(fast_last), Some(slow_first), Some(slow_last), Some(fast_first)) => {
            fast_last >= slow_first && slow_last >= fast_first
        }
        _ => true,
    };
    (fast.id != slow.id
        && overlap
        && slow.mean_time_ms >= fast.mean_time_ms * MIN_FACTOR
        && slow.mean_time_ms - fast.mean_time_ms >= MIN_GAP_MS)
        .then_some(Shape::Plans { fast, slow })
}

fn tail_shape(q: &QueryStat) -> Option<Shape<'_>> {
    let spread = q.spread.as_ref()?;
    let typical = spread.p50_ms.or(spread.p95_ms.map(|p| p.min(q.mean_time_ms)))?;
    let p99 = spread.p99_ms?;
    if p99 < typical.max(0.05) * MIN_TAIL_FACTOR || p99 < MIN_TAIL_MS {
        return None;
    }
    let threshold = typical * MIN_TAIL_FACTOR;
    let total: f64 = spread.histogram.iter().map(|b| b[2]).sum();
    let slow: f64 = spread.histogram.iter().filter(|b| b[0] >= threshold).map(|b| b[2]).sum();
    let slow_share = (total > 0.0 && slow > 0.0).then(|| slow / total);
    Some(Shape::Tail { typical_ms: typical, p99_ms: p99, slow_share })
}

pub fn parameter_sensitive(snapshot: &Snapshot) -> Vec<Finding> {
    let Some(workload) = snapshot.workload.as_ref() else { return Vec::new() };
    let mut findings: Vec<Finding> = app_queries(snapshot)
        .filter(|q| q.executions >= MIN_RUNS)
        .filter_map(|q| {
            let shape = plans_shape(q).or_else(|| tail_shape(q))?;
            let share = workload.share(q.total_time_ms);
            let slow_time_share = match &shape {
                Shape::Plans { slow, .. } => workload.share(slow.total_time_ms),
                _ => share,
            };
            (share >= 0.01 || slow_time_share >= 0.01).then(|| finding_for(snapshot, q, &shape, share))
        })
        .collect();
    findings.sort_by(|a, b| b.impact.score.total_cmp(&a.impact.score));
    findings.truncate(6);
    findings
}

fn compiled(plan: &PlanVariant, q: &QueryStat) -> Option<String> {
    let raw = plan.compiled.clone().or_else(|| {
        let dominant = q.plans.first()?;
        (dominant.id == plan.id).then(|| q.plan.as_ref()?.props.get("Parameters").cloned()).flatten()
    })?;
    let tidy = tidy_parameters(&raw);
    (!tidy.is_empty()).then_some(tidy)
}

/// Showplan writes compiled values as `@t = (2)` and lists output parameters as `@c = NULL`; keep what was sniffed.
pub(crate) fn tidy_parameters(raw: &str) -> String {
    raw.split(", @")
        .enumerate()
        .map(|(i, p)| if i == 0 { p.to_string() } else { format!("@{p}") })
        .filter_map(|p| {
            let (name, value) = p.split_once(" = ")?;
            let value = value.trim();
            if value.eq_ignore_ascii_case("NULL") {
                return None;
            }
            let value = value.strip_prefix('(').and_then(|v| v.strip_suffix(')')).unwrap_or(value);
            Some(format!("{name} = {value}"))
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn finding_for(snapshot: &Snapshot, q: &QueryStat, shape: &Shape, share: f64) -> Finding {
    let handle = capitalize(&query_handle(q));
    let mut f = finding("parameter-sensitive", &q.id, Category::Queries, subject(SubjectKind::Query, &q.id));
    if let Some(impact) = query_impact(snapshot, &[q]) {
        apply_query_impact(&mut f, &impact);
    }
    f.severity = severity_for_share(share);
    f.impact.score = score_for_share(share);
    let run = culprit_core::changes::run_time;
    match shape {
        Shape::Plans { fast, slow } => {
            let slow_share = slow.executions / q.executions.max(1.0);
            f.title = format!(
                "{handle} runs in {} for most calls but {} for {} of them",
                run(fast.mean_time_ms),
                run(slow.mean_time_ms),
                format::percent(slow_share)
            );
            f.impact.label =
                format!("{} slower for some calls", times(slow.mean_time_ms / fast.mean_time_ms.max(0.001)));
            let compiled_for =
                |p: &PlanVariant| compiled(p, q).map(|c| format!(", compiled for {c}")).unwrap_or_default();
            f.summary = format!(
                "SQL Server keeps two plans for this statement. Plan {} averages {}{}; plan {} averages {}{} and handled {} calls. A plan is compiled for the parameter values of the call that happened to compile it, then reused for every other value, so values that need a different strategy get the wrong one.",
                fast.id,
                run(fast.mean_time_ms),
                compiled_for(fast),
                slow.id,
                run(slow.mean_time_ms),
                compiled_for(slow),
                format::count(slow.executions),
            );
            f.metrics.push(Metric::new("Fast plan", fast.mean_time_ms, Unit::Ms));
            f.metrics.push(Metric::new("Slow plan", slow.mean_time_ms, Unit::Ms));
            f.metrics.push(Metric::new("Calls on the slow plan", slow_share, Unit::Ratio));
            f.confidence = Confidence::High;
            f.fix = Some(mssql_fix(q, Some(fast)));
        }
        Shape::Tail { typical_ms, p99_ms, slow_share } => {
            f.title = match slow_share {
                Some(s) if *s >= 0.001 => format!(
                    "{handle} usually takes {} but {} of its runs take over {}",
                    run(*typical_ms),
                    format::percent(*s),
                    run(typical_ms * MIN_TAIL_FACTOR)
                ),
                _ => {
                    format!("{handle} usually takes {} but 1 in 100 runs takes over {}", run(*typical_ms), run(*p99_ms))
                }
            };
            f.impact.label = format!("{} slower for some runs", times(p99_ms / typical_ms.max(0.001)));
            let locks = q.spread.as_ref().and_then(|s| s.lock_ms).filter(|l| *l > 0.0);
            let lock_share = locks.map(|l| (l / q.total_time_ms.max(0.001)).clamp(0.0, 1.0)).unwrap_or(0.0);
            let name = snapshot.server.name();
            let source = match snapshot.workload.as_ref().map(|w| w.source.as_str()) {
                Some("slow query log") => "The slow query log's runs of this statement".to_string(),
                _ => format!("{name}'s run-time histogram for this statement"),
            };
            f.summary = format!(
                "{source}: half the runs finish within {}, 99 in 100 within {}, the slowest took {}. {name} plans every call afresh, so slow calls usually touch far more rows for some values (skewed data) or wait for locks.{}",
                run(*typical_ms),
                run(*p99_ms),
                q.max_time_ms.map(run).unwrap_or_else(|| "longer".into()),
                if lock_share >= 0.2 {
                    format!(
                        " {} of its time is spent waiting for locks, so start with what holds them (Live).",
                        format::percent(lock_share)
                    )
                } else {
                    String::new()
                }
            );
            f.metrics.push(Metric::new("Typical run", *typical_ms, Unit::Ms));
            f.metrics.push(Metric::new("1 in 100 runs", *p99_ms, Unit::Ms));
            if let Some(s) = slow_share {
                f.metrics.push(Metric::new("Slow runs", *s, Unit::Ratio));
            }
            f.confidence = Confidence::High;
            f.fix = Some(mysql_fix(lock_share, snapshot.server.name() == "MariaDB"));
        }
    }
    f.queries = query_refs(&[q]);
    f.verify = vec![
        "Run the statement with a value from a slow call and one from a fast call and compare the plans (Editor → Explain, or Analyze outside production).".into(),
        "Re-scan after the change: Queries → Runtime spread should show the slow lane gone or close to the fast one.".into(),
    ];
    f
}

pub(crate) fn mssql_fix(q: &QueryStat, fast: Option<&PlanVariant>) -> Fix {
    let numeric = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_digit());
    let mut sql = Vec::new();
    if let Some(fast) = fast.filter(|p| numeric(&q.id) && numeric(&p.id)) {
        sql.push(format!(
            "-- 1. Keep the fast plan for every call (no code change; undo with sp_query_store_unforce_plan):\nEXEC sp_query_store_force_plan @query_id = {}, @plan_id = {};",
            q.id, fast.id
        ));
    }
    if numeric(&q.id) {
        sql.push(format!(
            "-- 2. Or plan each call for its own values (costs compile CPU per call; SQL Server 2022 / Azure SQL):\nEXEC sys.sp_query_store_set_hints @query_id = {}, @query_hints = N'OPTION (RECOMPILE)';",
            q.id
        ));
    }
    sql.push("-- 3. Or let SQL Server keep a plan per range of values (SQL Server 2022, compatibility level 160):\nALTER DATABASE SCOPED CONFIGURATION SET PARAMETER_SENSITIVE_PLAN_OPTIMIZATION = ON;".into());
    sql.push("-- 4. Or, in the application, plan for the typical value instead of the first one seen:\n--    append OPTION (OPTIMIZE FOR UNKNOWN) to the statement".into());
    Fix {
        summary: "Pick the lightest option that works: force the plan that is fast for everyone, plan per call for this statement only, or let SQL Server 2022 keep several plans. Each is reversible.".into(),
        sql: Some(sql.join("\n\n")),
        cost: Some("Forcing a plan pins it until you unforce it; RECOMPILE spends a compile on every call.".into()),
    }
}

pub(crate) fn postgres_fix() -> Fix {
    Fix {
        summary: "First see what differs: plan the statement with a value from a slow call and one from a fast call. If a generic plan is reused for very different values, plan per call for the application's role; if one column's values are very uneven, give the planner finer statistics for it.".into(),
        sql: Some(
            "-- Compare a slow and a fast value (outside production, ANALYZE runs the statement):\nEXPLAIN (ANALYZE, BUFFERS) <the statement with a slow value>;\n\n-- Plan every call for its own values (for the role the application connects as):\nALTER ROLE <app_role> SET plan_cache_mode = force_custom_plan;\n\n-- Finer statistics for an uneven column, then refresh them:\nALTER TABLE <table> ALTER COLUMN <column> SET STATISTICS 1000;\nANALYZE <table>;".into(),
        ),
        cost: None,
    }
}

pub(crate) fn mysql_fix(lock_share: f64, mariadb: bool) -> Fix {
    if lock_share >= 0.2 {
        return Fix {
            summary: "The slow runs mostly wait for row locks. Find the sessions holding them (Live shows blockers) and keep the transactions that touch these rows short.".into(),
            sql: None,
            cost: None,
        };
    }
    if mariadb {
        return Fix {
            summary: "Compare a slow and a fast value with ANALYZE (outside production). If some values match far more rows, an index that narrows them, or a histogram on the uneven column, lets MariaDB choose a better plan for each.".into(),
            sql: Some("-- Collect a histogram for an uneven column (used with the default use_stat_tables):\nANALYZE TABLE <table> PERSISTENT FOR COLUMNS (<column>) INDEXES ();".into()),
            cost: None,
        };
    }
    Fix {
        summary: "Compare a slow and a fast value with EXPLAIN ANALYZE (outside production). If some values match far more rows, an index that narrows them, or a histogram on the uneven column, lets MySQL choose a better plan for each.".into(),
        sql: Some("-- Give the optimizer the distribution of an uneven column:\nANALYZE TABLE <table> UPDATE HISTOGRAM ON <column> WITH 64 BUCKETS;".into()),
        cost: None,
    }
}

fn times(factor: f64) -> String {
    if factor >= 10.0 { format!("{}×", factor.round()) } else { format!("{:.1}×", factor).replace(".0×", "×") }
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(first) => first.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::support::tests::sample;

    fn plan(id: &str, runs: f64, mean: f64) -> PlanVariant {
        let mut p: PlanVariant = serde_json::from_value(serde_json::json!({
            "id": id, "executions": runs, "total_time_ms": runs * mean, "mean_time_ms": mean,
            "first_seen": null, "last_seen": null, "is_forced": false
        }))
        .expect("plan variant");
        p.compiled = Some(format!("@P1 = ({id})"));
        p
    }

    #[test]
    fn two_plans_one_slow_is_sensitivity() {
        let mut q = sample();
        q.executions = 1000.0;
        q.plans = vec![plan("815", 980.0, 2.0), plan("685", 20.0, 3400.0)];
        match plans_shape(&q) {
            Some(Shape::Plans { fast, slow }) => assert_eq!((fast.id.as_str(), slow.id.as_str()), ("815", "685")),
            other => panic!("{other:?}"),
        }
        q.plans = vec![plan("1", 500.0, 2.0), plan("2", 500.0, 3.0)];
        assert!(plans_shape(&q).is_none(), "1.5× apart is not two different worlds");
    }

    #[test]
    fn a_replaced_plan_is_a_regression_not_sensitivity() {
        let mut q = sample();
        q.executions = 1000.0;
        let mut old = plan("1", 500.0, 2.0);
        old.first_seen = Some("2026-10-10T00:00:00Z".into());
        old.last_seen = Some("2026-10-10T12:00:00Z".into());
        let mut new = plan("2", 500.0, 300.0);
        new.first_seen = Some("2026-10-10T13:00:00Z".into());
        new.last_seen = Some("2026-10-11T00:00:00Z".into());
        q.plans = vec![old, new];
        assert!(plans_shape(&q).is_none());
    }

    #[test]
    fn compiled_values_read_like_values() {
        assert_eq!(tidy_parameters("@c = NULL, @t = (2)"), "@t = 2");
        assert_eq!(tidy_parameters("@P1 = N'abc', @P2 = (42)"), "@P1 = N'abc', @P2 = 42");
    }

    #[test]
    fn histogram_tail_counts_the_slow_runs() {
        let mut q = sample();
        q.spread = Some(culprit_core::Spread {
            p50_ms: Some(2.0),
            p99_ms: Some(900.0),
            histogram: vec![[0.0, 2.0, 600.0], [2.0, 4.0, 380.0], [500.0, 1000.0, 20.0]],
            ..Default::default()
        });
        match tail_shape(&q) {
            Some(Shape::Tail { slow_share: Some(s), .. }) => assert!((s - 0.02).abs() < 1e-9),
            other => panic!("{other:?}"),
        }
    }
}
