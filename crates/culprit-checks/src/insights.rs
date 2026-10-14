//! Plan insights as findings: the plan-level problems in queries that matter, beyond what `plan-warnings`,
//! `filtered-scan` and `missing-index` already report. One finding per problem and table, however many queries share it.

use culprit_core::insights::{PlanInsight, insights};
use culprit_core::{
    Capability, Category, CheckInfo, Confidence, Engine, Finding, Fix, Metric, PlanKind, QueryRef, QueryStat, Severity,
    Snapshot, SubjectKind, Unit, format,
};

use crate::support::*;

/// Rules whose findings other checks already produce, with the evidence and wording users know.
const COVERED: &[&str] =
    &["implicit-conversion", "spill", "no-join-predicate", "missing-statistics", "scan-filter", "missing-index-hint"];

/// Queries below this share of database time aren't worth a finding, however odd their plan.
const MIN_SHARE: f64 = 0.01;

pub fn plan_insights_info() -> CheckInfo {
    CheckInfo {
        id: "plan-insights".into(),
        title: "Plan insights".into(),
        category: Category::Queries,
        engines: vec![Engine::Mssql, Engine::Postgres, Engine::Mysql],
        requires: vec![Capability::QueryPlans],
        description: "Key lookups, functions on filtered columns, leading wildcards, NOT IN, scalar functions, eager spools, misestimates and repeated loops in the plans of queries that matter.".into(),
    }
}

pub fn plan_insights(snapshot: &Snapshot) -> Vec<Finding> {
    let mut out: Vec<Finding> = Vec::new();
    for q in app_queries(snapshot) {
        let Some(plan) = &q.plan else { continue };
        let Some(impact) = query_impact(snapshot, &[q]) else { continue };
        if impact.share < MIN_SHARE {
            continue;
        }
        for i in insights(plan) {
            if COVERED.contains(&i.rule.as_str()) || i.severity == Severity::Low {
                continue;
            }
            let object =
                plan.nodes().into_iter().find(|n| Some(&n.id) == i.nodes.first()).and_then(|n| n.object.clone());
            let key = format!(
                "{}:{}",
                i.rule,
                object.as_deref().map(str::to_ascii_lowercase).unwrap_or_else(|| format!("query-{}", q.id))
            );
            let reference =
                QueryRef { id: q.id.clone(), plan_node: i.nodes.first().copied(), note: Some(rule_label(&i)) };
            match out.iter_mut().find(|f| f.id == format!("plan-insights:{key}")) {
                Some(existing) => {
                    if !existing.queries.iter().any(|r| r.id == q.id) {
                        existing.queries.push(reference);
                    }
                }
                None => out.push(build(snapshot, q, &i, &key, object, reference, plan.kind == PlanKind::Actual)),
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
    actual: bool,
) -> Finding {
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
    let total = snapshot.workload.as_ref().map_or(0.0, |w| w.total_time_ms);
    let cap = by_share.min(severity_for_time(snapshot, share * total, 1.0));
    f.severity = i.severity.min(cap);
    f.confidence = if actual { Confidence::High } else { Confidence::Medium };
    f.title = i.title.clone();
    f.summary = format!("{} Seen in {}.", i.detail, query_handle(q));
    f.queries = vec![reference];
    f.metrics.push(Metric::new("Share of the plan's work", i.impact, Unit::Ratio));
    f.metrics.push(Metric::new("Average duration", q.mean_time_ms, Unit::Ms));
    f.fix = Some(Fix { summary: i.fix.clone(), sql: i.fix_sql.clone(), cost: None });
    f.verify = vec![i.verify.clone(), "Re-scan: this finding is gone.".into()];
    if !actual {
        f.caveats.push("Based on the estimated plan; actual row counts can tell a different story.".into());
    }
    f
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
}
