//! Decide finding lifecycle and fix eligibility from evidence before changing state or composing prose.
use super::*;
use culprit_core::{Actionability, FixWithholdingBasis, HistoryBasis, RefinementDecision, RefinementOutcome};

#[derive(Clone, Copy)]
pub(super) enum Policy {
    Stopped,
    Creation,
    Once,
    Gone,
    Verification,
    IndexWork,
}

impl Policy {
    fn id(self) -> &'static str {
        match self {
            Self::Stopped => "stopped-running",
            Self::Creation => "stopped-no-creation",
            Self::Once => "ran-once",
            Self::Gone => "tables-absent",
            Self::Verification => "historical-verification",
            Self::IndexWork => "index-work",
        }
    }
    fn label(self) -> &'static str {
        match self {
            Self::Stopped => "statements that stopped running",
            Self::Creation => "nothing is created for statements that stopped",
            Self::Once => "statements that ran once",
            Self::Gone => "a table that is gone has nothing to change",
            Self::Verification => "a finding whose statements stopped has nothing to check",
            Self::IndexWork => "query-driven index changes need measured work",
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
        let trace = decisions.iter().map(|d| d.record(self.0, &findings[d.at])).collect();
        // All policy state is settled before rendering. Prose never chooses whether a finding is current or actionable.
        for decision in &decisions {
            decision.apply_state(&mut findings[decision.at]);
        }
        for decision in &decisions {
            decision.render(s, &mut findings[decision.at]);
        }
        trace
    }
}

#[derive(Clone, Debug, PartialEq)]
enum Change {
    SourceHistory,
    LastSeen(Option<String>),
    MissingQueries(Vec<String>),
    Quiet { seconds: f64, last: String, one: bool, by_first_scan: bool },
    Stopped { seconds: f64, last: String, one: bool, by_first_scan: bool, successor: Option<String> },
    Once { one: bool, slow_log: bool },
    Gone,
    Creation { one: bool, successor: Option<String> },
    Verification,
    IndexWork,
}

#[derive(Clone, Debug, PartialEq)]
struct Decision {
    at: usize,
    queries: Vec<String>,
    tables: Vec<String>,
    change: Change,
}

fn known_queries<'a>(s: &'a Snapshot, f: &Finding) -> Option<Vec<&'a QueryStat>> {
    let w = s.workload.as_ref()?;
    f.queries.iter().map(|r| w.query(&r.id)).collect()
}

pub(super) fn has_measured_index_work(s: &Snapshot, f: &Finding) -> bool {
    known_queries(s, f).is_some_and(|q| support::index_work_is_material(s, &q))
}

pub(super) fn recommends_unfiltered_index(s: &Snapshot, f: &Finding) -> bool {
    f.is_actionable()
        && has_measured_index_work(s, f)
        && f.fix
            .as_ref()
            .and_then(|fix| fix.sql.as_deref())
            .is_some_and(|sql| culprit_core::sql::creates_unfiltered_index(sql, s.server.engine))
}

