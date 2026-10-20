//! The `tuning-advice` check: Plan-forcing and index recommendations from automatic tuning, forced plans (those that stopped applying, and those that scan where an index added since could seek), Query Store hints that stopped applying, and automatic plan correction left off.

use super::*;
use crate::evidence::Requirement as Evidence;

pub fn tuning_advice_info() -> CheckInfo {
    CheckInfo {
        id: "tuning-advice".into(),
        title: "SQL Server's own tuning advice".into(),
        category: Category::Queries,
        engines: vec![Engine::Mssql],
        requires: vec![Capability::QueryStore],
        description:
            "Plan-forcing and index recommendations from automatic tuning, forced plans (those that stopped applying, \
            and those that scan where an index added since could seek), Query Store hints that stopped applying, and \
            automatic plan correction left off."
                .into(),
    }
}

check!(TuningAdviceCheck: tuning_advice_info, tuning_advice, &[Evidence::Unknown]);

/// A day or more between `last` and the end of the scan's data, as (seconds, `last`).
pub(super) fn quiet_since<'a>(snapshot: &Snapshot, last: Option<&'a str>) -> Option<(f64, &'a str)> {
    let last = last?;
    let end = snapshot.workload.as_ref()?.window_end.as_deref().and_then(parse_epoch_seconds)?;
    let silence = end - parse_epoch_seconds(last)?;
    (silence >= 86_400.0).then_some((silence, last))
}

/// Waiting for someone to apply it. SQL Server applied the others itself (Verifying, Success) or let them go
/// (Reverted, Expired): a plan it forced shows among the forced plans instead.
pub(super) fn waiting(r: &TuningRecommendation) -> bool {
    r.state.as_deref().is_none_or(|s| matches!(s.to_ascii_lowercase().as_str(), "active" | "pending"))
}

/// A plan-forcing recommendation still waiting to be applied.
pub(super) fn waiting_plan_fix(r: &TuningRecommendation) -> bool {
    waiting(r) && r.query_id.is_some() && r.recommended_plan_id.is_some()
}

/// Azure SQL's waiting recommendation to create an index on these key columns of a table. The missing-index finding for
/// the same index says so rather than a second finding.
pub(crate) fn recommended_create<'a>(
    snapshot: &'a Snapshot,
    schema: &str,
    table: &str,
    keys: &[String],
) -> Option<&'a TuningRecommendation> {
    snapshot.tuning.as_ref()?.recommendations.iter().find(|r| {
        waiting(r)
            && r.kind.eq_ignore_ascii_case("CreateIndex")
            && r.index.as_ref().is_some_and(|i| {
                i.schema.eq_ignore_ascii_case(schema)
                    && i.table.eq_ignore_ascii_case(table)
                    && !i.columns.is_empty()
                    && lower_set(&i.columns) == lower_set(keys)
            })
    })
}

