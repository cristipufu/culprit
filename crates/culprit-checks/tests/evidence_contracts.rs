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
            CheckEvidence::Insufficient(_) | CheckEvidence::Queries { .. } => "insufficient",
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

#[test]
fn an_unavailable_catalog_cannot_turn_a_disappearing_finding_into_a_verified_fix() {
    let before = culprit_checks::analyze(snapshot());
    let finding = before.findings.iter().find(|f| f.check == "duplicate-index").unwrap();
    let previous = culprit_checks::digest(&before, None, 100, 0);
    let mut s = snapshot();
    // Columns carry no capability lock, but the index checks require this source to be complete.
    s.sources.push(SourceOutcome {
        source: "columns".into(),
        state: SourceState::Unavailable,
        reason: Some("Catalog request timed out.".into()),
    });
    let after = culprit_checks::analyze(s);
    assert!(!after.locked.iter().any(|l| l.check.id == finding.check));
    assert!(!after.findings.iter().any(|f| f.id == finding.id));
    let verification = culprit_core::verification::compare_reports(&before, &after, std::slice::from_ref(&finding.id));
    assert_eq!(verification.findings[0].status, culprit_core::VerifyStatus::Unknown);
    assert!(verification.findings[0].note.as_deref().unwrap().contains("timed out"));
    for changes in [
        culprit_checks::digest(&after, Some(&before), 100, 0).since.unwrap(),
        culprit_checks::digest_since(&after, &previous, 100, 0).since.unwrap(),
    ] {
        assert!(!changes.fixed.iter().any(|f| f.id == finding.id));
    }
}

#[test]
fn a_finding_reclassified_under_another_cause_is_not_a_verified_improvement() {
    use culprit_core::{RefinementDecision, RefinementOutcome, VerifyStatus, verification::compare_reports};
    let before = culprit_checks::analyze(snapshot());
    let finding = before.findings.iter().find(|f| f.check == "duplicate-index").unwrap();
    let retained = before.findings.iter().find(|f| f.id != finding.id && f.is_actionable()).unwrap();
    let previous = culprit_checks::digest(&before, None, 100, 0);
    for outcome in [
        RefinementOutcome::Absorbed { into: retained.id.clone() },
        RefinementOutcome::Covered { by: vec![retained.id.clone()] },
    ] {
        let mut after = before.clone();
        after.findings.retain(|f| f.id != finding.id);
        let assessment = after.assessments.iter_mut().find(|a| a.check == finding.check).unwrap();
        assessment.status = CheckStatus::Findings;
        assessment.reason = None;
        let ids = std::slice::from_ref(&finding.id);
        assert_eq!(compare_reports(&before, &after, ids).findings[0].status, VerifyStatus::Fixed);
        after.refinement.push(RefinementDecision {
            rule: "a more specific cause explains this observation".into(),
            finding: Some(finding.id.clone()),
            outcome,
            queries: Vec::new(),
            tables: Vec::new(),
        });
        let verification = compare_reports(&before, &after, ids);
        assert_eq!(verification.findings[0].status, VerifyStatus::Unknown);
        assert!(verification.findings[0].note.as_deref().unwrap().contains("combined"));
        for changes in [
            culprit_checks::digest(&after, Some(&before), 100, 0).since.unwrap(),
            culprit_checks::digest_since(&after, &previous, 100, 0).since.unwrap(),
        ] {
            assert!(!changes.fixed.iter().any(|f| f.id == finding.id));
        }
    }
}

#[test]
fn removing_a_duplicate_is_verified_by_the_real_catalog_assessment() {
    let before = culprit_checks::analyze(snapshot());
    let finding = before.findings.iter().find(|f| f.check == "duplicate-index").unwrap();
    let previous = culprit_checks::digest(&before, None, 100, 0);
    let mut s = snapshot();
    s.indexes.retain(|i| format!("{}.{}.{}", i.schema, i.table, i.name) != finding.subject.name);
    let after = culprit_checks::analyze(s);
    assert!(after.assessments.iter().find(|a| a.check == finding.check).unwrap().reason.is_none());
    assert_eq!(
        culprit_core::verification::compare_reports(&before, &after, std::slice::from_ref(&finding.id)).findings[0]
            .status,
        culprit_core::VerifyStatus::Fixed
    );
    assert!(
        culprit_checks::digest_since(&after, &previous, 100, 0).since.unwrap().fixed.iter().any(|f| f.id == finding.id)
    );
}

