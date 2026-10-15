//! Parameter sensitivity: a query that is fast for most calls and slow for some, usually because a plan compiled for
//! one value is reused for very different ones (SQL Server, Postgres generic plans) or because some values touch far
//! more rows (any engine). Each engine shows it through what it keeps: Query Store's plans used side by side, and
//! MySQL's run-time histogram. A single plan with wildly varying run times (all engines, including Postgres's
//! min/max/stddev) is `unstable-query`, which shares the fix ladders below.

use culprit_core::{
    Category, CheckInfo, Confidence, Engine, Finding, Fix, Metric, PlanVariant, QueryStat, Snapshot, SubjectKind, Unit,
    format,
};

use crate::queries::{apply_impact, slow_log_only};
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
    /// Plans of one query side by side (Query Store): the fast ones most calls run with (`fast` the busiest of them,
    /// `fast_ms` the slowest of their averages, `fast_calls` their calls together), and a slow one still in use.
    Plans { fast: &'a PlanVariant, slow: &'a PlanVariant, fast_ms: f64, fast_calls: f64, fast_plans: usize },
    /// A latency histogram with a long tail (MySQL).
    Tail { typical_ms: f64, p99_ms: f64, slow_share: Option<f64> },
}

/// "Most calls": the fast plan carries at least this share of them, so forcing it changes little for most callers.
const MOST_CALLS: f64 = 0.5;
/// A slow plan quiet for this many of its typical intervals between runs (and at least an hour) has stopped.
const QUIET_INTERVALS: f64 = 20.0;

fn plans_shape<'a>(snapshot: &Snapshot, q: &'a QueryStat) -> Option<Shape<'a>> {
    // A forced plan already decides for every call; what it does is tuning-advice's to say.
    if q.plans.iter().any(|p| p.is_forced) {
        return None;
    }
    let busy: Vec<&PlanVariant> =
        q.plans.iter().filter(|p| p.executions >= 10.0 && p.executions >= q.executions * MIN_PLAN_SHARE).collect();
    // The slow plan: the slowest still in use. One that stopped is behind the query, but a slower one may still run.
    let slow = busy
        .iter()
        .copied()
        .filter(|p| !slow_plan_stopped(snapshot, p))
        .max_by(|a, b| a.mean_time_ms.total_cmp(&b.mean_time_ms))?;
    // The fast plans: each well ahead of it. Together they carry most calls; a rare fast plan beside a slow common one
    // isn't "fast for most calls".
    let fast_group: Vec<&PlanVariant> = busy
        .iter()
        .copied()
        .filter(|p| {
            p.id != slow.id
                && slow.mean_time_ms >= p.mean_time_ms * MIN_FACTOR
                && slow.mean_time_ms - p.mean_time_ms >= MIN_GAP_MS
        })
        .collect();
    let fast_calls: f64 = fast_group.iter().map(|p| p.executions).sum();
    if fast_calls < q.executions * MOST_CALLS {
        return None;
    }
    let fast = fast_group.iter().copied().max_by(|a, b| a.executions.total_cmp(&b.executions))?;
    let fast_ms = fast_group.iter().map(|p| p.mean_time_ms).fold(0.0, f64::max);
    // Side by side: the slow plan ran while one of the fast ones did, so values pick the plan. A fast plan that was
    // replaced is a regression instead.
    let ran_with_slow = |p: &PlanVariant| match (
        p.last_seen.as_deref(),
        slow.first_seen.as_deref(),
        slow.last_seen.as_deref(),
        p.first_seen.as_deref(),
    ) {
        (Some(fast_last), Some(slow_first), Some(slow_last), Some(fast_first)) => {
            fast_last >= slow_first && slow_last >= fast_first
        }
        _ => true,
    };
    fast_group.iter().any(|p| ran_with_slow(p)).then_some(Shape::Plans {
        fast,
        slow,
        fast_ms,
        fast_calls,
        fast_plans: fast_group.len(),
    })
}

/// The slow plan hasn't run for a long while before the data ends: whatever made it slow is behind the query.
fn slow_plan_stopped(snapshot: &Snapshot, slow: &PlanVariant) -> bool {
    let end = snapshot.workload.as_ref().and_then(|w| w.window_end.as_deref()).and_then(parse_epoch_seconds);
    let (Some(end), Some(last)) = (end, slow.last_seen.as_deref().and_then(parse_epoch_seconds)) else {
        return false;
    };
    let first = slow.first_seen.as_deref().and_then(parse_epoch_seconds).unwrap_or(last);
    let interval = (last - first).max(1.0) / slow.executions.max(1.0);
    end - last >= (interval * QUIET_INTERVALS).max(3600.0)
}

