//! Context is enforced at the history/analysis seams, including readers of saved data.
use culprit_core::{
    CountBaseline, DatabaseIdentity, Engine, EscalationBaseline, IndexContention, MeasurementPeriod, MeasurementScope,
    Period, Snapshot, VerifyStatus,
};
use culprit_engine::{
    history,
    trends::{self, Counters},
};

fn snapshot(host: &str, at: &str, time: f64, runs: f64) -> Snapshot {
    let mut s: Snapshot = serde_json::from_str(include_str!("../../../fixtures/postgres-demo.json")).unwrap();
    s.server.database = "shop".into();
    s.server.started_at = Some("2026-10-01T00:00:00Z".into());
    s.taken_at = at.into();
    s.id = at.replace(['-', ':'], "");
    let w = s.workload.as_mut().unwrap();
    w.window_start = Some("2026-10-01T00:00:00Z".into());
    w.window_end = Some(at.into());
    w.window_hours = (culprit_core::format::epoch_seconds(at).unwrap()
        - culprit_core::format::epoch_seconds(w.window_start.as_deref().unwrap()).unwrap())
        / 3600.0;
    w.timeline = None;
    w.queries.truncate(1);
    let q = &mut w.queries[0];
    q.executions = runs;
    q.total_time_ms = time;
    q.mean_time_ms = time / runs;
    q.trend = None;
    q.last_seen = None;
    q.first_seen = Some("2026-10-01T00:00:00Z".into());
    w.total_time_ms = time;
    w.total_executions = runs;
    s.bind_identity(DatabaseIdentity::server(Engine::Postgres, "shop", host, 5432, None));
    s
}
fn build(s: &mut Snapshot, history: &[Counters]) {
    trends::from_scans(s.workload.as_mut().unwrap(), &s.server.database, &s.taken_at, None, history);
}

#[test]
fn cumulative_history_never_blends_same_named_databases_on_different_endpoints() {
    let old = snapshot("first.example", "2026-10-10T00:00:00Z", 100.0, 10.0);
    let history = vec![Counters::from_snapshot(&old).unwrap()];
    let mut current = snapshot("second.example", "2026-10-11T00:00:00Z", 200.0, 20.0);
    build(&mut current, &history);
    assert!(current.workload.as_ref().unwrap().timeline.is_none());
    let mut same = snapshot("first.example", "2026-10-11T00:00:00Z", 200.0, 20.0);
    build(&mut same, &history);
    let timeline = same.workload.as_ref().unwrap().timeline.as_ref().unwrap();
    assert_eq!(timeline.time_ms.iter().sum::<f64>(), 100.0);
}

#[test]
fn saved_counter_context_round_trips_and_unknown_legacy_identity_is_not_backfilled() {
    let old = snapshot("first.example", "2026-10-10T00:00:00Z", 100.0, 10.0);
    let counters = Counters::from_snapshot(&old).unwrap();
    let saved = serde_json::to_string(&counters).unwrap();
    let loaded: Counters = serde_json::from_str(&saved).unwrap();
    assert_eq!(loaded.measurement, counters.measurement);
    let mut legacy = serde_json::to_value(&counters).unwrap();
    legacy.as_object_mut().unwrap().remove("measurement");
    let legacy: Counters = serde_json::from_value(legacy).unwrap();
    assert!(legacy.measurement.is_none());
    assert!(!loaded.same_series(&legacy));
    let mut current = snapshot("first.example", "2026-10-11T00:00:00Z", 200.0, 20.0);
    build(&mut current, &[legacy]);
    assert!(current.workload.as_ref().unwrap().timeline.is_none());
}