#[test]
fn a_measured_query_can_be_verified_without_borrowing_evidence_from_other_queries() {
    let mut s = snapshot();
    let w = s.workload.as_mut().unwrap();
    w.queries.truncate(1);
    let q = &mut w.queries[0];
    q.executions = 1000.0;
    q.mean_time_ms = 100.0;
    q.total_time_ms = 100_000.0;
    q.max_time_ms = Some(10_000.0);
    q.stddev_time_ms = Some(1000.0);
    q.plans.clear();
    let mut unrelated = q.clone();
    unrelated.id = "unrelated-one-off".into();
    unrelated.executions = 1.0;
    unrelated.stddev_time_ms = None;
    w.queries.push(unrelated);
    let before = culprit_checks::analyze(s.clone());
    let finding = before.findings.iter().find(|f| f.check == "unstable-query").expect("a real unstable statement");
    let previous = culprit_checks::digest(&before, None, 100, 0);
    s.workload.as_mut().unwrap().queries[0].stddev_time_ms = Some(1.0);
    let after = culprit_checks::analyze(s.clone());
    let verify = |after: &culprit_core::Report| {
        culprit_core::verification::compare_reports(&before, after, std::slice::from_ref(&finding.id)).findings[0]
            .status
    };
    assert_eq!(verify(&after), culprit_core::VerifyStatus::Fixed);
    assert!(
        culprit_checks::digest_since(&after, &previous, 100, 0).since.unwrap().fixed.iter().any(|f| f.id == finding.id)
    );
    // A known unrelated query must never stand in for the affected statement's missing measurements.
    s.workload.as_mut().unwrap().queries[0].stddev_time_ms = None;
    s.workload.as_mut().unwrap().queries[1].executions = 1000.0;
    s.workload.as_mut().unwrap().queries[1].stddev_time_ms = Some(1.0);
    let after = culprit_checks::analyze(s.clone());
    assert_eq!(verify(&after), culprit_core::VerifyStatus::Unknown);
    assert!(
        !culprit_checks::digest_since(&after, &previous, 100, 0)
            .since
            .unwrap()
            .fixed
            .iter()
            .any(|f| f.id == finding.id)
    );
    s.workload.as_mut().unwrap().queries[0].stddev_time_ms = Some(1.0);
    s.sources.push(SourceOutcome {
        source: "workload".into(),
        state: SourceState::Partial,
        reason: Some("Not every role was visible".into()),
    });
    assert_eq!(verify(&culprit_checks::analyze(s)), culprit_core::VerifyStatus::Unknown);
}

#[test]
fn complete_settings_and_catalogs_are_assessed_without_overriding_the_report() {
    let mut s = snapshot();
    s.sources.push(SourceOutcome { source: "settings".into(), state: SourceState::Collected, reason: None });
    assert!(matches!(assess(&[Requirement::Settings], &s), CheckEvidence::Available));
    s.sources[0].state = SourceState::Partial;
    assert!(!matches!(assess(&[Requirement::Settings], &s), CheckEvidence::Available));
    let report = culprit_checks::analyze(snapshot());
    for check in ["duplicate-index", "unindexed-fk", "overlapping-index"] {
        assert!(report.assessments.iter().find(|a| a.check == check).unwrap().reason.is_none(), "{check}");
    }
}

#[test]
fn schema_and_setting_changes_really_verify_with_natural_assessments() {
    let mut original: Snapshot = serde_json::from_str(include_str!("../../../fixtures/mssql-demo.json")).unwrap();
    original.tables[0].is_heap = Some(true);
    for check in ["heap-table", "config"] {
        let before = culprit_checks::analyze(original.clone());
        let finding = before
            .findings
            .iter()
            .find(|f| f.check == check && (check != "config" || f.id == "config:cost-threshold-for-parallelism"))
            .unwrap();
        let mut s = original.clone();
        if check == "heap-table" {
            s.tables.iter_mut().find(|t| t.qualified() == finding.subject.name).unwrap().is_heap = Some(false);
        } else {
            s.settings.iter_mut().find(|s| s.name == "cost threshold for parallelism").unwrap().value = "50".into();
        }
        let after = culprit_checks::analyze(s);
        let verification =
            culprit_core::verification::compare_reports(&before, &after, std::slice::from_ref(&finding.id));
        assert_eq!(
            verification.findings[0].status,
            culprit_core::VerifyStatus::Fixed,
            "{check}: {:?}",
            verification.findings[0].note
        );
    }
}

