mod actual;
mod advisor;
mod brief;
mod config;
mod config_health;
pub mod dashboard;
mod deadlocks;
mod digest;
pub mod drift;
mod index_health;
mod indexes;
mod insights;
mod maintenance;
mod pressure;
mod queries;
mod sensitivity;
mod signals;
pub(crate) mod support;
mod trends;
mod tuning;
pub mod waits;

use std::collections::BTreeSet;
use std::sync::Arc;

use culprit_core::{
    Capability, Category, CategoryCount, Check, CheckInfo, Finding, LockedCheck, QueryStat, Report, ServerInfo,
    Severity, SeverityCount, Snapshot, SubjectKind, Summary, format,
};

pub use advisor::index_advice;
pub use brief::agent_brief;
pub use dashboard::dashboard;
pub use digest::{digest, digest_since};
pub use drift::drift;

struct Builtin {
    info: fn() -> CheckInfo,
    run: fn(&Snapshot) -> Vec<Finding>,
}

impl Check for Builtin {
    fn info(&self) -> CheckInfo {
        (self.info)()
    }

    fn run(&self, snapshot: &Snapshot) -> Vec<Finding> {
        (self.run)(snapshot)
    }
}

pub fn all() -> Vec<Box<dyn Check>> {
    let builtins = [
        Builtin { info: indexes::missing_index_info, run: indexes::missing_index },
        Builtin { info: indexes::filtered_scan_info, run: indexes::filtered_scan },
        Builtin { info: indexes::unused_index_info, run: indexes::unused_index },
        Builtin { info: indexes::duplicate_index_info, run: indexes::duplicate_index },
        Builtin { info: indexes::unindexed_fk_info, run: indexes::unindexed_fk },
        Builtin { info: index_health::overlapping_index_info, run: index_health::overlapping_index },
        Builtin { info: index_health::over_indexed_info, run: index_health::over_indexed },
        Builtin { info: index_health::scan_heavy_info, run: index_health::scan_heavy },
        Builtin { info: tuning::tuning_advice_info, run: tuning::tuning_advice },
        Builtin { info: tuning::query_family_info, run: tuning::query_family },
        Builtin { info: queries::heavy_query_info, run: queries::heavy_query },
        Builtin { info: queries::chatty_query_info, run: queries::chatty_query },
        Builtin { info: queries::plan_warnings_info, run: queries::plan_warnings },
        Builtin { info: insights::plan_insights_info, run: insights::plan_insights },
        Builtin { info: queries::plan_regression_info, run: queries::plan_regression },
        Builtin { info: trends::regression_info, run: trends::regression },
        Builtin { info: queries::unstable_query_info, run: queries::unstable_query },
        Builtin { info: sensitivity::parameter_sensitive_info, run: sensitivity::parameter_sensitive },
        Builtin { info: queries::temp_spill_info, run: queries::temp_spill },
        Builtin { info: maintenance::stale_stats_info, run: maintenance::stale_stats },
        Builtin { info: maintenance::heap_table_info, run: maintenance::heap_table },
        Builtin { info: maintenance::dead_rows_info, run: maintenance::dead_rows },
        Builtin { info: maintenance::xid_age_info, run: maintenance::xid_age },
        Builtin { info: config::config_info, run: config::config },
        Builtin { info: deadlocks::deadlocks_info, run: deadlocks::deadlocks },
        Builtin { info: pressure::memory_grant_info, run: pressure::memory_grant },
        Builtin { info: pressure::memory_queue_info, run: pressure::memory_queue },
        Builtin { info: pressure::compile_pressure_info, run: pressure::compile_pressure },
        Builtin { info: pressure::worker_pressure_info, run: pressure::worker_pressure },
        Builtin { info: pressure::held_cleanup_info, run: pressure::held_cleanup },
        Builtin { info: pressure::network_wait_info, run: pressure::network_wait },
        Builtin { info: pressure::tier_limit_info, run: pressure::tier_limit },
        Builtin { info: pressure::checkpoint_pressure_info, run: pressure::checkpoint_pressure },
        Builtin { info: pressure::row_lock_waits_info, run: pressure::row_lock_waits },
        Builtin { info: signals::failing_query_info, run: signals::failing_query },
        Builtin { info: signals::planning_time_info, run: signals::planning_time },
        Builtin { info: signals::jit_overhead_info, run: signals::jit_overhead },
        Builtin { info: signals::log_volume_info, run: signals::log_volume },
        Builtin { info: signals::lock_time_info, run: signals::lock_time },
        Builtin { info: signals::big_result_info, run: signals::big_result },
        Builtin { info: signals::hot_rows_info, run: signals::hot_rows },
        Builtin { info: signals::lock_escalations_info, run: signals::lock_escalations },
        Builtin { info: signals::last_page_insert_info, run: signals::last_page_insert },
        Builtin { info: signals::untrusted_constraint_info, run: signals::untrusted_constraint },
        Builtin { info: signals::missing_histogram_info, run: signals::missing_histogram },
        Builtin { info: signals::hot_updates_info, run: signals::hot_updates },
        Builtin { info: signals::table_bloat_info, run: signals::table_bloat },
        Builtin { info: signals::large_rows_info, run: signals::large_rows },
        Builtin { info: signals::slow_storage_info, run: signals::slow_storage },
        Builtin { info: signals::connection_churn_info, run: signals::connection_churn },
        Builtin { info: signals::replica_lag_info, run: signals::replica_lag },
        Builtin { info: actual::last_actual_plans_info, run: actual::last_actual_plans },
    ];
    builtins.into_iter().map(|b| Box::new(b) as Box<dyn Check>).collect()
}