/// Runs in the slow tail the shape needs: with fewer, "1 in 100 runs" is one or two runs.
const MIN_TAIL_RUNS: f64 = 3.0;

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
    // The tail has to be several runs: a histogram counts them; without one, a hundredth of the runs must be.
    let enough = if total > 0.0 { slow >= MIN_TAIL_RUNS } else { q.executions * 0.01 >= MIN_TAIL_RUNS };
    if !enough {
        return None;
    }
    let slow_share = (total > 0.0 && slow > 0.0).then(|| slow / total);
    Some(Shape::Tail { typical_ms: typical, p99_ms: p99, slow_share })
}

pub fn parameter_sensitive(snapshot: &Snapshot) -> Vec<Finding> {
    let Some(workload) = snapshot.workload.as_ref() else { return Vec::new() };
    let mut findings: Vec<Finding> = app_queries(snapshot)
        .filter(|q| q.executions >= MIN_RUNS)
        .filter_map(|q| {
            let shape = plans_shape(snapshot, q).or_else(|| tail_shape(q))?;
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
        apply_impact(snapshot, &mut f, &impact);
    }
    f.severity = severity_for_workload_share(snapshot, share);
    f.impact.score = score_for_share(share);
    let run = culprit_core::changes::run_time;
    match shape {
        Shape::Plans { fast, slow, fast_ms, fast_calls, fast_plans } => {
            let slow_share = slow.executions / q.executions.max(1.0);
            let calls = |n: f64| format::percent(n / q.executions.max(1.0));
            // One plan for most calls, or several, each well ahead of the slow one: then the slowest of them is the bound.
            let fast_time =
                if *fast_plans == 1 { run(fast.mean_time_ms) } else { format!("{} or less", run(*fast_ms)) };
            f.title = format!(
                "{handle} runs in {fast_time} for most calls but {} for {} of them",
                run(slow.mean_time_ms),
                format::percent(slow_share)
            );
            f.impact.label = format!("{} slower for some calls", times(slow.mean_time_ms / fast_ms.max(0.001)));
            let compiled_for =
                |p: &PlanVariant| compiled(p, q).map(|c| format!(", compiled for `{c}`")).unwrap_or_default();
            let fast_side = if *fast_plans == 1 {
                format!(
                    "Plan {} handles {} of the calls and averages {}{}",
                    fast.id,
                    calls(*fast_calls),
                    run(fast.mean_time_ms),
                    compiled_for(fast)
                )
            } else {
                format!(
                    "{fast_plans} plans that average {} or less handle {} of the calls (plan {} the most, {} of them at {}{})",
                    run(*fast_ms),
                    calls(*fast_calls),
                    fast.id,
                    calls(fast.executions),
                    run(fast.mean_time_ms),
                    compiled_for(fast)
                )
            };
            f.summary = format!(
                "SQL Server runs this statement with {} plans side by side. {fast_side}; plan {} averages {}{} and handled {} calls. A plan is compiled for the parameter values of the call that happened to compile it, then reused for every other value, so values that need a different strategy get the wrong one.",
                if q.plans.len() == 2 { "two".to_string() } else { format::count(q.plans.len() as f64) },
                slow.id,
                run(slow.mean_time_ms),
                compiled_for(slow),
                format::count(slow.executions),
            );
            f.metrics.push(Metric::new("Fast plan", fast.mean_time_ms, Unit::Ms));
            f.metrics.push(Metric::new("Slow plan", slow.mean_time_ms, Unit::Ms));
            f.metrics.push(Metric::new("Calls on the slow plan", slow_share, Unit::Ratio));
            // Forcing is offered only for a plan that serves most calls on its own.
            let most_calls = (fast.executions >= q.executions * MOST_CALLS).then_some(*fast);
            if let Some(p) = most_calls {
                f.metrics.push(Metric::text(PLAN_MOST_CALLS_USE, p.id.clone()));
            }
            f.confidence = Confidence::High;
            f.fix = Some(mssql_fix(snapshot, q, most_calls));
        }
        Shape::Tail { typical_ms, p99_ms, slow_share } => {
            // The slow query log keeps only runs over long_query_time: its "typical" run is a typical slow one.
            let logged = slow_log_only(snapshot);
            let percentile_99 = q.executions >= 100.0;
            let subject = if logged { format!("Logged runs of {}", query_handle(q)) } else { handle.clone() };
            let usual = if logged { "usually take" } else { "usually takes" };
            f.title = match slow_share {
                Some(s) if *s >= 0.001 => format!(
                    "{subject} {usual} {} but {} of {} take over {}",
                    run(*typical_ms),
                    format::percent(*s),
                    if logged { "them" } else { "its runs" },
                    run(typical_ms * MIN_TAIL_FACTOR)
                ),
                _ => format!("{subject} {usual} {} but 1 in 100 takes over {}", run(*typical_ms), run(*p99_ms)),
            };
            f.impact.label = format!("{} slower for some runs", times(p99_ms / typical_ms.max(0.001)));
            let locks = q.spread.as_ref().and_then(|s| s.lock_ms).filter(|l| *l > 0.0);
            let lock_share = locks.map(|l| (l / q.total_time_ms.max(0.001)).clamp(0.0, 1.0)).unwrap_or(0.0);
            let name = snapshot.server.name();
            let from_log = snapshot.workload.as_ref().is_some_and(|w| w.source == "slow query log");
            let source = if from_log && !logged {
                "The slow query log's runs of this statement (it keeps every run)".to_string()
            } else if logged {
                let threshold = snapshot
                    .setting("long_query_time")
                    .and_then(|s| s.value.trim().parse::<f64>().ok())
                    .map(|s| format!(", {}", format::duration(s * 1000.0)))
                    .unwrap_or_default();
                format!(
                    "Of the runs of this statement the slow query log kept (only those over long_query_time{threshold}, so faster runs aren't counted)"
                )
            } else {
                format!("{name}'s run-time histogram for this statement")
            };
            f.summary = format!(
                "{source}: half finished within {}{}, the slowest took {}. {name} plans every call afresh, so slow calls usually touch far more rows for some values (skewed data) or wait for locks.{}",
                run(*typical_ms),
                if percentile_99 { format!(", 99 in 100 within {}", run(*p99_ms)) } else { String::new() },
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
            f.metrics.push(Metric::new(
                if logged { "Typical logged run" } else { "Typical run" },
                *typical_ms,
                Unit::Ms,
            ));
            if percentile_99 {
                f.metrics.push(Metric::new("1 in 100 runs", *p99_ms, Unit::Ms));
            }
            if let Some(s) = slow_share {
                f.metrics.push(Metric::new("Slow runs", *s, Unit::Ratio));
            }
            f.confidence = if logged { Confidence::Medium } else { Confidence::High };
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

/// The metric naming the plan most calls run with (refine reads it to compare with SQL Server's own recommendation).
pub(crate) const PLAN_MOST_CALLS_USE: &str = "Plan most calls use";

/// The fix ladder for SQL Server, lightest first, with only the steps this server has: Query Store hints and
/// parameter-sensitive plan optimization came with SQL Server 2022 (Azure SQL has both), and the optimization needs
/// compatibility level 160. Forcing is offered only for a plan most calls already run with.
pub(crate) fn mssql_fix(snapshot: &Snapshot, q: &QueryStat, fast: Option<&PlanVariant>) -> Fix {
    let numeric = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_digit());
    let server = &snapshot.server;
    let azure = server.platform().is_some_and(|p| p.is_azure_sql());
    let sql2022 = azure || culprit_core::version_at_least(&server.version, [16, 0, 0]);
    let compat = snapshot.setting("compatibility_level").and_then(|s| s.value.trim().parse::<u32>().ok());
    let psp_on = snapshot
        .setting("PARAMETER_SENSITIVE_PLAN_OPTIMIZATION")
        .is_some_and(|s| matches!(s.value.trim().to_ascii_uppercase().as_str(), "1" | "ON"));
    // (what it does, the SQL or the change to make, the option in a sentence)
    let mut steps: Vec<(String, String, &str)> = Vec::new();
    let force = fast.filter(|p| numeric(&q.id) && numeric(&p.id) && p.executions >= q.executions * MOST_CALLS);
    if let Some(fast) = force {
        steps.push((
            "Keep the plan most calls run fast with, for every call (no code change; undo with sp_query_store_unforce_plan)".into(),
            format!("EXEC sp_query_store_force_plan @query_id = {}, @plan_id = {};", q.id, fast.id),
            "force the plan most calls run fast with",
        ));
    }
    if numeric(&q.id) && sql2022 {
        steps.push((
            "Plan each call for its own values (costs compile CPU per call)".into(),
            format!("EXEC sys.sp_query_store_set_hints @query_id = {}, @query_hints = N'OPTION (RECOMPILE)';", q.id),
            "plan per call for this statement only",
        ));
    } else {
        steps.push((
            "Plan each call for its own values (costs compile CPU per call), in the application".into(),
            "--    append OPTION (RECOMPILE) to the statement".into(),
            "plan per call for this statement only",
        ));
    }
    if sql2022 && !psp_on && compat.is_none_or(|c| c >= 160) {
        steps.push((
            "Let SQL Server keep a plan per range of values".into(),
            "ALTER DATABASE SCOPED CONFIGURATION SET PARAMETER_SENSITIVE_PLAN_OPTIMIZATION = ON;".into(),
            "let SQL Server keep several plans",
        ));
    }
    steps.push((
        "In the application, plan for the typical value instead of the first one seen".into(),
        "--    append OPTION (OPTIMIZE FOR UNKNOWN) to the statement".into(),
        "plan for the typical value",
    ));
    let sql: Vec<String> = steps
        .iter()
        .enumerate()
        .map(|(i, (what, sql, _))| {
            let what = if i == 0 { what.clone() } else { format!("Or {}{}", what[..1].to_lowercase(), &what[1..]) };
            format!("-- {}. {what}:\n{sql}", i + 1)
        })
        .collect();
    let options: Vec<String> = steps.iter().map(|(_, _, option)| option.to_string()).collect();
    Fix {
        summary: format!("Pick the lightest option that works: {}. Each is reversible.", or_list(&options)),
        sql: Some(sql.join("\n\n")),
        cost: Some(if force.is_some() {
            "Forcing a plan pins it for every value until you unforce it: try a value from a slow call under it first. RECOMPILE spends a compile on every call.".into()
        } else {
            "RECOMPILE spends a compile on every call.".into()
        }),
    }
}

/// "a, b or c".
fn or_list(items: &[String]) -> String {
    match items {
        [] => String::new(),
        [one] => one.clone(),
        [rest @ .., last] => format!("{} or {last}", rest.join(", ")),
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

    fn mssql() -> Snapshot {
        crate::waits::snapshot_with(Engine::Mssql)
    }

    #[test]
    fn two_plans_one_slow_is_sensitivity() {
        let s = mssql();
        let mut q = sample();
        q.executions = 1000.0;
        q.plans = vec![plan("815", 980.0, 2.0), plan("685", 20.0, 3400.0)];
        match plans_shape(&s, &q) {
            Some(Shape::Plans { fast, slow, .. }) => {
                assert_eq!((fast.id.as_str(), slow.id.as_str()), ("815", "685"))
            }
            other => panic!("{other:?}"),
        }
        q.plans = vec![plan("1", 500.0, 2.0), plan("2", 500.0, 3.0)];
        assert!(plans_shape(&s, &q).is_none(), "1.5× apart is not two different worlds");
    }

    #[test]
    fn fast_plans_together_carry_most_calls_and_the_slow_one_still_runs() {
        // As on a real Azure SQL database: no single fast plan has half the calls, the slowest plan stopped days ago,
        // and a slow one ran half an hour before the data ends, while a fast one kept running.
        let mut s = mssql();
        let w = s.workload.as_mut().expect("workload");
        w.window_end = Some("2026-10-14T15:21:00Z".into());
        let mut q = sample();
        q.executions = 27_000.0;
        let at = |mut p: PlanVariant, first: &str, last: &str| {
            p.first_seen = Some(first.into());
            p.last_seen = Some(last.into());
            p
        };
        q.plans = vec![
            at(plan("2485", 11_000.0, 1.2), "2026-10-12T13:54:00Z", "2026-10-14T12:58:00Z"),
            at(plan("2179", 7_200.0, 0.1), "2026-10-11T16:29:00Z", "2026-10-12T13:37:00Z"),
            at(plan("3020", 3_700.0, 0.15), "2026-10-13T17:06:00Z", "2026-10-14T15:16:00Z"),
            at(plan("1944", 1_200.0, 27.0), "2026-10-10T12:54:00Z", "2026-10-12T11:47:00Z"),
            at(plan("3312", 1_240.0, 19.0), "2026-10-14T14:32:00Z", "2026-10-14T14:49:00Z"),
        ];
        match plans_shape(&s, &q) {
            Some(Shape::Plans { fast, slow, fast_plans, fast_calls, .. }) => {
                assert_eq!((fast.id.as_str(), slow.id.as_str(), fast_plans), ("2485", "3312", 3));
                assert!(fast_calls > q.executions * MOST_CALLS);
            }
            other => panic!("{other:?}"),
        }
        let f = finding_for(&s, &q, &plans_shape(&s, &q).unwrap(), 0.04);
        assert!(f.title.contains("1.2 ms or less for most calls"), "{}", f.title);
        // No plan serves most calls on its own: forcing one isn't offered.
        assert!(!f.metrics.iter().any(|m| m.label == PLAN_MOST_CALLS_USE), "{:?}", f.metrics);
    }

    #[test]
    fn the_fast_plan_has_to_carry_most_calls() {
        // 10 runs at 5 ms beside 290 at 60 ms: most calls are slow, so "fast for most calls" would be false, and
        // forcing the rare plan is the classic sniffing trap.
        let s = mssql();
        let mut q = sample();
        q.executions = 300.0;
        q.plans = vec![plan("1", 290.0, 60.0), plan("2", 10.0, 5.0)];
        assert!(plans_shape(&s, &q).is_none());
    }

    #[test]
    fn a_forced_plan_or_a_slow_plan_that_stopped_is_not_sensitivity() {
        let s = mssql();
        let mut q = sample();
        q.executions = 1000.0;
        q.plans = vec![plan("815", 980.0, 2.0), plan("685", 20.0, 3400.0)];
        q.plans[0].is_forced = true;
        assert!(plans_shape(&s, &q).is_none(), "a forced plan already decides");
        q.plans[0].is_forced = false;
        // The demo's data ends at 12:13; the slow plan last ran at 08:00, after 20 runs in two hours.
        q.plans[1].first_seen = Some("2026-10-11T06:00:00Z".into());
        q.plans[1].last_seen = Some("2026-10-11T08:00:00Z".into());
        assert!(plans_shape(&s, &q).is_none(), "the slow plan stopped running");
        q.plans[1].last_seen = Some("2026-10-11T12:00:00Z".into());
        assert!(plans_shape(&s, &q).is_some());
    }

    #[test]
    fn the_fix_ladder_offers_what_the_server_has() {
        let mut s = mssql();
        let mut q = sample();
        q.id = "42".into();
        q.executions = 1000.0;
        let fast = plan("815", 980.0, 2.0);
        // SQL Server 2022 with parameter-sensitive plan optimization already on (the demo).
        let sql = mssql_fix(&s, &q, Some(&fast)).sql.unwrap();
        assert!(sql.starts_with("-- 1. Keep the plan most calls run fast with"), "{sql}");
        assert!(sql.contains("sp_query_store_set_hints"), "{sql}");
        assert!(!sql.contains("PARAMETER_SENSITIVE_PLAN_OPTIMIZATION"), "it is already on: {sql}");
        // SQL Server 2019: no Query Store hints, no parameter-sensitive plans.
        s.server.version = "15.0.4355.3".into();
        let sql = mssql_fix(&s, &q, Some(&fast)).sql.unwrap();
        assert!(!sql.contains("sp_query_store_set_hints") && !sql.contains("PARAMETER_SENSITIVE"), "{sql}");
        assert!(sql.contains("-- 2. Or plan each call for its own values (costs compile CPU per call), in the application:\n--    append OPTION (RECOMPILE)"), "{sql}");
        // A plan that ran for a minority of calls is never the one to force.
        let rare = plan("2", 100.0, 2.0);
        assert!(!mssql_fix(&s, &q, Some(&rare)).sql.unwrap().contains("sp_query_store_force_plan"));
    }

    #[test]
    fn a_replaced_plan_is_a_regression_not_sensitivity() {
        let s = mssql();
        let mut q = sample();
        q.executions = 1000.0;
        let mut old = plan("1", 500.0, 2.0);
        old.first_seen = Some("2026-10-10T00:00:00Z".into());
        old.last_seen = Some("2026-10-10T12:00:00Z".into());
        let mut new = plan("2", 500.0, 300.0);
        new.first_seen = Some("2026-10-10T13:00:00Z".into());
        new.last_seen = Some("2026-10-11T00:00:00Z".into());
        q.plans = vec![old, new];
        assert!(plans_shape(&s, &q).is_none());
    }

    #[test]
    fn a_tail_of_one_or_two_runs_is_not_a_shape() {
        let mut q = sample();
        q.executions = 80.0;
        q.spread = Some(culprit_core::Spread {
            p50_ms: Some(2.0),
            p99_ms: Some(900.0),
            histogram: vec![[0.0, 2.0, 60.0], [2.0, 4.0, 18.0], [500.0, 1000.0, 2.0]],
            ..Default::default()
        });
        assert!(tail_shape(&q).is_none(), "two slow runs");
        q.spread.as_mut().unwrap().histogram[2][2] = 3.0;
        assert!(tail_shape(&q).is_some());
        // Without a histogram, a hundredth of the runs has to be three runs.
        q.spread.as_mut().unwrap().histogram.clear();
        assert!(tail_shape(&q).is_none());
        q.executions = 400.0;
        assert!(tail_shape(&q).is_some());
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
