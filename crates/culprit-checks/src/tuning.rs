//! What the engine already knows: SQL Server's own tuning recommendations, forced plans that stopped applying, and
//! statements the application sends with literals baked in (so each is compiled and tracked on its own).

use culprit_core::{
    Capability, Category, CheckInfo, Confidence, Engine, Finding, Fix, ForcedPlan, IndexStat, Metric, NodeKind,
    QueryRef, QueryStat, RecommendedIndex, Severity, Snapshot, SubjectKind, TuningRecommendation, Unit, format,
};

use crate::indexes::{is_unused, leading_index, merge_candidates, redundant_indexes, split_object};
use crate::support::*;

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

/// A day or more between `last` and the end of the scan's data, as (seconds, `last`).
fn quiet_since<'a>(snapshot: &Snapshot, last: Option<&'a str>) -> Option<(f64, &'a str)> {
    let last = last?;
    let end = snapshot.workload.as_ref()?.window_end.as_deref().and_then(parse_epoch_seconds)?;
    let silence = end - parse_epoch_seconds(last)?;
    (silence >= 86_400.0).then_some((silence, last))
}

/// Waiting for someone to apply it. SQL Server applied the others itself (Verifying, Success) or let them go
/// (Reverted, Expired): a plan it forced shows among the forced plans instead.
fn waiting(r: &TuningRecommendation) -> bool {
    r.state.as_deref().is_none_or(|s| matches!(s.to_ascii_lowercase().as_str(), "active" | "pending"))
}