pub fn infos() -> Vec<CheckInfo> {
    infos_with(&[])
}

/// Built-in checks plus checks plugged in by an embedding application, in run order.
fn with_extra(extra: &[Arc<dyn Check>]) -> Vec<Arc<dyn Check>> {
    let mut checks: Vec<Arc<dyn Check>> = all().into_iter().map(Arc::from).collect();
    for check in extra {
        let id = check.info().id;
        if checks.iter().any(|c| c.info().id == id) {
            continue;
        }
        checks.push(check.clone());
    }
    checks
}

/// Every check's metadata, including plugged-in ones. A plugged-in check whose id is already taken is ignored.
pub fn infos_with(extra: &[Arc<dyn Check>]) -> Vec<CheckInfo> {
    with_extra(extra).iter().map(|c| c.info()).collect()
}

fn missing_for(info: &CheckInfo, server: &ServerInfo) -> Vec<Capability> {
    info.requires.iter().copied().filter(|c| !server.has(*c)).collect()
}

/// Checks that apply to this engine but can't run with what this login can see.
pub fn locked(server: &ServerInfo) -> Vec<LockedCheck> {
    locked_with(server, &[])
}

pub fn locked_with(server: &ServerInfo, extra: &[Arc<dyn Check>]) -> Vec<LockedCheck> {
    infos_with(extra)
        .into_iter()
        .filter(|info| info.engines.contains(&server.engine))
        .filter_map(|info| {
            let missing = missing_for(&info, server);
            (!missing.is_empty()).then_some(LockedCheck { check: info, missing })
        })
        .collect()
}

/// Run every applicable built-in check, drop findings another finding already explains, rank, and summarize.
pub fn analyze(snapshot: Snapshot) -> Report {
    analyze_with(snapshot, &[])
}

/// [`analyze`] with extra checks plugged in by an embedding application. Plugged-in findings are ranked, de-duplicated and
/// summarized exactly like built-in ones; a plugged-in check whose id is already taken is ignored.
pub fn analyze_with(snapshot: Snapshot, extra: &[Arc<dyn Check>]) -> Report {
    let mut findings = Vec::new();
    let mut locked = Vec::new();
    for check in with_extra(extra) {
        let info = check.info();
        if !info.engines.contains(&snapshot.server.engine) {
            continue;
        }
        let missing = missing_for(&info, &snapshot.server);
        if !missing.is_empty() {
            locked.push(LockedCheck { check: info, missing });
            continue;
        }
        findings.extend(check.run(&snapshot));
    }
    dedupe(&mut findings);
    if snapshot.server.engine == culprit_core::Engine::Mssql {
        for f in findings.iter_mut() {
            without_database_prose(f, &snapshot.server.database);
        }
    }
    mark_inactive(&snapshot, &mut findings);
    mark_one_off(&snapshot, &mut findings);
    findings.sort_by(|a, b| {
        a.inactive.cmp(&b.inactive).then(b.impact.score.total_cmp(&a.impact.score)).then(b.severity.cmp(&a.severity))
    });
    let summary = summarize(&snapshot, &findings);
    Report { snapshot, findings, locked, summary }
}

