//! Merge lock evidence only after establishing complete statement and table relationships.
use super::*;
use culprit_core::{Actionability, FindingLockWait, RefinementDecision};

#[derive(Clone, Copy)]
pub(super) enum Policy {
    Episode,
    Chain,
}
impl Policy {
    fn id(self) -> &'static str {
        match self {
            Self::Episode => "one-lock-episode",
            Self::Chain => "one-blocking-chain",
        }
    }
    fn label(self) -> &'static str {
        match self {
            Self::Episode => "one lock episode on a table",
            Self::Chain => "one blocking chain on a table",
        }
    }
}
pub(super) struct Rule(pub Policy);
enum Evidence {
    Episode { query: String, table: String, wait: FindingLockWait },
    Chain { current: bool, promote: bool },
}
struct Decision {
    from: usize,
    into: usize,
    evidence: Evidence,
    total_lock_ms: f64,
}
fn span(q: &QueryStat) -> Option<(f64, f64)> {
    let (first, last) = (
        support::parse_epoch_seconds(q.first_seen.as_deref()?)?,
        support::parse_epoch_seconds(q.last_seen.as_deref()?)?,
    );
    (first <= last).then_some((first, last))
}
fn common_table(s: &Snapshot, a: &QueryStat, b: &QueryStat) -> Option<String> {
    s.tables.iter().find_map(|t| {
        let touching = support::queries_touching(s, &t.schema, &t.name);
        (touching.iter().any(|q| q.id == a.id) && touching.iter().any(|q| q.id == b.id)).then(|| t.qualified())
    })
}
fn decide(policy: Policy, s: &Snapshot, findings: &[Finding]) -> Option<Decision> {
    for (i, f) in findings.iter().enumerate().filter(|(_, f)| f.check == "lock-time") {
        let Some(queries) = merge::complete_queries(s, f).filter(|queries| {
            !queries.is_empty()
                && queries.iter().all(|q| {
                    let lock = signals::lock_ms(q);
                    lock.is_finite() && lock > 0.0 && q.total_time_ms.is_finite() && q.total_time_ms > 0.0
                })
        }) else {
            continue;
        };
        let (from, into, evidence) = match policy {
            Policy::Episode => {
                let Some(wait) = &f.facts.lock_wait else { continue };
                let a = queries[0];
                let Some((first, last)) = span(a) else { continue };
                let pair = findings.iter().enumerate().find_map(|(j, g)| {
                    if i == j || g.check != "lock-time" {
                        return None;
                    }
                    let other = merge::complete_queries(s, g)?;
                    let b = *other.first()?;
                    let other_wait = g.facts.lock_wait.as_ref()?;
                    let (b_first, b_last) = span(b)?;
                    (b_first <= last && first <= b_last).then_some(())?;
                    let table = common_table(s, a, b)?;
                    let t = s.tables.iter().find(|t| t.qualified() == table)?;
                    let touching = support::queries_touching(s, &t.schema, &t.name);
                    queries
                        .iter()
                        .chain(&other)
                        .all(|q| touching.iter().any(|touched| touched.id == q.id))
                        .then_some(())?;
                    Some((j, b, other_wait, table))
                });
                let Some((j, b, other_wait, table)) = pair else { continue };
                if wait.ms >= other_wait.ms {
                    (j, i, Evidence::Episode { query: b.id.clone(), table, wait: other_wait.clone() })
                } else {
                    (i, j, Evidence::Episode { query: a.id.clone(), table, wait: wait.clone() })
                }
            }
            Policy::Chain => {
                // Cover the entire episode. Sharing just one reference does not establish a cause for the others.
                let Some(into) = findings.iter().position(|g| {
                    g.check == "hot-rows"
                        && queries.iter().all(|q| g.queries.iter().any(|r| r.id == q.id))
                        && merge::complete_queries(s, g).is_some()
                }) else {
                    continue;
                };
                let current = f.is_actionable() && queries.iter().any(|q| !query_is_history(s, q));
                (i, into, Evidence::Chain { current, promote: current && !findings[into].is_actionable() })
            }
        };
        let Some(all) = merge::complete_queries(s, &findings[into]).zip(merge::complete_queries(s, &findings[from]))
        else {
            continue;
        };
        let mut seen = BTreeSet::new();
        let total_lock_ms = all
            .0
            .iter()
            .chain(&all.1)
            .filter(|q| seen.insert(q.id.clone()))
            .filter(|q| !matches!(evidence, Evidence::Chain { current: true, .. }) || !query_is_history(s, q))
            .map(|q| signals::lock_ms(q))
            .sum();
        return Some(Decision { from, into, evidence, total_lock_ms });
    }
    None
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
            let absorption = merge::Absorption::decide(s, findings, d.into, d.from);
            absorption.apply_state(findings);
            if let Evidence::Chain { promote: true, .. } = d.evidence {
                findings[d.into].actionability = Actionability::OptionalImprovement;
                findings[d.into].fix = absorption.source.fix.clone();
            }
            let target = &mut findings[d.into];
            let mut note = None;
            let window = s.workload.as_ref().map(|w| format!(" in {}", w.window_phrase())).unwrap_or_default();
            match d.evidence {
                Evidence::Episode { query, table, wait } => {
                    let w = s.workload.as_ref().unwrap();
                    let q = w.query(&query).unwrap();
                    let share = if wait.share >= 0.97 {
                        "nearly all of its time".into()
                    } else {
                        format!("{} of its time", format::percent(wait.share))
                    };
                    let waited = format!("{} ({share})", format::duration(wait.ms));
                    let start = w
                        .window_end
                        .as_deref()
                        .and_then(support::parse_epoch_seconds)
                        .map(|end| end - w.measurement_hours() * 3600.0);
                    let when = match (q.first_seen.as_deref(), q.last_seen.as_deref(), span(q), start) {
                        (Some(first), Some(last), Some((from, _)), Some(start)) if from >= start => {
                            culprit_core::span_when(first, last)
                        }
                        _ => "in the same window".into(),
                    };
                    target.summary.push_str(&format!(" {} (query {}) waited on locks on {table} {when} too, {waited}: likely the same episode of blocking, behind the same holder or one another.",capitalize(&support::query_handle(q)),q.id));
                    if wait.versioning_spares_read
                        && let Some(fix) = target.fix.as_mut()
                    {
                        let stopped = query_stopped(s, q).is_some();
                        fix.summary.push_str(&format!(" Query {}, a read, {} for writes on the same table while READ_COMMITTED_SNAPSHOT is off: with row versioning on (config:read-committed-snapshot has the change), it {} the last committed version of the rows instead.",q.id,if stopped {"waited"} else {"waits"},if stopped {"would have read"} else {"reads"}));
                    }
                    note = Some(format!(
                        "Waited {waited} on locks on {table} in the same hours; the holder isn't recorded."
                    ));
                    target.impact.label = format!("{} waiting on locks", format::duration(d.total_lock_ms));
                }
                Evidence::Chain { current, promote } => {
                    if promote {
                        let table = finding_table(target);
                        target.title = format!("Queries on {} recorded lock waits{window}", leaf(&table));
                    }
                    for q in merge::complete_queries(s, &absorption.source).unwrap() {
                        let lock = signals::lock_ms(q);
                        let share = (lock / q.total_time_ms).min(1.0);
                        let share = if share >= 0.97 {
                            "nearly all of its time".into()
                        } else {
                            format!("{} of its time", format::percent(share))
                        };
                        let waited = format::duration(lock);
                        target.summary.push_str(&format!(
                            " {} waited {waited} for these locks{window}, {share}.",
                            capitalize(&support::query_handle(q))
                        ));
                        if let Some(r) = target.queries.iter_mut().find(|r| r.id == q.id) {
                            r.note = Some(format!("Waited {waited} on locks, {share}{window}."));
                        }
                    }
                    if current {
                        target.impact.label = format!("{} waiting on locks", format::duration(d.total_lock_ms));
                    }
                }
            }
            absorption.render(findings, note);
            absorption.finish(findings);
        }
        trace
    }
}