#[test]
fn another_endpoints_counters_cannot_make_a_query_look_stopped() {
    let old = snapshot("first.example", "2026-10-07T00:00:00Z", 100.0, 10.0);
    let history = vec![Counters::from_snapshot(&old).unwrap()];
    let mut current = snapshot("second.example", "2026-10-11T00:00:00Z", 100.0, 10.0);
    trends::last_seen_from_scans(current.workload.as_mut().unwrap(), "shop", &current.taken_at, &history);
    assert!(current.workload.as_ref().unwrap().queries[0].last_seen.is_none());
    let mut same = snapshot("first.example", "2026-10-11T00:00:00Z", 100.0, 10.0);
    trends::last_seen_from_scans(same.workload.as_mut().unwrap(), "shop", &same.taken_at, &history);
    assert_eq!(same.workload.as_ref().unwrap().queries[0].last_seen.as_deref(), Some("2026-10-07T00:00:00Z"));
}

#[test]
fn invalid_scope_cannot_enter_database_findings_or_cumulative_trends() {
    let old = snapshot("first.example", "2026-10-10T00:00:00Z", 100.0, 10.0);
    let mut current = snapshot("first.example", "2026-10-11T00:00:00Z", 1_000_000.0, 1_000.0);
    current.workload.as_mut().unwrap().measurement.as_mut().unwrap().scope = MeasurementScope::Server;
    build(&mut current, &[Counters::from_snapshot(&old).unwrap()]);
    assert!(current.workload.as_ref().unwrap().timeline.is_none());
    let report = culprit_checks::analyze(current);
    assert!(report.snapshot.workload.is_none());
    assert!(report.findings.iter().all(|f| f.queries.is_empty()));
    assert!(report.snapshot.notes.iter().any(|n| n.message.contains("does not isolate this database")));
}

#[test]
fn every_counter_baseline_checks_database_identity_itself() {
    let mut old = snapshot("first.example", "2026-10-10T00:00:00Z", 100.0, 10.0);
    old.indexes[0].contention = Some(IndexContention::default());
    old.settings.push(culprit_core::Setting {
        name: "counter".into(),
        value: "2".into(),
        default_value: None,
        scope: "database".into(),
    });
    let mut same = snapshot("first.example", "2026-10-11T00:00:00Z", 200.0, 20.0);
    same.settings.push(culprit_core::Setting {
        name: "counter".into(),
        value: "4".into(),
        default_value: None,
        scope: "database".into(),
    });
    assert!(CountBaseline::from_setting(&old, &same, "counter").is_some());
    assert!(EscalationBaseline::from_scan(&old, &same).is_some());
    assert!(culprit_core::ContentionBaseline::from_scan(&old, &same).is_some());
    same.bind_identity(DatabaseIdentity::server(Engine::Postgres, "shop", "second.example", 5432, None));
    assert!(CountBaseline::from_setting(&old, &same, "counter").is_none());
    assert!(EscalationBaseline::from_scan(&old, &same).is_none());
    assert!(culprit_core::ContentionBaseline::from_scan(&old, &same).is_none());
    assert!(!history::same_database(&old, &same));
}

#[test]
fn verification_and_digest_do_not_call_another_database_a_fix() {
    let before = culprit_checks::analyze(snapshot("first.example", "2026-10-10T00:00:00Z", 1_000_000.0, 1_000.0));
    let mut after = culprit_checks::analyze(snapshot("second.example", "2026-10-11T00:00:00Z", 1_000_000.0, 1_000.0));
    assert!(!before.findings.is_empty());
    after.findings.clear();
    let verification = culprit_core::compare_reports(
        &before,
        &after,
        &before.findings.iter().map(|f| f.id.clone()).collect::<Vec<_>>(),
    );
    assert!(verification.findings.iter().all(|f| f.status == VerifyStatus::Unknown));
    assert!(verification.new_findings.is_empty());
    let baseline = culprit_checks::digest(&before, None, 5, 5);
    let digest = culprit_checks::digest_since(&after, &baseline, 5, 5);
    assert!(digest.since.is_none());
    assert!(culprit_checks::digest(&after, Some(&before), 5, 5).since.is_none());
}

