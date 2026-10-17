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
        rule("a plan's index request is the finding for its table and key", plan_request_is_the_index_finding),
        rule("a rewrite, not an index, fixes a scan behind a function or wildcard", rewrite_replaces_index),
        rule("a spill explains the big sort", spill_explains_sort),
        rule("a spill carries the result it sorts", spill_carries_its_result),
        rule("an index to add covers the foreign key", index_covers_foreign_key),
        rule("an index the engine builds for itself is the foreign key's", engine_built_index_is_the_foreign_keys),
        rule("an index to add replaces a histogram", index_replaces_histogram),
        rule("a regression carries its plan change", regression_carries_plan_change),
        rule("one plan story per query", one_plan_story),
        rule("a scan that came with a plan change carries the regression", scan_carries_its_regression),
        rule("dead rows are the table's empty space", dead_rows_explain_bloat),
        rule("updates that aren't HOT explain their WAL", cold_updates_explain_log),
        rule("large values explain the log their writes make", large_values_explain_log),
        rule("one lock episode on a table", one_lock_episode),
        rule("one blocking chain on a table", one_blocking_chain),
        rule("the plan cache finding carries the compile load", plan_cache_carries_compiles),
        rule("dead rows absorb the table's stale statistics", dead_rows_absorb_stale_stats),
        rule("a replication slot's finding carries the cleanup it holds back", slot_carries_held_cleanup),
        rule("waiting on the application explains a heavy query", network_wait_explains_heavy_query),
        rule("a heavy query another finding explains", explained_heavy_query),
        // Statements whose cost is behind them.
        rule("statements that stopped running", stopped_running),
        rule("nothing is created for statements that stopped", stopped_statements_get_nothing_created),
        rule("statements that ran once", ran_once_only),
        rule("a table that is gone has nothing to change", gone_tables_get_no_fix),
        rule("a finding whose statements stopped has nothing to check", nothing_to_check_once_stopped),
        // Wording.
        rule("names without the database", names_without_database),
        rule("a caveat repeats nothing the summary or the fix says", caveats_repeat_nothing),
    ]
}

/// SQL Server reports the same gap twice: as a missing-index suggestion and as a scan in the plan. Keep the suggestion, which
/// carries the optimizer's own estimate, and give it the plan evidence.
fn missing_index_absorbs_scan(_: &Snapshot, findings: &mut Vec<Finding>) {
    merge_scans_into_missing_indexes(findings);
}

