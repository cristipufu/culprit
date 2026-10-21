//! The `missing-index` check: Index suggestions recorded by SQL Server's optimizer, merged with each other and checked against existing indexes.

use super::*;
use crate::evidence::{Observation, Requirement as Evidence};
#[cfg(test)]
pub(crate) use culprit_core::index_filter::same_literal;
pub(crate) use culprit_core::index_filter::{compared_to, filter_equalities, is_literal};

mod candidates;
mod parameterized;

pub(crate) use candidates::{
    Candidate, SCAN_ROWS, asking_queries, candidate_fix, drop_large_includes, fits_index_filter, merge_candidates,
    readable_filter, too_small_to_index, writes_per_day,
};
#[cfg(test)]
pub(super) use parameterized::compares_to_parameter;
pub(crate) use parameterized::{Remainder, parameterized_filter};

use candidates::{LastAsked, MEASURED_READS, reads_per_run, request_severity, too_small_finding, wanted_before_window};
use parameterized::parameterized_filter_finding;

pub fn missing_index_info() -> CheckInfo {
    CheckInfo {
        id: "missing-index".into(),
        title: "Missing indexes".into(),
        category: Category::Indexes,
        engines: vec![Engine::Mssql],
        requires: vec![Capability::MissingIndexes],
        description: "Index suggestions recorded by SQL Server's optimizer, merged with each other and checked against existing indexes.".into(),
    }
}

check!(
    MissingIndexCheck: missing_index_info,
    missing_index,
    &[Evidence::IndexCatalog, Evidence::SourceWithLegacy(culprit_core::source_id::MISSING_INDEXES, Observation::MissingIndexes)]
);

pub fn missing_index(snapshot: &Snapshot) -> Vec<Finding> {
    let queries = snapshot.queries();
    merge_candidates(snapshot).into_iter().map(|c| missing_index_finding(snapshot, queries, c)).collect()
}

