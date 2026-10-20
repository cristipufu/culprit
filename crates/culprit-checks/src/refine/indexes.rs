//! Index evidence transfers are selected from column facts and plans, before any finding is modified.
use super::*;
use culprit_core::{RefinementDecision, RefinementOutcome};

#[derive(Clone, Copy)]
pub(super) enum Policy {
    MissingScan,
    PlanRequest,
    AutomaticForeignKey,
    Histogram,
}
impl Policy {
    fn id(self) -> &'static str {
        match self {
            Self::MissingScan => "missing-index-carries-scan",
            Self::PlanRequest => "index-carries-plan-request",
            Self::AutomaticForeignKey => "foreign-key-carries-automatic-index",
            Self::Histogram => "index-covers-histogram",
        }
    }
    fn label(self) -> &'static str {
        match self {
            Self::MissingScan => "a missing-index request absorbs the scan behind it",
            Self::PlanRequest => "a plan's index request is the finding for its table and key",
            Self::AutomaticForeignKey => "an index the engine builds for itself is the foreign key's",
            Self::Histogram => "an index to add replaces a histogram",
        }
    }
}
pub(super) struct Rule(pub Policy);
enum Action {
    Move { into: usize, missing_plan_node: bool },
    Automatic { into: usize, queries: Vec<String>, remaining: Option<Box<Finding>> },
    Histogram { by: Vec<usize>, columns: Vec<String>, full: bool },
}
struct Decision {
    at: usize,
    action: Action,
}

fn normal(name: &str) -> String {
    name.replace(['[', ']', '"', '`'], "").to_ascii_lowercase()
}
fn same_table(s: &Snapshot, a: &str, b: &str) -> bool {
    let resolve = |name: &str| {
        let mut found =
            s.tables.iter().filter(|t| culprit_core::insights::catalog_table_matches(name, &t.schema, &t.name));
        let first = found.next();
        (first, found.next().is_none())
    };
    match (resolve(a), resolve(b)) {
        ((Some(a), true), (Some(b), true)) => {
            a.schema.eq_ignore_ascii_case(&b.schema) && a.name.eq_ignore_ascii_case(&b.name)
        }
        ((None, true), (None, true)) => a.eq_ignore_ascii_case(b) && a.contains('.') && b.contains('.'),
        _ => false,
    }
}