#[test]
fn sharing_file_evidence_keeps_links_and_legacy_path_ids_do_not_prove_a_fix() {
    let mut s = snapshot();
    s.pressure.get_or_insert_default().files = vec![culprit_core::signals::FileIo {
        name: "/private/file-path-canary/data.db".into(),
        kind: culprit_core::FileKind::Data,
        database: Some(s.server.database.clone()),
        reads: 5000.0,
        read_ms: Some(250_000.0),
        read_bytes: Some(5000.0 * 8192.0),
        writes: 0.0,
        write_ms: Some(0.0),
        write_bytes: Some(0.0),
        scope: Some(culprit_core::FileScope::Database),
        stats_reset: None,
        write_operation: None,
    }];
    let original = culprit_checks::analyze(s.clone());
    let finding = original.findings.iter().find(|f| f.check == "slow-storage").unwrap();
    let shared = culprit_checks::analyze(culprit_core::redact::redact_snapshot(&s));
    assert!(shared.findings.iter().any(|f| f.id == finding.id), "shared links still open the original finding");
    assert!(!serde_json::to_string(&shared).unwrap().contains("file-path-canary"));
    let mut legacy = original;
    let old = legacy.findings.iter_mut().find(|f| f.check == "slow-storage").unwrap();
    old.id = format!("slow-storage:{}:/private/file-path-canary/data.db", s.server.database);
    let old_id = old.id.clone();
    legacy.assessments.clear();
    let verification = culprit_core::verification::compare_reports(&legacy, &shared, std::slice::from_ref(&old_id));
    assert_eq!(verification.findings[0].status, culprit_core::VerifyStatus::Unknown);
    assert!(
        !culprit_checks::digest(&shared, Some(&legacy), 100, 0).since.unwrap().fixed.iter().any(|f| f.id == old_id)
    );
    let mut previous = culprit_checks::digest(&legacy, None, 100, 0);
    for f in &mut previous.findings {
        f.check = None;
    }
    assert!(
        !culprit_checks::digest_since(&shared, &previous, 100, 0).since.unwrap().fixed.iter().any(|f| f.id == old_id)
    );
}

#[test]
fn measured_zero_jit_and_wal_verify_but_missing_costs_do_not() {
    for check in ["jit-overhead", "log-volume"] {
        let mut s = snapshot();
        let w = s.workload.as_mut().unwrap();
        w.queries.truncate(1);
        let q = &mut w.queries[0];
        q.text = "UPDATE public.orders SET status = 'ready' WHERE id = 1".into();
        q.executions = 1000.0;
        q.mean_time_ms = 100.0;
        q.total_time_ms = 100_000.0;
        q.rows = Some(1000.0);
        q.costs = Some(culprit_core::QueryCosts {
            jit_ms: Some(60_000.0),
            jit_functions: Some(1000.0),
            log_bytes: Some(10_000_000_000.0),
            log_share: Some(0.8),
            ..Default::default()
        });
        let before = culprit_checks::analyze(s.clone());
        let finding = before.findings.iter().find(|f| f.check == check).expect(check);
        let previous = culprit_checks::digest(&before, None, 100, 0);
        let costs = s.workload.as_mut().unwrap().queries[0].costs.as_mut().unwrap();
        costs.jit_ms = Some(0.0);
        costs.jit_functions = Some(0.0);
        costs.log_bytes = Some(0.0);
        costs.log_share = Some(0.0);
        let after = culprit_checks::analyze(s.clone());
        let verify = |after: &culprit_core::Report| {
            culprit_core::verification::compare_reports(&before, after, std::slice::from_ref(&finding.id)).findings[0]
                .status
        };
        assert_eq!(verify(&after), culprit_core::VerifyStatus::Fixed, "{check}");
        assert!(
            culprit_checks::digest_since(&after, &previous, 100, 0)
                .since
                .unwrap()
                .fixed
                .iter()
                .any(|f| f.id == finding.id),
            "{check}"
        );
        s.workload.as_mut().unwrap().queries[0].costs = None;
        assert_eq!(verify(&culprit_checks::analyze(s)), culprit_core::VerifyStatus::Unknown, "{check}");
    }
}

