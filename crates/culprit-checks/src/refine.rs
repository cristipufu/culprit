//! Rules over every check's findings: the pipeline's refine stage (see [`crate::pipeline`]). Each rule fixes, once for
//! every check and engine, what no single check can see on its own: two checks describing one cause, or a finding that
//! rests on statements whose cost is behind them. A new rule is a function here and a line in [`rules`].

use std::collections::{BTreeMap, BTreeSet};
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
        rule("an index the engine builds for itself is the foreign key's", engine_built_index_is_the_foreign_keys),
        rule("an index to add replaces a histogram", index_replaces_histogram),
        rule("a regression carries its plan change", regression_carries_plan_change),
        rule("one plan story per query", one_plan_story),
        rule("dead rows are the table's empty space", dead_rows_explain_bloat),
        rule("updates that aren't HOT explain their WAL", cold_updates_explain_log),
        rule("large values explain the log their writes make", large_values_explain_log),
        rule("one blocking chain on a table", one_blocking_chain),
        rule("the plan cache finding carries the compile load", plan_cache_carries_compiles),
        rule("dead rows absorb the table's stale statistics", dead_rows_absorb_stale_stats),
        rule("a replication slot's finding carries the cleanup it holds back", slot_carries_held_cleanup),
        rule("waiting on the application explains a heavy query", network_wait_explains_heavy_query),
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