pub fn tuning_advice(snapshot: &Snapshot) -> Vec<Finding> {
    let Some(tuning) = &snapshot.tuning else { return Vec::new() };
    let db = quote(Engine::Mssql, &snapshot.server.database);
    let mut out = Vec::new();

    for r in tuning.recommendations.iter().filter(|r| waiting(r)) {
        let Some(index) = &r.index else { continue };
        let advice = if r.kind.eq_ignore_ascii_case("CreateIndex") {
            create_index_advice(snapshot, r, index)
        } else if r.kind.eq_ignore_ascii_case("DropIndex") {
            drop_index_advice(snapshot, r, index)
        } else {
            None
        };
        out.extend(advice);
    }

    for r in tuning.recommendations.iter().filter(|r| waiting(r)) {
        let (Some(query_id), Some(plan_id)) = (&r.query_id, &r.recommended_plan_id) else { continue };
        let q = snapshot.workload.as_ref().and_then(|w| w.query(query_id));
        let mut f = finding(
            "tuning-advice",
            &format!("force:{query_id}"),
            Category::Queries,
            subject(SubjectKind::Query, query_id.to_string()),
        );
        // SQL Server's gain compares the two plans' average CPU per run, not their duration.
        let gain = r
            .estimated_gain
            .map(|g| format!(" (estimated {} less CPU per run)", format::percent(g)))
            .unwrap_or_default();
        f.title = format!(
            "SQL Server suggests forcing plan {plan_id} for {}{gain}",
            q.map(query_handle).unwrap_or_else(|| format!("query {query_id}"))
        );
        f.summary = format!(
            "Automatic tuning noticed that query {query_id} got slower when it switched to plan {} and recommends going \
             back to plan {plan_id}{}. {}",
            r.regressed_plan_id.as_deref().unwrap_or("?"),
            r.valid_since
                .as_deref()
                .map(|s| format!(" (since {})", s.replace('T', " ").trim_end_matches('Z')))
                .unwrap_or_default(),
            if r.error_prone {
                "The current plan also fails more often than the recommended one."
            } else {
                "Forcing is reversible and Query Store keeps both plans."
            }
        );
        match q.and_then(|q| query_impact(snapshot, &[q])) {
            Some(impact) => {
                apply_query_impact(&mut f, &impact);
                f.severity = severity_for_impact(snapshot, q.as_slice(), &impact);
            }
            // SQL Server recommends from about ten seconds of estimated CPU saved: with no time in the window behind
            // it (the query is outside the top statements, or newer), it is Low until the workload shows the time.
            None => {
                f.severity = Severity::Low;
                f.impact.score = base_score(f.severity);
                f.impact.label = "Recommended by SQL Server".into();
            }
        }
        f.confidence = Confidence::High;
        if let Some(g) = r.estimated_gain {
            f.metrics.push(Metric::new("Estimated CPU saved per run", g, Unit::Ratio));
        }
        f.metrics.push(Metric::text("Recommended plan", plan_id.clone()));
        f.facts.recommended_plan =
            Some(culprit_core::FindingPlanRecommendation { plan: plan_id.clone(), cpu_saved_share: r.estimated_gain });
        if let Some(bad) = &r.regressed_plan_id {
            f.metrics.push(Metric::text("Current plan", bad.clone()));
        }
        if let Some(q) = q {
            f.queries = vec![QueryRef { id: q.id.clone(), plan_node: None, note: None }];
        } else if let Some((silence, last)) = quiet_since(snapshot, r.last_execution.as_deref()) {
            // Not among the window's queries, and Query Store says it hasn't run for over a day: most likely a deploy
            // replaced the statement, and the advice is about code that is gone.
            f.inactive = true;
            f.impact.score *= 0.25;
            f.last_seen = Some(last.to_string());
            f.caveats.insert(
                0,
                format!(
                    "Query {query_id} has not run since {} ({} before this scan's data ends). A deploy may have \
                     changed it; check that the current version of this code doesn't have the same problem.",
                    last.get(..16).unwrap_or(last).replace('T', " "),
                    format::duration(silence * 1000.0)
                ),
            );
        }
        // Turning automatic plan correction on is advice only where it is off: on, it applies its own
        // recommendations, and forcing by hand is what the note says it is.
        let automatic = match crate::tuning::hand_forcing_note(snapshot) {
            Some(note) => format!(" {note}"),
            None => " To let SQL Server do this on its own, turn on automatic plan correction.".to_string(),
        };
        f.fix = Some(Fix {
            summary: format!(
                "Force the recommended plan (copy and run it yourself; Culprit never runs it).{automatic}"
            ),
            sql: Some(r.script.clone().unwrap_or_else(|| {
                format!("EXEC sp_query_store_force_plan @query_id = {query_id}, @plan_id = {plan_id};")
            })),
            cost: None,
        });
        f.verify = vec![
            format!(
                "Re-scan: query {query_id} runs with plan {plan_id} and its average duration is back to its earlier level."
            ),
            format!(
                "If it gets worse, undo with EXEC sp_query_store_unforce_plan @query_id = {query_id}, @plan_id = {plan_id};"
            ),
        ];
        out.push(f);
    }

    for p in tuning.forced_plans.iter().filter(|p| p.failures > 0.0) {
        let q = snapshot.workload.as_ref().and_then(|w| w.query(&p.query_id));
        let mut f = finding(
            "tuning-advice",
            &format!("forced-failing:{}", p.query_id),
            Category::Queries,
            subject(SubjectKind::Query, p.query_id.to_string()),
        );
        let reason = p.last_failure_reason.as_deref().unwrap_or("unknown reason");
        f.title = format!(
            "A forced plan for {} stopped applying {} times ({})",
            q.map(query_handle).unwrap_or_else(|| format!("query {}", p.query_id)),
            format::count(p.failures),
            reason
        );
        f.summary = format!(
            "Plan {} is forced for query {}, but SQL Server could not use it {} times, most recently because of {}. Each \
             time, it compiled a new plan instead, so the forcing gives none of the stability it was meant to. {}",
            p.plan_id,
            p.query_id,
            format::count(p.failures),
            reason,
            failure_hint(reason)
        );
        f.severity = if p.failures >= 100.0 { Severity::Medium } else { Severity::Low };
        f.impact.score = base_score(f.severity) + p.failures.log10().max(0.0);
        f.impact.label = format!("{} failed forcings", format::count(p.failures));
        f.confidence = Confidence::High;
        f.metrics.push(Metric::new("Forcing failures", p.failures, Unit::Count));
        f.metrics.push(Metric::text("Last reason", reason.to_string()));
        if let Some(q) = q {
            f.queries = vec![QueryRef { id: q.id.clone(), plan_node: None, note: None }];
        } else if let Some((silence, last)) = quiet_since(snapshot, p.last_execution.as_deref()) {
            // Not among the window's queries, and Query Store says it hasn't run for over a day: most likely a deploy
            // replaced the statement, and the advice is about code that is gone.
            f.inactive = true;
            f.impact.score *= 0.25;
            f.last_seen = Some(last.to_string());
            f.caveats.insert(
                0,
                format!(
                    "Query {} has not run since {} ({} before this scan's data ends). A deploy may have \
                     changed it; check that the current version of this code doesn't have the same problem.",
                    p.query_id,
                    last.get(..16).unwrap_or(last).replace('T', " "),
                    format::duration(silence * 1000.0)
                ),
            );
        }
        f.fix = Some(Fix {
            summary: "Restore what the plan needs (often an index that was dropped or renamed), or unforce it and let \
                SQL Server choose again."
                .into(),
            sql: Some(format!(
                "EXEC sp_query_store_unforce_plan @query_id = {}, @plan_id = {};",
                p.query_id, p.plan_id
            )),
            cost: None,
        });
        f.verify = vec![format!("Re-scan: plan {}'s failure count stops growing.", p.plan_id)];
        out.push(f);
    }

    // Forced plans that apply: SQL Server runs each as it is, also after an index is added or the data grows.
    let mut kept: Vec<(&ForcedPlan, Option<&QueryStat>, String)> = Vec::new();
    for p in tuning.forced_plans.iter().filter(|p| p.failures <= 0.0) {
        let q = snapshot.workload.as_ref().and_then(|w| w.query(&p.query_id));
        let applied = tuning.recommendations.iter().find(|r| {
            r.query_id.as_deref() == Some(p.query_id.as_str())
                && r.recommended_plan_id.as_deref() == Some(p.plan_id.as_str())
        });
        let automatic = p.forcing.as_deref().is_some_and(|f| f.eq_ignore_ascii_case("AUTO"))
            || applied.is_some_and(|r| !waiting(r));
        let how = match (automatic, applied.and_then(|r| r.applied_at.as_deref())) {
            (true, Some(at)) => {
                format!("forced by automatic tuning on {}", format::friendly_date(at, &snapshot.taken_at))
            }
            (true, None) => "forced by automatic tuning".to_string(),
            (false, _) if p.forcing.as_deref().is_some_and(|f| f.eq_ignore_ascii_case("MANUAL")) => {
                "forced by hand".to_string()
            }
            _ => "forced".to_string(),
        };
        let forced_at = applied.and_then(|r| r.applied_at.as_deref());
        match q.and_then(|q| seekable_scan(snapshot, q, &p.plan_id).map(|scan| (q, scan))) {
            Some((q, scan)) => out.push(forced_scan_finding(snapshot, p, q, &how, forced_at, scan)),
            None => kept.push((p, q, how)),
        }
    }
    if !kept.is_empty() {
        out.push(forced_plans_finding(snapshot, &kept));
    }

    for h in tuning.hints.iter().filter(|h| h.failures > 0.0) {
        let mut f = finding(
            "tuning-advice",
            &format!("hint-failing:{}", h.query_id),
            Category::Queries,
            subject(SubjectKind::Query, h.query_id.to_string()),
        );
        let reason = h.last_failure_reason.as_deref().unwrap_or("unknown reason");
        f.title =
            format!("A Query Store hint on query {} failed {} times ({reason})", h.query_id, format::count(h.failures));
        f.summary = format!(
            "The hint {} is set for query {}, but SQL Server could not apply it {} times, so the query ran without it.",
            h.hint.trim(),
            h.query_id,
            format::count(h.failures)
        );
        f.severity = Severity::Low;
        f.impact.score = base_score(f.severity);
        f.impact.label = format!("{} failed hints", format::count(h.failures));
        f.confidence = Confidence::High;
        f.fix = Some(Fix {
            summary: "Fix or clear the hint.".into(),
            sql: Some(format!("EXEC sys.sp_query_store_clear_hints @query_id = {};", h.query_id)),
            cost: None,
        });
        f.verify = vec!["Re-scan: the hint's failure count stops growing.".into()];
        out.push(f);
    }

    if let Some(plan) = tuning.automatic_tuning.iter().find(|o| o.name.eq_ignore_ascii_case("FORCE_LAST_GOOD_PLAN")) {
        let desired_on = plan.desired.eq_ignore_ascii_case("ON");
        let actual_on = plan.actual.eq_ignore_ascii_case("ON");
        let waiting_fixes = tuning.recommendations.iter().filter(|r| waiting_plan_fix(r)).count();
        let regressions = waiting_fixes > 0;
        if desired_on && !actual_on {
            let mut f = finding(
                "tuning-advice",
                "automatic-plan-correction",
                Category::Configuration,
                subject(SubjectKind::Setting, "FORCE_LAST_GOOD_PLAN"),
            );
            f.title = "Automatic plan correction is turned on but isn't running".into();
            f.summary = format!(
                "FORCE_LAST_GOOD_PLAN is set to ON, yet SQL Server reports it as {}{}. Plan regressions are not being \
                 corrected.",
                plan.actual,
                plan.reason.as_deref().map(|r| format!(" ({r})")).unwrap_or_default()
            );
            f.severity = Severity::Medium;
            f.impact.score = base_score(f.severity);
            f.impact.label = "Setting".into();
            f.confidence = Confidence::High;
            f.fix = Some(Fix {
                summary: "Usually Query Store is off or read-only; turn it back on (see the Query Store finding)."
                    .into(),
                sql: Some(format!("ALTER DATABASE {db} SET QUERY_STORE (OPERATION_MODE = READ_WRITE);")),
                cost: None,
            });
            f.verify = vec!["Re-scan: FORCE_LAST_GOOD_PLAN reports ON.".into()];
            out.push(f);
        } else if !actual_on && regressions {
            let mut f = finding(
                "tuning-advice",
                "automatic-plan-correction",
                Category::Configuration,
                subject(SubjectKind::Setting, "FORCE_LAST_GOOD_PLAN"),
            );
            f.title = "Automatic plan correction is off while SQL Server has fixes waiting".into();
            f.summary = format!(
                "SQL Server found {} plan {} it could fix by going back to the last good plan, but automatic plan \
                 correction is off, so they wait for someone to apply them.",
                waiting_fixes,
                if waiting_fixes == 1 { "regression" } else { "regressions" }
            );
            f.severity = Severity::Low;
            f.impact.score = base_score(f.severity) + 4.0;
            f.impact.label = "Setting".into();
            f.confidence = Confidence::Medium;
            f.fix = Some(Fix {
                summary:
                    "Let SQL Server force the last good plan itself when a query regresses; it verifies the result \
                    and backs out if the forced plan is not better."
                        .into(),
                sql: Some(format!("ALTER DATABASE {db} SET AUTOMATIC_TUNING (FORCE_LAST_GOOD_PLAN = ON);")),
                cost: None,
            });
            f.verify = vec!["Re-scan: recommendations move from Active to Verifying and then Success.".into()];
            out.push(f);
        }
    }
    out
}

