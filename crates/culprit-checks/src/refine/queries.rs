//! Query relationships come from recorded statement and plan identities, independently of their description.
use super::*;
use culprit_core::{FindingPlanChange, RefinementDecision};

#[derive(Clone, Copy)]
pub(super) enum Policy {
    SpillResult,
    PlanStory,
    ScanRegression,
}
impl Policy {
    fn id(self) -> &'static str {
        match self {
            Self::SpillResult => "spill-carries-result",
            Self::PlanStory => "one-plan-story",
            Self::ScanRegression => "scan-carries-regression",
        }
    }
    fn label(self) -> &'static str {
        match self {
            Self::SpillResult => "a spill carries the result it sorts",
            Self::PlanStory => "one plan story per query",
            Self::ScanRegression => "a scan that came with a plan change carries the regression",
        }
    }
}
pub(super) struct Rule(pub Policy);
enum Action {
    Spill,
    Timeline,
    Recommendation,
    Scan(ReadChange),
}
struct Decision {
    from: usize,
    into: usize,
    action: Action,
}
struct ReadChange {
    from: String,
    to: String,
    before: f64,
    after: f64,
}
impl ReadChange {
    fn recorded(q: &QueryStat, change: &FindingPlanChange) -> Option<Self> {
        if change.from == change.to || q.plan_id.as_deref().is_some_and(|id| id != change.to) {
            return None;
        }
        let per_run = |id: &str| {
            let p = q.workload_plans().iter().find(|p| p.id == id)?;
            let reads = p.logical_reads?;
            (p.executions.is_finite() && p.executions > 0.0 && reads.is_finite() && reads >= 0.0)
                .then_some(reads / p.executions)
        };
        let (before, after) = (per_run(&change.from)?, per_run(&change.to)?);
        (after > before * 2.0).then(|| Self { from: change.from.clone(), to: change.to.clone(), before, after })
    }
    fn render(&self, history: bool) -> String {
        format!(
            " The scan came with the plan change: plan {} read about {} pages a run, plan {} {} about {}.",
            self.from,
            format::count(self.before),
            self.to,
            if history { "read" } else { "reads" },
            format::count(self.after)
        )
    }
}
fn single(f: &Finding) -> Option<&str> {
    match f.queries.as_slice() {
        [r] => Some(&r.id),
        _ => None,
    }
}
fn decide(policy: Policy, s: &Snapshot, findings: &[Finding]) -> Option<Decision> {
    let w = s.workload.as_ref()?;
    // Timeline evidence joins the engine's recommendation before that recommendation joins a sensitivity finding.
    if matches!(policy, Policy::PlanStory) {
        for (into, rec) in findings.iter().enumerate().filter(|(_, f)| f.id.starts_with("tuning-advice:force:")) {
            let Some(id) = single(rec).filter(|id| w.query(id).is_some()) else { continue };
            if let Some(from) = findings
                .iter()
                .position(|f| matches!(f.check.as_str(), "regression" | "plan-regression") && single(f) == Some(id))
            {
                return Some(Decision { from, into, action: Action::Timeline });
            }
            if let Some(target) =
                findings.iter().position(|f| f.check == "parameter-sensitive" && single(f) == Some(id))
            {
                return Some(Decision { from: into, into: target, action: Action::Recommendation });
            }
        }
        return None;
    }
    for (from, f) in findings.iter().enumerate() {
        let Some(q) = single(f).and_then(|id| w.query(id)) else { continue };
        match policy {
            Policy::SpillResult if f.check == "big-result" && q.rows.is_some() => {
                if let Some(into) = findings.iter().position(|g| g.check == "temp-spill" && single(g) == Some(&q.id)) {
                    return Some(Decision { from, into, action: Action::Spill });
                }
            }
            Policy::ScanRegression if matches!(f.check.as_str(), "regression" | "plan-regression") => {
                let Some(reads) = f.facts.plan_change.as_ref().and_then(|change| ReadChange::recorded(q, change))
                else {
                    continue;
                };
                if let Some(into) = findings.iter().position(|g| {
                    matches!(g.check.as_str(), "filtered-scan" | "missing-index")
                        && g.queries.iter().any(|r| r.id == q.id && r.plan_node.is_some())
                }) {
                    return Some(Decision { from, into, action: Action::Scan(reads) });
                }
            }
            _ => {}
        }
    }
    None
}
fn add_metrics(target: &mut Finding, source: &Finding, skip_runs: bool) {
    for m in &source.metrics {
        if (!skip_runs || m.label != "Runs") && !target.metrics.iter().any(|x| x.label == m.label) {
            target.metrics.push(m.clone());
        }
    }
}
fn timeline(source: &Finding) -> String {
    let first =
        source.summary.split_once(". ").map_or(source.summary.as_str(), |(first, _)| first).trim_end_matches('.');
    match first.strip_prefix("Until ") {
        Some(rest) => format!("Query Store's timeline: until {rest}."),
        None => format!("{first}."),
    }
}
fn recommendation_caveat(rec: &Finding, sensitive: &Finding) -> String {
    let plan = rec.facts.recommended_plan.as_ref().map_or("another plan", |r| r.plan.as_str());
    let gain = rec
        .facts
        .recommended_plan
        .as_ref()
        .and_then(|r| r.cpu_saved_share)
        .map(|g| format!(" (estimated {} less CPU per run)", format::percent(g)))
        .unwrap_or_default();
    match sensitive.facts.usual_fast_plan.as_deref() {
        Some(fast) if fast == plan => format!(
            "SQL Server's automatic tuning also recommends forcing plan {plan}{gain}: the plan most calls run fast with, the fix's first step."
        ),
        Some(fast) => format!(
            "SQL Server's automatic tuning recommends forcing plan {plan}{gain}, while most calls run fast with plan {fast}. With plans used side by side for different values, forcing one for every value can slow the calls the other suits: try a value from a slow call under it first."
        ),
        None => format!("SQL Server's automatic tuning recommends forcing plan {plan}{gain}."),
    }
}
impl Refine for Rule {
    fn name(&self) -> &str {
        self.0.label()
    }
    fn refine(&self, s: &Snapshot, findings: &mut Vec<Finding>) {
        let _ = self.refine_traced(s, findings);
    }
    fn refine_traced(&self, s: &Snapshot, findings: &mut Vec<Finding>) -> Vec<RefinementDecision> {
        let mut trace = Vec::new();
        while let Some(d) = decide(self.0, s, findings) {
            trace.push(merge::trace(self.0.id(), findings, d.from, d.into));
            let source = findings[d.from].clone();
            let absorption = matches!(d.action, Action::Spill | Action::Scan(_))
                .then(|| merge::Absorption::decide(s, findings, d.into, d.from));
            if let Some(a) = &absorption {
                a.apply_state(findings);
            } else {
                let target = &mut findings[d.into];
                target.severity = target.severity.max(source.severity);
                target.impact.score = target.impact.score.max(source.impact.score);
                if matches!(d.action, Action::Timeline) {
                    target.impact.time_ms_per_day = target.impact.time_ms_per_day.or(source.impact.time_ms_per_day);
                }
            }
            let target = &mut findings[d.into];
            match d.action {
                Action::Spill => {
                    let q = s.workload.as_ref().unwrap().query(&source.queries[0].id).unwrap();
                    let said = signals::result_of_a_spill(s, q).expect("selected result rows");
                    target.summary.push_str(&said.summary);
                    if let Some(fix) = &mut target.fix {
                        fix.summary.push_str(&said.fix);
                    }
                    target.verify.extend(said.verify);
                    add_metrics(target, &source, true);
                }
                Action::Timeline => {
                    target.summary = format!("{} {}", target.summary.trim_end(), timeline(&source));
                    add_metrics(target, &source, false);
                    for line in source.verify {
                        if !target.verify.contains(&line) {
                            target.verify.push(line);
                        }
                    }
                    for line in source.caveats {
                        if !target.caveats.contains(&line) {
                            target.caveats.push(line);
                        }
                    }
                }
                Action::Recommendation => target.caveats.push(recommendation_caveat(&source, target)),
                Action::Scan(reads) => {
                    let q = s.workload.as_ref().unwrap().query(&source.queries[0].id).unwrap();
                    let sentence = timeline(&source);
                    let sentence = if target.queries.len() > 1 {
                        sentence.replacen("this query", &format!("query {}", q.id), 1)
                    } else {
                        sentence
                    };
                    target.summary =
                        format!("{} {sentence}{}", target.summary.trim_end(), reads.render(query_is_history(s, q)));
                    add_metrics(target, &source, false);
                }
            }
            if let Some(a) = absorption {
                a.render(findings, None);
                a.finish(findings);
            } else {
                findings.remove(d.from);
            }
        }
        trace
    }
}