/// SQL Server writes names in plans and DMVs with the database in front (`[shop_db].[dbo].[orders].[email]`). In
/// sentences that's noise, and in a shared report it names the database: prose keeps `[dbo].[orders].[email]`. Fix
/// scripts are left as they are.
pub(crate) fn without_database(text: &str, database: &str) -> String {
    if database.is_empty() {
        return text.to_string();
    }
    let prefix = format!("[{}].[", database.replace(']', "]]"));
    text.replace(&prefix, "[")
}

fn without_database_prose(f: &mut Finding, database: &str) {
    f.title = without_database(&f.title, database);
    f.summary = without_database(&f.summary, database);
    for line in f.verify.iter_mut().chain(f.caveats.iter_mut()) {
        *line = without_database(line, database);
    }
    if let Some(fix) = f.fix.as_mut() {
        fix.summary = without_database(&fix.summary, database);
    }
}

fn dedupe(findings: &mut Vec<Finding>) {
    merge_scans_into_missing_indexes(findings);

    // A function or leading wildcard around the filtered column is why the scan happens; an index on the column
    // (filtered-scan's advice) wouldn't be used until the query changes.
    let rewrites: Vec<(String, Vec<String>)> = findings
        .iter()
        .filter(|f| {
            f.id.starts_with("plan-insights:non-sargable:") || f.id.starts_with("plan-insights:leading-wildcard:")
        })
        .map(|f| (table_leaf(&f.subject.name), f.queries.iter().map(|q| q.id.clone()).collect()))
        .collect();
    findings.retain(|f| {
        f.check != "filtered-scan"
            || !rewrites.iter().any(|(table, queries)| {
                *table == table_leaf(&f.subject.name) && f.queries.iter().any(|q| queries.contains(&q.id))
            })
    });

    // A spill finding already explains a big sort in the same query, with the memory fix.
    let spilled: BTreeSet<String> = findings
        .iter()
        .filter(|f| f.check == "temp-spill" || f.id.contains(":spill"))
        .flat_map(|f| f.queries.iter().map(|q| q.id.clone()))
        .collect();
    findings.retain(|f| {
        !(f.id.starts_with("plan-insights:big-sort:") || f.id.starts_with("plan-insights:temp-table-sort:"))
            || !f.queries.iter().all(|q| spilled.contains(&q.id))
    });

    let index_keys: Vec<(String, Vec<String>)> = findings
        .iter()
        .filter(|f| matches!(f.check.as_str(), "missing-index" | "filtered-scan"))
        .map(|f| (f.subject.name.to_ascii_lowercase(), keys_of(&f.id)))
        .collect();
    findings.retain(|f| {
        if f.check != "unindexed-fk" {
            return true;
        }
        let table = f.subject.name.to_ascii_lowercase();
        let fk_cols = keys_of(&f.id);
        // The schema-wide finding names no key of its own.
        if fk_cols.is_empty() {
            return true;
        }
        !index_keys
            .iter()
            .any(|(t, keys)| *t == table && fk_cols.iter().all(|c| keys.iter().take(fk_cols.len()).any(|k| k == c)))
    });

    // An index on the filtered columns gives the optimizer exact estimates: the histogram is then the lesser fix.
    let mut keep = Vec::with_capacity(findings.len());
    for f in findings.iter_mut() {
        if f.check != "missing-histogram" {
            keep.push(true);
            continue;
        }
        let table = f.subject.name.to_ascii_lowercase();
        let columns = signals::histogram_columns(f);
        let indexed: Vec<&String> = columns
            .iter()
            .filter(|c| {
                index_keys.iter().any(|(t, keys)| *t == table && keys.iter().any(|k| k.eq_ignore_ascii_case(c)))
            })
            .collect();
        if !indexed.is_empty() && indexed.len() < columns.len() {
            let names: Vec<String> = indexed.iter().map(|c| c.to_string()).collect();
            f.caveats.push(format!(
                "An index is also suggested for {}; once it exists the optimizer estimates from the index instead.",
                culprit_core::format::list(&names)
            ));
        }
        keep.push(indexed.len() < columns.len());
    }
    let mut keep = keep.into_iter();
    findings.retain(|_| keep.next().unwrap_or(true));

    // A regression finding says when the query got slower and carries the plan change; the plan-only finding repeats it.
    let regressed: BTreeSet<String> = findings
        .iter()
        .filter(|f| f.check == "regression")
        .flat_map(|f| f.queries.iter().map(|q| q.id.clone()))
        .collect();
    findings.retain(|f| f.check != "plan-regression" || !f.queries.iter().any(|q| regressed.contains(&q.id)));

    let explained: BTreeSet<String> = findings
        .iter()
        .filter(|f| !matches!(f.check.as_str(), "heavy-query" | "unused-index" | "duplicate-index" | "stale-stats"))
        .flat_map(|f| f.queries.iter().map(|q| q.id.clone()))
        .collect();
    findings.retain(|f| f.check != "heavy-query" || !f.queries.iter().any(|q| explained.contains(&q.id)));
}