/// A scan in a query's forced plan of a table an existing index could seek, the way the scan filters it.
pub(super) struct SeekableScan<'a> {
    node: u32,
    schema: String,
    table: String,
    columns: Vec<String>,
    rows: f64,
    index: &'a IndexStat,
    /// The share of the rows read that the scan keeps.
    kept: f64,
    /// Columns the scan returns that the index doesn't hold, so a seek would look each row up in the table; empty when
    /// it covers them.
    lookups: Vec<String>,
}

// Tables of `SCAN_ROWS` rows or more: reading one in full where a seek would do costs something (the bar every index
// advice for a table's reads uses).
use crate::indexes::SCAN_ROWS;
/// An index that doesn't hold every column the query reads costs a lookup per row it finds: worth it over a scan only
/// for a filter far narrower than an index's usual bar.
const LOOKUPS_KEEP_AT_MOST: f64 = 0.01;

pub(super) fn seekable_scan<'a>(snapshot: &'a Snapshot, q: &QueryStat, plan_id: &str) -> Option<SeekableScan<'a>> {
    // The captured plan is the one that took the most time: the forced one, once the forcing has taken hold.
    let dominant = q.plans.iter().max_by(|a, b| a.total_time_ms.total_cmp(&b.total_time_ms))?;
    if dominant.id != plan_id {
        return None;
    }
    q.plan.as_ref()?.nodes().into_iter().find_map(|node| {
        if node.kind != NodeKind::Scan {
            return None;
        }
        let (schema, table) = split_object(node.object.as_deref()?)?;
        let rows = crate::indexes::scan_rows(snapshot, node, &schema, &table)?;
        if rows < SCAN_ROWS {
            return None;
        }
        // The bar every index claim uses (`PlanNode::kept_share`): a scan that keeps much of what it reads is the right
        // plan, forced or not. Rows read as `scan_rows` counts them: a whole table, unless the index is filtered.
        let kept =
            (node.act_rows.unwrap_or_else(|| node.est_rows_total()) / (rows * node.est_executions.max(1.0))).min(1.0);
        if kept > culprit_core::INDEX_KEEPS_AT_MOST {
            return None;
        }
        let cols = predicate_columns(node.predicate.as_deref()?);
        let mut columns = cols.equality.clone();
        columns.extend(cols.range.into_iter().take(1));
        if columns.is_empty() {
            return None;
        }
        let index = leading_index(snapshot, &schema, &table, &cols.equality, &columns)?;
        // Without the plan's output columns, coverage is unknown: count it as lookups.
        let lookups = match node.facts().output_columns.as_ref() {
            Some(list) => uncovered(snapshot, index, list),
            None => vec!["the columns it returns".to_string()],
        };
        if !lookups.is_empty() && kept > LOOKUPS_KEEP_AT_MOST {
            return None;
        }
        Some(SeekableScan { node: node.id, schema, table, columns, rows, index, kept, lookups })
    })
}

/// Columns of a plan's output list the index doesn't hold: not a key, not included, and not the clustered key every
/// nonclustered index carries.
pub(super) fn uncovered(snapshot: &Snapshot, index: &IndexStat, output: &[String]) -> Vec<String> {
    if index.is_clustered {
        return Vec::new();
    }
    let clustered = snapshot.indexes_on(&index.schema, &index.table).find(|i| i.is_clustered);
    let held: Vec<String> = index
        .key_names()
        .into_iter()
        .map(str::to_string)
        .chain(index.include.iter().cloned())
        .chain(clustered.map(|c| c.key_names().into_iter().map(str::to_string).collect::<Vec<_>>()).unwrap_or_default())
        .map(|c| c.to_ascii_lowercase())
        .collect();
    output
        .iter()
        .filter(|c| !c.is_empty() && !c.starts_with("Expr") && !held.contains(&c.to_ascii_lowercase()))
        .cloned()
        .collect()
}