/// A plan-forcing recommendation still waiting to be applied.
fn waiting_plan_fix(r: &TuningRecommendation) -> bool {
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
                f.severity = severity_for_share(impact.share).max(Severity::Medium);
            }
            None => {
                f.severity = Severity::Medium;
                f.impact.score = base_score(f.severity);
                f.impact.label = "Recommended by SQL Server".into();
            }
        }
        f.confidence = Confidence::High;
        if let Some(g) = r.estimated_gain {
            f.metrics.push(Metric::new("Estimated CPU saved per run", g, Unit::Ratio));
        }
        f.metrics.push(Metric::text("Recommended plan", plan_id.clone()));
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
                    "Query {query_id} has not run since {} ({} before this scan's data ends). A deploy most likely \
                     changed it; check that the current version of this code doesn't have the same problem.",
                    last.get(..16).unwrap_or(last).replace('T', " "),
                    format::duration(silence * 1000.0)
                ),
            );
        }
        f.fix = Some(Fix {
            summary:
                "Force the recommended plan (copy and run it yourself; Culprit never runs it). To let SQL Server do \
                this on its own, turn on automatic plan correction."
                    .into(),
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
                    "Query {} has not run since {} ({} before this scan's data ends). A deploy most likely \
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
struct SeekableScan<'a> {
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

/// Tables of this many rows or more: reading one in full where a seek would do costs something.
const SCAN_ROWS: f64 = 10_000.0;
/// An index that doesn't hold every column the query reads costs a lookup per row it finds: worth it over a scan only
/// for a filter far narrower than an index's usual bar.
const LOOKUPS_KEEP_AT_MOST: f64 = 0.01;

fn seekable_scan<'a>(snapshot: &'a Snapshot, q: &QueryStat, plan_id: &str) -> Option<SeekableScan<'a>> {
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
        let lookups = match node.props.get("OutputList") {
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
fn uncovered(snapshot: &Snapshot, index: &IndexStat, output: &str) -> Vec<String> {
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
        .split(',')
        .map(|c| c.trim().rsplit('.').next().unwrap_or_default().trim_matches(['[', ']', '"']).to_string())
        .filter(|c| !c.is_empty() && !c.starts_with("Expr") && !held.contains(&c.to_ascii_lowercase()))
        .collect()
}

fn forced_scan_finding(
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
    // database dated a change to its table), and not to before the forcing.
    let before_forcing = |at: &str| match (parse_epoch_seconds(at), forced_at.and_then(parse_epoch_seconds)) {
        (Some(created), Some(forced)) => created <= forced,
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
            f.severity = severity_for_share(impact.share).max(Severity::Medium);
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
const FORCED_LISTED: usize = 12;

fn forced_plans_finding(snapshot: &Snapshot, kept: &[(&ForcedPlan, Option<&QueryStat>, String)]) -> Finding {
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
    f.summary = format!(
        "SQL Server runs {} with the plan forced for {} instead of choosing one, even after an index is added or the \
         data grows. That holds a query that once regressed to its good plan, but once the query or its indexes \
         change, the forced plan can become the slower one without anything saying so.{automatic_sentence}",
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

fn create_index_advice(snapshot: &Snapshot, r: &TuningRecommendation, ix: &RecommendedIndex) -> Option<Finding> {
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
    let include =
        if ix.include.is_empty() { String::new() } else { format!(", including {}", format::list(&ix.include)) };
    f.summary = format!(
        "Azure SQL Database's automatic tuning recommends an index on {table_name} ({columns}{include}){}{}. It weighs \
         what the queries that read {} would save against what the index costs every write. The optimizer's own \
         missing-index requests since the last restart don't include it, so check that those queries still run as \
         they did.",
        r.valid_since
            .as_deref()
            .map(|s| format!(", since {}", format::friendly_date(s, &snapshot.taken_at)))
            .unwrap_or_default(),
        r.score.map(|s| format!(", and scores it {s:.0} out of 100")).unwrap_or_default(),
        ix.table
    );
    let rows = snapshot.table(&ix.schema, &ix.table).and_then(|t| t.rows);
    // An index that leads with the keys may hold the included columns too: then the recommendation predates it (a
    // migration added it since), and it expires on its own. One that lacks them can be widened instead.
    let same_keys = leading_index(snapshot, &ix.schema, &ix.table, &ix.columns, &ix.columns);
    let lacking: Vec<String> = match same_keys {
        Some(existing) => {
            let held: Vec<&str> =
                existing.key_names().into_iter().chain(existing.include.iter().map(String::as_str)).collect();
            ix.include.iter().filter(|c| !held.iter().any(|h| h.eq_ignore_ascii_case(c))).cloned().collect()
        }
        None => Vec::new(),
    };
    if same_keys.is_some() && lacking.is_empty() {
        return None;
    }
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
    if let Some(existing) = same_keys {
        let them = if lacking.len() == 1 { "it" } else { "them" };
        f.caveats.insert(
            0,
            format!(
                "{} already leads with {columns}; the recommended index adds {}, which spares the queries that read \
                 {them} the lookups back to the table. Including {them} in {} does the same with one index fewer to \
                 maintain.",
                existing.name,
                format::list(&lacking),
                existing.name
            ),
        );
    }
    let target = qualified(Engine::Mssql, &ix.schema, &ix.table);
    let name = ix.name.clone().unwrap_or_else(|| format!("ix_{}_{}", ix.table, ix.columns.join("_")));
    f.fix = Some(Fix {
        summary: "Create it online, outside the busiest hours. Azure SQL can also create recommended indexes itself \
            (automatic tuning's CREATE_INDEX) and drops one that doesn't help."
            .into(),
        sql: Some(r.script.clone().unwrap_or_else(|| {
            format!(
                "CREATE NONCLUSTERED INDEX {} ON {target} ({}){} WITH (ONLINE = ON);",
                quote(Engine::Mssql, &name),
                ix.columns.iter().map(|c| quote(Engine::Mssql, c)).collect::<Vec<_>>().join(", "),
                if ix.include.is_empty() {
                    String::new()
                } else {
                    format!(
                        " INCLUDE ({})",
                        ix.include.iter().map(|c| quote(Engine::Mssql, c)).collect::<Vec<_>>().join(", ")
                    )
                }
            )
        })),
        cost: None,
    });
    f.verify = vec![format!(
        "Re-scan a day later: the recommendation is gone, and the queries that read {} do fewer reads.",
        ix.table
    )];
    Some(f)
}

fn drop_index_advice(snapshot: &Snapshot, r: &TuningRecommendation, ix: &RecommendedIndex) -> Option<Finding> {
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

fn failure_hint(reason: &str) -> &'static str {
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

pub fn query_family_info() -> CheckInfo {
    CheckInfo {
        id: "query-family".into(),
        title: "Statements with literals baked in".into(),
        category: Category::Queries,
        engines: vec![Engine::Mssql, Engine::Postgres],
        requires: vec![],
        description:
            "The same statement sent many times with different literal values (or value-list lengths), so the \
            database compiles and tracks each copy separately; and plan caches filled with single-use plans."
                .into(),
    }
}

pub fn query_family(snapshot: &Snapshot) -> Vec<Finding> {
    let engine = snapshot.server.engine;
    let mut out = Vec::new();
    let threshold = if engine == Engine::Mssql { 20.0 } else { 5.0 };
    let total_ms = snapshot.workload.as_ref().map(|w| w.total_time_ms).unwrap_or(0.0);
    for family in snapshot.query_families.iter().filter(|f| f.variants >= threshold) {
        // What the copies differ in decides the fix; copies whose text is the same are split by something no
        // parameterization changes (the role that ran them, the schema a name resolved in, the module they're in).
        let Some(split) = family_split(snapshot, family) else { continue };
        let mut f = finding(
            "query-family",
            &family.key.chars().take(40).collect::<String>(),
            Category::Queries,
            subject(SubjectKind::Query, family.key.clone()),
        );
        let handle = statement_handle(without_declaration(&family.sample));
        let (title, why) = match (&split, engine) {
            (FamilySplit::Declarations(examples), _) => (
                format!(
                    "The app sends {} versions of {} that differ only in how their parameters are declared",
                    format::count(family.variants),
                    handle
                ),
                format!(
                    "because their parameters are declared differently from call to call ({}): drivers do this when a \
                     string parameter's size comes from each value (ADO.NET's AddWithValue, for one). SQL Server \
                     compiles and keeps a plan for each declaration",
                    match examples.as_slice() {
                        [one] => format!("this one declares `{one}`"),
                        _ => examples.iter().map(|e| format!("`{e}`")).collect::<Vec<_>>().join(" and "),
                    }
                ),
            ),
            (_, Engine::Mssql) => (
                format!(
                    "The app sends {} versions of {} that differ only in literal values",
                    format::count(family.variants),
                    handle
                ),
                "so SQL Server compiles each one separately and keeps a plan for each".to_string(),
            ),
            _ => (
                format!("{} is tracked as {} separate statements", capitalize(&handle), format::count(family.variants)),
                "because its value lists (IN lists, multi-row VALUES) have different lengths; each length is planned \
                 and counted on its own"
                    .to_string(),
            ),
        };
        f.title = title;
        f.summary = format!(
            "{} variants of the same statement ran {} times{}, {}{}. Statistics split across the copies too, so none \
             of them looks as heavy as the statement really is.",
            format::count(family.variants),
            format::count(family.executions),
            if total_ms > 0.0 {
                format!(" for {} of database time", format::percent((family.total_time_ms / total_ms).clamp(0.0, 1.0)))
            } else {
                String::new()
            },
            why,
            family
                .cached_bytes
                .filter(|b| *b >= 1024.0 * 1024.0)
                .map(|b| format!(" ({} of plan cache)", format::bytes(b)))
                .unwrap_or_default()
        );
        let share = if total_ms > 0.0 { family.total_time_ms / total_ms } else { 0.0 };
        f.severity = if family.variants >= 1000.0 || share >= 0.1 { Severity::Medium } else { Severity::Low };
        f.impact.score = base_score(f.severity) + family.variants.log10().max(0.0) * 3.0 + share * 20.0;
        f.impact.label = format!("{} copies", format::count(family.variants));
        f.impact.db_time_share = (share > 0.0).then_some(share);
        f.confidence = Confidence::High;
        f.metrics.push(Metric::new("Variants", family.variants, Unit::Count));
        f.metrics.push(Metric::new("Executions", family.executions, Unit::Count));
        if let Some(b) = family.cached_bytes {
            f.metrics.push(Metric::new("Plan cache", b, Unit::Bytes));
        }
        f.queries = family
            .query_ids
            .iter()
            .filter(|id| snapshot.workload.as_ref().is_some_and(|w| w.query(id).is_some()))
            .map(|id| QueryRef { id: id.clone(), plan_node: None, note: None })
            .collect();
        f.fix = Some(match (&split, engine) {
            (FamilySplit::Declarations(_), _) => Fix {
                summary: "Declare each string parameter with a fixed size, the column's length, instead of letting the \
                    driver size it from the value (in ADO.NET, set SqlParameter.Size or use Add with a SqlDbType and \
                    size rather than AddWithValue; other drivers have a similar setting). Then one plan serves every \
                    value. Forced parameterization can't help: these statements are already parameterized."
                    .into(),
                sql: None,
                cost: None,
            },
            (_, Engine::Mssql) => Fix {
                summary: "Send values as parameters (sp_executesql, or your driver's parameterized queries) so one plan \
                    serves every value. If you can't change the app, forced parameterization does it for the whole \
                    database."
                    .into(),
                sql: Some(format!(
                    "-- Prefer fixing the app. As a database-wide fallback:\nALTER DATABASE {} SET PARAMETERIZATION FORCED;",
                    quote(engine, &snapshot.server.database)
                )),
                cost: Some("Forced parameterization can make some queries pick a plan that suits one value badly (parameter sensitivity); watch the regression findings after switching.".into()),
            },
            _ => Fix {
                summary: "Pass value lists as one array parameter (`= ANY($1)` in Postgres) instead of a growing IN list, \
                    and batch multi-row inserts with UNNEST or COPY."
                    .into(),
                sql: None,
                cost: None,
            },
        });
        if matches!(&split, FamilySplit::Unknown) || matches!(&split, FamilySplit::Declarations(e) if e.len() < 2) {
            f.confidence = Confidence::Medium;
            f.caveats.push(
                "Culprit sees one of these statements' text, so what the others differ in is inferred from it.".into(),
            );
        }
        f.verify = vec!["Re-scan after a deploy: this family shrinks to one or a few statements.".into()];
        if !family.sample.is_empty() {
            f.caveats.push(format!("One of them, as recorded:\n{}", family.sample.trim()));
        }
        out.push(f);
    }

    if let Some(cache) = &snapshot.plan_cache
        && cache.total_bytes > 0.0
    {
        let share = cache.single_use_bytes / cache.total_bytes;
        if cache.single_use_bytes >= 256.0 * 1024.0 * 1024.0 && share >= 0.25 {
            let ad_hoc_on = snapshot.setting("optimize for ad hoc workloads").is_some_and(|s| s.value.trim() == "1");
            let mut f = finding(
                "query-family",
                "plan-cache",
                Category::Configuration,
                subject(SubjectKind::Setting, "plan cache"),
            );
            f.title = format!(
                "Plans used once fill {} of the plan cache ({})",
                format::percent(share),
                format::bytes(cache.single_use_bytes)
            );
            f.summary = format!(
                "{} plans in the cache were compiled for a statement that ran exactly once. They take memory that could \
                 hold data pages and reusable plans, and every one cost a compilation.{}",
                format::count(cache.single_use_plans),
                if ad_hoc_on {
                    ""
                } else {
                    " 'Optimize for ad hoc workloads' is off, so each keeps a full plan instead of a small stub."
                }
            );
            f.severity = if share >= 0.5 { Severity::Medium } else { Severity::Low };
            f.impact.score = base_score(f.severity) + share * 10.0;
            f.impact.label = format!("{} single-use", format::bytes(cache.single_use_bytes));
            f.confidence = Confidence::High;
            f.metrics.push(Metric::new("Single-use plans", cache.single_use_plans, Unit::Count));
            f.metrics.push(Metric::new("Their memory", cache.single_use_bytes, Unit::Bytes));
            f.metrics.push(Metric::new("Share of the cache", share, Unit::Ratio));
            f.fix = Some(Fix {
                summary: if ad_hoc_on {
                    "Parameterize the statements behind it (see the families above).".into()
                } else {
                    "Keep only a stub for a plan's first use, and parameterize the statements behind it.".into()
                },
                sql: (!ad_hoc_on).then(|| {
                    "EXEC sp_configure 'show advanced options', 1; RECONFIGURE;\nEXEC sp_configure 'optimize for ad hoc workloads', 1; RECONFIGURE;".into()
                }),
                cost: None,
            });
            f.verify = vec!["Re-scan after a day: single-use plans take a much smaller share of the cache.".into()];
            if snapshot.server.cloud.as_deref() == Some("Azure SQL Database") {
                f.caveats.push("On Azure SQL Database the setting is managed per database with ALTER DATABASE SCOPED CONFIGURATION SET OPTIMIZE_FOR_AD_HOC_WORKLOADS = ON.".into());
            }
            out.push(f);
        }
    }
    out
}

/// What the copies of a statement family differ in, as far as their texts show.
#[derive(Debug, PartialEq)]
enum FamilySplit {
    /// Literal values (SQL Server) or value-list lengths (PostgreSQL): what parameterizing fixes.
    Values,
    /// The same parameterized statement with its parameters declared differently (sizes taken from each value).
    Declarations(Vec<String>),
    /// Inferred from the one text Culprit has.
    Unknown,
}

/// None when the copies are the same text: the split is by who ran them (PostgreSQL keeps an entry per role), the
/// schema a name resolved in, the module they belong to or the connection's settings, which no change to how values
/// are sent fixes.
fn family_split(snapshot: &Snapshot, family: &culprit_core::QueryFamily) -> Option<FamilySplit> {
    let members: Vec<&QueryStat> = snapshot
        .workload
        .as_ref()
        .map(|w| family.query_ids.iter().filter_map(|id| w.query(id)).collect())
        .unwrap_or_default();
    if members.len() >= 2 {
        let first = normalized(&members[0].text);
        if members.iter().any(|q| normalized(&q.text) != first) {
            return Some(FamilySplit::Values);
        }
        let mut declarations: Vec<String> = Vec::new();
        for d in members.iter().filter_map(|q| q.parameters.as_deref()) {
            if !declarations.iter().any(|x| x == d) {
                declarations.push(d.to_string());
            }
        }
        return (declarations.len() >= 2)
            .then(|| FamilySplit::Declarations(declarations.into_iter().take(2).collect()));
    }
    match snapshot.server.engine {
        Engine::Mssql => match split_declaration(&family.sample) {
            // Parameters the application named (`@p0`, `@P1`, `@id`) and no literal left in the text: the copies can't
            // differ in values, so the declarations are what differs.
            (Some(declaration), body) if !auto_parameterized(declaration) && !has_literals(body) => {
                Some(FamilySplit::Declarations(vec![declaration.trim().to_string()]))
            }
            _ => Some(FamilySplit::Unknown),
        },
        // Entries are grouped with their value lists collapsed: without a list, the copies are the same text.
        _ => has_value_list(&family.sample).then_some(FamilySplit::Values),
    }
}

/// `(@p0 nvarchar(7),@p1 int)SELECT …` as (declaration, statement), the way Query Store keeps a parameterized text.
fn split_declaration(text: &str) -> (Option<&str>, &str) {
    let trimmed = text.trim_start();
    if !trimmed.starts_with('(') || !trimmed[1..].trim_start().starts_with('@') {
        return (None, text);
    }
    let mut depth = 0usize;
    for (i, c) in trimmed.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return (Some(&trimmed[1..i]), trimmed[i + 1..].trim_start());
                }
            }
            _ => {}
        }
    }
    (None, text)
}

fn without_declaration(text: &str) -> &str {
    split_declaration(text).1
}

/// SQL Server's own parameterization names parameters by number (`@1`, `@2`): the application sent literals.
fn auto_parameterized(declaration: &str) -> bool {
    declaration
        .split(',')
        .any(|d| d.trim().strip_prefix('@').is_some_and(|n| n.starts_with(|c: char| c.is_ascii_digit())))
}

/// A string or number literal in a statement (outside names, parameters and `TOP (n)`).
fn has_literals(body: &str) -> bool {
    let lower = body.to_ascii_lowercase();
    let chars: Vec<char> = lower.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '\'' {
            return true;
        }
        if c == '[' {
            while i < chars.len() && chars[i] != ']' {
                i += 1;
            }
        } else if c.is_ascii_digit() {
            let prev = i.checked_sub(1).map(|p| chars[p]);
            let in_name = prev.is_some_and(|p| p.is_ascii_alphanumeric() || matches!(p, '_' | '@' | '#' | '$'));
            let before: String = chars[..i].iter().collect();
            let top = before.trim_end().trim_end_matches('(').trim_end().ends_with("top");
            if !in_name && !top {
                return true;
            }
            while i < chars.len() && (chars[i].is_ascii_alphanumeric() || chars[i] == '.') {
                i += 1;
            }
            continue;
        }
        i += 1;
    }
    false
}

/// An IN list, multi-row VALUES or ARRAY of parameters: what PostgreSQL entries of different lengths come from.
fn has_value_list(text: &str) -> bool {
    let t = text.to_ascii_lowercase().split_whitespace().collect::<Vec<_>>().join(" ").replace("( ", "(");
    [" in ($", " in($", "values ($", "values($", "array[$", "any(array[$"].iter().any(|m| t.contains(m))
}

/// Statement text without comments and with whitespace collapsed, to compare two copies.
fn normalized(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while !rest.is_empty() {
        if let Some(after) = rest.strip_prefix("/*") {
            rest = after.split_once("*/").map_or("", |(_, r)| r);
        } else if let Some(after) = rest.strip_prefix("--") {
            rest = after.split_once('\n').map_or("", |(_, r)| r);
        } else {
            let c = rest.chars().next().expect("non-empty");
            out.push(c);
            rest = &rest[c.len_utf8()..];
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn capitalize(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(c) => c.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}