fn decide(policy: Policy, s: &Snapshot, findings: &[Finding]) -> Vec<Decision> {
    let mut out = Vec::new();
    for (at, f) in findings.iter().enumerate() {
        let action = match policy {
            Policy::MissingScan if f.check == "filtered-scan" => findings
                .iter()
                .position(|g| {
                    g.check == "missing-index"
                        && g.subject.name.eq_ignore_ascii_case(&f.subject.name)
                        && missing_index_serves_scan(&g.facts.index_keys, &f.facts.index_keys)
                })
                .map(|into| Action::Move { into, missing_plan_node: true }),
            Policy::PlanRequest if f.id.starts_with("plan-insights:missing-index-hint:") => {
                let Some(w) = &s.workload else {
                    continue;
                };
                let leads: Option<Vec<_>> = f
                    .queries
                    .iter()
                    .map(|r| {
                        let plan = w.query(&r.id)?.plan.as_ref()?;
                        let hint = plan.index_hints.iter().find(|h| same_table(s, &h.table, &f.subject.name))?;
                        hint.equality.iter().chain(&hint.inequality).next().map(|k| normal(k))
                    })
                    .collect();
                leads
                    .filter(|leads| !leads.is_empty())
                    .and_then(|leads| {
                        findings.iter().position(|g| {
                            matches!(g.check.as_str(), "missing-index" | "filtered-scan")
                                && same_table(s, &g.subject.name, &f.subject.name)
                                && leads
                                    .iter()
                                    .all(|lead| g.facts.index_keys.first().is_some_and(|key| normal(key) == *lead))
                        })
                    })
                    .map(|into| Action::Move { into, missing_plan_node: false })
            }
            Policy::AutomaticForeignKey if f.id.starts_with("plan-insights:automatic-index:") => {
                let Some(w) = &s.workload else {
                    continue;
                };
                if merge::complete_queries(s, f).is_none() {
                    continue;
                }
                let built: Vec<_> = f
                    .queries
                    .iter()
                    .filter_map(|r| {
                        let plan = w.query(&r.id)?.plan.as_ref()?;
                        let b = culprit_core::insights::engine_built_indexes(plan)
                            .into_iter()
                            .find(|b| Some(b.node) == r.plan_node)?;
                        Some((
                            r.id.clone(),
                            b.table.clone(),
                            b.keys.iter().map(|k| k.to_ascii_lowercase()).collect::<Vec<_>>(),
                        ))
                    })
                    .collect();
                let serves = |g: &Finding, table: &str, keys: &[String]| {
                    !g.facts.index_keys.is_empty()
                        && same_table(s, table, &g.subject.name)
                        && keys.len() >= g.facts.index_keys.len()
                        && keys.iter().zip(&g.facts.index_keys).all(|(a, b)| a.eq_ignore_ascii_case(b))
                };
                let Some(into) = findings.iter().position(|g| {
                    g.check == "unindexed-fk" && built.iter().any(|(_, table, keys)| serves(g, table, keys))
                }) else {
                    continue;
                };
                let queries: Vec<_> = built
                    .iter()
                    .filter(|(_, table, keys)| serves(&findings[into], table, keys))
                    .map(|(q, _, _)| q.clone())
                    .collect();
                let remaining_ids: Vec<_> =
                    f.queries.iter().filter(|r| !queries.contains(&r.id)).map(|r| r.id.clone()).collect();
                let remaining = if remaining_ids.is_empty() {
                    None
                } else {
                    // Re-run the detector on the remaining evidence, preserving the database-time denominator. Merely
                    // removing references would leave the old group's measurements and priority attached to a subset.
                    let remaining = remaining_ids.iter().filter_map(|id| w.query(id));
                    let Some(refreshed) =
                        crate::queries::plan_insights_for_queries(s, remaining).into_iter().find(|g| g.id == f.id)
                    else {
                        continue;
                    };
                    Some(Box::new(refreshed))
                };
                Some(Action::Automatic { into, queries, remaining })
            }
            Policy::Histogram if f.check == "missing-histogram" => {
                let wanted = &f.facts.histogram_columns;
                if wanted.is_empty() {
                    continue;
                }
                let indexes: Vec<_> = findings
                    .iter()
                    .enumerate()
                    .filter(|(_, g)| {
                        matches!(g.check.as_str(), "missing-index" | "filtered-scan")
                            && g.subject.name.eq_ignore_ascii_case(&f.subject.name)
                            && lifecycle::recommends_unfiltered_index(s, g)
                    })
                    .collect();
                // Match the detector's own leading-column rule. A column later in a composite key isn't enough to
                // establish standalone column statistics for predicates that omit that key's leading column.
                let columns: Vec<_> = wanted
                    .iter()
                    .filter(|c| {
                        indexes
                            .iter()
                            .any(|(_, g)| g.facts.index_keys.first().is_some_and(|k| k.eq_ignore_ascii_case(c)))
                    })
                    .cloned()
                    .collect();
                if columns.is_empty() {
                    continue;
                }
                let by = indexes
                    .iter()
                    .filter(|(_, g)| {
                        g.facts.index_keys.first().is_some_and(|k| columns.iter().any(|c| k.eq_ignore_ascii_case(c)))
                    })
                    .map(|(i, _)| *i)
                    .collect();
                Some(Action::Histogram { by, full: columns.len() == wanted.len(), columns })
            }
            _ => None,
        };
        if let Some(action) = action {
            out.push(Decision { at, action });
        }
    }
    out
}

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
            .map(|d| {
                let outcome = match &d.action {
                    Action::Move { into, .. } | Action::Automatic { into, remaining: None, .. } => {
                        RefinementOutcome::Absorbed { into: findings[*into].id.clone() }
                    }
                    Action::Automatic { into, queries, .. } => RefinementOutcome::EvidenceTransferred {
                        into: findings[*into].id.clone(),
                        queries: queries.clone(),
                    },
                    Action::Histogram { by, columns, full } => {
                        if *full {
                            RefinementOutcome::Covered { by: by.iter().map(|&i| findings[i].id.clone()).collect() }
                        } else {
                            RefinementOutcome::ColumnsCovered {
                                by: by.iter().map(|&i| findings[i].id.clone()).collect(),
                                columns: columns.clone(),
                            }
                        }
                    }
                };
                RefinementDecision {
                    rule: self.0.id().into(),
                    finding: Some(findings[d.at].id.clone()),
                    outcome,
                    queries: findings[d.at].queries.iter().map(|r| r.id.clone()).collect(),
                    tables: vec![findings[d.at].subject.name.clone()],
                }
            })
            .collect();
        let mut gone = BTreeSet::new();
        for d in &decisions {
            match &d.action {
                Action::Move { into, missing_plan_node } => {
                    let refs = findings[d.at].queries.clone();
                    for r in refs {
                        match findings[*into].queries.iter_mut().find(|q| q.id == r.id) {
                            Some(existing) if *missing_plan_node && existing.plan_node.is_none() => {
                                existing.plan_node = r.plan_node
                            }
                            Some(_) => {}
                            None => findings[*into].queries.push(r),
                        }
                    }
                    gone.insert(d.at);
                }
                Action::Automatic { into, queries, remaining } => {
                    let refs: Vec<_> =
                        findings[d.at].queries.iter().filter(|r| queries.contains(&r.id)).cloned().collect();
                    for r in refs {
                        match findings[*into].queries.iter_mut().find(|q| q.id == r.id) {
                            Some(existing) => existing.plan_node = existing.plan_node.or(r.plan_node),
                            None => findings[*into].queries.push(r),
                        }
                    }
                    if let Some(remaining) = remaining {
                        findings[d.at] = *remaining.clone();
                    } else {
                        gone.insert(d.at);
                    }
                }
                Action::Histogram { full: true, .. } => {
                    gone.insert(d.at);
                }
                Action::Histogram { .. } => {}
            }
        }
        // Preserve the established order when several observations decorate the same foreign key.
        for d in decisions.iter().rev() {
            if let Action::Automatic { into, queries, .. } = &d.action {
                findings[*into].summary.push_str(&format!(
                    " {} already builds this index for itself on every run of {}, and drops it after.",
                    s.server.engine,
                    if queries.len() == 1 {
                        "one of these statements".into()
                    } else {
                        format!("{} of these statements", queries.len())
                    }
                ));
            }
        }
        for d in &decisions {
            if let Action::Histogram { columns, full: false, .. } = &d.action {
                findings[d.at].caveats.push(format!(
                    "An index is also suggested for {}; once it exists the optimizer estimates from the index instead.",
                    format::list(columns)
                ));
            }
        }
        let mut at = 0;
        findings.retain(|_| {
            let keep = !gone.contains(&at);
            at += 1;
            keep
        });
        trace
    }
}

#[cfg(test)]
mod identity_tests {
    use super::*;
    #[test]
    fn neither_direction_of_an_unqualified_ambiguous_name_establishes_one_table() {
        let mut s: Snapshot = serde_json::from_str(include_str!("../../../../fixtures/mssql-demo.json")).unwrap();
        let mut table = s.tables[0].clone();
        table.schema = "one".into();
        table.name = "orders".into();
        let mut other = table.clone();
        other.schema = "two".into();
        s.tables = vec![table, other];
        assert!(!same_table(&s, "one.orders", "orders"));
        assert!(!same_table(&s, "orders", "one.orders"));
        assert!(!same_table(&s, "one.orders", "two.orders"));
        assert!(same_table(&s, "one.orders", "one.orders"));
        s.tables.pop();
        assert!(same_table(&s, "one.orders", "orders"));
    }
}