pub(super) fn forced_scan_finding(
    snapshot: &Snapshot,
    p: &ForcedPlan,
    q: &QueryStat,
    how: &str,
    forced_at: Option<&str>,
    scan: SeekableScan,
) -> Finding {
    let table_name = format!("{}.{}", scan.schema, scan.table);
    let index = &scan.index.name;
    let columns = format::list(&scan.columns);
    let mut f = finding(
        "tuning-advice",
        &format!("forced-scan:{}", p.query_id),
        Category::Queries,
        subject(SubjectKind::Query, p.query_id.clone()),
    );
    f.title = format!("A forced plan makes {} scan {} though {index} could seek it", query_handle(q), scan.table);
    let lookups = if scan.lookups.is_empty() {
        String::new()
    } else {
        format!(
            " {index} doesn't hold {}, so a seek would look each row it finds up in the table: still far less than \
             reading every row for the {} it keeps.",
            format::list(&scan.lookups),
            format::percent(scan.kept)
        )
    };
    // "Forced before the index existed" only where something dates the index as new (a scan saw it appear, or the
    // database dated a change to its table), and not to before the forcing. SQL Server doesn't say which time zone the
    // tuning view's times are in, so within the widest UTC offset (14 hours) of each other their order is unknown.
    const ZONE_UNKNOWN_SECONDS: f64 = 14.0 * 3600.0;
    let before_forcing = |at: &str| match (parse_epoch_seconds(at), forced_at.and_then(parse_epoch_seconds)) {
        (Some(created), Some(forced)) => created <= forced + ZONE_UNKNOWN_SECONDS,
        _ => false,
    };
    let newer = match scan.index.created_at_latest.as_deref().filter(|at| !before_forcing(at)) {
        Some(at) => format!(
            " {index} may be new (Culprit dates it to {} at the latest), so this plan was most likely forced before it \
             existed.",
            format::friendly_date(at, &snapshot.taken_at)
        ),
        None => String::new(),
    };
    f.summary = format!(
        "Plan {} is {how} for query {}. SQL Server runs a forced plan as it is, even after an index is added, so this \
         one reads all of {table_name} ({} rows) and keeps about {} of them to find the rows by {columns}, while \
         {index} leads with {} and could seek them.{lookups}{newer}",
        p.plan_id,
        p.query_id,
        format::count(scan.rows),
        format::percent(scan.kept),
        if scan.columns.len() == 1 { "it" } else { "them" },
    );
    match query_impact(snapshot, &[q]) {
        Some(impact) => {
            apply_query_impact(&mut f, &impact);
            f.severity = severity_for_impact(snapshot, &[q], &impact);
        }
        None => {
            f.severity = Severity::Medium;
            f.impact.score = base_score(f.severity);
            f.impact.label = "Forced plan".into();
        }
    }
    f.confidence = Confidence::Medium;
    f.metrics.push(Metric::text("Forced plan", p.plan_id.clone()));
    f.metrics.push(Metric::text("Index that could seek", index.clone()));
    f.metrics.push(Metric::new("Rows in table", scan.rows, Unit::Rows));
    f.queries = vec![QueryRef {
        id: q.id.clone(),
        plan_node: Some(scan.node),
        note: Some(format!("Scans {} under the forced plan.", scan.table)),
    }];
    f.last_seen = q.last_seen.clone();
    f.fix = Some(Fix {
        summary: format!(
            "Unforce it, so SQL Server compiles a plan that can seek {index}. If automatic plan correction is on and the \
             new plan turns out slower, it forces a plan again, chosen among the plans it has seen since."
        ),
        sql: Some(format!("EXEC sp_query_store_unforce_plan @query_id = {}, @plan_id = {};", p.query_id, p.plan_id)),
        cost: None,
    });
    f.verify = vec![
        format!("Re-scan: query {} seeks {index}, and its reads and average duration drop.", p.query_id),
        format!(
            "If it gets slower, force the plan again: EXEC sp_query_store_force_plan @query_id = {}, @plan_id = {};",
            p.query_id, p.plan_id
        ),
    ];
    f
}

/// Plans listed one by one in the forced-plans finding; the rest are counted.
pub(super) const FORCED_LISTED: usize = 12;

pub(super) fn forced_plans_finding(snapshot: &Snapshot, kept: &[(&ForcedPlan, Option<&QueryStat>, String)]) -> Finding {
    let n = kept.len();
    let automatic = kept.iter().filter(|(_, _, how)| how.contains("automatic tuning")).count();
    let mut f = finding(
        "tuning-advice",
        "forced-plans",
        Category::Queries,
        subject(SubjectKind::Database, snapshot.server.database.clone()),
    );
    f.title = match (n, automatic) {
        (1, 1) => "Automatic tuning forced a plan in Query Store".to_string(),
        (1, _) => "A plan is forced in Query Store".to_string(),
        (n, a) if a == n => format!("{n} plans are forced in Query Store, all by automatic tuning"),
        (n, 0) => format!("{n} plans are forced in Query Store"),
        (n, a) => format!("{n} plans are forced in Query Store, {a} by automatic tuning"),
    };
    let automatic_sentence = match (n, automatic) {
        (_, 0) => String::new(),
        (1, _) => " Automatic plan correction forced it after the query got slower with a new plan.".to_string(),
        (n, a) if a == n => {
            " Automatic plan correction forced all of them after their queries got slower with a new plan.".to_string()
        }
        (_, 1) => {
            " Automatic plan correction forced one of them after its query got slower with a new plan.".to_string()
        }
        (_, a) => {
            format!(" Automatic plan correction forced {a} of them after their queries got slower with a new plan.")
        }
    };
    // Automatic plan correction keeps checking the plans it forced and unforces one that turns out slower, while it is
    // on; a plan forced by hand (or once it's off) stays forced until someone unforces it.
    let correcting = crate::tuning::automatic_plan_correction(snapshot) == Some(true);
    let checked = if correcting { automatic } else { 0 };
    let unchecked = match (n - checked, checked) {
        (0, _) => " It is still on, so it keeps checking the plans it forced and unforces one that turns out slower."
            .to_string(),
        (_, 0) => " Once the query or its indexes change, a forced plan can become the slower one without anything \
                   saying so."
            .to_string(),
        (1, _) => " It is still on, so it keeps checking the plans it forced and unforces one that turns out slower. \
                   The other plan stays forced until someone unforces it: once its query or indexes change, it can \
                   become the slower one without anything saying so."
            .to_string(),
        (rest, _) => format!(
            " It is still on, so it keeps checking the plans it forced and unforces one that turns out slower. The \
             other {rest} stay forced until someone unforces them: once their queries or indexes change, one can \
             become the slower one without anything saying so."
        ),
    };
    f.summary = format!(
        "SQL Server runs {} with the plan forced for {} instead of choosing one, even after an index is added or the \
         data grows. A forced plan keeps a query on the plan it ran fast with, so a new plan can't slow it \
         down.{automatic_sentence}{unchecked}",
        if n == 1 { "this query" } else { "these queries" },
        if n == 1 { "it" } else { "each" },
    );
    f.severity = Severity::Low;
    f.impact.score = base_score(f.severity) + (n as f64).min(10.0) * 0.2;
    f.impact.label = format!("{n} forced {}", if n == 1 { "plan" } else { "plans" });
    f.confidence = Confidence::High;
    f.metrics.push(Metric::new("Forced plans", n as f64, Unit::Count));
    if automatic > 0 {
        f.metrics.push(Metric::new("Forced by automatic tuning", automatic as f64, Unit::Count));
    }
    let mut lines: Vec<String> = kept
        .iter()
        .take(FORCED_LISTED)
        .map(|(p, q, how)| {
            let what = q.map(query_handle).map(|h| format!(" ({h})")).unwrap_or_default();
            let ran = match (q, quiet_since(snapshot, p.last_execution.as_deref())) {
                (None, Some((_, last))) => {
                    format!("; not run since {}", format::friendly_date(last, &snapshot.taken_at))
                }
                _ => String::new(),
            };
            format!("Query {}{what}: plan {}, {how}{ran}.", p.query_id, p.plan_id)
        })
        .collect();
    if n > FORCED_LISTED {
        lines.push(format!("And {} more.", n - FORCED_LISTED));
    }
    f.caveats.push(format!("Forced plans:\n{}", lines.join("\n")));
    f.queries = kept
        .iter()
        .filter_map(|(p, q, _)| q.map(|q| (p, q)))
        .take(FORCED_LISTED)
        .map(|(p, q)| QueryRef {
            id: q.id.clone(),
            plan_node: None,
            note: Some(format!("Plan {} is forced.", p.plan_id)),
        })
        .collect();
    f.informational();
    f.fix = Some(Fix {
        summary: "Nothing to do while they help. Unforce one whose query or indexes changed since it was forced and \
            compare its duration before and after: SQL Server chooses a plan again."
            .into(),
        sql: Some(
            std::iter::once("-- One at a time, comparing the query's duration before and after".to_string())
                .chain(kept.iter().take(FORCED_LISTED).map(|(p, _, _)| {
                    format!("-- EXEC sp_query_store_unforce_plan @query_id = {}, @plan_id = {};", p.query_id, p.plan_id)
                }))
                .collect::<Vec<_>>()
                .join("\n"),
        ),
        cost: None,
    });
    f.verify = vec!["Re-scan after unforcing one: its query's average duration stays the same or drops.".into()];
    f
}

