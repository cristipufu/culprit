use culprit_checks::evidence::{QueryMeasure, Requirement, assess};
use culprit_core::{
    Category, Check, CheckEvidence, CheckInfo, CheckStatus, CollectorNote, Engine, Finding, Snapshot, SourceOutcome,
    SourceState,
};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

fn snapshot() -> Snapshot {
    serde_json::from_str(include_str!("../../../fixtures/postgres-demo.json")).unwrap()
}

#[test]
fn typed_success_is_not_reinterpreted_as_failure_by_informational_notes() {
    let mut s = snapshot();
    s.sources.push(SourceOutcome { source: "indexes".into(), state: SourceState::Collected, reason: None });
    s.notes.push(CollectorNote { collector: "indexes".into(), message: "Counters cover only this database.".into() });
    assert!(matches!(assess(&[Requirement::IndexCatalog], &s), CheckEvidence::Available));
    // An old saved scan with only a note retains the conservative legacy reading.
    s.sources.clear();
    assert!(matches!(assess(&[Requirement::IndexCatalog], &s), CheckEvidence::Unavailable(_)));
}

#[test]
fn empty_partial_and_unavailable_are_distinct_and_do_not_invent_negative_results() {
    let mut s = snapshot();
    for (state, expected) in [
        (SourceState::Collected, "available"),
        (SourceState::Empty, "available"),
        (SourceState::Partial, "insufficient"),
        (SourceState::Unavailable, "unavailable"),
        (SourceState::NotApplicable, "inapplicable"),
    ] {
        s.sources =
            vec![SourceOutcome { source: "team.policy".into(), state, reason: Some("Source explanation".into()) }];
        let result = assess(&[Requirement::Source("team.policy")], &s);
        let actual = match result {
            CheckEvidence::Available => "available",
            CheckEvidence::Insufficient(_) => "insufficient",
            CheckEvidence::Unavailable(_) => "unavailable",
            CheckEvidence::NotApplicable(_) => "inapplicable",
        };
        assert_eq!(actual, expected);
    }
    s.sources.clear();
    assert!(matches!(assess(&[Requirement::Source("team.policy")], &s), CheckEvidence::Insufficient(_)));
    let configured_source = String::from("configured.policy");
    s.sources.push(SourceOutcome { source: configured_source.clone(), state: SourceState::Collected, reason: None });
    assert!(matches!(assess(&[Requirement::Source(&configured_source)], &s), CheckEvidence::Available));
    assert!(matches!(assess(&[], &s), CheckEvidence::Insufficient(_)));
}

struct Policy {
    id: &'static str,
    calls: Arc<AtomicUsize>,
}
impl Check for Policy {
    fn info(&self) -> CheckInfo {
        CheckInfo {
            id: self.id.into(),
            title: "Team policy".into(),
            category: Category::Maintenance,
            engines: vec![Engine::Postgres],
            requires: Vec::new(),
            description: String::new(),
        }
    }
    fn evidence(&self, s: &Snapshot) -> CheckEvidence {
        assess(&[Requirement::Source("team.policy")], s)
    }
    fn run(&self, _: &Snapshot) -> Vec<Finding> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Vec::new()
    }
}

#[test]
fn external_checks_declare_requirements_without_a_central_id_registry() {
    for id in ["team.original", "team.renamed"] {
        for (state, expected, runs) in [
            (SourceState::Empty, CheckStatus::Assessed, 1),
            (SourceState::Partial, CheckStatus::InsufficientEvidence, 1),
            (SourceState::Unavailable, CheckStatus::Unavailable, 0),
            (SourceState::NotApplicable, CheckStatus::NotApplicable, 0),
        ] {
            let mut s = snapshot();
            s.sources.push(SourceOutcome {
                source: "team.policy".into(),
                state,
                reason: Some("Source explanation".into()),
            });
            let calls = Arc::new(AtomicUsize::new(0));
            let report = culprit_checks::analyze_with(s, &[Arc::new(Policy { id, calls: calls.clone() })]);
            assert_eq!(report.assessments.iter().find(|a| a.check == id).unwrap().status, expected);
            assert_eq!(calls.load(Ordering::SeqCst), runs);
        }
    }
}

