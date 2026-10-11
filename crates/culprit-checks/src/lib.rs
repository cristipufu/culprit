mod brief;
mod config;
mod indexes;
mod maintenance;
mod queries;
pub(crate) mod support;

use std::collections::BTreeSet;
use std::sync::Arc;

use culprit_core::{
    Capability, Category, CategoryCount, Check, CheckInfo, Finding, LockedCheck, QueryStat, Report, ServerInfo,
    Severity, SeverityCount, Snapshot, Summary, format,
};

pub use brief::agent_brief;

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
        Builtin { info: queries::heavy_query_info, run: queries::heavy_query },
        Builtin { info: queries::chatty_query_info, run: queries::chatty_query },
        Builtin { info: queries::plan_warnings_info, run: queries::plan_warnings },
        Builtin { info: queries::plan_regression_info, run: queries::plan_regression },
        Builtin { info: queries::unstable_query_info, run: queries::unstable_query },
        Builtin { info: queries::temp_spill_info, run: queries::temp_spill },
        Builtin { info: maintenance::stale_stats_info, run: maintenance::stale_stats },
        Builtin { info: maintenance::heap_table_info, run: maintenance::heap_table },
        Builtin { info: maintenance::dead_rows_info, run: maintenance::dead_rows },
        Builtin { info: maintenance::xid_age_info, run: maintenance::xid_age },
        Builtin { info: config::config_info, run: config::config },
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
    mark_inactive(&snapshot, &mut findings);
    findings.sort_by(|a, b| {
        a.inactive.cmp(&b.inactive).then(b.impact.score.total_cmp(&a.impact.score)).then(b.severity.cmp(&a.severity))
    });
    let summary = summarize(&snapshot, &findings);
    Report { snapshot, findings, locked, summary }
}

fn dedupe(findings: &mut Vec<Finding>) {
    merge_scans_into_missing_indexes(findings);

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
        !index_keys
            .iter()
            .any(|(t, keys)| *t == table && fk_cols.iter().all(|c| keys.iter().take(fk_cols.len()).any(|k| k == c)))
    });

    let explained: BTreeSet<String> = findings
        .iter()
        .filter(|f| !matches!(f.check.as_str(), "heavy-query" | "unused-index" | "duplicate-index" | "stale-stats"))
        .flat_map(|f| f.queries.iter().map(|q| q.id.clone()))
        .collect();
    findings.retain(|f| f.check != "heavy-query" || !f.queries.iter().any(|q| explained.contains(&q.id)));
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
fn mark_inactive(snapshot: &Snapshot, findings: &mut [Finding]) {
    const QUIET_INTERVALS: f64 = 20.0;
    const MIN_QUIET_SECONDS: f64 = 3600.0;
    let Some(workload) = &snapshot.workload else { return };
    let Some(end) = workload.window_end.as_deref().and_then(support::parse_epoch_seconds) else { return };
    let window_start = end - workload.window_hours * 3600.0;
    let quiet = |q: &QueryStat| -> Option<(f64, String)> {
        let last_text = q.last_seen.clone()?;
        let last = support::parse_epoch_seconds(&last_text)?;
        let first = q.first_seen.as_deref().and_then(support::parse_epoch_seconds).unwrap_or(window_start);
        let interval = (last - first).max(1.0) / q.executions.max(1.0);
        let silence = end - last;
        (silence >= (interval * QUIET_INTERVALS).max(MIN_QUIET_SECONDS)).then_some((silence, last_text))
    };
    const QUERY_DRIVEN: [&str; 7] = [
        "plan-warnings",
        "filtered-scan",
        "heavy-query",
        "chatty-query",
        "unstable-query",
        "plan-regression",
        "temp-spill",
    ];
    for f in findings.iter_mut().filter(|f| !f.queries.is_empty() && QUERY_DRIVEN.contains(&f.check.as_str())) {
        let queries: Vec<&QueryStat> = f.queries.iter().filter_map(|r| workload.query(&r.id)).collect();
        f.last_seen = queries.iter().filter_map(|q| q.last_seen.clone()).max_by(|a, b| {
            support::parse_epoch_seconds(a)
                .partial_cmp(&support::parse_epoch_seconds(b))
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        let silences: Vec<(f64, String)> = queries.iter().filter_map(|q| quiet(q)).collect();
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

fn summarize(snapshot: &Snapshot, findings: &[Finding]) -> Summary {
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

    let top_share = snapshot.workload.as_ref().and_then(|w| {
        let ids: BTreeSet<&str> = findings
            .iter()
            .filter(|f| f.impact.db_time_share.is_some() && !f.inactive)
            .take(3)
            .flat_map(|f| f.queries.iter().map(|q| q.id.as_str()))
            .collect();
        if ids.is_empty() {
            return None;
        }
        let time: f64 = w.queries.iter().filter(|q| ids.contains(q.id.as_str())).map(|q| q.total_time_ms).sum();
        Some(w.share(time))
    });

    let n = findings.iter().filter(|f| !f.inactive).count();
    let window = snapshot.workload.as_ref().map(support::window_phrase);
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
