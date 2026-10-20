//! A finding is covered only by an identified cause. Select relationships without mutating findings or reading prose.
use super::*;
use culprit_core::{RefinementDecision, RefinementOutcome};

#[derive(Clone, Copy)]
pub(super) enum Policy {
    Rewrite,
    SpillSort,
    ForeignKey,
    Regression,
    Network,
    Heavy,
}
impl Policy {
    fn id(self) -> &'static str {
        match self {
            Self::Rewrite => "rewrite-covers-scan",
            Self::SpillSort => "spill-covers-sort",
            Self::ForeignKey => "index-covers-foreign-key",
            Self::Regression => "regression-covers-plan-change",
            Self::Network => "network-covers-heavy-query",
            Self::Heavy => "cause-covers-heavy-query",
        }
    }
    fn label(self) -> &'static str {
        match self {
            Self::Rewrite => "a rewrite, not an index, fixes a scan behind a function or wildcard",
            Self::SpillSort => "a spill explains the big sort",
            Self::ForeignKey => "an index to add covers the foreign key",
            Self::Regression => "a regression carries its plan change",
            Self::Network => "waiting on the application explains a heavy query",
            Self::Heavy => "a heavy query another finding explains",
        }
    }
}

pub(super) struct Rule(pub Policy);
impl Refine for Rule {
    fn name(&self) -> &str {
        self.0.label()
    }
    fn refine(&self, s: &Snapshot, findings: &mut Vec<Finding>) {
        let _ = self.refine_traced(s, findings);
    }
    fn refine_traced(&self, s: &Snapshot, findings: &mut Vec<Finding>) -> Vec<RefinementDecision> {
        let decisions = decide(self.0, s, findings);
        let trace = decisions
            .iter()
            .map(|d| RefinementDecision {
                rule: self.0.id().into(),
                finding: Some(findings[d.at].id.clone()),
                outcome: RefinementOutcome::Covered { by: d.by.iter().map(|&at| findings[at].id.clone()).collect() },
                queries: findings[d.at].queries.iter().map(|r| r.id.clone()).collect(),
                tables: if findings[d.at].subject.kind == SubjectKind::Table {
                    vec![findings[d.at].subject.name.clone()]
                } else {
                    Vec::new()
                },
            })
            .collect();
        let gone: BTreeSet<_> = decisions.iter().map(|d| d.at).collect();
        let mut at = 0;
        findings.retain(|_| {
            let keep = !gone.contains(&at);
            at += 1;
            keep
        });
        trace
    }
}

#[derive(Clone, Debug, PartialEq)]
struct Decision {
    at: usize,
    by: Vec<usize>,
}

fn shares_query(a: &Finding, b: &Finding) -> bool {
    a.queries.iter().any(|a| b.queries.iter().any(|b| a.id == b.id))
}