fn table_leaf(name: &str) -> String {
    name.rsplit('.').next().unwrap_or(name).to_ascii_lowercase()
}

/// SQL Server reports the same gap twice: as a missing-index suggestion and as a scan in the plan. Keep the suggestion, which
/// carries the optimizer's own estimate, and give it the plan evidence.
fn merge_scans_into_missing_indexes(findings: &mut Vec<Finding>) {
    let mut absorbed = Vec::new();
    for (i, scan) in findings.iter().enumerate().filter(|(_, f)| f.check == "filtered-scan") {
        let scan_keys = keys_of(&scan.id);
        let target = findings.iter().position(|m| {
            m.check == "missing-index"
                && m.subject.name.eq_ignore_ascii_case(&scan.subject.name)
                && scan_keys.first().is_some_and(|k| keys_of(&m.id).contains(k))
        });
        if let Some(t) = target {
            absorbed.push((i, t));
        }
    }
    for &(scan, target) in &absorbed {
        let refs = findings[scan].queries.clone();
        let m = &mut findings[target];
        for r in refs {
            match m.queries.iter_mut().find(|q| q.id == r.id) {
                Some(existing) if existing.plan_node.is_none() => existing.plan_node = r.plan_node,
                Some(_) => {}
                None => m.queries.push(r),
            }
        }
    }
    let drop: BTreeSet<usize> = absorbed.iter().map(|(s, _)| *s).collect();
    let mut index = 0;
    findings.retain(|_| {
        let keep = !drop.contains(&index);
        index += 1;
        keep
    });
}

/// A query that stopped running long before the window ended was most likely changed or fixed by a deploy. Its history still
/// dominates a multi-day window, so without this the top of the list would be yesterday's problem. "Long" is relative to how
/// often the query ran: silence of 20 typical intervals (at least an hour), so a daily job is not mistaken for a fixed one.
/// Returns how long it has been quiet (seconds) and when it last ran.
pub(crate) fn query_stopped(snapshot: &Snapshot, q: &QueryStat) -> Option<(f64, String)> {
    const QUIET_INTERVALS: f64 = 20.0;
    const MIN_QUIET_SECONDS: f64 = 3600.0;
    // One run has no rhythm to stop: that's `ran_once`, not a deploy.
    if ran_once(q) {
        return None;
    }
    let workload = snapshot.workload.as_ref()?;
    let end = workload.window_end.as_deref().and_then(support::parse_epoch_seconds)?;
    let window_start = end - workload.window_hours * 3600.0;
    let last_text = q.last_seen.clone()?;
    let last = support::parse_epoch_seconds(&last_text)?;
    let first = q.first_seen.as_deref().and_then(support::parse_epoch_seconds).unwrap_or(window_start);
    let interval = (last - first).max(1.0) / q.executions.max(1.0);
    let silence = end - last;
    if silence < (interval * QUIET_INTERVALS).max(MIN_QUIET_SECONDS) {
        return None;
    }
    // A query that runs in bursts (an eval run, a batch) is quiet between them: being silent no longer than a gap it
    // already came back from is its rhythm, not a deploy.
    if let Some(gap) = longest_return(q, workload)
        && silence <= gap
    {
        return None;
    }
    Some((silence, last_text))
}

/// A statement seen once in the window: an export, a backfill, a migration or a query someone ran by hand looks like
/// this. One run shows what it cost, not that it will run again, so it isn't a pattern to fix.
pub(crate) fn ran_once(q: &QueryStat) -> bool {
    q.executions <= 1.0
}

/// Whether a statement's cost is behind it: it stopped running, or it ran once. Advice (an index to add, a plan to
/// look at) is for statements that keep running.
pub(crate) fn query_is_history(snapshot: &Snapshot, q: &QueryStat) -> bool {
    ran_once(q) || query_stopped(snapshot, q).is_some()
}

