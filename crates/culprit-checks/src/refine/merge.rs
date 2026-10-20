//! The semantic state of an absorption is computed before either finding's prose is composed.
use super::*;
use culprit_core::{Actionability, Impact, QueryRef, RefinementDecision, RefinementOutcome};

pub(super) fn trace(rule: &str, findings: &[Finding], from: usize, into: usize) -> RefinementDecision {
    RefinementDecision {
        rule: rule.into(),
        finding: Some(findings[from].id.clone()),
        outcome: RefinementOutcome::Absorbed { into: findings[into].id.clone() },
        queries: findings[from].queries.iter().map(|r| r.id.clone()).collect(),
        tables: if findings[from].subject.kind == SubjectKind::Table {
            vec![findings[from].subject.name.clone()]
        } else {
            Vec::new()
        },
    }
}

pub(super) struct Absorption {
    pub from: usize,
    pub into: usize,
    pub source: Finding,
    impact: Impact,
    queries: Vec<QueryRef>,
    severity: Severity,
    last_seen: Option<String>,
    share_label: Option<(f64, f64)>,
}
impl Absorption {
    pub fn decide(s: &Snapshot, findings: &[Finding], into: usize, from: usize) -> Self {
        let source = findings[from].clone();
        let target = &findings[into];
        let history = source.inactive
            || source.actionability == Actionability::History
            || s.workload
                .as_ref()
                .is_some_and(|w| source.queries.iter().filter_map(|r| w.query(&r.id)).any(|q| query_is_history(s, q)));
        let of_listed = |f: &Finding| f.impact.db_time_share.is_none() || share_of_listed(s, f).is_some();
        let listed_share = if (target.impact.db_time_share.is_some() || source.impact.db_time_share.is_some())
            && of_listed(target)
            && of_listed(&source)
        {
            s.workload.as_ref().and_then(|w| {
                let ids: BTreeSet<_> = target.queries.iter().chain(&source.queries).map(|r| &r.id).collect();
                let all: Option<Vec<_>> = ids.iter().map(|id| w.query(id)).collect();
                support::query_impact(s, &all?).map(|impact| impact.share)
            })
        } else {
            None
        };
        let mut queries = target.queries.clone();
        for q in &source.queries {
            if let Some(existing) = queries.iter_mut().find(|r| r.id == q.id) {
                if existing.note.is_none() {
                    existing.note = q.note.clone();
                }
            } else {
                queries.push(q.clone());
            }
        }
        let mut impact = target.impact.clone();
        impact.score = impact.score.max(source.impact.score);
        impact.db_time_share = match (impact.db_time_share, source.impact.db_time_share) {
            (Some(a), Some(b)) => Some(listed_share.unwrap_or(a.max(b))),
            (None, None) => None,
            (a, b) => listed_share.or(a).or(b),
        };
        if let Some(share) = listed_share {
            impact.score = impact.score.max(support::score_for_share(share));
        }
        impact.time_ms_per_day = impact.time_ms_per_day.or(source.impact.time_ms_per_day);
        Self {
            from,
            into,
            source: source.clone(),
            impact,
            queries,
            severity: if history { target.severity } else { target.severity.max(source.severity) },
            last_seen: target.last_seen.clone().max(source.last_seen.clone()),
            share_label: target.impact.db_time_share.zip(listed_share),
        }
    }
    pub fn apply_state(&self, findings: &mut [Finding]) {
        let target = &mut findings[self.into];
        target.queries = self.queries.clone();
        target.impact = self.impact.clone();
        target.severity = self.severity;
        target.last_seen = self.last_seen.clone();
    }
    pub fn render(&self, findings: &mut [Finding], note: Option<String>) {
        let target = &mut findings[self.into];
        if let Some((before, after)) = self.share_label
            && target.impact.label == support::share_label(before)
        {
            target.impact.label = support::share_label(after);
        }
        if let Some(note) = note {
            for q in &self.source.queries {
                if let Some(r) = target.queries.iter_mut().find(|r| r.id == q.id && r.note.is_none()) {
                    r.note = Some(note.clone());
                }
            }
        }
    }
    pub fn finish(self, findings: &mut Vec<Finding>) -> usize {
        findings.remove(self.from);
        if self.from < self.into { self.into - 1 } else { self.into }
    }
}

pub(super) fn complete_queries<'a>(s: &'a Snapshot, f: &Finding) -> Option<Vec<&'a QueryStat>> {
    let w = s.workload.as_ref()?;
    f.queries.iter().map(|r| w.query(&r.id)).collect()
}
