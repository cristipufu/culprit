//! Plan insights as findings: the plan-level problems in queries that matter, beyond what `plan-warnings`,
//! `filtered-scan` and `missing-index` already report. One finding per problem and table, however many queries share it.

use crate::evidence::{QueryMeasure, Requirement as Evidence};
use culprit_core::insights::PlanInsight;
use culprit_core::{
    Capability, Category, CheckInfo, Confidence, Engine, Finding, Fix, Metric, PlanKind, QueryRef, QueryStat, Severity,
    Snapshot, SubjectKind, Unit, format,
};

use crate::support::*;

/// Rules whose findings other checks already produce, with the evidence and wording users know. A plan's own
/// missing-index request isn't one of them: `missing-index` reads the server's list of requests, which a restart
/// empties, so the plan's request is a finding of its own until refine finds a `missing-index` or `filtered-scan`
/// finding for the same table and key ("a plan's index request is the finding for its table and key").
const COVERED: &[&str] = &["implicit-conversion", "spill", "no-join-predicate", "missing-statistics", "scan-filter"];

/// Queries below this share of database time aren't worth a finding, however odd their plan.
pub(crate) const MIN_SHARE: f64 = 0.01;

/// The engine's own verdict, shown on a plan without costs to say how much it matters (SQLite builds an index for itself
/// on every run): Low on the plan alone, weighed here by the statement's share of database time.
const WEIGHED_BY_THE_STATEMENT: &[&str] = &["automatic-index"];

pub fn plan_insights_info() -> CheckInfo {
    CheckInfo {
        id: "plan-insights".into(),
        title: "Plan insights".into(),
        category: Category::Queries,
        engines: vec![Engine::Mssql, Engine::Postgres, Engine::Mysql, Engine::Sqlite],
        requires: vec![Capability::QueryPlans],
        description: "Key lookups, functions on filtered columns, leading wildcards, NOT IN, scalar functions, eager spools, misestimates and repeated loops in the plans of queries that matter.".into(),
    }
}

check!(PlanInsightsCheck: plan_insights_info, plan_insights, &[Evidence::Query(QueryMeasure::CapturedPlans)]);

pub fn plan_insights(snapshot: &Snapshot) -> Vec<Finding> {
    plan_insights_for_queries(snapshot, app_queries(snapshot))
}

/// Rebuild a grouped observation from selected statements while retaining the snapshot's original workload,
/// catalog and measurement context. Refinement never clones or narrows the database-time denominator.
pub(crate) fn plan_insights_for_queries<'a>(
    snapshot: &Snapshot,
    queries: impl IntoIterator<Item = &'a QueryStat>,
) -> Vec<Finding> {
    let mut out: Vec<Finding> = Vec::new();
    for q in queries {
        let Some(plan) = &q.plan else { continue };
        let Some(impact) = query_impact(snapshot, &[q]) else { continue };
        if impact.share < MIN_SHARE {
            continue;
        }
        for i in crate::views::plan_advice::catalog_insights(snapshot, q, plan) {
            let weighed_here = WEIGHED_BY_THE_STATEMENT.contains(&i.rule.as_str());
            if COVERED.contains(&i.rule.as_str()) || (i.severity == Severity::Low && !weighed_here) {
                continue;
            }
            // An insight about the whole plan names no step of it, nor the table of the root it points at: it is the
            // query's.
            let step = i.nodes.first().copied().filter(|_| !whole_plan(plan, &i));
            let object = plan.nodes().into_iter().find(|n| Some(n.id) == step).and_then(|n| n.object.clone());
            let key = format!(
                "{}:{}",
                i.rule,
                object.as_deref().map(str::to_ascii_lowercase).unwrap_or_else(|| format!("query-{}", q.id))
            );
            let reference = QueryRef { id: q.id.clone(), plan_node: step, note: Some(rule_label(&i)) };
            match out.iter_mut().find(|f| f.id == format!("plan-insights:{key}")) {
                Some(existing) => {
                    if !existing.queries.iter().any(|r| r.id == q.id) {
                        existing.queries.push(reference);
                    }
                }
                None => out.push(build(snapshot, q, &i, &key, object, reference, plan)),
            }
        }
    }
    for f in out.iter_mut() {
        let queries: Vec<&QueryStat> =
            f.queries.iter().filter_map(|r| snapshot.workload.as_ref()?.query(&r.id)).collect();
        let Some(impact) = query_impact(snapshot, &queries) else { continue };
        apply_query_impact(f, &impact);
        f.metrics.retain(|m| m.label != "Share of DB time");
        f.metrics.insert(0, Metric::new("Share of DB time", impact.share, Unit::Ratio));
        if queries.len() > 1 {
            f.summary.push_str(&format!(
                " The same problem shows up in {} queries that together take {} of database time.",
                queries.len(),
                format::percent(impact.share)
            ));
            f.metrics.insert(1, Metric::new("Queries affected", queries.len() as f64, Unit::Count));
        }
    }
    out
}