#[test]
fn zero_waits_verify_only_the_category_actually_measured() {
    for (check, category) in [("lock-time", "Lock"), ("network-wait", "Network")] {
        let mut s: Snapshot = serde_json::from_str(include_str!("../../../fixtures/mssql-demo.json")).unwrap();
        s.settings.clear();
        let w = s.workload.as_mut().unwrap();
        w.queries.truncate(1);
        let q = &mut w.queries[0];
        q.executions = 1000.0;
        q.total_time_ms = 100_000.0;
        q.mean_time_ms = 100.0;
        q.waits =
            vec![culprit_core::WaitStat { category: category.into(), wait_type: None, wait_ms: 80_000.0, tasks: None }];
        w.total_time_ms = q.total_time_ms;
        let before = culprit_checks::analyze(s.clone());
        let finding = before.findings.iter().find(|f| f.check == check).expect(check);
        let previous = culprit_checks::digest(&before, None, 100, 0);
        let verify = |after: &culprit_core::Report| {
            culprit_core::compare_reports(&before, after, std::slice::from_ref(&finding.id)).findings[0].status
        };
        s.workload.as_mut().unwrap().queries[0].waits[0].wait_ms = 0.0;
        let after = culprit_checks::analyze(s.clone());
        assert_eq!(verify(&after), culprit_core::VerifyStatus::Fixed, "{check}");
        assert!(
            culprit_checks::digest_since(&after, &previous, 100, 0)
                .since
                .unwrap()
                .fixed
                .iter()
                .any(|f| f.id == finding.id)
        );
        s.workload.as_mut().unwrap().queries[0].waits[0].category = "IO".into();
        assert_eq!(verify(&culprit_checks::analyze(s.clone())), culprit_core::VerifyStatus::Unknown);
        s.workload.as_mut().unwrap().queries[0].waits[0].category = category.into();
        s.sources.push(SourceOutcome {
            source: "query_waits".into(),
            state: SourceState::Partial,
            reason: Some("Capture was disabled".into()),
        });
        assert_eq!(verify(&culprit_checks::analyze(s)), culprit_core::VerifyStatus::Unknown);
    }
}