/// SQL Server's plan asks for an index itself (`plan-insights:missing-index-hint:<table>`). Where a `missing-index`
/// finding (the server's list of requests) or a `filtered-scan` finding (the scan behind it) is about the same table and
/// leads with the same key, that finding is the one to act on, and it takes the statements; where none is (a restart
/// emptied the server's list, or its request fell under the bar), the plan's request stays a finding of its own, so a
/// request the plan makes is never hidden.
fn plan_request_is_the_index_finding(snapshot: &Snapshot, findings: &mut Vec<Finding>) {
    let Some(workload) = snapshot.workload.as_ref() else { return };
    let normal = |name: &str| name.replace(['[', ']', '"', '`'], "").to_ascii_lowercase();
    let same_table = |a: &str, b: &str| {
        let (a, b) = (normal(a), normal(b));
        a == b || ((!a.contains('.') || !b.contains('.')) && table_leaf(&a) == table_leaf(&b))
    };
    let mut covered: Vec<(usize, usize)> = Vec::new();
    for (i, f) in findings.iter().enumerate().filter(|(_, f)| f.id.starts_with("plan-insights:missing-index-hint:")) {
        // The key each statement's plan asks for on this table: its first equality column, else its first range one.
        let leads: Vec<Option<String>> = f
            .queries
            .iter()
            .map(|r| {
                let plan = workload.query(&r.id)?.plan.as_ref()?;
                let hint = plan.index_hints.iter().find(|h| same_table(&h.table, &f.subject.name))?;
                hint.equality.iter().chain(&hint.inequality).next().map(|k| normal(k))
            })
            .collect();
        let target = findings.iter().position(|m| {
            matches!(m.check.as_str(), "missing-index" | "filtered-scan")
                && same_table(&m.subject.name, &f.subject.name)
                && !leads.is_empty()
                && leads.iter().all(|lead| lead.is_some() && keys_of(&m.id).first() == lead.as_ref())
        });
        if let Some(t) = target {
            covered.push((i, t));
        }
    }
    for &(from, into) in &covered {
        let refs = findings[from].queries.clone();
        let target = &mut findings[into];
        for r in refs {
            if !target.queries.iter().any(|q| q.id == r.id) {
                target.queries.push(r);
            }
        }
    }
    let drop: BTreeSet<usize> = covered.iter().map(|(from, _)| *from).collect();
    let mut index = 0;
    findings.retain(|_| {
        let keep = !drop.contains(&index);
        index += 1;
        keep
    });
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

/// A statement that sends tens of thousands of rows a run and spills its sort or hash to temporary files: big-result
/// and temp-spill would each carry the statement's share, with two fixes. One finding, the spill's, which keeps the
/// share and the memory fix and says what the result adds (the rows each run sends, and returning fewer).
fn spill_carries_its_result(snapshot: &Snapshot, findings: &mut Vec<Finding>) {
    let Some(workload) = &snapshot.workload else { return };
    let mut i = 0;
    while i < findings.len() {
        let f = &findings[i];
        let candidate = (f.check == "big-result").then(|| f.queries.first().and_then(|r| workload.query(&r.id)));
        let Some(q) = candidate.flatten() else {
            i += 1;
            continue;
        };
        let target = findings
            .iter()
            .position(|s| s.check == "temp-spill" && matches!(s.queries.as_slice(), [r] if r.id == q.id));
        let (Some(target), Some(said)) = (target, signals::result_of_a_spill(snapshot, q)) else {
            i += 1;
            continue;
        };
        let metrics: Vec<culprit_core::Metric> =
            findings[i].metrics.iter().filter(|m| m.label != "Runs").cloned().collect();
        let s = &mut findings[target];
        s.summary.push_str(&said.summary);
        if let Some(fix) = s.fix.as_mut() {
            fix.summary.push_str(&said.fix);
        }
        s.verify.extend(said.verify);
        for m in metrics {
            if !s.metrics.iter().any(|x| x.label == m.label) {
                s.metrics.push(m);
            }
        }
        // `absorb` removed `i`: the next finding is now there.
        absorb(snapshot, findings, target, i, None);
    }
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
/// names why (a missing index, a spill, lock waits, a type mismatch…), that finding is the one to act on. It explains
/// the time only when it accounts for it: the time it accounts for in that statement is at least half the heavy
/// query's share. A finding about several statements accounts for each one's part of its share, not for the whole of
/// it: two statements' 16% doesn't explain either one's 8%. A finding that weighs no time (a table's lock
/// escalations), a part of it (an unstable query's slow runs, a few percent of a statement that runs all day) or none
/// it measured (see [`measures_no_time`]) leaves the heavy query standing, so the statement's everyday cost is still
/// said. What a plan shows explains the time only when it weighs as much: a plan warning or insight graded lower (a
/// join condition that is there after all, statistics the plan merely lacks) leaves the heavy query standing. The list
/// of forced plans ("nothing to do while they help") explains nothing.
fn explained_heavy_query(snapshot: &Snapshot, findings: &mut Vec<Finding>) {
    /// The share of a heavy query's time another finding must weigh to explain it.
    const ACCOUNTS_FOR: f64 = 0.5;
    let workload = snapshot.workload.as_ref();
    // A statement's own share of database time, weighed the way every check weighs it (over Culprit's scans where
    // they measured it).
    let own_share = |id: &str| workload.and_then(|w| w.query(id).map(|q| w.statement_share(q)));
    // Per statement: the grade of each finding that names a cause for it, and the share it accounts for there.
    let mut causes: BTreeMap<String, Vec<(Severity, f64)>> = BTreeMap::new();
    for f in findings.iter().filter(|f| {
        // An over-sized grant slows others, not the query itself; waiting on the application explains a query only
        // when it is most of its time (`network_wait_explains_heavy_query`); an unstable query's slow runs are a part
        // of its time, never what the statement costs every day.
        !matches!(
            f.check.as_str(),
            "heavy-query" | "unused-index" | "duplicate-index" | "memory-grant" | "network-wait" | "unstable-query"
        ) && !measures_no_time(f)
    }) {
        let Some(share) = f.impact.db_time_share else { continue };
        // A cause another check names explains it at any grade; a plan's remark only at the heavy query's.
        let weight =
            if matches!(f.check.as_str(), "plan-warnings" | "plan-insights") { f.severity } else { Severity::High };
        // Each statement's part of the finding's share, by its own share of database time.
        let parts: Vec<(String, f64)> = match f.queries.as_slice() {
            [one] => vec![(one.id.clone(), share)],
            refs => {
                let weights: Vec<(String, f64)> =
                    refs.iter().filter_map(|r| Some((r.id.clone(), own_share(&r.id)?))).collect();
                let total: f64 = weights.iter().map(|(_, w)| w).sum();
                if total <= 0.0 {
                    continue;
                }
                weights.into_iter().map(|(id, w)| (id, share * w / total)).collect()
            }
        };
        for (id, part) in parts {
            causes.entry(id).or_default().push((weight, part));
        }
    }
    findings.retain(|f| {
        let own = f.impact.db_time_share.unwrap_or(0.0);
        f.check != "heavy-query"
            || !f.queries.iter().any(|q| {
                causes.get(&q.id).is_some_and(|c| c.iter().any(|(w, s)| *w >= f.severity && *s >= own * ACCOUNTS_FOR))
            })
    });
}

/// A finding whose share of database time is its statements' but measures none of it: a guess at what the optimizer
/// may misjudge (a column with no histogram, statistics behind their table) or an engine's estimate of what an index
/// would save (Azure's index advice), and the list of forced plans. It never explains a heavy query away. SQL Server's
/// own plan advice stands on what it measured: the regression behind a plan to force, a forced plan's scan.
fn measures_no_time(f: &Finding) -> bool {
    matches!(f.check.as_str(), "missing-histogram" | "stale-stats")
        || (f.check == "tuning-advice"
            && !f.id.starts_with("tuning-advice:force:")
            && !f.id.starts_with("tuning-advice:forced-scan:"))
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

/// A caveat adds to what a finding says. A sentence of it that the summary or the fix already says (the same words,
/// whatever the case and the final stop) is dropped from it, and a caveat left with nothing is dropped. A caveat that
/// quotes a statement (it has a line break) is kept as it is.
#[allow(clippy::ptr_arg)] // every rule has the signature `rules` lists them by
fn caveats_repeat_nothing(_: &Snapshot, findings: &mut Vec<Finding>) {
    for f in findings.iter_mut() {
        let said: BTreeSet<String> = sentences(&f.summary)
            .into_iter()
            .chain(f.fix.iter().flat_map(|fix| sentences(&fix.summary)))
            .map(sentence_key)
            .collect();
        f.caveats = std::mem::take(&mut f.caveats)
            .into_iter()
            .filter_map(|caveat| {
                if caveat.contains('\n') {
                    return Some(caveat);
                }
                let all = sentences(&caveat);
                let kept: Vec<&str> = all.iter().copied().filter(|s| !said.contains(&sentence_key(s))).collect();
                match kept.len() {
                    0 => None,
                    n if n == all.len() => Some(caveat),
                    _ => Some(kept.join(" ")),
                }
            })
            .collect();
    }
}

/// The sentences of a text: split after a full stop, a question or an exclamation mark that a space and a capital
/// letter, a digit or a quote follow ("e.g. the" and "1.5 s" stay whole).
pub(crate) fn sentences(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    for w in chars.windows(3) {
        let [(at, end), (_, space), (_, next)] = [w[0], w[1], w[2]];
        if matches!(end, '.' | '?' | '!')
            && space == ' '
            && (next.is_uppercase() || next.is_ascii_digit() || matches!(next, '`' | '"' | '“'))
        {
            let sentence = text[start..at + 1].trim();
            if !sentence.is_empty() {
                out.push(sentence);
            }
            start = at + 1;
        }
    }
    let last = text[start..].trim();
    if !last.is_empty() {
        out.push(last);
    }
    out
}

/// A sentence as [`caveats_repeat_nothing`] compares it: lower case, spaces collapsed, no final stop.
fn sentence_key(sentence: &str) -> String {
    sentence.split_whitespace().collect::<Vec<_>>().join(" ").trim_end_matches(['.', '!', '?', ':', ';']).to_lowercase()
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
    name.rsplit('.').next().unwrap_or(name).trim_matches(['[', ']', '"', '`']).to_ascii_lowercase()
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
/// often the query ran ([`query_quiet`]: silence of 20 typical intervals, so a daily job is not mistaken for a fixed one),
/// and "stopped" is said only once that silence covers [`STOPPED_MIN_HOURS`]: a request every two hours, or a batch that
/// ran an hour ago, comes back. Returns how long it has been quiet (seconds) and when it last ran.
pub(crate) fn query_stopped(snapshot: &Snapshot, q: &QueryStat) -> Option<(f64, String)> {
    query_quiet(snapshot, q).filter(|(silence, _)| silent_long_enough(*silence))
}

/// Whether a finding on `queries` is told in the past: every one stopped running ([`query_stopped`]), the test
/// [`stopped_running`] marks the finding inactive by. Checks pick their title's verb by it ("spent", "was read").
pub(crate) fn all_stopped(snapshot: &Snapshot, queries: &[&QueryStat]) -> bool {
    !queries.is_empty() && queries.iter().all(|q| query_stopped(snapshot, q).is_some())
}

/// Hours of silence before "stopped" is said, of a statement or of the requests a statement made (a missing index):
/// the same 20 hours a sentence needs to say "a day" ([`culprit_core::PER_DAY_MIN_HOURS`]). Under that, a finding says
/// how long it has been quiet and stays active.
pub(crate) const STOPPED_MIN_HOURS: f64 = culprit_core::PER_DAY_MIN_HOURS;

/// Whether a silence of `seconds` before the scan's data ends is long enough to say "stopped" ([`STOPPED_MIN_HOURS`]).
pub(crate) fn silent_long_enough(seconds: f64) -> bool {
    seconds >= STOPPED_MIN_HOURS * 3600.0
}

/// Silence of 20 typical intervals (at least an hour) before the window ends, out of the query's own rhythm (a burst
/// that began inside the window, a gap it already came back from); how long ([`query_stopped`]) decides what it means.
/// Under [`STOPPED_MIN_HOURS`], a finding stays active and says how long it has been quiet ([`stopped_running`]).
fn query_quiet(snapshot: &Snapshot, q: &QueryStat) -> Option<(f64, String)> {
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

/// Once `q` stopped (see `query_stopped`), the statement that may have replaced it: one still running that writes the
/// same table the same way (an insert into it, for an insert), and wasn't already running before `q` began; the one
/// that runs most. Only for writes: many statements read a table, and none of them says it took a read's place.
pub(crate) fn query_successor<'a>(snapshot: &'a Snapshot, q: &QueryStat) -> Option<&'a QueryStat> {
    let engine = snapshot.server.engine;
    let table = support::write_targets(q, engine)
        .into_iter()
        .find_map(|o| snapshot.tables.iter().find(|t| support::names_table(&o, &t.schema, &t.name)))?;
    let verb = support::first_verb(support::main_statement(&q.text)).map(|(_, v)| v)?;
    support::app_queries(snapshot)
        .filter(|o| o.id != q.id && o.executions > 0.0 && !query_is_history(snapshot, o))
        .filter(|o| support::first_verb(support::main_statement(&o.text)).is_some_and(|(_, v)| v == verb))
        .filter(|o| support::writes_table(o, engine, &table.schema, &table.name))
        // One that was already running before this one began didn't replace it.
        .filter(|o| match (o.first_seen.as_deref(), q.first_seen.as_deref()) {
            (Some(theirs), Some(ours)) => {
                support::parse_epoch_seconds(theirs).zip(support::parse_epoch_seconds(ours)).is_none_or(|(a, b)| a >= b)
            }
            _ => true,
        })
        .max_by(|a, b| a.executions.total_cmp(&b.executions))
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

/// Findings about queries that all stopped running (see `query_stopped`) are kept, Low, ranked low and labelled, as a
/// single run is (`ran_once_only`): a deploy most likely changed or fixed them. Every finding marked history (inactive)
/// is Low, whichever check marked it. A finding about one statement names what may have replaced it
/// ([`query_successor`]): the version of the code to check is that one.
#[allow(clippy::ptr_arg)] // every rule has the signature `rules` lists them by
fn stopped_running(snapshot: &Snapshot, findings: &mut Vec<Finding>) {
    // A check that marked its own finding history (an index request no statement made since, advice for a statement
    // that stopped) grades it as this rule does.
    for f in findings.iter_mut().filter(|f| f.inactive) {
        f.severity = Severity::Low;
    }
    let Some(workload) = &snapshot.workload else { return };
    for f in findings.iter_mut().filter(|f| about_queries(f)) {
        let queries: Vec<&QueryStat> = f.queries.iter().filter_map(|r| workload.query(&r.id)).collect();
        f.last_seen = queries.iter().filter_map(|q| q.last_seen.clone()).max_by(|a, b| {
            support::parse_epoch_seconds(a)
                .partial_cmp(&support::parse_epoch_seconds(b))
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        let one = queries.len() == 1;
        let silences: Vec<(f64, String)> = queries.iter().filter_map(|q| query_stopped(snapshot, q)).collect();
        if queries.is_empty() {
            continue;
        }
        if silences.len() < queries.len() {
            // Quiet for less than a day: what the data shows, and the finding stays active.
            if let Some(caveat) = quiet_lately_caveat(snapshot, workload, &queries) {
                f.caveats.insert(0, caveat);
            }
            continue;
        }
        let (silence, last) = silences.into_iter().min_by(|a, b| a.0.total_cmp(&b.0)).expect("non-empty");
        f.inactive = true;
        // History, graded as a single run is: what it cost is behind it.
        f.severity = Severity::Low;
        f.impact.score *= 0.25;
        let successor = queries
            .first()
            .filter(|_| one)
            .and_then(|q| query_successor(snapshot, q))
            .map(|next| {
                format!(
                    " Query {}, {} too, still runs and may be what replaced it: check that one.",
                    next.id,
                    support::query_handle(next)
                )
            })
            .unwrap_or_default();
        let caveat = if last_ran_by_first_scan(workload, &last) {
            format!(
                "{} run in the {} since Culprit's first scan ({}); {} may have stopped earlier. A deploy before that scan may have changed or fixed {}, at a time the counters don't show; check that the current version of this code doesn't have the same problem.",
                if one { "This query hasn't" } else { "None of these queries has" },
                format::duration(silence * 1000.0),
                crate::queries::short_when(&last),
                if one { "it" } else { "they" },
                if one { "it" } else { "them" }
            )
        } else {
            format!(
                "{} run since {} ({} before this scan's data ends). A deploy may have changed or fixed {}; check that the current version of this code doesn't have the same problem.",
                if one { "This query has not" } else { "None of these queries has" },
                last.get(..16).unwrap_or(&last).replace('T', " "),
                format::duration(silence * 1000.0),
                if one { "it" } else { "them" }
            )
        };
        f.caveats.insert(0, caveat + &successor);
    }
}

/// The verify step of every finding whose statements stopped running (`stopped_running`, or a check that saw them stop:
/// a recommendation for a query Query Store last ran days ago), for every check, in place of its own, which is for
/// statements that run and promises a change no later scan shows: nothing to check unless they run again. One that
/// already says there is nothing to check (a table that is gone, index requests that stopped) keeps its own.
#[allow(clippy::ptr_arg)] // every rule has the signature `rules` lists them by
fn nothing_to_check_once_stopped(_: &Snapshot, findings: &mut Vec<Finding>) {
    for f in findings.iter_mut().filter(|f| f.inactive) {
        if f.verify
            .first()
            .is_some_and(|v| v.starts_with("Nothing to check") && (v.contains("unless") || v.contains(" gone")))
        {
            continue;
        }
        f.verify = vec![if f.queries.is_empty() && f.check == "missing-index" {
            "Nothing to check unless the optimizer asks for this index again within a scan's window."
        } else if f.queries.len() <= 1 {
            "Nothing to check unless it runs again: a later scan that sees it run lists this finding as current again."
        } else {
            "Nothing to check unless they run again: a later scan that sees one of them run lists this finding as current again."
        }
        .to_string()];
    }
}

/// The catalog and statements agree that their tables are history. Apply this to every query finding, including
/// writes whose log volume outlives the table, rather than leaving each check to propose work against a dropped table.
#[allow(clippy::ptr_arg)]
fn gone_tables_get_no_fix(snapshot: &Snapshot, findings: &mut Vec<Finding>) {
    let Some(workload) = &snapshot.workload else { return };
    for f in findings.iter_mut().filter(|f| about_queries(f)) {
        let statements: Vec<_> = f.queries.iter().filter_map(|r| workload.query(&r.id)).collect();
        // A finding's subject can be a column (customers.email). Only the statement's actual objects establish its
        // tables. For a write, the target is what the fix would change, not tables it only reads for a join or a key.
        let tables: BTreeSet<String> = statements
            .iter()
            .flat_map(|q| {
                let targets = support::write_targets(q, snapshot.server.engine);
                if !targets.is_empty() {
                    return targets;
                }
                q.plan.iter().flat_map(|p| p.nodes()).filter_map(|n| n.object.clone()).collect()
            })
            .collect();
        if tables.is_empty()
            || !tables.iter().all(|t| crate::indexes::table_gone(snapshot, &table_leaf(t), &statements))
        {
            continue;
        }
        f.severity = Severity::Low;
        f.fix = Some(culprit_core::Fix {
            summary: format!(
                "Nothing to change: {} {} in the database's catalog any more, so {} dropped or renamed since these statements ran. If the application now uses other tables, scan those before making a change.",
                format::list(&tables.iter().cloned().collect::<Vec<_>>()),
                if tables.len() == 1 { "isn't" } else { "aren't" },
                if tables.len() == 1 { "it was" } else { "they were" }
            ),
            sql: None,
            cost: None,
        });
        f.verify = vec!["Nothing to check: the tables are gone. If these statements return against replacement tables, scan those to see what needs a change.".into()];
    }
}

/// A finding on statements that stopped running (see [`stopped_running`]) offers nothing to create for them: an index
/// or statistics made for code that no longer runs serve nothing and cost every write. Its fix says so and names what
/// may have replaced the statement. SQL that creates nothing (a setting that has the next runs logged, an event session
/// that catches what runs in its place, unforcing a plan) stays. One rule for every check; missing-index says it in its
/// own words already.
#[allow(clippy::ptr_arg)] // every rule has the signature `rules` lists them by
fn stopped_statements_get_nothing_created(snapshot: &Snapshot, findings: &mut Vec<Finding>) {
    let Some(workload) = &snapshot.workload else { return };
    for f in findings.iter_mut().filter(|f| f.inactive) {
        if !f.fix.as_ref().and_then(|fix| fix.sql.as_deref()).is_some_and(creates_something) {
            continue;
        }
        let queries: Vec<&QueryStat> = f.queries.iter().filter_map(|r| workload.query(&r.id)).collect();
        let one = queries.len() <= 1;
        let successor = queries
            .first()
            .filter(|_| queries.len() == 1)
            .and_then(|q| query_successor(snapshot, q))
            .map(|next| {
                format!(" If query {}, {} too, took its place, look at that one.", next.id, support::query_handle(next))
            })
            .unwrap_or_default();
        f.fix = Some(culprit_core::Fix {
            summary: format!(
                "Nothing to create: {} stopped running, so an index or statistics made for {} now would serve code \
                 that no longer runs. If {} again, a scan shows this finding again with the SQL for {}.{successor}",
                if one { "the statement behind this finding" } else { "the statements behind this finding" },
                if one { "it" } else { "them" },
                if one { "it runs" } else { "they run" },
                if one { "it" } else { "them" },
            ),
            sql: None,
            cost: None,
        });
    }
}

/// Whether fix SQL creates something in the database: an index or statistics (`CREATE …`, `ALTER TABLE … ADD`, an
/// index rebuilt with more columns). An event session (`CREATE EVENT SESSION`) only watches, and a setting or a plan
/// forced creates no object. Comments don't count.
pub(crate) fn creates_something(sql: &str) -> bool {
    let text: Vec<&str> = sql.lines().map(|line| line.split("--").next().unwrap_or("")).collect();
    let words: Vec<String> = text.join(" ").split_whitespace().map(|w| w.to_ascii_lowercase()).collect();
    words.windows(2).any(|w| match (w[0].as_str(), w[1].as_str()) {
        ("create", next) => next != "event",
        ("alter", "table") | ("alter", "index") => {
            words.iter().any(|w| w == "add" || w == "rebuild" || w.starts_with("include"))
        }
        _ => false,
    })
}

/// For a finding whose statements are all quiet out of their rhythm, at least one for less than [`STOPPED_MIN_HOURS`]:
/// how long the data has been without them, and that it is too soon to say they stopped. None when one still runs.
fn quiet_lately_caveat(
    snapshot: &Snapshot,
    workload: &culprit_core::Workload,
    queries: &[&QueryStat],
) -> Option<String> {
    let quiet: Vec<(f64, String)> = queries.iter().filter_map(|q| query_quiet(snapshot, q)).collect();
    if quiet.len() < queries.len() {
        return None;
    }
    let (silence, last) = quiet.into_iter().min_by(|a, b| a.0.total_cmp(&b.0))?;
    let one = queries.len() == 1;
    let span = format::duration(silence * 1000.0);
    Some(if last_ran_by_first_scan(workload, &last) {
        format!(
            "{} run in the {span} since Culprit's first scan ({}): too short a time to say {} stopped.",
            if one { "This query hasn't" } else { "None of these queries has" },
            crate::queries::short_when(&last),
            if one { "it" } else { "they" },
        )
    } else {
        format!(
            "{} run in the last {span} of this scan's data (since {}): too short a time to say {} stopped.",
            if one { "This query hasn't" } else { "None of these queries has" },
            crate::queries::short_when(&last),
            if one { "it" } else { "they" },
        )
    })
}

/// Whether a statement's last run is only the oldest of Culprit's scans that tracked it. A source that keeps no last
/// run (pg_stat_statements) is dated by the scans: one whose count hasn't grown since the first of them last ran by
/// then at the latest, and may have stopped long before (see `culprit_engine::trends::last_seen_from_scans`).
pub(crate) fn last_ran_by_first_scan(workload: &culprit_core::Workload, last: &str) -> bool {
    let first = workload.timeline.as_ref().and_then(|t| t.starts.first()).and_then(|s| support::parse_epoch_seconds(s));
    !workload.windowed
        && first.zip(support::parse_epoch_seconds(last)).is_some_and(|(first, last)| (first - last).abs() < 1.0)
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

/// A statement whose plan changed for the worse, and whose current plan reads a table for want of an index: the
/// regression (plan-regression, or a regression a new plan started with) and the scan's finding (filtered-scan, or
/// the missing-index request that absorbed it) are one cause, with two fixes (force the old plan, add the index), where
/// what each plan read says the scan came with the change. The scan's finding stays, with the index; the regression's
/// timeline joins it as its first sentence, with what each plan read.
fn scan_carries_its_regression(snapshot: &Snapshot, findings: &mut Vec<Finding>) {
    let Some(workload) = &snapshot.workload else { return };
    let mut i = 0;
    while i < findings.len() {
        let f = &findings[i];
        let regressed = f.check == "plan-regression"
            || (f.check == "regression" && f.metrics.iter().any(|m| m.label == crate::queries::PLAN_CHANGE));
        let Some(q) = regressed.then(|| f.queries.first().and_then(|r| workload.query(&r.id))).flatten() else {
            i += 1;
            continue;
        };
        // The scan is in the plan that runs now: the finding points at its step.
        let target = findings.iter().position(|s| {
            matches!(s.check.as_str(), "filtered-scan" | "missing-index")
                && s.queries.iter().any(|r| r.id == q.id && r.plan_node.is_some())
        });
        // One cause only where the reads say the scan came with the plan change (the plan before read far less): a
        // statement whose plans both read its table whole (no index was ever there) got slower for another reason,
        // which its regression's own finding tells.
        let came = plans_read(q, query_is_history(snapshot, q));
        let Some(target) = target.filter(|_| !came.is_empty()) else {
            i += 1;
            continue;
        };
        let source = findings[i].clone();
        let first = source.summary.split_once(". ").map_or(source.summary.as_str(), |(first, _)| first);
        let first = first.trim_end_matches('.');
        let s = &mut findings[target];
        let first = if s.queries.len() > 1 {
            first.replacen("this query", &format!("query {}", q.id), 1)
        } else {
            first.to_string()
        };
        let sentence = match first.strip_prefix("Until ") {
            Some(rest) => format!("Query Store's timeline: until {rest}."),
            None => format!("{first}."),
        };
        s.summary = format!("{} {sentence}{came}", s.summary.trim_end());
        for m in source.metrics {
            if !s.metrics.iter().any(|x| x.label == m.label) {
                s.metrics.push(m);
            }
        }
        absorb(snapshot, findings, target, i, None);
    }
}

/// What the plan a statement ran fast with read a run, against the plan it runs now, where the source counted reads
/// per plan: " The scan came with the plan change: plan 276 read about 3 pages a run, plan 282 reads about 5.7K." For a
/// statement that no longer runs (`history`), its last plan "read" too.
fn plans_read(q: &QueryStat, history: bool) -> String {
    let per_run =
        |p: &culprit_core::PlanVariant| p.logical_reads.filter(|_| p.executions > 0.0).map(|r| r / p.executions);
    let now = q
        .plans
        .iter()
        .find(|p| q.plan_id.as_deref() == Some(p.id.as_str()))
        .or_else(|| q.plans.iter().max_by(|a, b| a.last_seen.cmp(&b.last_seen)));
    let Some(now) = now else { return String::new() };
    let fast = q.plans.iter().filter(|p| p.id != now.id).min_by(|a, b| a.mean_time_ms.total_cmp(&b.mean_time_ms));
    match (fast.and_then(|p| Some((p, per_run(p)?))), per_run(now)) {
        (Some((fast, before)), Some(after)) if after > before * 2.0 => format!(
            " The scan came with the plan change: plan {} read about {} pages a run, plan {} {} about {}.",
            fast.id,
            format::count(before),
            now.id,
            if history { "read" } else { "reads" },
            format::count(after)
        ),
        _ => String::new(),
    }
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
    // A finding's share counts every statement it lists: where each finding's share is its own statements' (or it has
    // none), the one left is the share of all the statements it lists, not the larger of the two.
    let of_listed = |f: &Finding| f.impact.db_time_share.is_none() || share_of_listed(snapshot, f).is_some();
    let some_share = findings[into].impact.db_time_share.is_some() || gone.impact.db_time_share.is_some();
    let listed_share = (some_share && of_listed(&findings[into]) && of_listed(&gone))
        .then(|| {
            let workload = snapshot.workload.as_ref()?;
            let ids: BTreeSet<&str> =
                findings[into].queries.iter().chain(&gone.queries).map(|r| r.id.as_str()).collect();
            let all: Vec<&QueryStat> = ids.iter().filter_map(|id| workload.query(id)).collect();
            support::query_impact(snapshot, &all).map(|i| i.share)
        })
        .flatten();
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
    let before = target.impact.db_time_share;
    target.impact.db_time_share = match (target.impact.db_time_share, gone.impact.db_time_share) {
        (Some(a), Some(b)) => Some(listed_share.unwrap_or(a.max(b))),
        (None, None) => None,
        (a, b) => listed_share.or(a).or(b),
    };
    if let Some(share) = listed_share {
        target.impact.score = target.impact.score.max(support::score_for_share(share));
        // A label that said the old share says the new one.
        if before.is_some_and(|b| target.impact.label == support::share_label(b)) {
            target.impact.label = support::share_label(share);
        }
    }
    target.impact.time_ms_per_day = target.impact.time_ms_per_day.or(gone.impact.time_ms_per_day);
    target.last_seen = target.last_seen.clone().max(gone.last_seen.clone());
    if !history {
        target.severity = target.severity.max(gone.severity);
    }
    findings.remove(from);
}

/// A finding's share of database time when it is the share of the statements it lists, as the check gave it (its
/// statements' own `query_impact`): `None` for a share of something else (a scan's part of a plan, a capped or
/// recomputed figure) or a finding without one.
fn share_of_listed(snapshot: &Snapshot, f: &Finding) -> Option<f64> {
    let workload = snapshot.workload.as_ref()?;
    let share = f.impact.db_time_share?;
    let listed: Vec<&QueryStat> = f.queries.iter().filter_map(|r| workload.query(&r.id)).collect();
    let theirs = support::query_impact(snapshot, &listed)?.share;
    ((theirs - share).abs() <= 0.0005).then_some(share)
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

/// Inserting or updating large values logs them in full: a log-volume finding about a write to a large-rows table that
/// logs more than a page a row is that table's large values (log-volume already gave large-rows' fix for it), whether
/// it sets all of them (an insert, about the average row) or some (an update of a few columns, a fraction of it). One
/// finding, large-rows, with the statement among those it lists. Once one joins, large-rows says what every statement
/// it lists does: how often they run, and the log each one that writes the table made, in one sentence.
fn large_values_explain_log(snapshot: &Snapshot, findings: &mut Vec<Finding>) {
    /// Log a row past which the row's large values are what is logged.
    const LARGE_VALUES_PER_ROW: f64 = 8_192.0;
    let Some(workload) = &snapshot.workload else { return };
    // The large-rows findings a writer joined, by id.
    let mut joined: BTreeSet<String> = BTreeSet::new();
    let mut i = 0;
    while i < findings.len() {
        let f = &findings[i];
        let candidate = (f.check == "log-volume")
            .then(|| f.queries.first().and_then(|r| workload.query(&r.id)))
            .flatten()
            .and_then(|q| Some((written_table(q)?, metric(f, "Per row")?)))
            .filter(|(_, per_row)| *per_row >= LARGE_VALUES_PER_ROW);
        let Some(((_, table), _)) = candidate else {
            i += 1;
            continue;
        };
        let target = findings.iter().position(|l| l.check == "large-rows" && leaf(&finding_table(l)) == table);
        let Some(target) = target else {
            i += 1;
            continue;
        };
        joined.insert(findings[target].id.clone());
        absorb(snapshot, findings, target, i, None);
    }
    for l in findings.iter_mut().filter(|l| joined.contains(&l.id)) {
        let listed: Vec<&QueryStat> = l.queries.iter().filter_map(|r| workload.query(&r.id)).collect();
        l.summary = statements_below(snapshot, &l.summary, &listed);
        let table = leaf(&finding_table(l)).to_string();
        let writers: Vec<(&QueryStat, &'static str, f64)> = listed
            .iter()
            .filter_map(|q| {
                let (verb, written) = written_table(q)?;
                let bytes = q.costs.as_ref()?.log_bytes.filter(|b| *b > 0.0)?;
                (written == table).then_some((*q, verb, bytes))
            })
            .collect();
        let Some(said) = log_of_writers(snapshot, &writers) else { continue };
        l.summary.push_str(&said);
        let total: f64 = writers.iter().map(|(_, _, bytes)| bytes).sum();
        match l.metrics.iter_mut().find(|m| m.label == "Log written by its writes") {
            Some(m) => m.value = total,
            None => {
                l.metrics.push(culprit_core::Metric::new("Log written by its writes", total, culprit_core::Unit::Bytes))
            }
        }
        for (q, _, bytes) in &writers {
            if let Some(r) = l.queries.iter_mut().find(|r| r.id == q.id && r.note.is_none()) {
                r.note = Some(match per_row(q, *bytes) {
                    Some(b) => format!("Writes about {} of {} a row.", format::bytes(b), log_name(snapshot)),
                    None => format!("Wrote {} of {}.", format::bytes(*bytes), log_name(snapshot)),
                });
            }
        }
    }
}

/// What the engine calls its log, as a sentence names it.
fn log_name(snapshot: &Snapshot) -> &'static str {
    if snapshot.server.engine == Engine::Postgres { "WAL" } else { "transaction log" }
}

/// Log a statement wrote for each row it changed, when it counted them.
fn per_row(q: &QueryStat, bytes: f64) -> Option<f64> {
    q.rows.filter(|r| *r > 0.0).map(|rows| bytes / rows)
}

/// The log the writes of a large-rows table made, said once for all of them: one writer by its handle, several by what
/// they are ("77.2 MB by an insert (about 412 KB a row) and 15.3 MB by 2 updates (about 90.8 KB a row)").
fn log_of_writers(snapshot: &Snapshot, writers: &[(&QueryStat, &'static str, f64)]) -> Option<String> {
    const IN_FULL: &str = "the large values are logged in full, so batching the writes or dropping indexes won't shrink it; smaller or fewer values will.";
    let log = log_name(snapshot);
    let a_row = |bytes: f64, rows: Option<f64>| {
        rows.filter(|r| *r > 0.0).map(|r| format!("about {} a row", format::bytes(bytes / r)))
    };
    if let [(q, _, bytes)] = writers {
        return Some(format!(
            " {} wrote {} of {log} for them{}: {IN_FULL}",
            capitalize(&support::query_handle(q)),
            format::bytes(*bytes),
            a_row(*bytes, q.rows).map(|r| format!(", {r}")).unwrap_or_default()
        ));
    }
    // By kind of write, the kind that logged most first; a row count only where every one of them counted its rows.
    let mut kinds: Vec<(&'static str, usize, f64, Option<f64>)> = Vec::new();
    for (q, verb, bytes) in writers {
        match kinds.iter_mut().find(|(v, ..)| v == verb) {
            Some((_, n, b, rows)) => {
                *n += 1;
                *b += bytes;
                *rows = rows.zip(q.rows).map(|(a, r)| a + r);
            }
            None => kinds.push((verb, 1, *bytes, q.rows)),
        }
    }
    kinds.sort_by(|a, b| b.2.total_cmp(&a.2));
    let total: f64 = kinds.iter().map(|(_, _, b, _)| b).sum();
    let noun = |verb: &str, n: usize| match (verb, n) {
        ("insert", 1) => "an insert".to_string(),
        ("update", 1) => "an update".to_string(),
        (verb, 1) => format!("a {verb}"),
        (verb, n) => format!("{n} {verb}s"),
    };
    Some(match kinds.as_slice() {
        [] => return None,
        // Several writes of one kind (one alone is said by its handle above).
        [(verb, n, bytes, rows)] => format!(
            " The {n} {verb}s below logged {} of {log} for them{}: {IN_FULL}",
            format::bytes(*bytes),
            a_row(*bytes, *rows).map(|r| format!(", {r}")).unwrap_or_default()
        ),
        _ => {
            let parts: Vec<String> = kinds
                .iter()
                .map(|(verb, n, bytes, rows)| {
                    format!(
                        "{} by {}{}",
                        format::bytes(*bytes),
                        noun(verb, *n),
                        a_row(*bytes, *rows).map(|r| format!(" ({r})")).unwrap_or_default()
                    )
                })
                .collect();
            format!(
                " The {} statements below that write them logged {} of {log}: {}. {}",
                writers.len(),
                format::bytes(total),
                format::list(&parts),
                capitalize(IN_FULL)
            )
        }
    })
}

/// large-rows' sentence about the statements that read or write whole rows ("The 3 statements below read or write
/// them 696 times in 2 hours, going through about 294 pages (2.3 MB) a run."), said again of every statement it lists
/// once a writer joined them, so the count is of the statements below ([`signals::statements_below`]).
fn statements_below(snapshot: &Snapshot, summary: &str, listed: &[&QueryStat]) -> String {
    let start = summary.find(" The statement below ").or_else(|| {
        summary.match_indices(" The ").map(|(at, _)| at).find(|at| {
            summary[at + " The ".len()..]
                .split_once(" statements below ")
                .is_some_and(|(n, _)| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()))
        })
    });
    let Some(start) = start else { return summary.to_string() };
    let Some(end) = summary[start..].find(" a run.").map(|at| start + at + " a run.".len()) else {
        return summary.to_string();
    };
    if listed.is_empty() {
        return summary.to_string();
    }
    format!("{}{}{}", &summary[..start], signals::statements_below(snapshot, listed), &summary[end..])
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

/// Statements that waited on locks on the same table in the same hours (their runs' first to last overlap) waited in one
/// episode of blocking, behind the same holder or each other, and the fix is the same: one finding, the one that waited
/// longest, lists them all and says what each waited.
fn one_lock_episode(snapshot: &Snapshot, findings: &mut Vec<Finding>) {
    let Some(workload) = snapshot.workload.as_ref() else { return };
    let statement = |f: &Finding| workload.query(&f.queries.first()?.id);
    let span = |q: &QueryStat| {
        Some((
            support::parse_epoch_seconds(q.first_seen.as_deref()?)?,
            support::parse_epoch_seconds(q.last_seen.as_deref()?)?,
        ))
    };
    // The table both statements touch, as the catalog names it.
    let shared_table = |a: &QueryStat, b: &QueryStat| {
        snapshot.tables.iter().find_map(|t| {
            let touching = support::queries_touching(snapshot, &t.schema, &t.name);
            (touching.iter().any(|q| q.id == a.id) && touching.iter().any(|q| q.id == b.id))
                .then(|| format!("{}.{}", t.schema, t.name))
        })
    };
    let mut i = 0;
    while i < findings.len() {
        if findings[i].check != "lock-time" {
            i += 1;
            continue;
        }
        let Some((a, (first, last))) = statement(&findings[i]).and_then(|q| Some((q, span(q)?))) else {
            i += 1;
            continue;
        };
        let episode = findings.iter().enumerate().find_map(|(j, g)| {
            if j == i || g.check != "lock-time" {
                return None;
            }
            let b = statement(g)?;
            let (b_first, b_last) = span(b)?;
            (b_first <= last && first <= b_last).then_some(())?;
            Some((j, b, shared_table(a, b)?))
        });
        let Some((j, b, table)) = episode else {
            i += 1;
            continue;
        };
        // The one that waited longest keeps the finding.
        let (into, from, gone) =
            if metric(&findings[i], "Lock wait") >= metric(&findings[j], "Lock wait") { (i, j, b) } else { (j, i, a) };
        let lock = metric(&findings[from], "Lock wait").unwrap_or(0.0);
        let share = metric(&findings[from], "Share of its time").map(|s| {
            if s >= 0.97 { "nearly all of its time".to_string() } else { format!("{} of its time", format::percent(s)) }
        });
        let waited =
            format!("{} {}", format::duration(lock), share.as_ref().map(|s| format!("({s})")).unwrap_or_default());
        // When it waited, where its runs' span lies inside the data's window; a span that reaches back before it (a
        // digest first seen before its counters began) is said as the window's.
        let window_start = workload
            .window_end
            .as_deref()
            .and_then(support::parse_epoch_seconds)
            .map(|end| end - workload.window_hours * 3600.0);
        let when = match (gone.first_seen.as_deref(), gone.last_seen.as_deref(), span(gone), window_start) {
            (Some(first), Some(last), Some((from, _)), Some(start)) if from >= start => {
                culprit_core::span_when(first, last)
            }
            _ => "in the same window".to_string(),
        };
        findings[into].summary.push_str(&format!(
            " {} (query {}) waited on locks on {table} {when} too, {}: likely the same episode of blocking, behind the same holder or one another.",
            capitalize(&support::query_handle(gone)),
            gone.id,
            waited.trim_end()
        ));
        // What the absorbed one's fix adds for its own statement (a read that row versioning would spare) stays said.
        let versioning = findings[from]
            .fix
            .as_ref()
            .is_some_and(|f| f.summary.starts_with("This is a read, and READ_COMMITTED_SNAPSHOT is off"));
        let stopped = query_stopped(snapshot, gone).is_some();
        if versioning && let Some(fix) = findings[into].fix.as_mut() {
            fix.summary.push_str(&format!(
                " Query {}, a read, {} for writes on the same table while READ_COMMITTED_SNAPSHOT is off: with row versioning on (config:read-committed-snapshot has the change), it {} the last committed version of the rows instead.",
                gone.id,
                if stopped { "waited" } else { "waits" },
                if stopped { "would have read" } else { "reads" }
            ));
        }
        // The metrics stay the first statement's, as its title says them; the other's are in the sentence and its note.
        let note =
            format!("Waited {} on locks on {table} in the same hours; the holder isn't recorded.", waited.trim_end());
        absorb(snapshot, findings, into, from, Some(note));
        let target = if from < into { into - 1 } else { into };
        let lock: f64 =
            findings[target].queries.iter().filter_map(|r| workload.query(&r.id)).map(signals::lock_ms).sum();
        findings[target].impact.label = format!("{} waiting on locks", format::duration(lock));
        if from < i {
            i -= 1;
        }
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
        let changed =
            stale.metrics.iter().find(|m| m.label == crate::checks::maintenance::CHANGES_SINCE_UPDATE).map(|m| m.value);
        f.summary.push_str(&format!(
            " Its statistics are behind too{}: VACUUM (ANALYZE) refreshes them in the same pass.",
            changed.map(|c| format!(" ({} changes since the last analyze)", format::count(c))).unwrap_or_default()
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
mod tests {
    use super::*;
    use crate::support::{finding, subject};
    use culprit_core::{Category, QueryRef};

    fn fixture(name: &str) -> Snapshot {
        let text =
            std::fs::read_to_string(format!("{}/../../fixtures/{name}", env!("CARGO_MANIFEST_DIR"))).expect("fixture");
        serde_json::from_str(&text).expect("snapshot")
    }

    fn query<'a>(s: &'a mut Snapshot, id: &str) -> &'a mut QueryStat {
        s.workload.as_mut().expect("workload").queries.iter_mut().find(|q| q.id == id).expect("query")
    }

    /// A finding of `check` about the statements `ids`, weighing `share` of database time.
    fn about(check: &str, key: &str, ids: &[&str], share: f64) -> Finding {
        let mut f = finding(check, key, Category::Queries, subject(SubjectKind::Query, key));
        f.impact.db_time_share = Some(share);
        f.queries = ids.iter().map(|id| QueryRef { id: id.to_string(), plan_node: None, note: None }).collect();
        f
    }

    // The fresh MySQL reading: missing-histogram's guess about two statements' estimates (16% of database time, the
    // two together) took away both heavy queries (8.5% and 7.9%), the planted scan with them. A finding explains a
    // heavy query only by the time it measured in that statement.
    #[test]
    fn a_heavy_query_is_explained_only_by_the_time_a_finding_measured_in_it() {
        let s = fixture("mysql-demo.json");
        let w = s.workload.as_ref().unwrap();
        let ids = ["19bd4ff468839ea3", "c7b5ac9bb4bcecfe"];
        let shares: Vec<f64> =
            ids.iter().map(|id| w.query(id).map(|q| w.share_of(q.total_time_ms, q)).unwrap()).collect();
        let (a, b) = (shares[0], shares[1]);
        let heavy = |at: usize| about("heavy-query", ids[at], &ids[at..=at], shares[at]);
        let left = |cause: Finding| {
            let mut findings = vec![heavy(0), heavy(1), cause];
            explained_heavy_query(&s, &mut findings);
            findings.into_iter().filter(|f| f.check == "heavy-query").map(|f| f.id).collect::<Vec<_>>()
        };
        // A guess at estimates measures no time: both stay.
        assert_eq!(left(about("missing-histogram", "shop.orders", &ids, a + b)).len(), 2);
        assert_eq!(left(about("stale-stats", "shop.orders", &ids, a + b)).len(), 2);
        assert_eq!(left(about("tuning-advice", "create-index:shop.orders(promo_code)", &ids, a + b)).len(), 2);
        // A scan both statements make is each one's time: both go.
        assert!(left(about("filtered-scan", "shop.orders(promo_code)", &ids, a + b)).is_empty());
        // Waits that are a third of their time together are a third of each one's: under half, both stay, though
        // the total passes half of either.
        assert!((a + b) / 3.0 >= a.min(b) * 0.5, "{a} {b}");
        assert_eq!(left(about("hot-rows", "shop.orders", &ids, (a + b) / 3.0)).len(), 2);
        // SQL Server's plan to force stands on the regression it measured.
        let force = about("tuning-advice", &format!("force:{}", ids[0]), &ids[..1], a);
        assert_eq!(left(force), vec![format!("heavy-query:{}", ids[1])]);
    }

    // The PostgreSQL demo: the sort that spills and the result it returns are one statement, one share: big-result
    // and temp-spill each said 45% of database time, with two fixes.
    #[test]
    fn a_spill_carries_the_big_result_it_sorts() {
        let report = crate::analyze(fixture("postgres-demo.json"));
        let id = "9065391607317686058";
        assert!(!report.findings.iter().any(|f| f.id == format!("big-result:{id}")));
        let spill = report.findings.iter().find(|f| f.id == format!("temp-spill:{id}")).expect("the spill");
        assert!(
            spill.summary.contains(" Each run also sends 247K rows (≈ 17.4 MB) to the application"),
            "{}",
            spill.summary
        );
        let fix = &spill.fix.as_ref().unwrap().summary;
        assert!(
            fix.contains(
                "Where the application doesn't use every row it receives, return only what's used: page the results"
            ),
            "{fix}"
        );
        // The spill's step names the two scans to compare; the rows' step reads its rows between them.
        assert!(
            spill.verify.iter().any(|v| v
                == "If it now returns fewer rows, the rows it returns per run between those two scans should drop to what a page or a job actually uses."),
            "{:?}",
            spill.verify
        );
        assert!(spill.metrics.iter().any(|m| m.label == "Rows per run"));
        assert_eq!(report.findings.iter().filter(|f| f.queries.iter().any(|q| q.id == id)).count(), 1);
    }

    // The fresh SQL Server reading: plan-regression:244 said to force the fast plan and filtered-scan to create an
    // index, for one statement whose plan began scanning when the index it sought went away.
    #[test]
    fn a_scan_that_came_with_a_plan_change_carries_the_regression() {
        let mut s = fixture("mssql-demo.json");
        let q = query(&mut s, "8");
        q.plan_id = Some("47".into());
        for p in q.plans.iter_mut() {
            p.logical_reads = Some(p.executions * if p.id == "47" { 5_700.0 } else { 3.0 });
        }
        let mut scan = about("filtered-scan", "shop.orders(shipped_at)", &["8"], 0.12);
        scan.queries[0].plan_node = Some(2);
        scan.summary = "One query plans to read every row of shop.orders.".into();
        let mut regression = about("plan-regression", "8", &["8"], 0.12);
        regression.summary = "Query Store saw this query run with plan 8 at 9.5 ms on average until Oct 11, 10:44 UTC, and with plan 47 at 32 ms since Oct 11, 10:46 UTC (93 runs). The plan choice changed, typically after a statistics update.".into();
        regression.metrics.push(culprit_core::Metric::new("Fast plan", 9.5, culprit_core::Unit::Ms));
        let mut findings = vec![regression.clone(), scan.clone()];
        scan_carries_its_regression(&s, &mut findings);
        assert_eq!(findings.len(), 1, "{:?}", findings.iter().map(|f| &f.id).collect::<Vec<_>>());
        assert_eq!(
            findings[0].summary,
            "One query plans to read every row of shop.orders. Query Store saw this query run with plan 8 at 9.5 ms on average until Oct 11, 10:44 UTC, and with plan 47 at 32 ms since Oct 11, 10:46 UTC (93 runs). The scan came with the plan change: plan 8 read about 3 pages a run, plan 47 reads about 5.7K."
        );
        assert!(findings[0].metrics.iter().any(|m| m.label == "Fast plan"));
        // A scan the plan doesn't show (SQL Server's index request alone) is another story: the regression stays.
        scan.queries[0].plan_node = None;
        let mut findings = vec![regression, scan];
        scan_carries_its_regression(&s, &mut findings);
        assert_eq!(findings.len(), 2);
    }

    // The aptabase-us reading: an insert stopped and another took its place, named only in one finding's fix.
    #[test]
    fn a_stopped_statement_names_what_may_have_replaced_it() {
        let mut s = fixture("postgres-demo.json");
        let gone = query(&mut s, "3432126458605143571");
        gone.text = "INSERT INTO events (customer_id, kind) VALUES ($1, $2) ON CONFLICT DO NOTHING".into();
        gone.plan = None;
        // Quiet for the last four hours of the window: too soon to say it stopped. It stays active and says so.
        gone.last_seen = Some("2026-10-11T08:00:00Z".into());
        let next = query(&mut s, "2885972271926804069");
        next.text =
            "INSERT INTO events (customer_id, kind) VALUES ($1, $2) ON CONFLICT (id) DO UPDATE SET kind = $2".into();
        next.plan = None;
        let mut findings = vec![about("heavy-query", "3432126458605143571", &["3432126458605143571"], 0.2)];
        stopped_running(&s, &mut findings);
        assert!(!findings[0].inactive);
        assert!(
            findings[0].caveats[0].starts_with("This query hasn't run in the last ")
                && findings[0].caveats[0].ends_with("too short a time to say it stopped."),
            "{:?}",
            findings[0].caveats
        );
        // Quiet since the morning before: a day of silence, so it stopped.
        query(&mut s, "3432126458605143571").last_seen = Some("2026-10-10T08:00:00Z".into());
        let mut findings = vec![about("heavy-query", "3432126458605143571", &["3432126458605143571"], 0.2)];
        stopped_running(&s, &mut findings);
        assert!(findings[0].inactive);
        assert!(
            findings[0].caveats[0].ends_with(
                "Query 2885972271926804069, an insert into events too, still runs and may be what replaced it: check that one."
            ),
            "{:?}",
            findings[0].caveats
        );
        // A read of the same table took no write's place.
        query(&mut s, "2885972271926804069").text = "SELECT kind FROM events WHERE customer_id = $1".into();
        let mut findings = vec![about("heavy-query", "3432126458605143571", &["3432126458605143571"], 0.2)];
        stopped_running(&s, &mut findings);
        assert!(!findings[0].caveats[0].contains("replaced"), "{:?}", findings[0].caveats);
    }
}

#[cfg(test)]
mod tables_tests {
    use super::*;
    use crate::support::{finding, subject};
    use culprit_core::{Category, Fix, QueryRef};

    fn fixture(name: &str) -> Snapshot {
        let text =
            std::fs::read_to_string(format!("{}/../../fixtures/{name}", env!("CARGO_MANIFEST_DIR"))).expect("fixture");
        serde_json::from_str(&text).expect("snapshot")
    }

    // CX-10d: a caveat repeated the fix ("… already leads with …; the recommended index differs from it only by …").
    // Whatever check wrote it, a caveat sentence the summary or the fix already says goes; the rest of it stays.
    #[test]
    fn a_caveat_repeats_nothing_the_summary_or_the_fix_says() {
        let s = fixture("postgres-demo.json");
        let mut f = finding("tuning-advice", "x", Category::Indexes, subject(SubjectKind::Table, "app.job_runs"));
        f.summary = "The index helps. It costs every write.".into();
        f.fix = Some(Fix { summary: "Nothing to create: ix_a already leads with a.".into(), sql: None, cost: None });
        f.caveats = vec![
            "Nothing to create: ix_a already leads with a".into(),
            "It costs every write. Measure it first.".into(),
            "Quoted, as recorded:\nIt costs every write.".into(),
            "Something new.".into(),
        ];
        let mut findings = vec![f];
        caveats_repeat_nothing(&s, &mut findings);
        assert_eq!(
            findings[0].caveats,
            vec!["Measure it first.", "Quoted, as recorded:\nIt costs every write.", "Something new."]
        );
        // "e.g. the" and "1.5 s" are inside one sentence.
        assert_eq!(
            sentences("Use one, e.g. the first. It takes 1.5 s. Done"),
            vec!["Use one, e.g. the first.", "It takes 1.5 s.", "Done"]
        );
    }

    // CX-11: large-rows listed four statements and claimed the share of three: folding log-volume's statement in kept
    // the larger of the two shares. A finding's share counts every statement it lists.
    #[test]
    fn a_finding_that_absorbs_another_counts_every_statement_it_lists() {
        let s = fixture("postgres-demo.json");
        let w = s.workload.as_ref().unwrap();
        let ids: Vec<String> = w.queries.iter().take(3).map(|q| q.id.clone()).collect();
        let share = |of: &[String]| {
            let qs: Vec<&QueryStat> = of.iter().filter_map(|id| w.query(id)).collect();
            support::query_impact(&s, &qs).unwrap().share
        };
        let about = |check: &str, of: &[String], share: Option<f64>| {
            let mut f = finding(check, &of.join("+"), Category::Queries, subject(SubjectKind::Table, "public.x"));
            f.impact.db_time_share = share;
            f.queries = of.iter().map(|id| QueryRef { id: id.clone(), plan_node: None, note: None }).collect();
            f
        };
        let (two, one) = (ids[..2].to_vec(), ids[2..].to_vec());
        let mut findings =
            vec![about("large-rows", &two, Some(share(&two))), about("log-volume", &one, Some(share(&one)))];
        absorb(&s, &mut findings, 0, 1, None);
        assert_eq!(findings.len(), 1);
        assert!((findings[0].impact.db_time_share.unwrap() - share(&ids)).abs() < 1e-9, "{:?}", findings[0].impact);
        // A finding with no share of its own counts its statements too once it takes one in.
        let mut findings = vec![about("hot-rows", &two, None), about("lock-time", &one, Some(share(&one)))];
        absorb(&s, &mut findings, 0, 1, None);
        assert!((findings[0].impact.db_time_share.unwrap() - share(&ids)).abs() < 1e-9, "{:?}", findings[0].impact);
        // A share that isn't its statements' (a scan's part of a plan) is kept as the larger of the two.
        let mut findings =
            vec![about("filtered-scan", &two, Some(0.001)), about("log-volume", &one, Some(share(&one)))];
        absorb(&s, &mut findings, 0, 1, None);
        assert_eq!(findings[0].impact.db_time_share, Some(0.001f64.max(share(&one))));
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