fn build(
    snapshot: &Snapshot,
    q: &QueryStat,
    i: &PlanInsight,
    key: &str,
    object: Option<String>,
    reference: QueryRef,
    plan: &culprit_core::Plan,
) -> Finding {
    let actual = plan.kind == PlanKind::Actual;
    let weighed = plan.weighed();
    let subject = match &object {
        Some(o) => subject(SubjectKind::Table, o.clone()),
        None => subject(SubjectKind::Query, q.id.clone()),
    };
    let mut f = finding("plan-insights", key, Category::Queries, subject);
    let share = query_impact(snapshot, &[q]).map_or(0.0, |impact| impact.share);
    // A plan problem matters as much as the query it slows down: a 1% query never becomes a top finding, and one that
    // costs seconds a day is Low whatever its share.
    let by_share = match severity_for_share(share) {
        Severity::Low => Severity::Medium,
        _ => Severity::High,
    };
    // What that share comes to a day, on the basis it is on (Culprit's scans where they measured it).
    let cap = by_share.min(amount_severity_of(snapshot, &[q], share));
    // An engine verdict the plan can't weigh (SQLite's automatic index) takes the statement's weight, at most Medium:
    // the plan doesn't say how much of the run the throwaway index costs.
    f.severity = if WEIGHED_BY_THE_STATEMENT.contains(&i.rule.as_str()) {
        cap.min(Severity::Medium)
    } else {
        i.severity.min(cap)
    };
    f.confidence = if actual { Confidence::High } else { Confidence::Medium };
    f.title = i.title.clone();
    f.summary = format!("{} Seen in {}.", i.detail, query_handle(q));
    f.queries = vec![reference];
    // A share of the plan's work only where the plan states one (`insights::stated_share`): a plan without costs
    // (SQLite) has none, a rule's weight is no measurement, and a cheap plan's or a guessed step's share isn't where the
    // statement's work is.
    if let Some(share) = culprit_core::insights::stated_share(plan, i) {
        f.metrics.push(Metric::new("Share of the plan's work", share, Unit::Ratio));
    }
    f.metrics.push(Metric::new("Average duration", q.mean_time_ms, Unit::Ms));
    f.fix = Some(Fix { summary: i.fix.clone(), sql: i.fix_sql.clone(), cost: None });
    f.verify = vec![i.verify.clone(), "Re-scan: this finding is gone.".into()];
    if !actual && weighed {
        f.caveats.push("Based on the estimated plan; actual row counts can tell a different story.".into());
    }
    f
}

/// Rules about the whole plan that weigh it by a time of their own (compiling it, JIT-compiling it) rather than a share
/// of a step's cost.
const TIMED_WHOLE_PLAN: &[&str] = &["slow-compile", "jit"];

/// An insight about the plan as a whole (it runs on one CPU, the optimizer stopped searching, its compile, a grant for
/// the whole run): it points at the plan's root with a rule's own weight, or a time of its own, not at a step whose
/// share of the cost is the problem.
fn whole_plan(plan: &culprit_core::Plan, i: &PlanInsight) -> bool {
    i.nodes.first() == Some(&plan.root.id) && (i.fixed_weight || TIMED_WHOLE_PLAN.contains(&i.rule.as_str()))
}

fn rule_label(i: &PlanInsight) -> String {
    culprit_core::insights::RULES
        .iter()
        .find(|r| r.0 == i.rule)
        .map(|r| r.2.to_string())
        .unwrap_or_else(|| i.rule.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(name: &str) -> culprit_core::Report {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures").join(name);
        serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
    }

    #[test]
    fn findings_are_complete_capped_and_not_duplicated() {
        for name in ["mssql-demo.report.json", "pg-demo.report.json", "mysql-demo.report.json"] {
            let snapshot = report(name).snapshot;
            let found = plan_insights(&snapshot);
            for f in &found {
                assert!(f.id.starts_with("plan-insights:"));
                assert!(!COVERED.iter().any(|c| f.id.starts_with(&format!("plan-insights:{c}:"))), "{name}: {}", f.id);
                assert!(f.fix.is_some() && f.verify.len() == 2 && !f.queries.is_empty(), "{name}: {}", f.id);
                assert!(f.impact.db_time_share.is_some());
            }
            let mut ids: Vec<&String> = found.iter().map(|f| &f.id).collect();
            ids.sort();
            ids.dedup();
            assert_eq!(ids.len(), found.len(), "{name}: one finding per problem and table");
        }
    }

    #[test]
    fn an_insight_about_the_whole_plan_names_no_step() {
        // Query 263 runs on one CPU (MaxDOPSetToOne): the plan's root it points at isn't where the problem is.
        let snapshot = report("mssql-demo.report.json").snapshot;
        let found = plan_insights(&snapshot);
        let blocked = found
            .iter()
            .find(|f| f.id.starts_with("plan-insights:parallelism-blocked:"))
            .unwrap_or_else(|| panic!("{:?}", found.iter().map(|f| &f.id).collect::<Vec<_>>()));
        assert_eq!(blocked.id, "plan-insights:parallelism-blocked:query-263");
        assert!(blocked.queries.iter().all(|r| r.plan_node.is_none()), "{:?}", blocked.queries);
        // A rule about a step still names it.
        assert!(
            found.iter().filter(|f| !f.id.contains(":parallelism-blocked:")).all(|f| f.queries[0].plan_node.is_some())
        );
    }
}