/// Azure's score below which it expects an index to save little next to what it costs every write.
const LOW_SCORE: f64 = 10.0;

/// An average run under this is fast: no slow read for an index to speed up (the bar from which a plan does real work,
/// `culprit_core::insights`' ten milliseconds).
const SLOW_RUN_MS: f64 = 10.0;

/// Statements listed under a recommendation, the heaviest first; the rest are counted.
const LISTED: usize = 5;

pub(super) fn create_index_advice(
    snapshot: &Snapshot,
    r: &TuningRecommendation,
    ix: &RecommendedIndex,
) -> Option<Finding> {
    if ix.columns.is_empty() {
        return None;
    }
    // The optimizer asks for the same index: the missing-index finding says Azure recommends it too.
    if merge_candidates(snapshot).iter().any(|c| {
        c.schema.eq_ignore_ascii_case(&ix.schema)
            && c.table.eq_ignore_ascii_case(&ix.table)
            && lower_set(&c.keys()) == lower_set(&ix.columns)
    }) {
        return None;
    }
    let table_name = format!("{}.{}", ix.schema, ix.table);
    let columns = format::list(&ix.columns);
    let mut f = finding(
        "tuning-advice",
        &format!("create-index:{table_name}({})", ix.columns.join(",")),
        Category::Indexes,
        subject(SubjectKind::Table, &table_name),
    );
    f.title = format!("Azure SQL recommends an index on {} ({columns})", ix.table);
    let including =
        if ix.include.is_empty() { String::new() } else { format!(", including {}", format::list(&ix.include)) };
    // The statements behind it, where the scan captured them: those that compare every key with a value. One heavy
    // enough, and measured reading pages for nothing, is evidence of its own that the index would pay.
    let filtering = statements_filtering_on(snapshot, ix);
    let keys = if ix.columns.len() == 1 { columns.clone() } else { format!("all of {columns}") };
    let behind = match filtering.len() {
        0 => format!(
            "none of the statements this scan captured compares {keys} with a value: the ones it was recommended for \
             may have changed, stopped running, or not be among them"
        ),
        1 => format!("{} (below) compares {keys} with a value", query_handle(filtering[0])),
        n if n <= LISTED => format!("{n} statements this scan captured (below) compare {keys} with a value"),
        n => format!(
            "{n} statements this scan captured compare {keys} with a value; the {LISTED} that take the most time are \
             below"
        ),
    };
    f.summary = format!(
        "Azure SQL Database's automatic tuning recommends an index on {table_name} ({columns}{including}){}{}. It weighs \
         what the queries that read {} would save against what the index costs every write. The optimizer's own \
         missing-index requests since the last restart don't include it; {behind}.",
        r.valid_since
            .as_deref()
            .map(|s| format!(", since {}", format::friendly_date(s, &snapshot.taken_at)))
            .unwrap_or_default(),
        r.score.map(|s| format!(", and scores it {s:.0} out of 100")).unwrap_or_default(),
        ix.table
    );
    f.queries = query_refs(&filtering.iter().take(LISTED).copied().collect::<Vec<_>>());
    let rows = snapshot.table(&ix.schema, &ix.table).and_then(|t| t.rows);
    // Large values stay out of INCLUDE, as in the optimizer's requests: copying them into an index costs more than the
    // lookups back to the table it spares.
    let mut include = ix.include.clone();
    let large_note = crate::indexes::drop_large_includes(snapshot, &ix.schema, &ix.table, &mut include);
    let large: Vec<String> = ix.include.iter().filter(|c| !include.contains(c)).cloned().collect();
    // An index that leads with the keys may hold the included columns too: then the recommendation predates it (a
    // migration added it since), and it expires on its own; the clustered index holds every column. One that lacks
    // them can be widened instead.
    let same_keys = leading_index(snapshot, &ix.schema, &ix.table, &ix.columns, &ix.columns);
    let lacking: Vec<String> = match same_keys {
        Some(existing) => {
            let held: Vec<&str> =
                existing.key_names().into_iter().chain(existing.include.iter().map(String::as_str)).collect();
            include.iter().filter(|c| !held.iter().any(|h| h.eq_ignore_ascii_case(c))).cloned().collect()
        }
        None => Vec::new(),
    };
    if same_keys.is_some_and(|e| e.is_clustered) || (same_keys.is_some() && lacking.is_empty() && large.is_empty()) {
        return None;
    }
    // Where an index with these keys exists, a statement reading for nothing isn't missing this one.
    let heavy = heavy_reader(snapshot, &filtering).filter(|_| same_keys.is_none());
    // Nor is one whose plan reads the table through another index while one that leads with keys it compares with a
    // value is there: what it reads comes from the plan's choice (the heavy-query finding's to fix), so it is no
    // evidence for creating this one.
    let picked = heavy.as_ref().and_then(|h| picked_elsewhere(snapshot, ix, h));
    let heavy = heavy.filter(|_| picked.is_none());
    f.severity = if r.score.is_some_and(|s| s >= 50.0) { Severity::Medium } else { Severity::Low };
    f.impact.score = base_score(f.severity) + r.score.unwrap_or(0.0) / 10.0;
    f.impact.label = "Recommended by Azure SQL".into();
    f.confidence = Confidence::Medium;
    if let Some(score) = r.score {
        f.metrics.push(Metric::new("Azure's score (0–100)", score, Unit::Count));
    }
    if let Some(rows) = rows {
        f.metrics.push(Metric::new("Rows in table", rows, Unit::Rows));
    }
    // Azure's score weighs what it expects the index to save against its upkeep: a low one is its own word that the
    // index is hardly worth having, unless a heavy statement the scan measured on exactly these keys says otherwise.
    let low = r.score.filter(|s| *s < LOW_SCORE);
    let rated = match (low, &heavy) {
        (Some(s), Some(h)) => format!(
            "Azure scores it {s:.0} out of 100, for what it expects the index to save next to what it costs every \
             write, but {}, query {} below, filters on exactly these columns, takes {} of database time and reads {} \
             pages a run to return {}: an index on them would find its rows directly. ",
            query_handle(h.query),
            h.query.id,
            format::percent(h.share),
            format::count(h.pages),
            if h.rows <= 0.0 { "nothing" } else { "less than a row" }
        ),
        (Some(s), None) => format!(
            "Azure scores it {s:.0} out of 100: it expects the index to save little next to what it costs every \
             write, so it's usually not worth having. "
        ),
        (None, _) => String::new(),
    };
    // Low on Azure's word alone: the advice is conditional, and so is checking it.
    let conditional = low.is_some() && heavy.is_none();
    let target = qualified(Engine::Mssql, &ix.schema, &ix.table);
    let name = ix.name.clone().unwrap_or_else(|| format!("ix_{}_{}", ix.table, ix.columns.join("_")));
    let create = |why: Option<String>| Fix {
        summary: format!(
            "{}{rated}{}",
            why.map(|w| format!("{w} ")).unwrap_or_default(),
            if conditional {
                format!(
                    "If the queries that read {} are slow all the same, create it online, outside the busiest hours.",
                    ix.table
                )
            } else if low.is_some() {
                "Create it online, outside the busiest hours.".to_string()
            } else {
                "Create it online, outside the busiest hours. Azure SQL can also create recommended indexes itself \
                 (automatic tuning's CREATE_INDEX) and drops one that doesn't help."
                    .to_string()
            }
        ),
        // Azure's own script, unless it includes a column left out above.
        sql: Some(r.script.clone().filter(|_| large.is_empty()).unwrap_or_else(|| {
            format!(
                "CREATE NONCLUSTERED INDEX {} ON {target} ({}){} WITH (ONLINE = ON);",
                quote(Engine::Mssql, &name),
                ix.columns.iter().map(|c| quote(Engine::Mssql, c)).collect::<Vec<_>>().join(", "),
                if include.is_empty() {
                    String::new()
                } else {
                    format!(
                        " INCLUDE ({})",
                        include.iter().map(|c| quote(Engine::Mssql, c)).collect::<Vec<_>>().join(", ")
                    )
                }
            )
        })),
        cost: None,
    };
    // The condition a low score leaves ("if the queries are slow all the same") is one the scan can answer: where every
    // statement behind the recommendation runs fast on average, nothing is slow for the index to speed up.
    let fast = (conditional && same_keys.is_none() && picked.is_none() && !filtering.is_empty())
        .then(|| {
            filtering.iter().map(|q| q.mean_time_ms).fold((f64::MAX, 0.0f64), |(lo, hi), t| (lo.min(t), hi.max(t)))
        })
        .filter(|(_, slowest)| *slowest < SLOW_RUN_MS);
    let fix = match (same_keys, picked.as_ref().filter(|_| low.is_some())) {
        (None, None) if let Some((fastest, slowest)) = fast => {
            let average = if format::duration(fastest) == format::duration(slowest) {
                format::duration(slowest)
            } else {
                format!("{}–{}", format::duration(fastest), format::duration(slowest))
            };
            let statements = match filtering.len() {
                1 => format!(
                    "the statement this scan captured that compares {keys} with a value averages {average} a run"
                ),
                n => format!(
                    "the {n} statements this scan captured that compare {keys} with a value average {average} a run"
                ),
            };
            // A key of two values (a bit, a boolean) splits the table in two at most: it narrows little.
            let bits = ix.columns.iter().all(|c| {
                snapshot.columns.iter().any(|col| {
                    col.schema.eq_ignore_ascii_case(&ix.schema)
                        && col.table.eq_ignore_ascii_case(&ix.table)
                        && col.name.eq_ignore_ascii_case(c)
                        && matches!(col.data_type.to_ascii_lowercase().as_str(), "bit" | "boolean" | "bool")
                })
            });
            let narrows = if bits {
                format!(
                    ", and {}, {}: {} at most two values, so an index on {} narrows the rows little",
                    if ix.columns.len() == 1 { "its key" } else { "its keys" },
                    if ix.columns.len() == 1 {
                        format!("{columns}, is a bit column")
                    } else {
                        format!("{columns}, are bit columns")
                    },
                    if ix.columns.len() == 1 { "it holds" } else { "each holds" },
                    if ix.columns.len() == 1 { "it" } else { "them" }
                )
            } else {
                String::new()
            };
            let copies = if ix.include.is_empty() {
                String::new()
            } else {
                format!(
                    "; every write that changes one of the {} columns it includes would update it too",
                    ix.include.len()
                )
            };
            Fix {
                summary: format!(
                    "{rated}Nothing to create: {statements}, so there is no slow read for it to speed up{narrows}{copies}. Let the recommendation expire."
                ),
                sql: None,
                cost: None,
            }
        }
        (Some(existing), _) => {
            // What the index really leads with: its own key order, which needn't be the recommendation's.
            let leads = format::list(
                &existing.key_names()[..ix.columns.len()].iter().map(|k| k.to_string()).collect::<Vec<_>>(),
            );
            // The fix says what the existing index has and what the recommendation would add: no caveat repeats it.
            if lacking.is_empty() {
                Fix {
                    summary: format!(
                        "{rated}Nothing to create: {} already finds these rows by {leads}, and the recommended index \
                         would only copy {} into a second index. Let the recommendation expire.",
                        existing.name,
                        format::list(&large)
                    ),
                    sql: None,
                    cost: None,
                }
            } else {
                let them = if lacking.len() == 1 { "it" } else { "them" };
                f.caveats.push(format!(
                    "{} already leads with {leads}; the recommended index adds {}, which spares the queries that read \
                     {them} the lookups back to the table. Including {them} in {} does the same with one index fewer to \
                     maintain.",
                    existing.name,
                    format::list(&lacking),
                    existing.name
                ));
                match crate::indexes::widen_index(snapshot, existing, &lacking) {
                    crate::indexes::Widening::Sql(fix) => Fix { summary: format!("{rated}{}", fix.summary), ..fix },
                    crate::indexes::Widening::Refused(why) => create(Some(why)),
                }
            }
        }
        // Azure's low score, and the one statement that would say otherwise reads past an index there is: nothing to
        // create, and the heavy-query finding has the fix.
        (None, Some(p)) => Fix {
            summary: format!(
                "{rated}Nothing to create: {} already leads with {}, which query {} compares with a value, but its \
                 plan picks {} expecting {}, and its runs read {} pages each to return {}. The recorded pages do not \
                 establish why that plan was chosen: inspect a representative actual plan before choosing a change \
                 (see heavy-query:{}). Let the recommendation expire.",
                p.other.name,
                format::list(&p.keys),
                p.query.id,
                p.used,
                rows_expected(p.estimate),
                format::count(p.pages),
                if p.rows <= 0.0 { "nothing" } else { "less than a row" },
                p.query.id
            ),
            sql: None,
            cost: None,
        },
        (None, None) => {
            if let Some((other, keys)) = leading_with_some(snapshot, ix) {
                // The heavy statement's plan says which index it reads the table through: the engine's own choice.
                let instead = heavy.as_ref().and_then(|h| {
                    let used = indexes_read_through(h.query, &ix.schema, &ix.table);
                    (!used.is_empty() && !used.iter().any(|u| u.eq_ignore_ascii_case(&other.name)))
                        .then(|| (h.query.id.clone(), format::list(&used)))
                });
                let check = match (instead, &picked) {
                    (Some((id, used)), _) => format!(
                        ", but query {id}'s plan reads {} through {used} instead, so that index isn't serving it.",
                        ix.table
                    ),
                    (None, Some(p)) => format!(
                        ", but query {}'s plan reads {} through {} instead, expecting {}: heavy-query:{} is about \
                         that choice.",
                        p.query.id,
                        ix.table,
                        p.used,
                        rows_expected(p.estimate),
                        p.query.id
                    ),
                    (None, None) => format!(
                        ": check whether the queries that read {} use it before adding an index beside it.",
                        ix.table
                    ),
                };
                f.caveats.push(format!(
                    "{} already leads with {}{}, {} of the {} recommended keys{check}",
                    other.name,
                    format::list(&keys),
                    other
                        .filter
                        .as_deref()
                        .map(|filter| format!(" for the rows where {}", crate::indexes::readable_filter(filter)))
                        .unwrap_or_default(),
                    keys.len(),
                    ix.columns.len(),
                ));
            }
            create(None)
        }
    };
    // Columns left out of INCLUDE matter only for an index worth creating.
    if let Some(note) = large_note.filter(|_| fix.sql.is_some()) {
        f.caveats.push(note);
    }
    let nothing_to_create = fix.sql.is_none() && ((same_keys.is_some() && lacking.is_empty()) || fast.is_some());
    let picked_nothing = fix.sql.is_none() && same_keys.is_none();
    if fix.sql.is_none() {
        f.informational();
    }
    f.fix = Some(fix);
    let fewer_reads = match &heavy {
        Some(h) => format!("query {} reads far fewer than {} pages a run", h.query.id, format::count(h.pages)),
        None => format!("the queries that read {} do fewer reads", ix.table),
    };
    f.verify = vec![if nothing_to_create {
        "Nothing to change: once Azure lets the recommendation expire, this finding goes with it.".to_string()
    } else if let Some(p) = picked.as_ref().filter(|_| picked_nothing) {
        format!(
            "Nothing to create here: gather the actual-plan and IO evidence described in heavy-query:{} first. \
             After a change supported by that evidence, compare equivalent runs of query {} for lower reads and \
             time per run. When Azure lets the recommendation expire, this finding goes with it.",
            p.query.id, p.query.id
        )
    } else if conditional {
        format!("Only if you apply it: re-scan a day later; the recommendation is gone, and {fewer_reads}.")
    } else {
        format!("Re-scan a day later: the recommendation is gone, and {fewer_reads}.")
    }];
    Some(f)
}