#[test]
fn different_selected_windows_do_not_verify_query_findings_as_fixed() {
    let before = culprit_checks::analyze(snapshot("first.example", "2026-10-10T00:00:00Z", 1_000_000.0, 1_000.0));
    let query = before.findings.iter().find(|f| !f.queries.is_empty()).expect("planted query finding");
    let mut after = before.clone();
    after.findings.clear();
    after.snapshot.workload.as_mut().unwrap().measurement.as_mut().unwrap().period = MeasurementPeriod::Window {
        span: Some(Period { start: "2026-10-10T23:00:00Z".into(), end: "2026-10-11T00:00:00Z".into() }),
        hours: Some(1.0),
        requested_minutes: Some(60),
    };
    let verification = culprit_core::compare_reports(&before, &after, std::slice::from_ref(&query.id));
    assert_eq!(verification.findings[0].status, VerifyStatus::Unknown);
}

#[test]
fn source_and_period_changes_never_enter_counter_deltas_or_stopped_query_inference() {
    use culprit_core::{MeasurementSource, WorkloadKind};
    let old = snapshot("db.example", "2026-10-10T00:00:00Z", 100.0, 10.0);
    let previous = Counters::from_snapshot(&old).unwrap();
    for source in [WorkloadKind::PlanCache, WorkloadKind::PerformanceSchema] {
        let mut current = snapshot("db.example", "2026-10-11T00:00:00Z", 100.0, 10.0);
        current.workload.as_mut().unwrap().measurement.as_mut().unwrap().source = MeasurementSource::Workload(source);
        build(&mut current, std::slice::from_ref(&previous));
        trends::last_seen_from_scans(
            current.workload.as_mut().unwrap(),
            "shop",
            &current.taken_at,
            std::slice::from_ref(&previous),
        );
        let w = current.workload.as_ref().unwrap();
        assert!(w.timeline.is_none());
        assert!(w.queries[0].last_seen.is_none());
    }
    let mut current = old.clone();
    current.workload.as_mut().unwrap().measurement.as_mut().unwrap().period =
        MeasurementPeriod::Instant { at: "2026-10-11T00:00:00Z".into() };
    assert!(Counters::from_snapshot(&current).is_none());
}

#[test]
fn cache_entry_ages_never_establish_source_resets() {
    use culprit_core::{MeasurementContext, WorkloadKind};
    let mut old = snapshot("db.example", "2026-10-10T00:00:00Z", 100.0, 10.0);
    let mut current = snapshot("db.example", "2026-10-11T00:00:00Z", 100.0, 10.0);
    for s in [&mut old, &mut current] {
        let identity = s.database_identity();
        let w = s.workload.as_mut().unwrap();
        w.kind = Some(WorkloadKind::PlanCache);
        w.window_end = Some("2026-10-09T00:00:00Z".into());
        w.measurement = Some(MeasurementContext::workload(identity, w, &s.taken_at, None));
    }
    // The oldest entry changed between scans, but the tracked entry's count did not change.
    current.workload.as_mut().unwrap().window_start = Some("2026-10-10T12:00:00Z".into());
    let before = Counters::from_snapshot(&old).unwrap();
    let after = Counters::from_snapshot(&current).unwrap();
    assert_eq!(trends::step_kind(trends::epoch(&old.taken_at).unwrap(), &before, &after), trends::StepKind::Growth);
    trends::last_seen_from_scans(current.workload.as_mut().unwrap(), "shop", &current.taken_at, &[before]);
    assert_eq!(current.workload.as_ref().unwrap().queries[0].last_seen.as_deref(), Some(old.taken_at.as_str()));
}