fn decide(policy: Policy, s: &Snapshot, findings: &[Finding]) -> Vec<Decision> {
    let heavy = if matches!(policy, Policy::Heavy) { heavy_causes(s, findings) } else { BTreeMap::new() };
    let mut out = Vec::new();
    for (at, f) in findings.iter().enumerate() {
        let by: Vec<_> = match policy {
            Policy::Rewrite if f.check == "filtered-scan" => findings
                .iter()
                .enumerate()
                .filter(|(_, g)| {
                    (g.id.starts_with("plan-insights:non-sargable:")
                        || g.id.starts_with("plan-insights:leading-wildcard:"))
                        && table_leaf(&g.subject.name) == table_leaf(&f.subject.name)
                        && shares_query(f, g)
                })
                .map(|(at, _)| at)
                .collect(),
            Policy::SpillSort
                if f.id.starts_with("plan-insights:big-sort:")
                    || f.id.starts_with("plan-insights:temp-table-sort:") =>
            {
                let spilled: Vec<_> = findings
                    .iter()
                    .enumerate()
                    .filter(|(_, g)| g.check == "temp-spill" || g.id.contains(":spill"))
                    .collect();
                // A malformed finding with no query evidence cannot be covered by vacuous truth.
                if f.queries.is_empty()
                    || !f.queries.iter().all(|r| spilled.iter().any(|(_, g)| g.queries.iter().any(|q| q.id == r.id)))
                {
                    Vec::new()
                } else {
                    spilled.iter().filter(|(_, g)| shares_query(f, g)).map(|(at, _)| *at).collect()
                }
            }
            Policy::ForeignKey if f.check == "unindexed-fk" => {
                let cols = &f.facts.index_keys;
                if cols.is_empty() {
                    continue;
                }
                findings
                    .iter()
                    .enumerate()
                    .filter(|(_, g)| {
                        if !matches!(g.check.as_str(), "missing-index" | "filtered-scan")
                            || !g.subject.name.eq_ignore_ascii_case(&f.subject.name)
                        {
                            return false;
                        }
                        // Use the same eligibility decision as index-work. A candidate whose SQL will be withheld cannot
                        // promise to cover a foreign key; this is independent of the relative order of these rules.
                        let eligible = lifecycle::recommends_unfiltered_index(s, g);
                        let keys = &g.facts.index_keys;
                        eligible && cols.iter().all(|c| keys.iter().take(cols.len()).any(|k| k.eq_ignore_ascii_case(c)))
                    })
                    .map(|(at, _)| at)
                    .collect()
            }
            Policy::Regression if f.check == "plan-regression" => findings
                .iter()
                .enumerate()
                .filter(|(_, g)| g.check == "regression" && shares_query(f, g))
                .map(|(at, _)| at)
                .collect(),
            Policy::Network if f.check == "heavy-query" => findings
                .iter()
                .enumerate()
                .filter(|(_, g)| {
                    g.check == "network-wait"
                        && g.facts.network_wait_share.is_some_and(|share| share >= 0.5)
                        && shares_query(f, g)
                })
                .map(|(at, _)| at)
                .collect(),
            Policy::Heavy if f.check == "heavy-query" => {
                let Some(own) = f.impact.db_time_share else {
                    continue;
                };
                f.queries
                    .iter()
                    .flat_map(|r| heavy.get(&r.id).into_iter().flatten())
                    .filter(|(_, weight, part)| *weight >= f.severity && *part >= own * 0.5)
                    .map(|(at, _, _)| *at)
                    .collect::<BTreeSet<_>>()
                    .into_iter()
                    .collect()
            }
            _ => Vec::new(),
        };
        if !by.is_empty() {
            out.push(Decision { at, by });
        }
    }
    out
}

fn heavy_causes(s: &Snapshot, findings: &[Finding]) -> BTreeMap<String, Vec<(usize, Severity, f64)>> {
    let own_share = |id: &str| s.workload.as_ref().and_then(|w| w.query(id).map(|q| w.statement_share(q)));
    let mut causes: BTreeMap<String, Vec<(usize, Severity, f64)>> = BTreeMap::new();
    for (at, f) in findings.iter().enumerate().filter(|(_, f)| {
        !matches!(
            f.check.as_str(),
            "heavy-query" | "unused-index" | "duplicate-index" | "memory-grant" | "network-wait" | "unstable-query"
        ) && !measures_no_time(f)
    }) {
        let Some(share) = f.impact.db_time_share else {
            continue;
        };
        let weight =
            if matches!(f.check.as_str(), "plan-warnings" | "plan-insights") { f.severity } else { Severity::High };
        let parts: Vec<_> = match f.queries.as_slice() {
            [one] => vec![(one.id.clone(), share)],
            refs => {
                let weights: Option<Vec<_>> = refs.iter().map(|r| Some((r.id.clone(), own_share(&r.id)?))).collect();
                let Some(weights) = weights else {
                    continue;
                };
                let total: f64 = weights.iter().map(|(_, w)| w).sum();
                if total <= 0.0 {
                    continue;
                }
                weights.into_iter().map(|(id, w)| (id, share * w / total)).collect()
            }
        };
        for (id, part) in parts {
            causes.entry(id).or_default().push((at, weight, part));
        }
    }
    causes
}

