//! Rules over every check's findings: the pipeline's refine stage (see [`crate::pipeline`]). Each rule fixes, once for
//! every check and engine, what no single check can see on its own: two checks describing one cause, or a finding that
//! rests on statements whose cost is behind them. A new rule is a function here and a line in [`rules`].

use std::collections::BTreeSet;
use std::sync::Arc;

use culprit_core::{Engine, Finding, QueryStat, Severity, Snapshot, SubjectKind, format};

use crate::pipeline::{Refine, Step};
use crate::{signals, support};

/// The built-in rules, in the order they run.
pub(crate) fn rules() -> Vec<Arc<dyn Refine>> {
    let rule =
        |name: &'static str, run: fn(&Snapshot, &mut Vec<Finding>)| Arc::new(Step { name, run }) as Arc<dyn Refine>;
    vec![
        // One cause, one finding.
        rule("a missing-index request absorbs the scan behind it", missing_index_absorbs_scan),
        rule("a rewrite, not an index, fixes a scan behind a function or wildcard", rewrite_replaces_index),
        rule("a spill explains the big sort", spill_explains_sort),
        rule("an index to add covers the foreign key", index_covers_foreign_key),
        rule("an index to add replaces a histogram", index_replaces_histogram),
        rule("a regression carries its plan change", regression_carries_plan_change),
        rule("a heavy query another finding explains", explained_heavy_query),
        // Statements whose cost is behind them.
        rule("statements that stopped running", stopped_running),
        rule("statements that ran once", ran_once_only),
        // Wording.
        rule("names without the database", names_without_database),
    ]
}

/// SQL Server reports the same gap twice: as a missing-index suggestion and as a scan in the plan. Keep the suggestion, which
/// carries the optimizer's own estimate, and give it the plan evidence.
fn missing_index_absorbs_scan(_: &Snapshot, findings: &mut Vec<Finding>) {
    merge_scans_into_missing_indexes(findings);
}

/// A function or leading wildcard around the filtered column is why the scan happens; an index on the column
/// (filtered-scan's advice) wouldn't be used until the query changes.
fn rewrite_replaces_index(_: &Snapshot, findings: &mut Vec<Finding>) {
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
}

/// A spill finding already explains a big sort in the same query, with the memory fix.
fn spill_explains_sort(_: &Snapshot, findings: &mut Vec<Finding>) {
    let spilled: BTreeSet<String> = findings
        .iter()
        .filter(|f| f.check == "temp-spill" || f.id.contains(":spill"))
        .flat_map(|f| f.queries.iter().map(|q| q.id.clone()))
        .collect();
    findings.retain(|f| {
        !(f.id.starts_with("plan-insights:big-sort:") || f.id.starts_with("plan-insights:temp-table-sort:"))
            || !f.queries.iter().all(|q| spilled.contains(&q.id))
    });
}

/// The keys of every index the findings suggest adding, by table.
fn suggested_index_keys(findings: &[Finding]) -> Vec<(String, Vec<String>)> {
    findings
        .iter()
        .filter(|f| matches!(f.check.as_str(), "missing-index" | "filtered-scan"))
        .map(|f| (f.subject.name.to_ascii_lowercase(), keys_of(&f.id)))
        .collect()
}

/// An index suggested for a table's queries that leads with a foreign key's columns serves the key too.
fn index_covers_foreign_key(_: &Snapshot, findings: &mut Vec<Finding>) {
    let index_keys = suggested_index_keys(findings);
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
}

/// An index on the filtered columns gives the optimizer exact estimates: the histogram is then the lesser fix.
fn index_replaces_histogram(_: &Snapshot, findings: &mut Vec<Finding>) {
    let index_keys = suggested_index_keys(findings);
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
}

/// A regression finding says when the query got slower and carries the plan change; the plan-only finding repeats it.
fn regression_carries_plan_change(_: &Snapshot, findings: &mut Vec<Finding>) {
    let regressed: BTreeSet<String> = findings
        .iter()
        .filter(|f| f.check == "regression")
        .flat_map(|f| f.queries.iter().map(|q| q.id.clone()))
        .collect();
    findings.retain(|f| f.check != "plan-regression" || !f.queries.iter().any(|q| regressed.contains(&q.id)));
}

/// "This query takes a lot of time" is the fallback for a heavy query nothing else explains; when another finding
/// names why (a missing index, a spill, a type mismatch…), that finding is the one to act on.
fn explained_heavy_query(_: &Snapshot, findings: &mut Vec<Finding>) {
    let explained: BTreeSet<String> = findings
        .iter()
        .filter(|f| !matches!(f.check.as_str(), "heavy-query" | "unused-index" | "duplicate-index" | "stale-stats"))
        .flat_map(|f| f.queries.iter().map(|q| q.id.clone()))
        .collect();
    findings.retain(|f| f.check != "heavy-query" || !f.queries.iter().any(|q| explained.contains(&q.id)));
}

/// SQL Server writes names with the database in front; prose drops it (see `without_database`).
#[allow(clippy::ptr_arg)] // every rule has the signature `rules` lists them by
fn names_without_database(snapshot: &Snapshot, findings: &mut Vec<Finding>) {
    if snapshot.server.engine == Engine::Mssql {
        for f in findings.iter_mut() {
            without_database_prose(f, &snapshot.server.database);
        }
    }
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
                && missing_index_serves_scan(&keys_of(&m.id), &scan_keys)
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
#[allow(clippy::ptr_arg)] // every rule has the signature `rules` lists them by
fn ran_once_only(snapshot: &Snapshot, findings: &mut Vec<Finding>) {
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

/// Findings about queries that all stopped running (see `query_stopped`) are kept, ranked low and labelled: a deploy most likely changed or fixed them.
#[allow(clippy::ptr_arg)] // every rule has the signature `rules` lists them by
fn stopped_running(snapshot: &Snapshot, findings: &mut Vec<Finding>) {
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

// ── indexes ──

/// Whether the index a missing-index request creates (`missing_keys`, in its DDL's order) serves the scan a plan shows
/// filtering on `scan_keys`: the index leads with a column the scan filters on, so the scan's rows can be sought by it.
/// An index that holds the scan's column only further along (`(customer_id, status)` for a scan on `status`) could at
/// best be scanned, so that scan keeps its own finding and its own index.
pub(crate) fn missing_index_serves_scan(missing_keys: &[String], scan_keys: &[String]) -> bool {
    missing_keys.first().is_some_and(|lead| scan_keys.iter().any(|k| k.eq_ignore_ascii_case(lead)))
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