fn decide(policy: Policy, s: &Snapshot, findings: &[Finding]) -> Vec<Decision> {
    let mut decisions = Vec::new();
    let mut push = |at: usize, change, tables| {
        decisions.push(Decision {
            at,
            queries: findings[at].queries.iter().map(|r| r.id.clone()).collect(),
            tables,
            change,
        })
    };
    for (at, f) in findings.iter().enumerate() {
        match policy {
            Policy::Stopped => {
                if f.inactive {
                    push(at, Change::SourceHistory, Vec::new());
                }
                if !about_queries(f) {
                    continue;
                }
                let Some(w) = &s.workload else { continue };
                let observed: Vec<_> = f.queries.iter().filter_map(|r| w.query(&r.id)).collect();
                let last = observed.iter().filter_map(|q| q.last_seen.clone()).max_by(|a, b| {
                    support::parse_epoch_seconds(a)
                        .partial_cmp(&support::parse_epoch_seconds(b))
                        .unwrap_or(std::cmp::Ordering::Equal)
                });
                push(at, Change::LastSeen(last), Vec::new());
                // Missing linked queries cannot establish that ALL statements stopped or ran once.
                let Some(queries) = known_queries(s, f).filter(|q| !q.is_empty()) else {
                    let missing = f.queries.iter().filter(|r| w.query(&r.id).is_none()).map(|r| r.id.clone()).collect();
                    push(at, Change::MissingQueries(missing), Vec::new());
                    continue;
                };
                let one = queries.len() == 1;
                let silences: Vec<_> = queries.iter().filter_map(|q| query_stopped(s, q)).collect();
                if silences.len() == queries.len() {
                    let (seconds, last) = silences.into_iter().min_by(|a, b| a.0.total_cmp(&b.0)).unwrap();
                    let by_first_scan = last_ran_by_first_scan(w, &last);
                    let successor = one.then(|| query_successor(s, queries[0])).flatten().map(|q| q.id.clone());
                    push(at, Change::Stopped { seconds, last, one, by_first_scan, successor }, Vec::new());
                } else {
                    let quiet: Vec<_> = queries.iter().filter_map(|q| query_quiet(s, q)).collect();
                    if quiet.len() == queries.len() {
                        let (seconds, last) = quiet.into_iter().min_by(|a, b| a.0.total_cmp(&b.0)).unwrap();
                        let by_first_scan = last_ran_by_first_scan(w, &last);
                        push(at, Change::Quiet { seconds, last, one, by_first_scan }, Vec::new());
                    }
                }
            }
            Policy::Once if !f.inactive && about_queries(f) => {
                if let Some(q) = known_queries(s, f).filter(|q| !q.is_empty() && q.iter().all(|q| ran_once(q))) {
                    push(
                        at,
                        Change::Once { one: q.len() == 1, slow_log: crate::queries::slow_log_only(s) },
                        Vec::new(),
                    );
                }
            }
            Policy::Gone if about_queries(f) => {
                let Some(statements) = known_queries(s, f) else { continue };
                let tables: BTreeSet<String> = statements
                    .iter()
                    .flat_map(|q| {
                        let targets = support::write_targets(q, s.server.engine);
                        if !targets.is_empty() {
                            return targets;
                        }
                        q.plan.iter().flat_map(|p| p.nodes()).filter_map(|n| n.object.clone()).collect()
                    })
                    .collect();
                if !tables.is_empty()
                    && tables.iter().all(|t| crate::indexes::table_gone(s, &table_leaf(t), &statements))
                {
                    push(at, Change::Gone, tables.into_iter().collect());
                }
            }
            Policy::Creation if f.inactive && s.workload.is_some() => {
                if f.fix.as_ref().and_then(|fix| fix.sql.as_deref()).is_some_and(creates_something) {
                    let q = known_queries(s, f).unwrap_or_default();
                    let successor = (q.len() == 1).then(|| query_successor(s, q[0])).flatten().map(|q| q.id.clone());
                    push(at, Change::Creation { one: f.queries.len() <= 1, successor }, Vec::new());
                }
            }
            Policy::Verification if f.inactive => push(at, Change::Verification, Vec::new()),
            Policy::IndexWork if !f.inactive => {
                if f.check == "unindexed-fk"
                    || (f.queries.is_empty()
                        && !matches!(
                            f.check.as_str(),
                            "missing-index" | "filtered-scan" | "plan-insights" | "tuning-advice"
                        ))
                {
                    continue;
                }
                let Some(sql) = f.fix.as_ref().and_then(|fix| fix.sql.as_deref()) else { continue };
                if culprit_core::sql::creates_index(sql, s.server.engine) && !has_measured_index_work(s, f) {
                    push(at, Change::IndexWork, Vec::new());
                }
            }
            _ => {}
        }
    }
    decisions
}

impl Decision {
    fn record(&self, policy: Policy, finding: &Finding) -> RefinementDecision {
        use Change::*;
        let outcome = match &self.change {
            SourceHistory => RefinementOutcome::Historical {
                basis: HistoryBasis::SourceMarked,
                last_seen: finding.last_seen.clone(),
                quiet_seconds: None,
            },
            LastSeen(last_seen) => RefinementOutcome::LastExecution { last_seen: last_seen.clone() },
            MissingQueries(missing_queries) => {
                RefinementOutcome::InsufficientEvidence { missing_queries: missing_queries.clone() }
            }
            Quiet { seconds, last, .. } => RefinementOutcome::Quiet { last_seen: last.clone(), seconds: *seconds },
            Stopped { seconds, last, .. } => RefinementOutcome::Historical {
                basis: HistoryBasis::Stopped,
                last_seen: Some(last.clone()),
                quiet_seconds: Some(*seconds),
            },
            Once { slow_log, .. } => RefinementOutcome::Historical {
                basis: if *slow_log { HistoryBasis::SingleLoggedExecution } else { HistoryBasis::SingleExecution },
                last_seen: finding.last_seen.clone(),
                quiet_seconds: None,
            },
            Gone => RefinementOutcome::Historical {
                basis: HistoryBasis::TablesAbsent,
                last_seen: finding.last_seen.clone(),
                quiet_seconds: None,
            },
            Creation { .. } => RefinementOutcome::FixWithheld { basis: FixWithholdingBasis::StatementsStopped },
            IndexWork => RefinementOutcome::FixWithheld { basis: FixWithholdingBasis::InsufficientMeasuredWork },
            Verification => RefinementOutcome::HistoricalVerification,
        };
        RefinementDecision {
            rule: policy.id().into(),
            finding: Some(finding.id.clone()),
            outcome,
            queries: self.queries.clone(),
            tables: self.tables.clone(),
        }
    }