#[cfg(test)]
pub(super) fn explained_heavy_query(s: &Snapshot, findings: &mut Vec<Finding>) {
    Rule(Policy::Heavy).refine(s, findings);
}

#[cfg(test)]
mod tests {
    use super::*;
    use culprit_core::{Category, Fix, Metric, QueryRef, Unit};

    fn snapshot() -> Snapshot {
        let mut s: Snapshot = serde_json::from_str(include_str!("../../../../fixtures/mssql-demo.json")).unwrap();
        let w = s.workload.as_mut().unwrap();
        w.measurement = None;
        w.timeline = None;
        w.window_hours = 24.0;
        w.queries.truncate(1);
        let q = &mut w.queries[0];
        q.executions = 100.0;
        q.mean_time_ms = 10_000.0;
        q.total_time_ms = 1_000_000.0;
        q.first_seen = None;
        q.last_seen = None;
        q.trend = None;
        q.costs = None;
        w.total_time_ms = q.total_time_ms;
        s
    }
    fn finding(check: &str, key: &str, query: &str) -> Finding {
        let mut f = support::finding(check, key, Category::Queries, support::subject(SubjectKind::Query, key));
        f.queries = vec![QueryRef { id: query.into(), plan_node: None, note: None }];
        f.impact.db_time_share = Some(1.0);
        f.severity = Severity::High;
        f
    }
    fn index_pair(s: &Snapshot) -> Vec<Finding> {
        let mut candidate = finding("filtered-scan", "shop.orders(state)", &s.queries()[0].id);
        candidate.subject = support::subject(SubjectKind::Table, "shop.orders");
        candidate.facts.index_keys = vec!["state".into()];
        candidate.fix = Some(Fix {
            summary: "Candidate index".into(),
            sql: Some("CREATE INDEX ix_orders_state ON shop.orders (state)".into()),
            cost: None,
        });
        let mut fk = finding("unindexed-fk", "shop.orders(state)", &s.queries()[0].id);
        fk.subject = candidate.subject.clone();
        fk.facts.index_keys = candidate.facts.index_keys.clone();
        vec![candidate, fk]
    }

    #[test]
    fn a_covered_finding_names_the_cause_that_remains() {
        let s = snapshot();
        let id = &s.queries()[0].id;
        let mut findings = vec![finding("heavy-query", id, id), finding("temp-spill", id, id)];
        let trace = Rule(Policy::Heavy).refine_traced(&s, &mut findings);
        assert_eq!(findings.len(), 1);
        assert!(matches!(&trace[0].outcome,RefinementOutcome::Covered {by} if by==&vec![findings[0].id.clone()]));
        assert_eq!(trace[0].finding, Some(format!("heavy-query:{id}")));
    }

    #[test]
    fn unknown_time_or_missing_allocating_queries_do_not_explain_time_away() {
        let s = snapshot();
        let id = &s.queries()[0].id;
        let mut heavy = finding("heavy-query", id, id);
        let cause = finding("temp-spill", id, id);
        heavy.impact.db_time_share = None;
        assert!(decide(Policy::Heavy, &s, &[heavy.clone(), cause.clone()]).is_empty());
        heavy.impact.db_time_share = Some(1.0);
        let mut mixed = cause;
        mixed.queries.push(QueryRef { id: "not-in-the-scan".into(), plan_node: None, note: None });
        assert!(decide(Policy::Heavy, &s, &[heavy, mixed]).is_empty());
    }