/// The heavy-query check's bar: a statement that takes this share of database time is one of the heaviest.
const HEAVY_SHARE: f64 = 0.05;

/// Pages a run from which a read that returns nothing is reading for nothing (the heavy-query check's own bar).
const NOTHING_READS: f64 = 100.0;

/// The statements the scan captured that compare every key of a recommended index with a value (`tenant_id = @p0 AND
/// is_deleted = 0`): those the index would find rows for by its keys. Not monitoring or session statements.
fn statements_filtering_on<'a>(snapshot: &'a Snapshot, ix: &RecommendedIndex) -> Vec<&'a QueryStat> {
    let value = |v: &str| (v.len() > 1 && v.starts_with('@')) || crate::indexes::is_literal(v) || v.starts_with("n'");
    let mut found: Vec<&QueryStat> = queries_touching(snapshot, &ix.schema, &ix.table)
        .into_iter()
        .filter(|q| !is_monitoring_query(q) && !culprit_core::sql::is_session_statement(&q.text))
        .filter(|q| ix.columns.iter().all(|c| crate::indexes::compared_to(&q.text, &ix.table, c, value)))
        .collect();
    found.sort_by(|a, b| b.total_time_ms.total_cmp(&a.total_time_ms));
    found
}

/// A statement behind a recommendation measured heavy and reading for nothing.
struct HeavyReader<'a> {
    query: &'a QueryStat,
    share: f64,
    pages: f64,
    rows: f64,
}

