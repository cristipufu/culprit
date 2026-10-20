//! Declarative requirements for a negative check result. Partial evidence can still produce findings;
//! it cannot establish that an unobserved problem is absent. Requirements live beside each check.

use culprit_core::{CheckEvidence, Snapshot, SourceState};

/// Facts a source can contribute, independent of any check's identifier.
#[derive(Clone, Copy, Debug)]
pub enum Observation {
    LogFailures,
    RequestCounts,
    Spaces,
    Sequences,
    Multixacts,
    OperationalCounters,
    Cluster,
    WalSize,
}

#[derive(Clone, Copy, Debug)]
pub enum QueryMeasure {
    Work,
    CapturedPlans,
    Timeline,
    PlanVariants,
    Variance,
    ParameterSpread,
    Failures,
    Planning,
    Jit,
    LogBytes,
    Rows,
    Histograms,
    TempWrites,
    Grants,
    Waits,
}

/// A reusable evidence contract. Custom checks can use the same evaluator in `Check::evidence`.
#[derive(Clone, Copy, Debug)]
pub enum Requirement<'a> {
    Observed(Observation, &'a [&'a str]),
    Source(&'a str),
    IndexCatalog,
    ForeignKeys,
    IndexHistory,
    Query(QueryMeasure),
    Pressure { signal: Option<&'a str>, timed_files: bool, complete: bool },
    Settings,
    Deadlocks,
    Unknown,
    Incomplete(&'a str),
}

pub fn source_failed(s: &Snapshot, names: &[&str]) -> Option<String> {
    s.source_problem(names)
}

pub fn index_catalog(s: &Snapshot) -> Option<String> {
    source_failed(s, &["tables", "indexes", "foreign_keys", "columns", "catalog"])
}

pub fn assess(requirements: &[Requirement<'_>], s: &Snapshot) -> CheckEvidence {
    if requirements.is_empty() {
        return CheckEvidence::Insufficient(
            "This check does not declare the evidence needed to establish a negative result.".into(),
        );
    }
    let mut result = CheckEvidence::Available;
    for requirement in requirements {
        match requirement.assess(s) {
            evidence @ CheckEvidence::Unavailable(_) => return evidence,
            evidence @ CheckEvidence::NotApplicable(_) => result = evidence,
            evidence @ CheckEvidence::Insufficient(_) if matches!(result, CheckEvidence::Available) => {
                result = evidence
            }
            _ => {}
        }
    }
    result
}

impl Requirement<'_> {
    fn assess(&self, s: &Snapshot) -> CheckEvidence {
        use CheckEvidence::*;
        let insufficient = |why: &str| Insufficient(why.into());
        match self {
            Self::Observed(observation, sources) => {
                let known = match observation {
                    Observation::LogFailures => !s.operations.log_failures.is_empty(),
                    Observation::RequestCounts => s.queries().iter().any(|q| q.requests.is_some()),
                    Observation::Spaces => !s.operations.spaces.is_empty(),
                    Observation::Sequences => !s.operations.sequences.is_empty(),
                    Observation::Multixacts => !s.operations.multixacts.is_empty(),
                    Observation::OperationalCounters => !s.operations.counters.is_empty(),
                    Observation::Cluster => s.operations.cluster.is_some(),
                    Observation::WalSize => s.operations.sqlite.as_ref().is_some_and(|w| w.wal_bytes.is_some()),
                };
                if let Some(reason) = source_failed(s, sources) {
                    return if known { Insufficient(reason) } else { Unavailable(reason) };
                }
                if known {
                    return Available;
                }
                if !sources.is_empty()
                    && sources
                        .iter()
                        .all(|name| s.source_outcome(name).is_some_and(|o| o.state == SourceState::NotApplicable))
                {
                    return NotApplicable(
                        s.source_outcome(sources[0])
                            .and_then(|o| o.reason.clone())
                            .unwrap_or_else(|| "This source does not apply to this database.".into()),
                    );
                }
                insufficient(
                    "This source was not recorded, was not applicable, or did not expose visible objects. No clean result can be established from its absence.",
                )
            }
            Self::Source(name) => match s.source_outcome(name) {
                Some(o) => match o.state {
                    SourceState::Collected | SourceState::Empty => Available,
                    SourceState::Partial => {
                        Insufficient(o.reason.clone().unwrap_or_else(|| format!("{name} was only partly collected.")))
                    }
                    SourceState::Unavailable => {
                        Unavailable(o.reason.clone().unwrap_or_else(|| format!("{name} was unavailable.")))
                    }
                    SourceState::NotApplicable => NotApplicable(
                        o.reason.clone().unwrap_or_else(|| format!("{name} does not apply to this database.")),
                    ),
                },
                None => source_failed(s, &[*name]).map(Unavailable).unwrap_or_else(|| {
                    insufficient("This scan did not record the required source's collection outcome.")
                }),
            },
            Self::IndexCatalog => index_catalog(s)
                .map(|reason| Unavailable(format!("Current schema evidence was incomplete: {reason}")))
                .unwrap_or(Available),
            Self::ForeignKeys if s.foreign_keys.is_empty() => {
                if ["foreign_keys", "catalog"].iter().any(|name| {
                    s.source_outcome(name).is_some_and(|o| {
                        matches!(o.state, SourceState::Collected | SourceState::Empty | SourceState::NotApplicable)
                    })
                }) {
                    NotApplicable("No foreign keys were collected in this database.".into())
                } else {
                    insufficient(
                        "No foreign keys were recorded, but this scan does not establish a complete foreign-key catalog.",
                    )
                }
            }
            Self::ForeignKeys => Available,
            Self::IndexHistory if !crate::indexes::watched_long_enough(s) => {
                insufficient("At least a week of comparable index-usage counters is needed to assess absence of reads.")
            }
            Self::IndexHistory => Available,
            Self::Query(measure) => query(*measure, s),
            Self::Pressure { signal, timed_files, complete } => {
                let Some(p) = &s.pressure else {
                    return Unavailable(
                        "The source measurements needed for this assessment were not collected.".into(),
                    );
                };
                if signal.is_some_and(|key| p.signal(key).is_none()) {
                    return insufficient("The required pressure measurement was not recorded by this scan.");
                }
                if signal
                    .and_then(|key| p.signal(key))
                    .is_some_and(|s| !s.value.is_finite() || s.status == culprit_core::SignalStatus::Info)
                {
                    return insufficient(
                        "This pressure measurement's scope or observation period does not support a health assessment.",
                    );
                }
                if *timed_files
                    && !p.files.iter().any(|f| {
                        f.avg_read_ms().into_iter().chain(f.avg_write_ms()).any(|ms| ms.is_finite() && ms >= 0.0)
                    })
                {
                    return insufficient("Storage operations were not timed in this scan.");
                }
                if *complete {
                    Available
                } else {
                    insufficient("The available evidence does not establish complete coverage of this check.")
                }
            }
            Self::Settings if s.settings.is_empty() => insufficient("No settings were collected."),
            Self::Deadlocks if s.deadlocks.is_none() => insufficient("No deadlock evidence was collected."),
            Self::Settings | Self::Deadlocks => {
                insufficient("The available evidence does not establish complete coverage of this check.")
            }
            Self::Unknown => insufficient("The available evidence does not establish complete coverage of this check."),
            Self::Incomplete(reason) => insufficient(reason),
        }
    }
}

fn query(measure: QueryMeasure, s: &Snapshot) -> CheckEvidence {
    use CheckEvidence::*;
    let insufficient = |why: &str| Insufficient(why.into());
    let Some(w) = &s.workload else {
        if matches!(measure, QueryMeasure::Grants) && s.pressure.as_ref().is_some_and(|p| !p.grants.is_empty()) {
            return insufficient("Current grant evidence is available, but statement history was not collected.");
        }
        return Unavailable(s.no_workload_reason().unwrap_or_else(|| "No statement history was collected.".into()));
    };
    if w.queries.is_empty() {
        return insufficient("No statements were captured; this does not establish the absence of workload problems.");
    }
    if matches!(measure, QueryMeasure::CapturedPlans) {
        if w.queries.iter().any(|q| q.plan.is_none()) {
            return insufficient(
                "Only some statements have captured plans; unmeasured runs and uncaptured statements remain unassessed.",
            );
        }
        return insufficient(
            "Captured plans were inspected. Actual rows, timing and runtime-only problems are assessed only where that plan records them.",
        );
    }
    if matches!(measure, QueryMeasure::Timeline) && !w.timeline.as_ref().is_some_and(|t| t.len() >= 4) {
        return insufficient("A comparable baseline and later workload are needed to assess regressions.");
    }
    if matches!(measure, QueryMeasure::PlanVariants) && !w.queries.iter().any(|q| q.plans.len() > 1) {
        return insufficient("No comparable earlier and later plan variants were recorded.");
    }
    let costs = |f: fn(&culprit_core::QueryCosts) -> bool| w.queries.iter().all(|q| q.costs.as_ref().is_some_and(f));
    let known = match measure {
        QueryMeasure::Variance => {
            w.queries.iter().all(|q| q.stddev_time_ms.is_some() && q.max_time_ms.is_some() && q.executions >= 10.0)
        }
        QueryMeasure::ParameterSpread => w.queries.iter().all(|q| q.plans.len() > 1 || q.spread.is_some()),
        QueryMeasure::Failures => costs(|c| c.failed.is_some() || c.aborted.is_some()),
        QueryMeasure::Planning => costs(|c| c.plan_ms.is_some()),
        QueryMeasure::Jit => costs(|c| c.jit_ms.is_some()),
        QueryMeasure::LogBytes => costs(|c| c.log_bytes.is_some()),
        QueryMeasure::Rows => w.queries.iter().all(|q| q.rows_per_execution().is_some()),
        QueryMeasure::Histograms => s.histograms.is_some(),
        QueryMeasure::TempWrites => w.queries.iter().all(|q| {
            q.temp_writes.is_some() || q.plan.as_ref().is_some_and(|p| p.kind == culprit_core::PlanKind::Actual)
        }),
        QueryMeasure::Grants => s.pressure.as_ref().is_some_and(|p| !p.grants.is_empty()),
        QueryMeasure::Waits => w.queries.iter().any(|q| !q.waits.is_empty()),
        _ => true,
    };
    if !known {
        return insufficient("This scan did not record the measurements this check needs.");
    }
    let source = match w.kind() {
        Some(culprit_core::WorkloadKind::QueryStore) => "query_store",
        Some(culprit_core::WorkloadKind::PlanCache) => "plan_cache",
        Some(culprit_core::WorkloadKind::QueryLog) => "query_log",
        _ => "workload",
    };
    if let Some(reason) = source_failed(s, &[source]) {
        return Insufficient(reason);
    }
    Available
}