    fn apply_state(&self, f: &mut Finding) {
        match &self.change {
            Change::SourceHistory => f.severity = Severity::Low,
            Change::LastSeen(last) => f.last_seen = last.clone(),
            Change::Stopped { .. } => {
                f.inactive = true;
                f.severity = Severity::Low;
                f.impact.score *= 0.25;
            }
            Change::Once { .. } => {
                f.actionability = Actionability::History;
                f.severity = Severity::Low;
                f.impact.score *= 0.25;
            }
            Change::Gone => {
                f.actionability = Actionability::History;
                f.severity = Severity::Low;
                f.fix = Some(culprit_core::Fix { summary: String::new(), sql: None, cost: None });
            }
            Change::Creation { .. } => {
                f.fix = Some(culprit_core::Fix { summary: String::new(), sql: None, cost: None });
            }
            Change::IndexWork => {
                if f.actionability != Actionability::History {
                    f.actionability = Actionability::Information;
                }
                f.severity = Severity::Low;
                if let Some(fix) = &mut f.fix {
                    fix.sql = None;
                    fix.cost = None;
                }
            }
            Change::Quiet { .. } | Change::Verification | Change::MissingQueries(_) => {}
        }
    }

    fn render(&self, s: &Snapshot, f: &mut Finding) {
        let successor = |id: &Option<String>, creation: bool| {
            id.as_deref()
                .and_then(|id| s.workload.as_ref()?.query(id))
                .map(|q| {
                    if creation {
                        format!(
                            " If query {}, {} too, took its place, look at that one.",
                            q.id,
                            support::query_handle(q)
                        )
                    } else {
                        format!(
                            " Query {}, {} too, still runs and may be what replaced it: check that one.",
                            q.id,
                            support::query_handle(q)
                        )
                    }
                })
                .unwrap_or_default()
        };
        match &self.change {
            Change::SourceHistory | Change::LastSeen(_) | Change::MissingQueries(_) => {}
            Change::Once { one, slow_log } => {
                let window = s.workload.as_ref().unwrap().window_phrase();
                let caveat = if *slow_log {
                    format!(
                        "{} logged once in {}: the slow query log keeps only runs over long_query_time, so it may run more often, faster. One slow run shows what it cost once, not a pattern; it needs work if it keeps showing up.",
                        if *one { "This statement was" } else { "Each of these statements was" },
                        window
                    )
                } else {
                    format!(
                        "{} once in {}. One run shows what it cost, not that it will run again: an export, a backfill or a query someone ran by hand looks like this. It needs work only if it keeps running.",
                        if *one { "This statement ran" } else { "Each of these statements ran" },
                        window
                    )
                };
                f.caveats.insert(0, caveat);
                f.verify.insert(0, "Re-scan later: if it hasn't run again, there is nothing to do.".into());
            }
            Change::Quiet { seconds, last, one, by_first_scan } => {
                let span = format::duration(seconds * 1000.0);
                let caveat = if *by_first_scan {
                    format!(
                        "{} run in the {span} since Culprit's first scan ({}): too short a time to say {} stopped.",
                        if *one { "This query hasn't" } else { "None of these queries has" },
                        crate::queries::short_when(last),
                        if *one { "it" } else { "they" }
                    )
                } else {
                    format!(
                        "{} run in the last {span} of this scan's data (since {}): too short a time to say {} stopped.",
                        if *one { "This query hasn't" } else { "None of these queries has" },
                        crate::queries::short_when(last),
                        if *one { "it" } else { "they" }
                    )
                };
                f.caveats.insert(0, caveat);
            }
            Change::Stopped { seconds, last, one, by_first_scan, successor: next } => {
                let caveat = if *by_first_scan {
                    format!(
                        "{} run in the {} since Culprit's first scan ({}); {} may have stopped earlier. A deploy before that scan may have changed or fixed {}, at a time the counters don't show; check that the current version of this code doesn't have the same problem.",
                        if *one { "This query hasn't" } else { "None of these queries has" },
                        format::duration(seconds * 1000.0),
                        crate::queries::short_when(last),
                        if *one { "it" } else { "they" },
                        if *one { "it" } else { "them" }
                    )
                } else {
                    format!(
                        "{} run since {} ({} before this scan's data ends). A deploy may have changed or fixed {}; check that the current version of this code doesn't have the same problem.",
                        if *one { "This query has not" } else { "None of these queries has" },
                        last.get(..16).unwrap_or(last).replace('T', " "),
                        format::duration(seconds * 1000.0),
                        if *one { "it" } else { "them" }
                    )
                };
                f.caveats.insert(0, caveat + &successor(next, false));
            }
            Change::Gone => {
                f.fix.as_mut().unwrap().summary = format!(
                    "Nothing to change: {} {} in the database's catalog any more, so {} dropped or renamed since these statements ran. If the application now uses other tables, scan those before making a change.",
                    format::list(&self.tables),
                    if self.tables.len() == 1 { "isn't" } else { "aren't" },
                    if self.tables.len() == 1 { "it was" } else { "they were" }
                );
                f.verify=vec!["Nothing to check: the tables are gone. If these statements return against replacement tables, scan those to see what needs a change.".into()];
            }
            Change::Creation { one, successor: next } => {
                f.fix.as_mut().unwrap().summary = format!(
                    "Nothing to create: {} stopped running, so an index or statistics made for {} now would serve code that no longer runs. If {} again, a scan shows this finding again with the SQL for {}.{}",
                    if *one { "the statement behind this finding" } else { "the statements behind this finding" },
                    if *one { "it" } else { "them" },
                    if *one { "it runs" } else { "they run" },
                    if *one { "it" } else { "them" },
                    successor(next, true)
                );
            }
            Change::IndexWork => {
                if let Some(fix) = &mut f.fix {
                    fix.summary = format!(
                        "Keep this as an index candidate. The captured workload does not establish enough measured work to recommend creating it now. Compare representative plans, reads and write maintenance if the workload grows. Only if later measurements justify a change, evaluate this candidate: {}",
                        fix.summary
                    );
                }
                f.verify=vec!["Compare representative plans, reads, execution time and write maintenance before choosing an index. Recheck the access path as the table or call volume grows; an old plan or optimizer request does not establish current benefit.".into()];
            }
            Change::Verification => {
                // Preserve a detector's more specific historical sentence. This only avoids redundant rendering;
                // eligibility has already been settled from inactive state, never from these words.
                if f.verify
                    .first()
                    .is_some_and(|v| v.starts_with("Nothing to check") && (v.contains("unless") || v.contains(" gone")))
                {
                    return;
                }
                f.verify=vec![if f.queries.is_empty()&&f.check=="missing-index" {"Nothing to check unless the optimizer asks for this index again within a scan's window."}
                    else if f.queries.len()<=1 {"Nothing to check unless it runs again: a later scan that sees it run lists this finding as current again."}
                    else {"Nothing to check unless they run again: a later scan that sees one of them run lists this finding as current again."}.into()];
            }
        }
    }
}