/// The longest stretch of empty timeline buckets the query came back after, widened by the two partial buckets around
/// it (when in them it ran isn't known). None when it never paused for a whole bucket.
fn longest_return(q: &QueryStat, workload: &culprit_core::Workload) -> Option<f64> {
    let trend = q.trend.as_ref()?;
    let timeline = workload.timeline.as_ref()?;
    let at = |i: usize| timeline.starts.get(i).and_then(|s| support::parse_epoch_seconds(s));
    let width = at(1)? - at(0)?;
    let active: Vec<usize> =
        trend.executions.iter().enumerate().filter(|(_, e)| e.is_some_and(|e| e > 0.0)).map(|(i, _)| i).collect();
    let empty = active.windows(2).map(|w| w[1] - w[0] - 1).max().filter(|&n| n >= 1)?;
    Some((empty + 2) as f64 * width)
}

/// A finding is about queries when its subject is one (lock time, tuning advice, parameter sensitivity…) or its check
/// reads plans: what it says rests on how those statements ran.
fn about_queries(f: &Finding) -> bool {
    const QUERY_DRIVEN: [&str; 9] = [
        "plan-warnings",
        "plan-insights",
        "filtered-scan",
        "heavy-query",
        "chatty-query",
        "unstable-query",
        "plan-regression",
        "regression",
        "temp-spill",
    ];
    !f.queries.is_empty() && (f.subject.kind == SubjectKind::Query || QUERY_DRIVEN.contains(&f.check.as_str()))
}

/// Findings about statements that each ran once in the window (see `ran_once`) stay on the list, Low, ranked low, and
/// say so first: on any engine and for any check, a single run is what happened, not a pattern to fix.
fn mark_one_off(snapshot: &Snapshot, findings: &mut [Finding]) {
    let Some(workload) = &snapshot.workload else { return };
    for f in findings.iter_mut().filter(|f| !f.inactive && about_queries(f)) {
        let queries: Vec<&QueryStat> = f.queries.iter().filter_map(|r| workload.query(&r.id)).collect();
        if queries.is_empty() || !queries.iter().all(|q| ran_once(q)) {
            continue;
        }
        f.severity = Severity::Low;
        f.impact.score *= 0.25;
        f.caveats.insert(
            0,
            format!(
                "{} once in {}. One run shows what it cost, not that it will run again: an export, a backfill or a query someone ran by hand looks like this. It needs work only if it keeps running.",
                if queries.len() == 1 { "This statement ran" } else { "Each of these statements ran" },
                workload.window_phrase()
            ),
        );
        f.verify.insert(0, "Re-scan later: if it hasn't run again, there is nothing to do.".into());
    }
}