/// Of the statements behind a recommendation, the heaviest that takes a heavy query's share of database time and
/// reads at least [`NOTHING_READS`] pages a run to return less than a row: its filter keeps nothing of what it reads, so
/// an index on its keys finds its rows directly (the share kept is far under `INDEX_KEEPS_AT_MOST`). What the runs
/// measured outweighs Azure's estimate.
fn heavy_reader<'a>(snapshot: &Snapshot, statements: &[&'a QueryStat]) -> Option<HeavyReader<'a>> {
    let workload = snapshot.workload.as_ref()?;
    statements.iter().find_map(|&q| {
        // A heavy query's share as every surface states it (`Workload::statement_share`).
        let share = workload.statement_share(q);
        let (pages, rows) = (q.reads_per_execution()?, q.rows_per_execution()?);
        (!q.writes_rows() && share >= HEAVY_SHARE && pages >= NOTHING_READS && rows < 1.0).then_some(HeavyReader {
            query: q,
            share,
            pages,
            rows,
        })
    })
}

/// A heavy statement behind a recommendation whose plan reads the table through `used`, picked on an estimate of
/// `estimate` rows, while `other` already leads with `keys`, columns it compares with a value.
struct PickedElsewhere<'a> {
    query: &'a QueryStat,
    other: &'a IndexStat,
    keys: Vec<String>,
    used: String,
    estimate: f64,
    pages: f64,
    rows: f64,
}

/// A heavy statement with many recorded pages beside a seek estimated at few rows, while another index leads with a
/// key it compares with a value and the seek doesn't seek on. That index can serve the predicate (unfiltered, or its
/// filter as the statement writes it); the page count proves no row misestimate or reason for the choice. The
/// heavy-query finding asks for actual measurements. `None` without a plan or a seek on the table.
fn picked_elsewhere<'a>(
    snapshot: &'a Snapshot,
    ix: &RecommendedIndex,
    h: &HeavyReader<'a>,
) -> Option<PickedElsewhere<'a>> {
    let seek = first_seek(h.query, &ix.schema, &ix.table)?;
    let used = seek.index.as_deref()?.trim_matches(['[', ']', '"']).to_string();
    let estimate = seek.est_rows_total();
    if estimate * culprit_core::MISESTIMATE_FACTOR > h.pages {
        return None;
    }
    let sought = predicate_columns(seek.seek_predicate.as_deref().unwrap_or_default());
    let is_sought = |c: &str| sought.equality.iter().chain(&sought.range).any(|s| s.eq_ignore_ascii_case(c));
    let recommended = |c: &str| ix.columns.iter().any(|k| k.eq_ignore_ascii_case(c));
    snapshot
        .indexes
        .iter()
        .filter(|i| i.schema.eq_ignore_ascii_case(&ix.schema) && i.table.eq_ignore_ascii_case(&ix.table))
        .filter(|i| !i.is_disabled && !i.name.eq_ignore_ascii_case(&used))
        .filter(|i| i.filter.as_deref().is_none_or(|f| crate::indexes::fits_filter(&ix.table, f, &[h.query])))
        .filter(|i| i.key_names().first().is_some_and(|k| recommended(k) && !is_sought(k)))
        .map(|i| {
            let keys: Vec<String> =
                i.key_names().into_iter().take_while(|k| recommended(k)).map(str::to_string).collect();
            (i, keys)
        })
        .max_by_key(|(i, keys)| (keys.len(), i.filter.is_none(), std::cmp::Reverse(i.columns.len())))
        .map(|(other, keys)| PickedElsewhere {
            query: h.query,
            other,
            keys,
            used,
            estimate,
            pages: h.pages,
            rows: h.rows,
        })
}