#[cfg(test)]
pub(super) fn stopped_running(s: &Snapshot, findings: &mut Vec<Finding>) {
    Rule(Policy::Stopped).refine(s, findings);
}

#[cfg(test)]
mod tests {
    use super::*;
    use culprit_core::{Category, Fix, QueryRef, WorkloadKind};

    fn scenario() -> (Snapshot, Finding) {
        let mut s: Snapshot = serde_json::from_str(include_str!("../../../../fixtures/mssql-demo.json")).unwrap();
        let w = s.workload.as_mut().unwrap();
        w.measurement = None;
        w.timeline = None;
        w.window_hours = 96.0;
        w.window_end = Some("2026-10-20T00:00:00Z".into());
        let q = &mut w.queries[0];
        q.first_seen = Some("2026-10-16T00:00:00Z".into());
        q.last_seen = Some("2026-10-18T00:00:00Z".into());
        q.executions = 100.0;
        q.trend = None;
        let mut f = support::finding(
            "acme.observation",
            "example",
            Category::Queries,
            support::subject(SubjectKind::Query, &q.id),
        );
        f.queries = vec![QueryRef { id: q.id.clone(), plan_node: None, note: None }];
        f.severity = Severity::High;
        f.impact.score = 80.0;
        f.summary = "Original observation".into();
        f.fix = Some(Fix {
            summary: "Consider this candidate".into(),
            sql: Some("CREATE INDEX ix_items_state ON shop.items (state)".into()),
            cost: Some("Writes".into()),
        });
        (s, f)
    }