#[test]
fn every_selected_window_retains_actual_source_coverage() {
    use culprit_core::{MeasurementContext, SnapshotOptions, WorkloadKind};
    let mut s = snapshot("db.example", "2026-10-11T00:00:00Z", 100.0, 10.0);
    for hours in [1, 6, 24, 7 * 24, 30 * 24] {
        let options = SnapshotOptions { window_minutes: Some(hours * 60), ..Default::default() };
        let identity = s.database_identity();
        let w = s.workload.as_mut().unwrap();
        w.kind = Some(WorkloadKind::QueryStore);
        w.windowed = true;
        w.window_hours = 0.5;
        w.window_start = Some("2026-10-10T23:30:00Z".into());
        w.window_end = Some("2026-10-11T00:00:00Z".into());
        let context = MeasurementContext::workload(identity, w, &s.taken_at, Some(options.window_in_minutes()));
        assert_eq!(context.period.hours(), Some(0.5), "requested duration never invents source history");
        assert!(
            matches!(context.period, MeasurementPeriod::Window { requested_minutes: Some(m), .. } if m == hours * 60)
        );
    }
}

#[test]
fn persisted_growth_and_reread_baselines_skip_another_endpoint() {
    let dir = std::env::temp_dir().join(format!("culprit-measurement-baselines-{}", std::process::id()));
    let mut old = snapshot("first.example", "2026-10-09T00:00:00Z", 100.0, 10.0);
    old.indexes[0].contention = Some(IndexContention::default());
    culprit_engine::reports::save(&dir, &culprit_checks::analyze(old)).unwrap();
    let mut current = snapshot("second.example", "2026-10-11T00:00:00Z", 200.0, 20.0);
    current.indexes[0].contention = Some(IndexContention::default());
    assert!(history::growth_baseline(&dir, &current).is_none());
    culprit_engine::reports::rebase_on_scans(&dir, &mut current);
    assert!(current.contention_before.is_none());
    assert!(current.escalations_before.is_none());
    current.bind_identity(DatabaseIdentity::server(Engine::Postgres, "shop", "first.example", 5432, None));
    assert!(history::growth_baseline(&dir, &current).is_some());
    culprit_engine::reports::rebase_on_scans(&dir, &mut current);
    assert!(current.contention_before.is_some());
    assert!(current.escalations_before.is_some());
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn public_comparisons_reject_internally_conflicting_context() {
    let before = snapshot("db.example", "2026-10-10T00:00:00Z", 100.0, 10.0);
    let mut after = snapshot("db.example", "2026-10-11T00:00:00Z", 200.0, 20.0);
    after.workload.as_mut().unwrap().measurement.as_mut().unwrap().database =
        DatabaseIdentity::server(Engine::Postgres, "shop", "another.example", 5432, None);
    assert!(!before.report_comparison(&after).allows_comparison());
    assert!(Counters::from_snapshot(&after).is_none());
    let digest = culprit_checks::digest(&culprit_checks::analyze(before), None, 5, 5);
    assert!(!digest.comparison(&after).allows_comparison());
}

#[test]
fn typed_count_baselines_distinguish_resets_from_unexplained_shrinking_counters() {
    use culprit_core::{CountBaseline, ResetMarker};
    let before = snapshot("db.example", "2026-10-10T00:00:00Z", 100.0, 10.0).workload_measurement().unwrap();
    let mut after = snapshot("db.example", "2026-10-11T00:00:00Z", 200.0, 20.0).workload_measurement().unwrap();
    assert!(CountBaseline::in_context("2026-10-10T00:00:00Z", 10.0, 5.0, &before, &after).is_none());
    after.reset = ResetMarker::At("2026-10-10T12:00:00Z".into());
    let reset = CountBaseline::in_context("2026-10-10T00:00:00Z", 10.0, 5.0, &before, &after).unwrap();
    assert!(reset.restarted);
    assert_eq!(reset.count, 0.0);
    after.scope = MeasurementScope::Server;
    assert!(CountBaseline::in_context("2026-10-10T00:00:00Z", 10.0, 20.0, &before, &after).is_none());
}
