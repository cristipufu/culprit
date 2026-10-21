//! The `tuning-advice` check: Plan-forcing and index recommendations from automatic tuning, forced plans (those that stopped applying, and those that scan where an index added since could seek), Query Store hints that stopped applying, and automatic plan correction left off.

use super::*;
use crate::evidence::{Observation, Requirement as Evidence};

mod forced_plans;
mod index_advice;

pub(crate) use index_advice::recommended_create;

use forced_plans::{failure_hint, forced_plans_finding, forced_scan_finding, seekable_scan};
use index_advice::{create_index_advice, drop_index_advice};

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

check!(
    TuningAdviceCheck: tuning_advice_info,
    tuning_advice,
    &[Evidence::SourceWithLegacy(culprit_core::source_id::TUNING, Observation::Tuning)]
);

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