#[test]
fn requirements_compose_and_missing_measurements_remain_unknown() {
    let mut s = snapshot();
    s.sources.push(SourceOutcome {
        source: "indexes".into(),
        state: SourceState::Partial,
        reason: Some("One catalog part timed out".into()),
    });
    assert!(matches!(
        assess(&[Requirement::IndexCatalog, Requirement::Query(QueryMeasure::Work)], &s),
        CheckEvidence::Unavailable(_)
    ));
    assert!(
        matches!(
            assess(&[Requirement::Incomplete("Not enough history"), Requirement::IndexCatalog], &s),
            CheckEvidence::Unavailable(_)
        ),
        "a missing mandatory source wins regardless of declaration order"
    );
    s.sources.clear();
    s.workload.as_mut().unwrap().queries.iter_mut().for_each(|q| q.costs = None);
    assert!(matches!(assess(&[Requirement::Query(QueryMeasure::Planning)], &s), CheckEvidence::Insufficient(_)));
    s.workload = None;
    assert!(matches!(assess(&[Requirement::Query(QueryMeasure::Work)], &s), CheckEvidence::Unavailable(_)));
}

#[test]
fn partial_workload_keeps_positive_evidence_without_claiming_complete_assessment() {
    let mut s = snapshot();
    s.sources.push(SourceOutcome {
        source: "workload".into(),
        state: SourceState::Partial,
        reason: Some("Other roles are not visible".into()),
    });
    assert!(matches!(assess(&[Requirement::Query(QueryMeasure::Work)], &s), CheckEvidence::Insufficient(_)));
    assert!(culprit_checks::analyze(s).findings.iter().any(|f| f.check == "heavy-query"));
}

#[test]
fn successful_fallback_is_not_blocked_by_an_unused_source_failure() {
    let mut s = snapshot();
    s.workload.as_mut().unwrap().kind = Some(culprit_core::WorkloadKind::PlanCache);
    s.sources = vec![
        SourceOutcome {
            source: "query_store".into(),
            state: SourceState::Unavailable,
            reason: Some("Query Store unavailable".into()),
        },
        SourceOutcome { source: "plan_cache".into(), state: SourceState::Collected, reason: None },
    ];
    assert!(matches!(assess(&[Requirement::Query(QueryMeasure::Work)], &s), CheckEvidence::Available));
}

#[test]
fn an_unrecorded_empty_catalog_is_not_proof_that_foreign_keys_do_not_exist() {
    let mut s = snapshot();
    s.foreign_keys.clear();
    assert!(matches!(
        assess(&[Requirement::IndexCatalog, Requirement::ForeignKeys], &s),
        CheckEvidence::Insufficient(_)
    ));
    s.sources.push(SourceOutcome { source: "foreign_keys".into(), state: SourceState::Empty, reason: None });
    assert!(matches!(
        assess(&[Requirement::IndexCatalog, Requirement::ForeignKeys], &s),
        CheckEvidence::NotApplicable(_)
    ));
    s.sources[0].state = SourceState::Unavailable;
    assert!(matches!(
        assess(&[Requirement::ForeignKeys, Requirement::IndexCatalog], &s),
        CheckEvidence::Unavailable(_)
    ));
}

#[test]
fn ungraded_or_invalid_pressure_is_not_a_healthy_negative_assessment() {
    let mut s = snapshot();
    let mut pressure = culprit_core::Pressure::default();
    pressure.signals.push(culprit_core::Signal::new(
        "measured",
        "Measured",
        0.0,
        culprit_core::SignalUnit::Count,
        culprit_core::SignalScope::Now,
    ));
    s.pressure = Some(pressure);
    let needs = [Requirement::Pressure { signal: Some("measured"), timed_files: false, complete: true }];
    assert!(matches!(assess(&needs, &s), CheckEvidence::Insufficient(_)));
    s.pressure.as_mut().unwrap().signals[0].status = culprit_core::SignalStatus::Good;
    assert!(matches!(assess(&needs, &s), CheckEvidence::Available));
    s.pressure.as_mut().unwrap().signals[0].value = f64::NAN;
    assert!(matches!(assess(&needs, &s), CheckEvidence::Insufficient(_)));
}

#[test]
fn every_failed_schema_source_blocks_advice_that_needs_a_complete_catalog() {
    for source in ["tables", "indexes", "columns", "foreign_keys", "catalog"] {
        let mut s = snapshot();
        s.sources.push(SourceOutcome {
            source: source.into(),
            state: SourceState::Unavailable,
            reason: Some("Catalog read failed".into()),
        });
        assert!(matches!(assess(&[Requirement::IndexCatalog], &s), CheckEvidence::Unavailable(_)));
        let report = culprit_checks::analyze(s);
        assert!(
            culprit_checks::index_advice(&report).candidates.iter().all(|c| c.sql.is_none()),
            "{source} is unavailable"
        );
    }
}
