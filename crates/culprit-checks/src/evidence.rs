//! Declarative requirements for a negative check result. Partial evidence can still produce findings;
//! it cannot establish that an unobserved problem is absent. Requirements live beside each check.

use culprit_core::{CheckEvidence, Snapshot, SourceState, source_id};

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
    MissingIndexes,
    Tuning,
}

#[derive(Clone, Copy, Debug)]
pub enum QueryMeasure {
    Work,
    Plans,
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
    Waits(&'static str),
}

#[derive(Clone, Copy, Debug)]
pub enum TableMeasure {
    Heap,
    DeadRows,
    Xid,
    Bloat,
    HotUpdates,
    RowSize,
    Scans,
    Writes,
}

/// A reusable evidence contract. Custom checks can use the same evaluator in `Check::evidence`.
#[derive(Clone, Copy, Debug)]
pub enum Requirement<'a> {
    Observed(Observation, &'a [&'a str]),
    Source(&'a str),
    /// Explicit outcomes win. Older scans may establish this read through its retained payload.
    SourceWithLegacy(&'a str, Observation),
    IndexCatalog,
    ForeignKeys,
    IndexHistory,
    Query(QueryMeasure),
    Tables(TableMeasure),
    Statistics,
    Pressure {
        signal: Option<&'a str>,
        timed_files: bool,
        complete: bool,
    },
    Settings,
    Deadlocks,
    Unknown,
    Incomplete(&'a str),
}

pub fn source_failed(s: &Snapshot, names: &[&str]) -> Option<String> {
    s.source_problem(names)
}

pub fn index_catalog(s: &Snapshot) -> Option<String> {
    source_failed(
        s,
        &[source_id::TABLES, source_id::INDEXES, source_id::FOREIGN_KEYS, source_id::COLUMNS, source_id::CATALOG],
    )
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
            evidence @ CheckEvidence::Insufficient(_) if matches!(result, CheckEvidence::Queries { .. }) => {
                result = evidence
            }
            CheckEvidence::Queries { assessed, reason } => match &mut result {
                CheckEvidence::Available => result = CheckEvidence::Queries { assessed, reason },
                CheckEvidence::Queries { assessed: previous, reason: previous_reason } => {
                    previous.retain(|q| assessed.contains(q));
                    if previous_reason.is_none() {
                        *previous_reason = reason;
                    }
                }
                _ => {}
            },
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
                    Observation::MissingIndexes => !s.missing_indexes.is_empty(),
                    Observation::Tuning => s.tuning.is_some(),
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
            Self::SourceWithLegacy(name, observation) => {
                if s.sources.is_empty() && source_failed(s, &[*name]).is_none()
                    && matches!(Self::Observed(*observation, &[]).assess(s), Available)
                { Available } else { Self::Source(name).assess(s) }
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
                if [source_id::FOREIGN_KEYS, source_id::CATALOG].iter().any(|name| {
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
            Self::Tables(measure) => tables(*measure, s),
            Self::Statistics => {
                if let Some(reason) = source_failed(s, &[source_id::STATISTICS, source_id::TABLES]) {
                    return Unavailable(reason);
                }
                if !s.statistics.is_empty()
                    || s.source_outcome(source_id::STATISTICS).is_some_and(|o| matches!(o.state, SourceState::Collected | SourceState::Empty))
                    || (!s.tables.is_empty() && s.tables.iter().all(|t| t.modifications_since_analyze.is_some()))
                {
                    Available
                } else {
                    insufficient("Statistics update measurements were not collected completely.")
                }
            }
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
            Self::Settings => source_failed(s, &[
                source_id::SETTINGS,
                source_id::ADVICE_SETTINGS,
                source_id::SERVER_VARIABLES,
                source_id::TABLES,
                source_id::CATALOG,
                source_id::STATISTICS,
                source_id::WORKLOAD,
                source_id::GLOBAL_STATUS,
                source_id::PERFORMANCE_COUNTERS,
                source_id::PLAN_CACHE_USE,
                source_id::RESOURCE_USE,
            ]).map(Insufficient).unwrap_or(Available),
            Self::Deadlocks => source_failed(s, &[source_id::DEADLOCKS]).map(Unavailable).unwrap_or_else(|| {
                let d = s.deadlocks.as_ref().unwrap();
                if d.counted.is_some() || d.note.is_some() {
                    insufficient("A cumulative counter or a limited deadlock sample cannot verify the absence of this deadlock pattern.")
                } else { Available }
            }),
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
    let source = match w.kind() {
        Some(culprit_core::WorkloadKind::QueryStore) => source_id::QUERY_STORE,
        Some(culprit_core::WorkloadKind::PlanCache) => source_id::PLAN_CACHE,
        Some(culprit_core::WorkloadKind::QueryLog) => source_id::QUERY_LOG,
        _ => source_id::WORKLOAD,
    };
    if let Some(reason) = source_failed(s, &[source]) {
        return Insufficient(reason);
    }
    let detail_sources: &[&str] = match measure {
        QueryMeasure::Waits(_) => &[source_id::QUERY_WAITS],
        QueryMeasure::Failures => &[source_id::QUERY_COSTS, source_id::QUERY_ERRORS],
        QueryMeasure::Planning | QueryMeasure::Jit | QueryMeasure::LogBytes => &[source_id::QUERY_COSTS],
        QueryMeasure::Grants => &[source_id::GRANTS],
        QueryMeasure::Histograms => &[source_id::HISTOGRAMS],
        _ => &[],
    };
    if let Some(reason) = source_failed(s, detail_sources) {
        return Insufficient(reason);
    }
    let assessed: Vec<_> = w
        .queries
        .iter()
        .filter(|q| {
            let costs = |f: fn(&culprit_core::QueryCosts) -> bool| q.costs.as_ref().is_some_and(f);
            match measure {
                QueryMeasure::PlanVariants => {
                    q.plans.len() >= 2
                        && q.plans.iter().all(|p| {
                            p.executions >= culprit_core::PLAN_VARIANT_MIN_RUNS
                                && p.mean_time_ms.is_finite()
                                && p.first_seen.as_deref().and_then(culprit_core::format::epoch_seconds).is_some()
                                && p.last_seen.as_deref().and_then(culprit_core::format::epoch_seconds).is_some()
                        })
                }
                QueryMeasure::Variance => q.stddev_time_ms.is_some() && q.max_time_ms.is_some() && q.executions >= 50.0,
                QueryMeasure::ParameterSpread => !q.plans.is_empty() || q.spread.is_some(),
                QueryMeasure::Failures => costs(|c| c.failed.is_some() || c.aborted.is_some()),
                QueryMeasure::Planning => costs(|c| c.plan_ms.is_some()),
                QueryMeasure::Jit => costs(|c| c.jit_ms.is_some()),
                QueryMeasure::LogBytes => costs(|c| c.log_bytes.is_some()),
                QueryMeasure::Rows => q.rows_per_execution().is_some(),
                QueryMeasure::Histograms => s.histograms.is_some(),
                QueryMeasure::TempWrites => q.temp_writes.is_some(),
                QueryMeasure::Grants => s.pressure.as_ref().is_some_and(|p| {
                    p.grants
                        .iter()
                        .any(|g| q.query_hash.as_deref() == Some(g.query_hash.as_str()) && g.executions >= 5.0)
                }),
                QueryMeasure::Waits(category) => q
                    .waits
                    .iter()
                    .any(|w| w.category.eq_ignore_ascii_case(category) && w.wait_ms.is_finite() && w.wait_ms >= 0.0),
                QueryMeasure::Plans => q.plan.is_some(),
                _ => true,
            }
        })
        .map(|q| q.id.clone())
        .collect();
    if matches!(measure, QueryMeasure::Work | QueryMeasure::Timeline | QueryMeasure::Histograms)
        && assessed.len() == w.queries.len()
    {
        Available
    } else if assessed.len() == w.queries.len() {
        Queries { assessed, reason: None }
    } else if assessed.is_empty() && matches!(measure, QueryMeasure::PlanVariants) {
        insufficient("No statement has sufficiently sampled earlier and later plan variants to compare in this window.")
    } else if assessed.is_empty() {
        insufficient("This scan did not record the measurements this check needs.")
    } else {
        Queries {
            assessed,
            reason: Some(
                "Some statements lack the measurements this check needs; only measured statements were assessed."
                    .into(),
            ),
        }
    }
}

fn tables(measure: TableMeasure, s: &Snapshot) -> CheckEvidence {
    use CheckEvidence::*;
    if let Some(reason) = source_failed(s, &[source_id::TABLES, source_id::CATALOG]) {
        return Unavailable(reason);
    }
    let storage = matches!(measure, TableMeasure::Bloat | TableMeasure::HotUpdates);
    if storage && let Some(reason) = source_failed(s, &[source_id::TABLE_STORAGE]) {
        return Unavailable(reason);
    }
    let complete = !s.tables.is_empty()
        && s.tables.iter().all(|t| match measure {
            TableMeasure::Heap => t.is_heap.is_some() && t.rows.is_some(),
            TableMeasure::DeadRows => t.rows.is_some() && t.dead_rows.is_some(),
            TableMeasure::Xid => t.xid_age.is_some(),
            TableMeasure::Bloat => t.storage.as_ref().is_some_and(|v| v.bloat_bytes.is_some()),
            TableMeasure::HotUpdates => {
                t.updates.is_some() && t.storage.as_ref().is_some_and(|v| v.hot_updates.is_some())
            }
            TableMeasure::RowSize => t.rows.is_some() && t.lob_bytes.is_some() && t.total_bytes.is_some(),
            TableMeasure::Scans => t.rows.is_some() && t.seq_rows_read.is_some(),
            TableMeasure::Writes => crate::indexes::measured_writes(s, &t.schema, &t.name).is_some(),
        });
    if complete
        || (s.tables.is_empty()
            && [source_id::TABLES, source_id::CATALOG]
                .iter()
                .any(|n| s.source_outcome(n).is_some_and(|o| o.state == SourceState::Empty)))
    {
        Available
    } else {
        Insufficient("Some tables lack the measurements this check needs.".into())
    }
}