    #[test]
    fn history_is_decided_and_applied_before_any_words_change() {
        let (s, f) = scenario();
        let mut findings = vec![f.clone()];
        let decisions = decide(Policy::Stopped, &s, &findings);
        assert!(decisions.iter().any(|d| matches!(d.change, Change::Stopped { seconds: 172800.0, .. })));
        for d in &decisions {
            d.apply_state(&mut findings[d.at]);
        }
        assert!(findings[0].inactive);
        assert_eq!((findings[0].severity, findings[0].impact.score), (Severity::Low, 20.0));
        assert_eq!(findings[0].summary, f.summary);
        assert_eq!(findings[0].fix, f.fix);
        assert!(findings[0].caveats.is_empty());
        for d in &decisions {
            d.render(&s, &mut findings[d.at]);
        }
        assert!(findings[0].caveats[0].contains("has not run since"));
    }

    #[test]
    fn prose_does_not_choose_lifecycle_or_index_eligibility() {
        let (mut s, mut f) = scenario();
        for policy in
            [Policy::Stopped, Policy::Once, Policy::Gone, Policy::Creation, Policy::Verification, Policy::IndexWork]
        {
            f.inactive = matches!(policy, Policy::Creation | Policy::Verification);
            s.workload.as_mut().unwrap().queries[0].executions =
                if matches!(policy, Policy::Once) { 1.0 } else { 100.0 };
            let expected = decide(policy, &s, &[f.clone()]);
            let mut rewritten = f.clone();
            rewritten.title = "Completely different title".into();
            rewritten.summary = "Until nobody knows. It also did something.".into();
            rewritten.caveats = vec!["A translated caveat".into()];
            rewritten.verify = vec!["Some other verification wording".into()];
            rewritten.fix.as_mut().unwrap().summary = "Another explanation of the same SQL".into();
            assert_eq!(expected, decide(policy, &s, &[rewritten]));
        }
    }

    #[test]
    fn missing_linked_queries_cannot_establish_history_for_the_whole_finding() {
        let (mut s, mut f) = scenario();
        f.queries.push(QueryRef { id: "not-in-the-scan".into(), plan_node: None, note: None });
        let mut findings = vec![f];
        let trace = Rule(Policy::Stopped).refine_traced(&s, &mut findings);
        assert!(!findings[0].inactive);
        assert!(!trace.iter().any(|r| matches!(r.outcome, RefinementOutcome::Historical { .. })));
        assert!(trace.iter().any(|r|matches!(&r.outcome,RefinementOutcome::InsufficientEvidence {missing_queries} if missing_queries==&vec!["not-in-the-scan".to_string()])));
        s.workload.as_mut().unwrap().queries[0].executions = 1.0;
        Rule(Policy::Once).refine(&s, &mut findings);
        assert_ne!(findings[0].actionability, Actionability::History);
        assert_eq!(findings[0].impact.score, 80.0);
        s.tables.clear();
        Rule(Policy::Gone).refine(&s, &mut findings);
        assert!(findings[0].fix.as_ref().unwrap().sql.is_some());
    }

    #[test]
    fn slow_log_decisions_distinguish_one_logged_run_from_one_execution() {
        let (mut s, f) = scenario();
        let w = s.workload.as_mut().unwrap();
        w.kind = Some(WorkloadKind::SlowLog);
        w.queries[0].executions = 1.0;
        let mut findings = vec![f];
        let stopped = Rule(Policy::Stopped).refine_traced(&s, &mut findings);
        assert!(
            !stopped
                .iter()
                .any(|d| matches!(d.outcome, RefinementOutcome::Historical { basis: HistoryBasis::Stopped, .. }))
        );
        let trace = Rule(Policy::Once).refine_traced(&s, &mut findings);
        assert!(matches!(
            trace[0].outcome,
            RefinementOutcome::Historical { basis: HistoryBasis::SingleLoggedExecution, .. }
        ));
        assert!(findings[0].caveats[0].contains("may run more often, faster"));
    }

    #[test]
    fn stopped_statements_withhold_creation_but_keep_diagnostic_settings() {
        let (s, mut f) = scenario();
        f.inactive = true;
        let mut findings = vec![f];
        let trace = Rule(Policy::Creation).refine_traced(&s, &mut findings);
        assert!(matches!(
            trace[0].outcome,
            RefinementOutcome::FixWithheld { basis: FixWithholdingBasis::StatementsStopped }
        ));
        assert!(findings[0].fix.as_ref().unwrap().sql.is_none());
        findings[0].fix.as_mut().unwrap().sql = Some("SET lock_timeout = '2s'".into());
        assert!(decide(Policy::Creation, &s, &findings).is_empty());
        Rule(Policy::Creation).refine(&s, &mut findings);
        assert!(findings[0].fix.as_ref().unwrap().sql.is_some());
    }
}