/// Findings about queries that all stopped running (see `query_stopped`) are kept, ranked low and labelled.
fn mark_inactive(snapshot: &Snapshot, findings: &mut [Finding]) {
    let Some(workload) = &snapshot.workload else { return };
    for f in findings.iter_mut().filter(|f| about_queries(f)) {
        let queries: Vec<&QueryStat> = f.queries.iter().filter_map(|r| workload.query(&r.id)).collect();
        f.last_seen = queries.iter().filter_map(|q| q.last_seen.clone()).max_by(|a, b| {
            support::parse_epoch_seconds(a)
                .partial_cmp(&support::parse_epoch_seconds(b))
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        let silences: Vec<(f64, String)> = queries.iter().filter_map(|q| query_stopped(snapshot, q)).collect();
        if queries.is_empty() || silences.len() < queries.len() {
            continue;
        }
        let (silence, last) = silences.into_iter().min_by(|a, b| a.0.total_cmp(&b.0)).expect("non-empty");
        f.inactive = true;
        f.impact.score *= 0.25;
        f.caveats.insert(
            0,
            format!(
                "{} not run since {} ({} before this scan's data ends). A deploy most likely changed or fixed {}; check that the current version of this code doesn't have the same problem.",
                if queries.len() == 1 { "This query has" } else { "None of these queries has" },
                last.get(..16).unwrap_or(&last).replace('T', " "),
                format::duration(silence * 1000.0),
                if queries.len() == 1 { "it" } else { "them" }
            ),
        );
    }
}

fn keys_of(id: &str) -> Vec<String> {
    let Some(start) = id.rfind('(') else {
        return Vec::new();
    };
    id[start + 1..]
        .trim_end_matches(')')
        .split(',')
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty())
        .collect()
}

/// The report's summary for these findings: counts, the share the top ones touch and the headline.
pub fn summarize(snapshot: &Snapshot, findings: &[Finding]) -> Summary {
    let mut by_category: Vec<CategoryCount> = Vec::new();
    for category in [Category::Indexes, Category::Queries, Category::Maintenance, Category::Configuration] {
        let count = findings.iter().filter(|f| f.category == category).count() as u32;
        if count > 0 {
            by_category.push(CategoryCount { category, count });
        }
    }
    let by_severity = [Severity::High, Severity::Medium, Severity::Low]
        .into_iter()
        .map(|severity| SeverityCount {
            severity,
            count: findings.iter().filter(|f| f.severity == severity).count() as u32,
        })
        .filter(|c| c.count > 0)
        .collect();

    let top_share = support::top_findings_share(snapshot, findings.iter());

    let n = findings.iter().filter(|f| !f.inactive).count();
    let window = support::summary_window(snapshot);
    let headline = match (n, top_share, &window) {
        (0, _, Some(window)) => format!("Nothing stands out over {window}."),
        (0, _, None) => "Nothing stands out.".to_string(),
        (n, Some(share), Some(window)) if share >= 0.05 => format!(
            "{} {}. The top ones touch {} of database time over {window}.",
            n,
            plural(n, "opportunity", "opportunities"),
            format::percent(share)
        ),
        (n, _, _) => format!("{} {}.", n, plural(n, "opportunity", "opportunities")),
    };
    let fixed = findings.iter().filter(|f| f.inactive).count();
    let headline = match fixed {
        0 => headline,
        1 => format!("{headline} 1 more stopped running, likely changed by a deploy."),
        k => format!("{headline} {k} more stopped running, likely changed by a deploy."),
    };
    Summary { opportunities: n as u32, by_category, by_severity, top_share, headline }
}

fn plural<'a>(n: usize, one: &'a str, many: &'a str) -> &'a str {
    if n == 1 { one } else { many }
}

#[cfg(test)]
mod window_tests {
    use culprit_core::Engine;

    use super::summarize;
    use crate::waits::snapshot_with;

    /// A scan with `--window 6h` reads about six hours of Query Store intervals, and the headline says so.
    #[test]
    fn the_headline_names_an_hours_window() {
        let mut snapshot = snapshot_with(Engine::Mssql);
        let workload = snapshot.workload.as_mut().expect("fixture has a workload");
        workload.windowed = true;
        workload.window_hours = 6.0;
        assert_eq!(summarize(&snapshot, &[]).headline, "Nothing stands out over the last 6 hours.");
    }

    /// Query Store keeps whole hourly intervals, so `--window 6h` reads the interval it starts in too: the headline names
    /// the window asked for, and the rounding.
    #[test]
    fn the_headline_names_the_window_asked_for_and_the_rounding() {
        let mut snapshot = snapshot_with(Engine::Mssql);
        snapshot.options = Some(culprit_core::SnapshotOptions::default().with_window_minutes(360));
        let workload = snapshot.workload.as_mut().expect("fixture has a workload");
        workload.windowed = true;
        workload.window_hours = 6.9;
        assert_eq!(
            summarize(&snapshot, &[]).headline,
            "Nothing stands out over the last 6 hours (Query Store keeps whole intervals, so this covers 7)."
        );
        // Rounding that doesn't show isn't mentioned.
        snapshot.workload.as_mut().unwrap().window_hours = 6.3;
        assert_eq!(summarize(&snapshot, &[]).headline, "Nothing stands out over the last 6 hours.");
        // Less data than asked for: what it covers.
        snapshot.workload.as_mut().unwrap().window_hours = 2.0;
        assert_eq!(summarize(&snapshot, &[]).headline, "Nothing stands out over the last 2 hours.");
    }
}

#[cfg(test)]
mod prose_tests {
    use super::without_database;

    #[test]
    fn prose_names_lose_the_database() {
        assert_eq!(
            without_database("CONVERT_IMPLICIT(nvarchar(200),[shop_db].[dbo].[customers].[email],0)", "shop_db"),
            "CONVERT_IMPLICIT(nvarchar(200),[dbo].[customers].[email],0)"
        );
        assert_eq!(without_database("[other].[dbo].[t]", "shop_db"), "[other].[dbo].[t]");
    }
}