/// "1 row", "12 rows": what a plan expects a read to find.
fn rows_expected(rows: f64) -> String {
    let rows = if rows >= 10.0 { rows.round() } else { (rows * 10.0).round() / 10.0 };
    format!("{} {}", format::count(rows), if rows == 1.0 { "row" } else { "rows" })
}

/// The first step of a statement's captured plan on a table, when it seeks an index.
fn first_seek<'a>(q: &'a QueryStat, schema: &str, table: &str) -> Option<&'a culprit_core::PlanNode> {
    q.plan
        .as_ref()?
        .nodes()
        .into_iter()
        .find(|n| {
            n.object
                .as_deref()
                .and_then(split_object)
                .is_some_and(|(s, t)| s.eq_ignore_ascii_case(schema) && t.eq_ignore_ascii_case(table))
                && matches!(n.kind, NodeKind::Seek | NodeKind::Scan)
        })
        .filter(|n| n.kind == NodeKind::Seek && n.index.is_some())
}

/// The indexes a statement's captured plan seeks or scans on a table, in plan order (not the lookups back to it): how
/// the optimizer chose to read it. Empty without a plan.
fn indexes_read_through(q: &QueryStat, schema: &str, table: &str) -> Vec<String> {
    let mut used: Vec<String> = Vec::new();
    for n in q.plan.as_ref().map(|p| p.nodes()).unwrap_or_default() {
        let on_table = n
            .object
            .as_deref()
            .and_then(split_object)
            .is_some_and(|(s, t)| s.eq_ignore_ascii_case(schema) && t.eq_ignore_ascii_case(table));
        if let (true, NodeKind::Seek | NodeKind::Scan, Some(index)) = (on_table, n.kind, n.index.as_deref())
            && !used.iter().any(|u| u.eq_ignore_ascii_case(index))
        {
            used.push(index.to_string());
        }
    }
    used
}

/// The enabled index on the recommendation's table that leads with the most of its keys, in any order, when one leads
/// with any: the queries behind the recommendation may already use it. The unfiltered one first, then the narrowest.
fn leading_with_some<'a>(snapshot: &'a Snapshot, ix: &RecommendedIndex) -> Option<(&'a IndexStat, Vec<String>)> {
    snapshot
        .indexes
        .iter()
        .filter(|i| {
            i.schema.eq_ignore_ascii_case(&ix.schema) && i.table.eq_ignore_ascii_case(&ix.table) && !i.is_disabled
        })
        .filter_map(|i| {
            let keys: Vec<String> = i
                .key_names()
                .into_iter()
                .take_while(|k| ix.columns.iter().any(|c| c.eq_ignore_ascii_case(k)))
                .map(str::to_string)
                .collect();
            (!keys.is_empty()).then_some((i, keys))
        })
        .max_by_key(|(i, keys)| (keys.len(), i.filter.is_none(), std::cmp::Reverse(i.columns.len())))
}

pub(super) fn drop_index_advice(
    snapshot: &Snapshot,
    r: &TuningRecommendation,
    ix: &RecommendedIndex,
) -> Option<Finding> {
    let name = ix.name.as_deref()?;
    // Already gone, or the unused- and duplicate-index findings already say to drop it.
    let existing = snapshot.indexes.iter().find(|i| {
        i.schema.eq_ignore_ascii_case(&ix.schema)
            && i.table.eq_ignore_ascii_case(&ix.table)
            && i.name.eq_ignore_ascii_case(name)
    })?;
    if is_unused(snapshot, existing) || redundant_indexes(snapshot).iter().any(|(a, _)| std::ptr::eq(*a, existing)) {
        return None;
    }
    let table_name = format!("{}.{}", ix.schema, ix.table);
    let mut f = finding(
        "tuning-advice",
        &format!("drop-index:{table_name}.{}", existing.name),
        Category::Indexes,
        subject(SubjectKind::Index, format!("{table_name}.{}", existing.name)),
    );
    f.title = format!("Azure SQL recommends dropping {} on {}", existing.name, ix.table);
    f.summary = format!(
        "Azure SQL Database's automatic tuning recommends dropping {}{}{}, judging from a longer history than the usage \
         counters here{}.",
        existing.name,
        r.reason.as_deref().filter(|s| !s.is_empty()).map(|s| format!(" ({s})")).unwrap_or_default(),
        r.valid_since
            .as_deref()
            .map(|s| format!(" since {}", format::friendly_date(s, &snapshot.taken_at)))
            .unwrap_or_default(),
        existing
            .reads
            .map(|reads| format!(", which count {} reads of it {}", format::count(reads), since_phrase(snapshot)))
            .unwrap_or_default()
    );
    f.severity = Severity::Low;
    f.impact.score = base_score(f.severity) + r.score.unwrap_or(0.0) / 20.0;
    f.impact.label = "Recommended by Azure SQL".into();
    f.confidence = Confidence::Medium;
    if let Some(size) = existing.size_bytes {
        f.metrics.push(Metric::new("Size", size, Unit::Bytes));
    }
    if let Some(reads) = existing.reads {
        f.metrics.push(Metric::new("Reads", reads, Unit::Count));
    }
    let target = qualified(Engine::Mssql, &existing.schema, &existing.table);
    f.fix = Some(Fix {
        summary:
            "Disable it first: the definition stays, nothing reads or maintains it, and a rebuild brings it back. \
            Drop it after a full business cycle without complaints."
                .into(),
        sql: Some(format!("ALTER INDEX {} ON {target} DISABLE;", quote(Engine::Mssql, &existing.name))),
        cost: None,
    });
    if let Some(definition) = &existing.definition {
        f.caveats.push(format!("Its definition, to recreate it:\n{definition}"));
    }
    f.verify = vec![format!("Watch the queries on {} for a week after disabling; none should get slower.", ix.table)];
    Some(f)
}

pub(super) fn failure_hint(reason: &str) -> &'static str {
    let r = reason.to_ascii_uppercase();
    if r.contains("NO_INDEX") {
        "An index the plan uses no longer exists."
    } else if r.contains("NO_PLAN") || r.contains("GENERAL_FAILURE") {
        "The query or schema changed so the plan can no longer be produced."
    } else if r.contains("TIME_OUT") || r.contains("TIMEOUT") {
        "Compiling with the forced plan took too long."
    } else {
        ""
    }
}

#[cfg(test)]
mod output_tests {
    use super::*;
    #[test]
    fn captured_output_column_names_are_not_split_into_qualifiers_or_csv_fields() {
        let mut snapshot: Snapshot =
            serde_json::from_str(include_str!("../../../../../fixtures/mssql-demo.json")).unwrap();
        let mut index = snapshot.indexes[0].clone();
        index.is_clustered = false;
        index.columns = vec!["id".into()];
        index.include = vec!["odd.column".into(), "a, b".into(), "two words".into()];
        snapshot.indexes = vec![index.clone()];
        assert!(
            uncovered(&snapshot, &index, &["id".into(), "odd.column".into(), "a, b".into(), "two words".into()])
                .is_empty()
        );
        assert_eq!(uncovered(&snapshot, &index, &["uncovered.column".into()]), vec!["uncovered.column"]);
    }
}
