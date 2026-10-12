//! What the engine already knows: SQL Server's own tuning recommendations, forced plans that stopped applying, and
//! statements the application sends with literals baked in (so each is compiled and tracked on its own).

use culprit_core::{
    Capability, Category, CheckInfo, Confidence, Engine, Finding, Fix, Metric, QueryRef, Severity, Snapshot,
    SubjectKind, Unit, format,
};

use crate::support::*;

pub fn tuning_advice_info() -> CheckInfo {
    CheckInfo {
        id: "tuning-advice".into(),
        title: "SQL Server's own tuning advice".into(),
        category: Category::Queries,
        engines: vec![Engine::Mssql],
        requires: vec![Capability::QueryStore],
        description:
            "Plan-forcing recommendations from automatic tuning, forced plans and Query Store hints that stopped \
            applying, and automatic plan correction left off."
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

pub fn tuning_advice(snapshot: &Snapshot) -> Vec<Finding> {
    let Some(tuning) = &snapshot.tuning else { return Vec::new() };
    let db = quote(Engine::Mssql, &snapshot.server.database);
    let mut out = Vec::new();

    for r in tuning.recommendations.iter().filter(|r| {
        r.state.as_deref().is_none_or(|s| matches!(s.to_ascii_lowercase().as_str(), "active" | "verifying" | "pending"))
    }) {
        let (Some(query_id), Some(plan_id)) = (&r.query_id, &r.recommended_plan_id) else { continue };
        let q = snapshot.workload.as_ref().and_then(|w| w.query(query_id));
        let mut f = finding(
            "tuning-advice",
            &format!("force:{query_id}"),
            Category::Queries,
            subject(SubjectKind::Query, format!("query {query_id}")),
        );
        let gain = r.estimated_gain.map(|g| format!(" (estimated {} faster)", format::percent(g))).unwrap_or_default();
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
            f.metrics.push(Metric::new("Estimated gain", g, Unit::Ratio));
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
            subject(SubjectKind::Query, format!("query {}", p.query_id)),
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

    for h in tuning.hints.iter().filter(|h| h.failures > 0.0) {
        let mut f = finding(
            "tuning-advice",
            &format!("hint-failing:{}", h.query_id),
            Category::Queries,
            subject(SubjectKind::Query, format!("query {}", h.query_id)),
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
        let regressions = !tuning.recommendations.is_empty();
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
                tuning.recommendations.len(),
                if tuning.recommendations.len() == 1 { "regression" } else { "regressions" }
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
        let mut f = finding(
            "query-family",
            &family.key.chars().take(40).collect::<String>(),
            Category::Queries,
            subject(SubjectKind::Query, family.key.clone()),
        );
        let handle = statement_handle(&family.sample);
        let (title, why) = match engine {
            Engine::Mssql => (
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
        f.fix = Some(match engine {
            Engine::Mssql => Fix {
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
            if snapshot.server.cloud.is_some() {
                f.caveats.push("On Azure SQL Database the setting is managed per database with ALTER DATABASE SCOPED CONFIGURATION SET OPTIMIZE_FOR_AD_HOC_WORKLOADS = ON.".into());
            }
            out.push(f);
        }
    }
    out
}

fn capitalize(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(c) => c.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}