/// An index suggested for a table's queries that leads with a foreign key's columns serves the key too: one that will
/// be created, unfiltered. A filtered index (`WHERE is_deleted = 0`) can't serve a delete on the parent, and one whose
/// statements are all history is advice nobody will follow: the key keeps its own finding.
fn index_covers_foreign_key(snapshot: &Snapshot, findings: &mut Vec<Finding>) {
    let history = |f: &Finding| {
        let workload = snapshot.workload.as_ref();
        let queries: Vec<&QueryStat> = f.queries.iter().filter_map(|r| workload?.query(&r.id)).collect();
        !queries.is_empty() && queries.iter().all(|q| query_is_history(snapshot, q))
    };
    let filtered = |f: &Finding| {
        f.fix.as_ref().and_then(|x| x.sql.as_deref()).is_some_and(|s| s.to_ascii_uppercase().contains(" WHERE "))
    };
    let index_keys =
        suggested_index_keys(&findings.iter().filter(|f| !filtered(f) && !history(f)).cloned().collect::<Vec<_>>());
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
/// names why (a missing index, a spill, lock waits, a type mismatch…), that finding is the one to act on. What a plan
/// shows explains the time only when it weighs as much: a plan warning or insight graded lower (a join condition
/// that is there after all, statistics the plan merely lacks) leaves the heavy query standing. The list of forced plans
/// ("nothing to do while they help") explains nothing.
fn explained_heavy_query(_: &Snapshot, findings: &mut Vec<Finding>) {
    let mut explained: BTreeMap<String, Severity> = BTreeMap::new();
    for f in findings.iter().filter(|f| {
        // An over-sized grant slows others, not the query itself; waiting on the application explains a query only
        // when it is most of its time (`network_wait_explains_heavy_query`).
        !matches!(
            f.check.as_str(),
            "heavy-query" | "unused-index" | "duplicate-index" | "stale-stats" | "memory-grant" | "network-wait"
        ) && !f.id.starts_with("tuning-advice:forced-plans")
    }) {
        // A cause another check names explains it at any grade; a plan's remark only at the heavy query's.
        let weight =
            if matches!(f.check.as_str(), "plan-warnings" | "plan-insights") { f.severity } else { Severity::High };
        for q in &f.queries {
            let s = explained.entry(q.id.clone()).or_insert(weight);
            *s = (*s).max(weight);
        }
    }
    findings.retain(|f| {
        f.check != "heavy-query" || !f.queries.iter().any(|q| explained.get(&q.id).is_some_and(|s| *s >= f.severity))
    });
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

/// SQLite builds an index for itself on every run (`plan-insights:automatic-index:*`) on a foreign key's columns: the
/// foreign key's finding, which carries every statement through the key, keeps the cause and takes the evidence. The
/// statements are weighed one by one: one whose plan builds an index on other columns of the table keeps the insight.
fn engine_built_index_is_the_foreign_keys(snapshot: &Snapshot, findings: &mut Vec<Finding>) {
    let Some(w) = snapshot.workload.as_ref() else { return };
    // (insight, foreign key finding, the insight's statements whose built index is the key's)
    let mut folded: Vec<(usize, usize, Vec<String>)> = Vec::new();
    for (at, f) in findings.iter().enumerate() {
        if !f.id.starts_with("plan-insights:automatic-index:") {
            continue;
        }
        // The index each referenced plan builds: its table and keys, read the one way the insight reads them.
        let built: Vec<(String, String, Vec<String>)> = f
            .queries
            .iter()
            .filter_map(|r| {
                let plan = w.query(&r.id)?.plan.as_ref()?;
                let b = culprit_core::insights::engine_built_indexes(plan)
                    .into_iter()
                    .find(|b| Some(b.node) == r.plan_node)?;
                Some((r.id.clone(), table_leaf(&b.table), b.keys.iter().map(|k| k.to_ascii_lowercase()).collect()))
            })
            .collect();
        let serves = |g: &Finding, table: &str, keys: &[String]| {
            let fk_cols = keys_of(&g.id);
            !fk_cols.is_empty() && table == table_leaf(&g.subject.name) && keys.starts_with(&fk_cols)
        };
        let Some(fk) = findings
            .iter()
            .position(|g| g.check == "unindexed-fk" && built.iter().any(|(_, table, keys)| serves(g, table, keys)))
        else {
            continue;
        };
        let matching: Vec<String> = built
            .iter()
            .filter(|(_, table, keys)| serves(&findings[fk], table, keys))
            .map(|(q, _, _)| q.clone())
            .collect();
        folded.push((at, fk, matching));
    }
    let mut gone: BTreeSet<usize> = BTreeSet::new();
    for (at, fk, matching) in folded.iter().rev() {
        let (moved, kept): (Vec<_>, Vec<_>) =
            findings[*at].queries.clone().into_iter().partition(|r| matching.contains(&r.id));
        let target = &mut findings[*fk];
        target.summary.push_str(&format!(
            " {} already builds this index for itself on every run of {}, and drops it after.",
            snapshot.server.engine,
            if moved.len() == 1 {
                "one of these statements".to_string()
            } else {
                format!("{} of these statements", moved.len())
            }
        ));
        for r in moved {
            match target.queries.iter_mut().find(|x| x.id == r.id) {
                // Point at the step that builds it.
                Some(existing) => existing.plan_node = existing.plan_node.or(r.plan_node),
                None => target.queries.push(r),
            }
        }
        if kept.is_empty() {
            gone.insert(*at);
        } else {
            findings[*at].queries = kept;
        }
    }
    let mut i = 0;
    findings.retain(|_| {
        let keep = !gone.contains(&i);
        i += 1;
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
    // A day and a bit: long enough that a nightly job would have come back.
    const DAILY_SECONDS: f64 = 26.0 * 3600.0;
    // One run has no rhythm to stop: that's `ran_once`, not a deploy. The slow query log keeps only slow runs: one
    // not logged since may still run, faster than long_query_time.
    if ran_once(q) || crate::queries::slow_log_only(snapshot) {
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
    // Runs that came in one burst inside the window (a nightly job, a batch: they began after it started and covered
    // a small part of it) have no rhythm the window shows: quiet since is how such a job looks, until a day has passed
    // without it coming back. A query running from the window's start (within a bucket of it) ran before it too.
    let bucket = workload
        .timeline
        .as_ref()
        .and_then(|t| {
            Some(support::parse_epoch_seconds(t.starts.get(1)?)? - support::parse_epoch_seconds(t.starts.first()?)?)
        })
        .unwrap_or(900.0);
    let began_inside = first - window_start > bucket.max(900.0);
    if began_inside && (last - first) < workload.window_hours * 3600.0 * 0.5 && silence < DAILY_SECONDS {
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
    // A family's statements are literal copies of one: each runs once and is gone by construction, the family runs on.
    if f.check == "query-family" {
        return false;
    }
    const QUERY_DRIVEN: [&str; 11] = [
        // An index request or a table's scans stand on the statements that asked for them or read it.
        "missing-index",
        "scan-heavy",
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
        // The slow query log keeps only runs over long_query_time: once in it is one slow run, not one run.
        let caveat = if crate::queries::slow_log_only(snapshot) {
            format!(
                "{} logged once in {}: the slow query log keeps only runs over long_query_time, so it may run more often, faster. One slow run shows what it cost once, not a pattern; it needs work if it keeps showing up.",
                if queries.len() == 1 { "This statement was" } else { "Each of these statements was" },
                workload.window_phrase()
            )
        } else {
            format!(
                "{} once in {}. One run shows what it cost, not that it will run again: an export, a backfill or a query someone ran by hand looks like this. It needs work only if it keeps running.",
                if queries.len() == 1 { "This statement ran" } else { "Each of these statements ran" },
                workload.window_phrase()
            )
        };
        f.caveats.insert(0, caveat);
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

// ── queries ──

/// One finding per query's plan change. SQL Server's own recommendation to force a plan (`tuning-advice:force:`) is
/// the one to act on: a `regression` or `plan-regression` finding for the same query would tell the same change again,
/// possibly forcing another plan, so it becomes the recommendation's timeline evidence. When the query's plans run
/// side by side for different values (`parameter-sensitive`), that finding is the story and the recommendation becomes
/// a caveat on it: forcing one plan for every value is only the first step of its fix, and may be the wrong plan.
fn one_plan_story(_: &Snapshot, findings: &mut Vec<Finding>) {
    let recommendations: Vec<(usize, String)> = findings
        .iter()
        .enumerate()
        .filter(|(_, f)| f.id.starts_with("tuning-advice:force:"))
        .filter_map(|(i, f)| f.queries.first().map(|q| (i, q.id.clone())))
        .collect();
    let mut drop = BTreeSet::new();
    for (t, query) in recommendations {
        let about = |f: &Finding| f.queries.iter().any(|q| q.id == query);
        let told: Vec<usize> = findings
            .iter()
            .enumerate()
            .filter(|(i, f)| *i != t && matches!(f.check.as_str(), "regression" | "plan-regression") && about(f))
            .map(|(i, _)| i)
            .collect();
        for e in told {
            let source = findings[e].clone();
            let rec = &mut findings[t];
            // The first sentence says when and by how much; the rest is the other finding's own reading of the cause.
            if let Some((first, _)) = source.summary.split_once(". ").or(Some((source.summary.as_str(), ""))) {
                let first = first.trim_end_matches('.');
                let sentence = match first.strip_prefix("Until ") {
                    Some(rest) => format!("Query Store's timeline: until {rest}."),
                    None => format!("{first}."),
                };
                rec.summary = format!("{} {sentence}", rec.summary.trim_end());
            }
            for m in source.metrics {
                if !rec.metrics.iter().any(|x| x.label == m.label) {
                    rec.metrics.push(m);
                }
            }
            for line in source.verify {
                if !rec.verify.contains(&line) {
                    rec.verify.push(line);
                }
            }
            for line in source.caveats {
                if !rec.caveats.contains(&line) {
                    rec.caveats.push(line);
                }
            }
            rec.severity = rec.severity.max(source.severity);
            rec.impact.score = rec.impact.score.max(source.impact.score);
            rec.impact.time_ms_per_day = rec.impact.time_ms_per_day.or(source.impact.time_ms_per_day);
            drop.insert(e);
        }
        if let Some(s) = findings.iter().position(|f| f.check == "parameter-sensitive" && about(f)) {
            let caveat = recommendation_caveat(&findings[t], &findings[s]);
            let (severity, score) = (findings[t].severity, findings[t].impact.score);
            let sensitive = &mut findings[s];
            sensitive.caveats.push(caveat);
            sensitive.severity = sensitive.severity.max(severity);
            sensitive.impact.score = sensitive.impact.score.max(score);
            drop.insert(t);
        }
    }
    let mut index = 0;
    findings.retain(|_| {
        let keep = !drop.contains(&index);
        index += 1;
        keep
    });
}

/// SQL Server's recommendation, said on the parameter-sensitivity finding for the same query.
fn recommendation_caveat(recommendation: &Finding, sensitive: &Finding) -> String {
    let text = |f: &Finding, label: &str| f.metrics.iter().find(|m| m.label == label).and_then(|m| m.text.clone());
    let plan = text(recommendation, "Recommended plan").unwrap_or_else(|| "another plan".into());
    let gain = recommendation
        .metrics
        .iter()
        .find(|m| m.label == "Estimated CPU saved per run")
        .map(|m| format!(" (estimated {} less CPU per run)", format::percent(m.value)))
        .unwrap_or_default();
    match text(sensitive, crate::queries::PLAN_MOST_CALLS_USE) {
        Some(fast) if fast == plan => format!(
            "SQL Server's automatic tuning also recommends forcing plan {plan}{gain}: the plan most calls run fast with, the fix's first step."
        ),
        Some(fast) => format!(
            "SQL Server's automatic tuning recommends forcing plan {plan}{gain}, while most calls run fast with plan {fast}. With plans used side by side for different values, forcing one for every value can slow the calls the other suits: try a value from a slow call under it first."
        ),
        None => format!("SQL Server's automatic tuning recommends forcing plan {plan}{gain}."),
    }
}

// ── signals ──────────────────────────────────────────────────────────────────────────────────────────

/// A metric's value on a finding, by label.
fn metric(f: &Finding, label: &str) -> Option<f64> {
    f.metrics.iter().find(|m| m.label == label).map(|m| m.value)
}

/// The table a finding about a table names, as `schema.table` lowercased: its subject, or for hot-rows (whose subject
/// is the index most of the waiting was on) its id.
fn finding_table(f: &Finding) -> String {
    match f.id.strip_prefix("hot-rows:") {
        Some(table) => table.to_ascii_lowercase(),
        None => f.subject.name.to_ascii_lowercase(),
    }
}

/// The table a write statement changes, by name only (`orders`), and its verb: insert, update or merge.
fn written_table(q: &QueryStat) -> Option<(&'static str, String)> {
    let handle = support::query_handle(q);
    [("insert", "an insert into "), ("update", "an update of "), ("merge", "a merge into ")]
        .into_iter()
        .find_map(|(verb, prefix)| handle.strip_prefix(prefix).map(|t| (verb, t.to_ascii_lowercase())))
}

/// Takes the finding at `from` out of `findings` and lets `into` (the finding that names the cause) absorb what it
/// adds: its queries, its rank and, unless its statements are history (they stopped running, or ran once), its
/// severity. The finding after `from` takes its place.
fn absorb(snapshot: &Snapshot, findings: &mut Vec<Finding>, into: usize, from: usize, note: Option<String>) {
    let gone = findings[from].clone();
    let history = gone.inactive
        || snapshot
            .workload
            .as_ref()
            .is_some_and(|w| gone.queries.iter().filter_map(|r| w.query(&r.id)).any(|q| query_is_history(snapshot, q)));
    let target = &mut findings[into];
    for q in &gone.queries {
        match target.queries.iter_mut().find(|r| r.id == q.id) {
            Some(r) => {
                if r.note.is_none() {
                    r.note = note.clone();
                }
            }
            None => target.queries.push(culprit_core::QueryRef {
                id: q.id.clone(),
                plan_node: q.plan_node,
                note: note.clone(),
            }),
        }
    }
    target.impact.score = target.impact.score.max(gone.impact.score);
    target.impact.db_time_share = match (target.impact.db_time_share, gone.impact.db_time_share) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (a, b) => a.or(b),
    };
    target.impact.time_ms_per_day = target.impact.time_ms_per_day.or(gone.impact.time_ms_per_day);
    target.last_seen = target.last_seen.clone().max(gone.last_seen.clone());
    if !history {
        target.severity = target.severity.max(gone.severity);
    }
    findings.remove(from);
}

/// The position of the first finding of `check` about `table` (`schema.table`, lowercased).
fn position_on(findings: &[Finding], check: &str, table: &str) -> Option<usize> {
    findings.iter().position(|f| f.check == check && finding_table(f) == table)
}

fn leaf(table: &str) -> &str {
    table.rsplit('.').next().unwrap_or(table)
}

/// PostgreSQL's bloat estimate counts every byte beyond the live rows, so dead rows VACUUM hasn't cleared yet show up
/// as empty space too, with a rewrite (pg_repack, VACUUM FULL) as the fix. When the dead rows account for the space,
/// dead-rows already says it with the right fix, VACUUM: keep that one, and say what the estimate saw.
fn dead_rows_explain_bloat(snapshot: &Snapshot, findings: &mut Vec<Finding>) {
    let mut i = 0;
    while i < findings.len() {
        if findings[i].check != "table-bloat" {
            i += 1;
            continue;
        }
        let table = finding_table(&findings[i]);
        let share = metric(&findings[i], "Share").unwrap_or(1.0);
        let dead = snapshot
            .tables
            .iter()
            .find(|t| t.qualified().eq_ignore_ascii_case(&table))
            .and_then(signals::dead_share)
            .filter(|d| *d >= share * signals::DEAD_EXPLAINS_BLOAT);
        let Some(target) = dead.and_then(|_| position_on(findings, "dead-rows", &table)) else {
            i += 1;
            continue;
        };
        let bytes = metric(&findings[i], "Empty space (estimated)").unwrap_or(0.0);
        findings[target].caveats.push(format!(
            "PostgreSQL's statistics also estimate about {} of {} ({}) as empty space: these dead rows. VACUUM makes it reusable; rewrite the table (pg_repack) only if the estimate stays high after a vacuum.",
            format::bytes(bytes),
            leaf(&table),
            format::percent(share)
        ));
        // `absorb` removed `i`: the next finding is now there.
        absorb(snapshot, findings, target, i, None);
    }
}

/// An update that isn't HOT writes the new row version, an entry in every index and, after each checkpoint, whole
/// pages: the WAL a log-volume finding counts for an UPDATE is what hot-updates explains for its table, when that
/// statement made most of the table's non-HOT updates. One finding, the table's, with the statement and its WAL.
fn cold_updates_explain_log(snapshot: &Snapshot, findings: &mut Vec<Finding>) {
    let Some(workload) = &snapshot.workload else { return };
    let mut i = 0;
    while i < findings.len() {
        let f = &findings[i];
        let candidate = (f.check == "log-volume")
            .then(|| f.queries.first().and_then(|r| workload.query(&r.id)))
            .flatten()
            .and_then(|q| Some((q, written_table(q)?)))
            .filter(|(_, (verb, _))| *verb == "update");
        let Some((q, (_, table))) = candidate else {
            i += 1;
            continue;
        };
        let target = findings.iter().position(|h| {
            h.check == "hot-updates"
                && leaf(&finding_table(h)) == table
                && snapshot.tables.iter().find(|t| t.qualified().eq_ignore_ascii_case(&finding_table(h))).is_some_and(
                    |t| {
                        let cold =
                            t.updates.unwrap_or(0.0) - t.storage.as_ref().and_then(|s| s.hot_updates).unwrap_or(0.0);
                        q.rows.is_some_and(|rows| cold > 0.0 && rows >= 0.5 * cold)
                    },
                )
        });
        let Some(target) = target else {
            i += 1;
            continue;
        };
        let bytes = metric(&findings[i], "Log written").unwrap_or(0.0);
        let per_row = metric(&findings[i], "Per row");
        let handle = support::query_handle(q);
        let once = ran_once(q);
        let h = &mut findings[target];
        h.summary.push_str(&format!(
            " {} changed {} of these rows and wrote {} of WAL for them{}: an update that isn't HOT logs the new row version and an entry in every index, and the first change to each page after a checkpoint logs the whole page.",
            capitalize(&handle),
            format::count(q.rows.unwrap_or(0.0)),
            format::bytes(bytes),
            per_row.map(|b| format!(", about {} a row", format::bytes(b))).unwrap_or_default()
        ));
        h.metrics.push(culprit_core::Metric::new("WAL its update wrote", bytes, culprit_core::Unit::Bytes));
        if once {
            h.severity = Severity::Low;
            h.caveats.insert(
                0,
                format!(
                    "The update behind most of these ran once in {}: a backfill or a migration looks like this, and its share stays in the counts long after. It needs work only if updates like it keep coming.",
                    workload.window_phrase()
                ),
            );
        }
        absorb(snapshot, findings, target, i, Some(format!("Wrote {} of WAL.", format::bytes(bytes))));
    }
}

/// Inserting or updating large values logs them in full: a log-volume finding whose bytes per row are about the size
/// of a large-rows table's average row is that table's large values, and batching or dropping indexes (log-volume's
/// advice) can't shrink them. One finding, large-rows, with the statement and its log.
fn large_values_explain_log(snapshot: &Snapshot, findings: &mut Vec<Finding>) {
    let Some(workload) = &snapshot.workload else { return };
    let log = if snapshot.server.engine == Engine::Postgres { "WAL" } else { "transaction log" };
    let mut i = 0;
    while i < findings.len() {
        let f = &findings[i];
        let candidate = (f.check == "log-volume")
            .then(|| f.queries.first().and_then(|r| workload.query(&r.id)))
            .flatten()
            .and_then(|q| Some((q, written_table(q)?, metric(f, "Per row")?)));
        let Some((q, (_, table), per_row)) = candidate else {
            i += 1;
            continue;
        };
        let target = findings.iter().position(|l| {
            l.check == "large-rows"
                && leaf(&finding_table(l)) == table
                && metric(l, "Average row").is_some_and(|avg| avg > 0.0 && (0.5..=3.0).contains(&(per_row / avg)))
        });
        let Some(target) = target else {
            i += 1;
            continue;
        };
        let bytes = metric(&findings[i], "Log written").unwrap_or(0.0);
        let handle = support::query_handle(q);
        let l = &mut findings[target];
        l.summary.push_str(&format!(
            " {} wrote {} of {log} for them, about {} a row: the large values are logged in full, so batching the writes or dropping indexes won't shrink it; smaller or fewer values will.",
            capitalize(&handle),
            format::bytes(bytes),
            format::bytes(per_row)
        ));
        l.metrics.push(culprit_core::Metric::new("Log its writes wrote", bytes, culprit_core::Unit::Bytes));
        absorb(snapshot, findings, target, i, Some(format!("Writes about {} of {log} a row.", format::bytes(per_row))));
    }
}

/// Statements waiting on locks on a table and that table's lock waits are one blocking chain: hot-rows (the table's
/// waits, with its statements) is the finding; each statement's lock-time joins it as a note.
fn one_blocking_chain(snapshot: &Snapshot, findings: &mut Vec<Finding>) {
    let mut i = 0;
    while i < findings.len() {
        if findings[i].check != "lock-time" {
            i += 1;
            continue;
        }
        let ids: Vec<String> = findings[i].queries.iter().map(|q| q.id.clone()).collect();
        let target =
            findings.iter().position(|h| h.check == "hot-rows" && h.queries.iter().any(|q| ids.contains(&q.id)));
        let Some(target) = target else {
            i += 1;
            continue;
        };
        let lock = metric(&findings[i], "Lock wait").unwrap_or(0.0);
        let share = metric(&findings[i], "Share of its time").map(|s| {
            if s >= 0.97 { "nearly all of its time".to_string() } else { format!("{} of its time", format::percent(s)) }
        });
        let window = snapshot.workload.as_ref().map(|w| format!(" in {}", w.window_phrase())).unwrap_or_default();
        let note = format!(
            "Waited {} on locks{}{window}.",
            format::duration(lock),
            share.as_ref().map(|s| format!(", {s}")).unwrap_or_default()
        );
        let handle = snapshot
            .workload
            .as_ref()
            .and_then(|w| w.query(ids.first()?))
            .map(support::query_handle)
            .unwrap_or_else(|| "A statement".into());
        findings[target].summary.push_str(&format!(
            " {} waited {} for these locks{window}{}.",
            capitalize(&handle),
            format::duration(lock),
            share.map(|s| format!(", {s}")).unwrap_or_default()
        ));
        absorb(snapshot, findings, target, i, Some(note));
    }
}

fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    chars.next().map(|c| c.to_uppercase().collect::<String>() + chars.as_str()).unwrap_or_default()
}

// ── pressure, config, maintenance ──────────────────────────────────────────────────────────────────

/// Plans compiled for statements with literal values show twice on SQL Server: as compiles on almost every batch
/// (compile-pressure, the CPU) and as single-use plans filling the cache (query-family:plan-cache, the memory). One
/// cause, one fix (parameters): the plan-cache finding stays, with the compile numbers in it.
fn plan_cache_carries_compiles(_: &Snapshot, findings: &mut Vec<Finding>) {
    let Some(compiles) = findings.iter().position(|f| f.check == "compile-pressure") else { return };
    let Some(cache) = findings.iter().position(|f| f.id == "query-family:plan-cache") else { return };
    let absorbed = findings[compiles].clone();
    let f = &mut findings[cache];
    f.summary.push_str(&format!(" The CPU side of the same cause: {}.", absorbed.title.trim_end_matches('.')));
    f.metrics.extend(absorbed.metrics.into_iter().filter(|m| m.label.starts_with("Compiles")));
    f.severity = f.severity.max(absorbed.severity);
    f.impact.score = f.impact.score.max(absorbed.impact.score);
    f.verify.extend(absorbed.verify);
    findings.remove(compiles);
}

/// A table full of dead rows has had many changes since its last analyze too: the stale-stats finding for it is the
/// same backlog, and VACUUM (ANALYZE), the dead-rows fix, refreshes the statistics as well.
fn dead_rows_absorb_stale_stats(_: &Snapshot, findings: &mut Vec<Finding>) {
    let dead: Vec<String> =
        findings.iter().filter(|f| f.check == "dead-rows").map(|f| f.subject.name.to_ascii_lowercase()).collect();
    let mut absorbed: Vec<Finding> = Vec::new();
    findings.retain(|f| {
        let same = f.check == "stale-stats" && dead.contains(&f.subject.name.to_ascii_lowercase());
        if same {
            absorbed.push(f.clone());
        }
        !same
    });
    for stale in absorbed {
        let Some(f) = findings
            .iter_mut()
            .find(|f| f.check == "dead-rows" && f.subject.name.eq_ignore_ascii_case(&stale.subject.name))
        else {
            continue;
        };
        let changed = stale.metrics.iter().find(|m| m.label == "Rows changed since update").map(|m| m.value);
        f.summary.push_str(&format!(
            " Its statistics are behind too{}: VACUUM (ANALYZE) refreshes them in the same pass.",
            changed.map(|c| format!(" ({} rows changed since the last analyze)", format::count(c))).unwrap_or_default()
        ));
        f.severity = f.severity.max(stale.severity);
        f.impact.score = f.impact.score.max(stale.impact.score);
        if let Some(fix) = f.fix.as_mut()
            && !fix.sql.as_deref().is_some_and(|s| s.contains("ANALYZE"))
        {
            fix.sql = fix.sql.take().map(|s| format!("{s}\nANALYZE {};", stale.subject.name));
        }
    }
}

/// An abandoned replication slot keeps WAL (replica-lag) and, when it has an xmin, holds VACUUM back too
/// (held-cleanup): one slot, one finding, the slot's, with both effects.
fn slot_carries_held_cleanup(_: &Snapshot, findings: &mut Vec<Finding>) {
    let Some(held) =
        findings.iter().position(|f| f.check == "held-cleanup" && f.title.starts_with("Replication slot "))
    else {
        return;
    };
    let slot = findings[held].id.trim_start_matches("held-cleanup:").to_string();
    let Some(lag) = findings.iter().position(|f| f.check == "replica-lag" && f.id == format!("replica-lag:{slot}"))
    else {
        return;
    };
    let absorbed = findings[held].clone();
    let f = &mut findings[lag];
    let effect = absorbed.title.trim_start_matches(&format!("Replication slot {slot} ")).to_string();
    f.summary.push_str(&format!(
        " It also {effect}: rows deleted or updated after it can't be removed from any table, in any database."
    ));
    f.metrics.extend(absorbed.metrics.into_iter().filter(|m| m.label == "Transactions behind"));
    f.severity = f.severity.max(absorbed.severity);
    f.impact.score = f.impact.score.max(absorbed.impact.score);
    findings.remove(held);
}

/// A query that spends most of its time waiting for the application to read its results is explained by that: the
/// network-wait finding replaces the generic heavy-query one. Under half its time, the rest is its own work and the
/// heavy-query finding stays.
fn network_wait_explains_heavy_query(_: &Snapshot, findings: &mut Vec<Finding>) {
    let explained: BTreeSet<String> = findings
        .iter()
        .filter(|f| f.check == "network-wait")
        .filter(|f| f.metrics.iter().any(|m| m.label == "Waiting on the app" && m.value >= 0.5))
        .flat_map(|f| f.queries.iter().map(|q| q.id.clone()))
        .collect();
    findings.retain(|f| f.check != "heavy-query" || !f.queries.iter().any(|q| explained.contains(&q.id)));
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