#[test]
fn zero_grants_verify_but_leaving_the_sample_or_cache_does_not() {
    let mut s: Snapshot = serde_json::from_str(include_str!("../../../fixtures/mssql-demo.json")).unwrap();
    s.workload.as_mut().unwrap().queries.truncate(1);
    let hash = "0x0123456789ABCDEF".to_string();
    s.workload.as_mut().unwrap().queries[0].query_hash = Some(hash.clone());
    s.pressure.get_or_insert_default().grants = vec![culprit_core::GrantStat {
        query_hash: hash,
        executions: 100.0,
        avg_granted_kb: 100_000.0,
        avg_used_kb: 1000.0,
        max_granted_kb: 100_000.0,
        last_granted_kb: Some(100_000.0),
        last_used_kb: Some(1000.0),
    }];
    let before = culprit_checks::analyze(s.clone());
    let finding = before.findings.iter().find(|f| f.check == "memory-grant").unwrap();
    let previous = culprit_checks::digest(&before, None, 100, 0);
    let mut recent_zero = s.clone();
    let grant = &mut recent_zero.pressure.as_mut().unwrap().grants[0];
    grant.last_granted_kb = Some(0.0);
    grant.last_used_kb = Some(0.0);
    let after_recent_zero = culprit_checks::analyze(recent_zero);
    assert_eq!(
        culprit_core::compare_reports(&before, &after_recent_zero, std::slice::from_ref(&finding.id)).findings[0]
            .status,
        culprit_core::VerifyStatus::Fixed,
        "known zero latest grants override oversized historical averages"
    );
    let grant = &mut s.pressure.as_mut().unwrap().grants[0];
    grant.avg_granted_kb = 0.0;
    grant.avg_used_kb = 0.0;
    grant.max_granted_kb = 0.0;
    grant.last_granted_kb = Some(0.0);
    grant.last_used_kb = Some(0.0);
    let after = culprit_checks::analyze(s.clone());
    let verify = |after: &culprit_core::Report| {
        culprit_core::compare_reports(&before, after, std::slice::from_ref(&finding.id)).findings[0].status
    };
    assert_eq!(verify(&after), culprit_core::VerifyStatus::Fixed);
    assert!(
        culprit_checks::digest_since(&after, &previous, 100, 0).since.unwrap().fixed.iter().any(|f| f.id == finding.id)
    );
    s.pressure.as_mut().unwrap().grants[0].executions = 1.0;
    assert_eq!(verify(&culprit_checks::analyze(s.clone())), culprit_core::VerifyStatus::Unknown);
    s.pressure.as_mut().unwrap().grants.clear();
    let after = culprit_checks::analyze(s);
    assert_eq!(verify(&after), culprit_core::VerifyStatus::Unknown);
    assert!(
        !culprit_checks::digest_since(&after, &previous, 100, 0)
            .since
            .unwrap()
            .fixed
            .iter()
            .any(|f| f.id == finding.id)
    );
}

#[test]
fn legacy_payloads_establish_their_read_but_empty_or_failed_sources_do_not() {
    use culprit_checks::evidence::Observation;
    let mut s: Snapshot = serde_json::from_str(include_str!("../../../fixtures/mssql-demo.json")).unwrap();
    let missing = Requirement::SourceWithLegacy("missing_indexes", Observation::MissingIndexes);
    let tuning = Requirement::SourceWithLegacy("tuning", Observation::Tuning);
    assert!(matches!(assess(&[missing], &s), CheckEvidence::Available));
    assert!(matches!(assess(&[tuning], &s), CheckEvidence::Insufficient(_)));
    s.tuning = Some(Default::default());
    assert!(matches!(assess(&[tuning], &s), CheckEvidence::Available));
    s.notes.push(CollectorNote { collector: "tuning".into(), message: "Subread failed".into() });
    assert!(matches!(assess(&[tuning], &s), CheckEvidence::Unavailable(_)));
    s.sources.push(SourceOutcome {
        source: "missing_indexes".into(),
        state: SourceState::Partial,
        reason: Some("Some requests could not be read".into()),
    });
    assert!(matches!(assess(&[missing], &s), CheckEvidence::Insufficient(_)));
    s.sources.clear();
    s.missing_indexes.clear();
    assert!(matches!(assess(&[missing], &s), CheckEvidence::Insufficient(_)));
    s.sources.push(SourceOutcome { source: "missing_indexes".into(), state: SourceState::Empty, reason: None });
    assert!(matches!(assess(&[missing], &s), CheckEvidence::Available));
}

#[test]
fn failed_statement_detail_sources_prevent_clean_assessments() {
    use culprit_core::source_id;
    for (measure, source) in [
        (QueryMeasure::Failures, source_id::QUERY_COSTS),
        (QueryMeasure::Failures, source_id::QUERY_ERRORS),
        (QueryMeasure::Planning, source_id::QUERY_COSTS),
        (QueryMeasure::Jit, source_id::QUERY_COSTS),
        (QueryMeasure::LogBytes, source_id::QUERY_COSTS),
        (QueryMeasure::Grants, source_id::GRANTS),
        (QueryMeasure::Histograms, source_id::HISTOGRAMS),
    ] {
        let mut s = snapshot();
        s.histograms = Some(Vec::new());
        s.sources = vec![SourceOutcome {
            source: source.into(),
            state: SourceState::Partial,
            reason: Some("Statement detail query timed out.".into()),
        }];
        assert!(
            matches!(assess(&[Requirement::Query(measure)], &s), CheckEvidence::Insufficient(_)),
            "{measure:?} with failed {source}"
        );
    }
}