    #[test]
    fn a_sort_with_no_statement_evidence_is_not_vacuously_covered() {
        let s = snapshot();
        let id = &s.queries()[0].id;
        let mut sort = finding("plan-insights", "big-sort:shop.orders", id);
        sort.queries.clear();
        assert!(decide(Policy::SpillSort, &s, &[sort, finding("temp-spill", id, id)]).is_empty());
    }

    #[test]
    fn a_candidate_withheld_for_missing_work_cannot_hide_the_foreign_key() {
        let mut s = snapshot();
        let w = s.workload.as_mut().unwrap();
        let q = &mut w.queries[0];
        q.mean_time_ms = 0.001;
        q.total_time_ms = 0.1;
        w.total_time_ms = 0.1;
        assert!(!lifecycle::has_measured_index_work(&s, &index_pair(&s)[0]));
        for reverse in [false, true] {
            let mut findings = index_pair(&s);
            if reverse {
                lifecycle::Rule(lifecycle::Policy::IndexWork).refine(&s, &mut findings);
            }
            Rule(Policy::ForeignKey).refine(&s, &mut findings);
            if !reverse {
                lifecycle::Rule(lifecycle::Policy::IndexWork).refine(&s, &mut findings);
            }
            assert!(findings.iter().any(|f| f.check == "unindexed-fk"));
        }
        let material = snapshot();
        let mut findings = index_pair(&material);
        let trace = Rule(Policy::ForeignKey).refine_traced(&material, &mut findings);
        assert_eq!(findings.len(), 1);
        assert!(matches!(&trace[0].outcome,RefinementOutcome::Covered {by} if by==&vec![findings[0].id.clone()]));
    }

    #[test]
    fn filtered_or_sql_less_candidates_leave_foreign_key_advice_visible() {
        let s = snapshot();
        for sql in [None, Some("CREATE INDEX ix_orders_state ON shop.orders (state) WHERE state = 1".into())] {
            let mut findings = index_pair(&s);
            findings[0].fix.as_mut().unwrap().sql = sql;
            Rule(Policy::ForeignKey).refine(&s, &mut findings);
            assert_eq!(findings.len(), 2);
        }
    }

    #[test]
    fn translating_human_text_does_not_change_coverage() {
        let s = snapshot();
        let id = &s.queries()[0].id;
        let mut network = finding("network-wait", id, id);
        network.metrics.push(Metric::new("Waiting on the app", 0.75, Unit::Ratio));
        network.facts.network_wait_share = Some(0.75);
        let mut rewrite = finding("plan-insights", "non-sargable:shop.orders", id);
        rewrite.subject = support::subject(SubjectKind::Table, "shop.orders");
        let pairs = [
            (Policy::Rewrite, vec![finding("filtered-scan", "shop.orders", id), rewrite]),
            (Policy::Heavy, vec![finding("heavy-query", id, id), finding("temp-spill", id, id)]),
            (Policy::Network, vec![finding("heavy-query", id, id), network]),
            (Policy::ForeignKey, index_pair(&s)),
            (Policy::Regression, vec![finding("plan-regression", id, id), finding("regression", id, id)]),
            (
                Policy::SpillSort,
                vec![finding("plan-insights", "big-sort:shop.orders", id), finding("temp-spill", id, id)],
            ),
        ];
        for (policy, mut findings) in pairs {
            let expected = decide(policy, &s, &findings);
            assert!(!expected.is_empty());
            for f in &mut findings {
                f.title = "Other title".into();
                f.summary = "Different wording".into();
                f.caveats = vec!["Other caveat".into()];
                f.verify = vec!["Other words".into()];
                for metric in &mut f.metrics {
                    metric.label = "Translated evidence label".into();
                    if metric.text.is_some() {
                        metric.text = Some("Translated evidence text".into());
                    }
                }
                if let Some(fix) = &mut f.fix {
                    fix.summary = "Other fix wording".into();
                }
            }
            assert_eq!(expected, decide(policy, &s, &findings));
        }
    }
}