pub(super) fn missing_index_finding(snapshot: &Snapshot, queries: &[QueryStat], mut c: Candidate) -> Finding {
    let keys = c.keys();
    let table_name = format!("{}.{}", c.schema, c.table);
    let key_text = format::list(&keys);
    let mut f = finding(
        "missing-index",
        &format!("{table_name}({})", keys.join(",")),
        Category::Indexes,
        subject(SubjectKind::Table, &table_name),
    );
    f.facts.index_keys = keys.clone();
    let linked = asking_queries(queries, &c);
    if let Some(miss) = parameterized_filter(snapshot, &c, &linked) {
        return parameterized_filter_finding(snapshot, f, &c, &linked, &miss);
    }
    let table = snapshot.table(&c.schema, &c.table);
    let rows = table.and_then(|t| t.rows);
    let lookups = c.seeks + c.scans;
    // Asked for only before the window: history once that is a day back, otherwise said and kept active.
    let asked = wanted_before_window(snapshot, &c);
    let lately = asked.as_ref().filter(|a| !a.stopped());

    // Asked for by statements that all stopped running: told in the past, as refine then marks it.
    let history = crate::all_stopped(snapshot, &linked) || asked.as_ref().is_some_and(LastAsked::stopped);
    let is = if history { "was" } else { "is" };
    f.title = format!("{} {is} searched by {} without a supporting index", c.table, key_text);
    let mut summary = format!(
        "SQL Server's optimizer asked for an index on {} ({}) {} times {}. Without it, each of those lookups {} more of {}{} than it {} to. It {} the index would make them about {:.0}% cheaper.",
        table_name,
        key_text,
        format::count(lookups),
        since_phrase(snapshot),
        if history { "read" } else { "reads" },
        table_name,
        rows.map(|r| format!(" ({} rows)", format::count(r))).unwrap_or_default(),
        if history { "needed" } else { "needs" },
        if history { "estimated" } else { "estimates" },
        c.impact_pct
    );
    match query_impact(snapshot, &linked) {
        Some(impact) => {
            summary.push_str(&format!(
                " The {} that asked for it {} for {} of database time.",
                if linked.len() == 1 { "query".to_string() } else { format!("{} queries", linked.len()) },
                if history {
                    "accounted"
                } else if linked.len() == 1 {
                    "accounts"
                } else {
                    "account"
                },
                format::percent(impact.share)
            ));
            apply_query_impact(&mut f, &impact);
            // What the index saves is the queries' time times the optimizer's estimate of the gain: 11% of a query at
            // 0.01% of database time is nothing to rank or raise above Low.
            let gain = (c.impact_pct / 100.0).clamp(0.0, 1.0);
            f.impact.score = score_for_share(impact.share * gain);
            f.severity = request_severity(snapshot, &c, &linked);
            // How sure, not how much (the severity says that): the optimizer's estimate is a share of what its
            // queries read. Where their runs measured real reading ([`MEASURED_READS`] pages a run) of a table of
            // SCAN_ROWS rows or more, a large gain is certain; on a small table, or runs that read a few pages, a
            // large share of very little is a guess.
            let measured = reads_per_run(&linked).is_some_and(|pages| pages >= MEASURED_READS);
            f.confidence = if c.impact_pct >= 50.0 && measured && rows.is_some_and(|r| r >= SCAN_ROWS) {
                Confidence::High
            } else {
                Confidence::Medium
            };
        }
        None if linked.is_empty() && snapshot.workload.is_some() => {
            summary.push_str(&match lately {
                Some(a) => format!(" {}", a.lately_sentence()),
                None => " None of the queries in this scan asked for it: the requests came from plans that ran before the scan's window, or that Query Store no longer keeps.".to_string(),
            });
            f.severity = request_severity(snapshot, &c, &linked);
            f.impact.score = (6.0 * (c.weight + 1.0).log10()).clamp(4.0, 40.0);
            f.impact.label = format!("{} lookups", format::count(lookups));
            f.confidence = if lookups >= 1000.0 { Confidence::Medium } else { Confidence::Low };
        }
        None => {
            f.severity = request_severity(snapshot, &c, &linked);
            f.impact.score = (6.0 * (c.weight + 1.0).log10()).clamp(4.0, 40.0);
            f.impact.label = format!("{} lookups", format::count(lookups));
            f.confidence = if lookups >= 1000.0 { Confidence::Medium } else { Confidence::Low };
        }
    }
    // Azure SQL weighs the same index over a longer history than the optimizer's counters, which reset on restart.
    if let Some(r) = crate::tuning::recommended_create(snapshot, &c.schema, &c.table, &keys) {
        summary.push_str(&format!(
            " Azure SQL's automatic tuning recommends the same index{}.",
            r.score.map(|s| format!(", scoring it {s:.0} out of 100")).unwrap_or_default()
        ));
        f.confidence = Confidence::High;
    }
    f.summary = summary;
    f.queries = query_refs(&linked);
    f.metrics.push(Metric::new("Lookups that wanted it", lookups, Unit::Count));
    f.metrics.push(Metric::new("Estimated improvement", c.impact_pct / 100.0, Unit::Ratio));
    if let Some(r) = rows {
        f.metrics.push(Metric::new("Rows in table", r, Unit::Rows));
    }
    if let Some(share) = f.impact.db_time_share {
        f.metrics.insert(0, Metric::new("Share of DB time", share, Unit::Ratio));
    }

    match &asked {
        Some(a) if a.stopped() => {
            // An old request is history even if an associated query hash still runs with another plan.
            f.actionability = culprit_core::Actionability::History;
            f.inactive = crate::all_stopped(snapshot, &linked);
            f.title = format!("{} was searched by {} without a supporting index", c.table, key_text);
            f.severity = Severity::Low;
            f.impact.score *= 0.25;
            f.last_seen = Some(a.last.clone());
            f.caveats.insert(
                0,
                format!(
                    "SQL Server's optimizer last asked for this index on {}, {} before this scan's data ends: the \
                     queries that wanted it have most likely changed or stopped running. Its requests are kept until \
                     the server restarts.",
                    format::friendly_date(&a.last, &snapshot.taken_at),
                    format::duration(a.silence * 1000.0),
                ),
            );
        }
        // Said in the summary when no query of the window is linked to it; beside linked ones, as a caveat.
        Some(a) if !linked.is_empty() => f.caveats.insert(0, a.lately_sentence()),
        _ => {}
    }

    if too_small_to_index(snapshot, &c, &linked) {
        return too_small_finding(snapshot, f, &c, &linked);
    }
    if let Some(a) = asked.as_ref().filter(|_| !f.inactive) {
        f.actionability =
            if a.stopped() { culprit_core::Actionability::History } else { culprit_core::Actionability::Information };
        f.severity = Severity::Low;
        f.title = format!("SQL Server previously requested an index on {} ({key_text})", c.table);
        f.summary = format!(
            "SQL Server last requested this index {}, before {}. The {} requests are cumulative {}. Associated statements may still run, but their activity does not establish that this historical request is still needed in the selected period.",
            crate::queries::when_phrase(&a.last),
            a.window,
            format::count(lookups),
            since_phrase(snapshot)
        );
        f.impact.db_time_share = None;
        f.impact.time_ms_per_day = None;
        f.impact.score = base_score(Severity::Low);
        f.impact.label = "Historical optimizer request".into();
        f.metrics.retain(|m| m.label != "Share of DB time");
        for r in &mut f.queries {
            r.note = Some("Associated with a request made before the selected window; current query activity does not date that request.".into());
        }
        f.fix = Some(Fix { summary: "Keep this candidate as context. Check representative plans and reads in the period that matters before choosing an index. If the optimizer requests it again, compare the measured benefit with index storage and write maintenance.".into(), sql: None, cost: None });
        f.verify = vec!["A later scan must show a new request inside its workload window and enough measured work before this becomes an index recommendation.".into()];
        return f;
    }
    // A request no query in the window made is history: it gets no index of its own.
    if f.inactive {
        let writes = measured_writes(snapshot, &c.schema, &c.table)
            .filter(|(writes, _)| *writes > 0.0)
            .map(|(writes, days)| {
                format!(
                    ", and an index on {key_text} would add to every write on {table_name} ({})",
                    how_many(snapshot, writes, days, "write", "writes")
                )
            })
            .unwrap_or_default();
        f.fix = Some(Fix {
            summary: format!(
                "Nothing to create unless the statements that wanted it come back: they have most likely changed or \
                 stopped running{writes}. If they come back, the optimizer asks for it again, and a scan then shows \
                 them and the index to create."
            ),
            sql: None,
            cost: None,
        });
        f.verify = vec![
            "Nothing to check unless they come back: if the optimizer asks for this index again within a scan's \
             window, this finding names the queries behind it and gives the index to create."
                .into(),
        ];
        return f;
    }
    // Columns to include matter only for an index worth creating.
    if let Some(note) = drop_large_includes(snapshot, &c.schema, &c.table, &mut c.include) {
        f.caveats.push(note);
    }
    let wide_include = c.include.len() > MAX_INCLUDE;
    if wide_include {
        f.caveats.push(format!(
            "SQL Server also suggested including {} more columns. That makes a large, write-heavy index; the fix keeps the keys only. Add INCLUDE columns only for the columns the hot queries actually select.",
            c.include.len()
        ));
        c.include.clear();
    }

    let fixed = candidate_fix(snapshot, &c, &linked);
    // A widen rebuilds an existing index under its own name: the queries seek that one, not a new one.
    let first = match (&fixed.widens, fixed.fix.sql.is_some()) {
        (Some(name), true) => {
            format!("After testing the rebuild of {name}, capture a newly compiled plan under representative load. A stored suggestion can remain in old plan XML.")
        }
        (Some(name), false) => format!("Check whether newly compiled plans use {name}; an old stored suggestion alone does not establish a current gap."),
        (None, _) => "Test the candidate index with representative parameters and load, then capture a newly compiled plan. Old stored suggestions need not disappear.".into(),
    };
    f.verify = vec![first];
    if !linked.is_empty() {
        f.verify.push(format!(
            "Check whether the queries listed here use {} and compare reads and execution time under equivalent parameters and load, including index storage and write cost.",
            fixed.widens.as_deref().unwrap_or("the new index")
        ));
    }
    f.fix = Some(fixed.fix);
    f.caveats.push("Missing-index suggestions are per-query and ignore the cost of maintaining the index; review overlapping suggestions together.".into());
    f
}
