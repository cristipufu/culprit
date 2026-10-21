use std::{path::PathBuf, sync::Arc};

use async_trait::async_trait;
use culprit_core::operations::{OperationalCounter, OperationalCounterKind};
use culprit_core::{
    AnalysisOrigin, AuthKind, BaselinePurpose, CollectionProvenance, ComponentVersion, ContentionBaseline,
    CountBaseline, DatabaseIdentity, Engine, Error, EscalationBaseline, ExplainMode, FileScope, IndexContention,
    LiveState, Plan, Provider, Result, ResultSet, Setting, SizeBaseline, Snapshot, SnapshotOptions,
};
use culprit_engine::{
    NewConnection, Workspace, reading, reports,
    trends::{self, Counters},
};

fn snapshot() -> Snapshot {
    serde_json::from_str(include_str!("../../../fixtures/mssql-demo.json")).unwrap()
}

fn home(tag: &str) -> PathBuf {
    std::env::temp_dir().join(format!("culprit-provenance-{tag}-{}", std::process::id()))
}

#[test]
fn saved_readers_replay_original_input_and_keep_collection_without_rewriting_the_file() {
    let home = home("readers");
    let ws = Workspace::open_in(&home);
    let collected = snapshot();
    let expected_tables = collected.tables.len();
    let mut report = culprit_checks::analyze(collected.clone());
    report.evidence.as_mut().unwrap().collected = Some(Box::new(collected.clone()));
    let provenance = report.provenance.as_mut().unwrap();
    provenance.collection = Some(CollectionProvenance {
        collector: ComponentVersion::package("example-collector", "1", None),
        started_at: collected.taken_at.clone(),
        completed_at: None,
    });
    provenance.analyzer = ComponentVersion::package("older-analyzer", "0.1", None);
    // Simulate preparation in a previous build that discarded useful evidence from its rendered picture.
    report.snapshot.tables.clear();
    report.snapshot.workload = None;
    let dir = ws.paths().reports_dir(&collected.connection);
    reports::save(&dir, &report).unwrap();
    let path = dir.join(format!("{}.json", collected.id));
    let persisted = std::fs::read(&path).unwrap();
    let loaded = ws.analyze_file(&path).unwrap();
    assert_eq!(loaded.snapshot.tables.len(), expected_tables);
    assert!(loaded.snapshot.workload.is_some());
    assert_eq!(loaded.provenance.as_ref().unwrap().origin, AnalysisOrigin::RetainedInput);
    assert_eq!(loaded.provenance.as_ref().unwrap().previous_analyzer.as_ref().unwrap().component, "older-analyzer");
    assert_eq!(
        serde_json::to_value(loaded.evidence.as_ref().unwrap().collected.as_ref().unwrap()).unwrap(),
        serde_json::to_value(&collected).unwrap()
    );
    assert_eq!(ws.latest_report(&collected.connection, None).unwrap().unwrap().snapshot.tables.len(), expected_tables);
    assert_eq!(reading::open(&path).unwrap().snapshot.tables.len(), expected_tables);
    let expected = ws.index_advice_for(culprit_checks::analyze(collected));
    assert_eq!(serde_json::to_value(ws.index_advice_for(report)).unwrap(), serde_json::to_value(expected).unwrap());
    assert_eq!(std::fs::read(&path).unwrap(), persisted, "rereading never replaces collected evidence on disk");
    std::fs::remove_dir_all(home).unwrap();
}

#[test]
fn repeated_replay_is_stable_and_does_not_nest_or_accumulate_evidence() {
    let home = home("stable");
    let ws = Workspace::open_in(&home);
    let path = home.join("report.json");
    std::fs::create_dir_all(&home).unwrap();
    let mut report = culprit_checks::analyze(snapshot());
    report.provenance.as_mut().unwrap().analyzer = ComponentVersion::package("older-analyzer", "0.1", None);
    std::fs::write(&path, serde_json::to_vec(&report).unwrap()).unwrap();
    let first = ws.analyze_file(&path).unwrap();
    std::fs::write(&path, serde_json::to_vec(&first).unwrap()).unwrap();
    let second = ws.analyze_file(&path).unwrap();
    assert_eq!(serde_json::to_value(&first).unwrap(), serde_json::to_value(&second).unwrap());
    assert!(serde_json::to_value(&second.evidence.as_ref().unwrap().input).unwrap().get("evidence").is_none());
    std::fs::remove_dir_all(home).unwrap();
}

#[test]
fn legacy_report_and_bare_snapshot_have_explicit_origins_without_invented_collection() {
    let home = home("legacy");
    let ws = Workspace::open_in(&home);
    std::fs::create_dir_all(&home).unwrap();
    let snapshot = snapshot();
    let mut report = serde_json::to_value(culprit_checks::analyze(snapshot.clone())).unwrap();
    report.as_object_mut().unwrap().remove("evidence");
    report.as_object_mut().unwrap().remove("provenance");
    let path = home.join("legacy.json");
    std::fs::write(&path, serde_json::to_vec(&report).unwrap()).unwrap();
    let result = ws.analyze_file(&path).unwrap();
    assert_eq!(result.provenance.as_ref().unwrap().origin, AnalysisOrigin::PreparedReport);
    assert!(result.provenance.as_ref().unwrap().previous_analyzer.is_none());
    assert!(result.provenance.as_ref().unwrap().collection.is_none());
    assert!(result.evidence.as_ref().unwrap().collected.is_none());
    std::fs::write(&path, serde_json::to_vec(&snapshot).unwrap()).unwrap();
    assert_eq!(ws.analyze_file(&path).unwrap().provenance.as_ref().unwrap().origin, AnalysisOrigin::Snapshot);
    std::fs::remove_dir_all(home).unwrap();
}

#[cfg(feature = "mssql")]
#[test]
fn parsing_provenance_distinguishes_native_reparse_missing_text_and_retained_plan() {
    use culprit_core::PlanParsingOutcome;

    let home = home("parsing");
    let ws = Workspace::open_in(&home);
    std::fs::create_dir_all(&home).unwrap();
    let mut snapshot = snapshot();
    let mut query = snapshot.workload.as_ref().unwrap().queries[0].clone();
    query.plan = Some(culprit_engine::parse_plan(include_str!("plans/mssql-seek.sqlplan")).unwrap());
    let queries = ["native", "missing", "malformed"]
        .into_iter()
        .map(|id| {
            let mut q = query.clone();
            q.id = id.into();
            match id {
                "missing" => q.plan.as_mut().unwrap().raw = None,
                "malformed" => q.plan.as_mut().unwrap().raw = Some("<broken>".into()),
                _ => {}
            }
            q
        })
        .collect();
    snapshot.workload.as_mut().unwrap().queries = queries;
    let path = home.join("snapshot.json");
    std::fs::write(&path, serde_json::to_vec(&snapshot).unwrap()).unwrap();
    let report = ws.analyze_file(&path).unwrap();
    let trace = &report.provenance.as_ref().unwrap().parsing;
    assert_eq!(trace.len(), 3);
    for (id, expected) in [
        ("native", PlanParsingOutcome::Reparsed),
        ("missing", PlanParsingOutcome::NoNativeText),
        ("malformed", PlanParsingOutcome::Retained),
    ] {
        let parsed = trace.iter().find(|p| p.query == id).unwrap();
        assert_eq!(parsed.outcome, expected);
        assert_eq!(parsed.parser, culprit_mssql::component_version());
    }
    std::fs::remove_dir_all(home).unwrap();
}

struct ReplicaReplay;
#[async_trait]
impl Provider for ReplicaReplay {
    fn engine(&self) -> Engine {
        Engine::Postgres
    }
    async fn probe(&self) -> Result<culprit_core::ServerInfo> {
        Ok(replica().server)
    }
    async fn snapshot(&self, _: &str, _: &SnapshotOptions) -> Result<Snapshot> {
        Ok(replica())
    }
    async fn explain(&self, _: &str, _: ExplainMode) -> Result<Plan> {
        Err(Error::invalid("replay does not explain"))
    }
    async fn live(&self) -> Result<LiveState> {
        Err(Error::invalid("replay has no live sessions"))
    }
    async fn run(&self, _: &str, _: usize) -> Result<ResultSet> {
        Err(Error::invalid("replay does not execute"))
    }
}

fn replica() -> Snapshot {
    let mut s: Snapshot = serde_json::from_str(include_str!("../../../fixtures/postgres-demo.json")).unwrap();
    s.settings.push(Setting {
        name: culprit_core::REPLICA_SETTING.into(),
        value: "standby".into(),
        default_value: None,
        scope: "server".into(),
    });
    s.indexes[0].reads = Some(123.0);
    s
}

#[tokio::test]
async fn scan_keeps_provider_counters_before_history_and_does_not_invent_a_runtime_collectors_version() {
    let home = home("collection");
    let ws = Workspace::open_in(&home)
        .with_provider(Engine::Postgres, Arc::new(|_| Ok(Box::new(ReplicaReplay) as Box<dyn Provider>)));
    ws.add_connection(NewConnection {
        name: "replayed".into(),
        input: "postgres://reader@replay.invalid:5432/demo".into(),
        auth: Some(AuthKind::None),
        ..Default::default()
    })
    .unwrap();
    let report = ws.scan("replayed", &SnapshotOptions::default()).await.unwrap();
    let source = report.evidence.as_ref().unwrap().collected.as_ref().unwrap();
    assert_eq!(source.indexes[0].reads, Some(123.0));
    assert_eq!(report.analysis_input().indexes[0].reads, None);
    assert_eq!(report.snapshot.indexes[0].reads, None);
    let provenance = report.provenance.as_ref().unwrap();
    assert_eq!(provenance.origin, AnalysisOrigin::Collected);
    let collection = provenance.collection.as_ref().unwrap();
    assert_eq!(collection.collector.component, "external provider");
    assert!(collection.collector.version.is_none());
    assert!(collection.completed_at.is_some());
    let saved = ws.latest_report("replayed", None).unwrap().unwrap();
    assert_eq!(saved.evidence.as_ref().unwrap().collected.as_ref().unwrap().indexes[0].reads, Some(123.0));
    assert_eq!(saved.provenance.as_ref().unwrap().collection, provenance.collection);
    std::fs::remove_dir_all(home).unwrap();
}

#[test]
fn lock_and_growth_baselines_keep_exact_source_ids_and_legacy_dates_do_not_borrow_the_current_endpoint() {
    let mut previous = snapshot();
    previous.id = "20261010T000000.123Z".into();
    previous.taken_at = "2026-10-10T00:00:00Z".into();
    previous.server.started_at = Some("2026-10-01T00:00:00Z".into());
    previous.bind_identity(DatabaseIdentity::server(
        Engine::Mssql,
        &previous.server.database.clone(),
        "locks.invalid",
        1433,
        None,
    ));
    previous.indexes[0].contention =
        Some(IndexContention { row_lock_waits: 2.0, row_lock_wait_ms: 4.0, ..Default::default() });
    let mut current = previous.clone();
    current.id = "20261012T000000.456Z".into();
    current.taken_at = "2026-10-12T00:00:00Z".into();
    current.evictions_before = None;
    current.operations.counters.clear();
    current.operations.sqlite = None;
    current.workload.as_mut().unwrap().history_inputs.clear();
    current.escalations_before = Some(EscalationBaseline::from_scan(&previous, &current).unwrap());
    current.contention_before = Some(ContentionBaseline::from_scan(&previous, &current).unwrap());
    current.sizes_before = Some(SizeBaseline::from_scan(&previous).unwrap());
    let refs = current.baseline_references();
    for purpose in [BaselinePurpose::Escalations, BaselinePurpose::Contention, BaselinePurpose::TableGrowth] {
        let reference = refs.iter().find(|r| r.purpose == purpose).unwrap();
        assert_eq!(reference.scan.id.as_deref(), Some(previous.id.as_str()));
        assert_eq!(reference.scan.database, previous.identity);
        assert!(reference.measurement.is_none(), "index counters do not borrow the workload clock");
    }
    let mut legacy = serde_json::to_value(current).unwrap();
    for field in ["escalations_before", "contention_before", "sizes_before"] {
        legacy[field].as_object_mut().unwrap().remove("reference");
    }
    let legacy: Snapshot = serde_json::from_value(legacy).unwrap();
    for reference in legacy.baseline_references() {
        assert!(reference.scan.id.is_none());
        assert!(reference.scan.database.is_none());
        assert_eq!(reference.scan.taken_at, previous.taken_at);
    }
}

fn point(host: &str, at: &str, id: &str, runs: f64) -> Snapshot {
    let mut s: Snapshot = serde_json::from_str(include_str!("../../../fixtures/postgres-demo.json")).unwrap();
    s.id = id.into();
    s.taken_at = at.into();
    s.server.database = "shop".into();
    s.server.started_at = Some("2026-10-01T00:00:00Z".into());
    let w = s.workload.as_mut().unwrap();
    w.windowed = false;
    w.window_start = Some("2026-10-01T00:00:00Z".into());
    w.window_end = Some(at.into());
    w.window_hours = (culprit_core::format::epoch_seconds(at).unwrap()
        - culprit_core::format::epoch_seconds(w.window_start.as_deref().unwrap()).unwrap())
        / 3600.0;
    w.total_time_ms = runs * 10.0;
    w.total_executions = runs;
    w.queries.truncate(1);
    w.queries[0].executions = runs;
    w.queries[0].total_time_ms = runs * 10.0;
    w.queries[0].last_seen = None;
    w.queries[0].first_seen = w.window_start.clone();
    w.queries[0].trend = None;
    w.timeline = None;
    w.history_inputs.clear();
    s.bind_identity(DatabaseIdentity::server(Engine::Postgres, "shop", host, 5432, None));
    s
}

#[test]
fn replay_builds_an_initial_scan_timeline_and_replaces_only_the_history_it_used() {
    let home = home("initial-timeline");
    let old = point("first.invalid", "2026-10-10T00:00:00Z", "20261010T000000.000Z", 10.0);
    let mut current = point("first.invalid", "2026-10-11T00:00:00Z", "20261011T000000.000Z", 20.0);
    reports::save(&home, &culprit_checks::analyze(old.clone())).unwrap();
    assert!(current.workload.as_ref().unwrap().timeline.is_none());
    reports::rebuild_timeline(&home, &mut current);
    let w = current.workload.as_ref().unwrap();
    assert_eq!(w.timeline.as_ref().unwrap().time_ms.iter().sum::<f64>(), 100.0);
    let refs = w.history_inputs.clone();
    reports::rebuild_timeline(&home, &mut current);
    assert_eq!(current.workload.as_ref().unwrap().history_inputs, refs);
    assert_eq!(refs.iter().filter(|r| r.purpose == BaselinePurpose::WorkloadTrend).count(), 1);
    // Pruning the old artifact does not erase measurements already retained with a historical scan.
    current.sizes_before = SizeBaseline::from_scan(&old);
    let retained = current.sizes_before.clone();
    std::fs::remove_dir_all(&home).unwrap();
    reports::rebase_on_scans(&home, &mut current);
    assert_eq!(current.sizes_before, retained);
    assert!(current.workload.as_ref().unwrap().timeline.is_some());
    assert_eq!(current.workload.as_ref().unwrap().history_inputs, refs);
}

#[cfg(feature = "postgres")]
#[test]
fn replay_refreshes_row_size_estimates_and_forgets_them_when_the_new_plan_has_no_width() {
    let home = home("row-width");
    std::fs::create_dir_all(&home).unwrap();
    let ws = Workspace::open_in(&home);
    let native = include_str!("plans/pg-index.json");
    let plan = culprit_engine::parse_plan(native).unwrap();
    let expected = culprit_core::signals::plan_row_bytes(&plan).expect("fixture width");
    let mut s = point("first.invalid", "2026-10-11T00:00:00Z", "20261011T000000.000Z", 20.0);
    let q = &mut s.workload.as_mut().unwrap().queries[0];
    q.plan = Some(plan);
    q.plan.as_mut().unwrap().root.facts_mut().estimated_row_bytes = Some(1.0);
    q.costs.get_or_insert_default().row_bytes = Some(1.0);
    let path = home.join("native.json");
    std::fs::write(&path, serde_json::to_vec(&s).unwrap()).unwrap();
    let replayed = ws.analyze_file(&path).unwrap();
    assert_eq!(replayed.snapshot.queries()[0].costs.as_ref().unwrap().row_bytes, Some(expected));
    let mut value: serde_json::Value = serde_json::from_str(native).unwrap();
    value[0]["Plan"].as_object_mut().unwrap().remove("Plan Width");
    s.workload.as_mut().unwrap().queries[0].plan.as_mut().unwrap().raw = Some(value.to_string().into());
    std::fs::write(&path, serde_json::to_vec(&s).unwrap()).unwrap();
    assert_eq!(ws.analyze_file(&path).unwrap().snapshot.queries()[0].costs.as_ref().unwrap().row_bytes, None);
    std::fs::remove_dir_all(home).unwrap();
}

#[test]
fn workload_history_keeps_distinct_ids_with_the_same_rounded_time_and_excludes_other_endpoints() {
    let at = "2026-10-10T00:00:00Z";
    let first = point("first.invalid", at, "20261010T000000.001Z", 1.0);
    let second = point("first.invalid", at, "20261010T000000.999Z", 2.0);
    let foreign = point("other.invalid", at, "20261010T000000.500Z", 100.0);
    let history: Vec<_> =
        [&first, &second, &foreign].into_iter().map(|s| Counters::from_snapshot(s).unwrap()).collect();
    let mut current = point("first.invalid", "2026-10-11T00:00:00Z", "20261011T000000.000Z", 3.0);
    trends::from_scans(current.workload.as_mut().unwrap(), "shop", &current.taken_at, None, &history);
    trends::last_seen_from_scans(current.workload.as_mut().unwrap(), "shop", &current.taken_at, &history);
    let refs = current.baseline_references();
    let ids: Vec<_> = refs
        .iter()
        .filter(|r| r.purpose == BaselinePurpose::WorkloadTrend)
        .map(|r| r.scan.id.as_deref().unwrap())
        .collect();
    assert_eq!(ids, vec![first.id.as_str(), second.id.as_str()]);
    assert!(refs.iter().all(|r| r.scan.id.as_deref() != Some(foreign.id.as_str())));
    assert!(refs.iter().all(|r| r.measurement.is_some()));
}

#[test]
fn eviction_and_operational_comparisons_retain_their_actual_source_point() {
    let mut previous = point("first.invalid", "2026-10-10T00:00:00Z", "20261010T000000.123Z", 1.0);
    previous.settings.retain(|s| s.name != "pg_stat_statements.dealloc");
    previous.settings.push(Setting {
        name: "pg_stat_statements.dealloc".into(),
        value: "4".into(),
        default_value: None,
        scope: "database".into(),
    });
    previous.operations.counters = vec![OperationalCounter {
        kind: OperationalCounterKind::StandbyConflicts,
        subject: "standby".into(),
        scope: FileScope::Database,
        count: 1.0,
        since: Some("2026-10-01T00:00:00Z".into()),
        last_event_at: None,
        before: None,
    }];
    let mut current = point("first.invalid", "2026-10-11T00:00:00Z", "20261011T000000.456Z", 3.0);
    current.settings.retain(|s| s.name != "pg_stat_statements.dealloc");
    current.settings.push(Setting {
        name: "pg_stat_statements.dealloc".into(),
        value: "7".into(),
        default_value: None,
        scope: "database".into(),
    });
    current.operations.counters = previous.operations.counters.clone();
    current.operations.counters[0].count = 2.0;
    current.evictions_before =
        Some(CountBaseline::from_setting(&previous, &current, "pg_stat_statements.dealloc").unwrap());
    culprit_core::operations::compare(&previous, &mut current);
    let refs = current.baseline_references();
    for purpose in [BaselinePurpose::Evictions, BaselinePurpose::Operations] {
        assert_eq!(refs.iter().find(|r| r.purpose == purpose).unwrap().scan.id.as_deref(), Some(previous.id.as_str()));
    }
    assert!(refs.iter().find(|r| r.purpose == BaselinePurpose::Evictions).unwrap().measurement.is_some());
    assert!(refs.iter().find(|r| r.purpose == BaselinePurpose::Operations).unwrap().measurement.is_none());
}

#[test]
fn simultaneous_readings_do_not_make_a_quiet_query_look_recently_active() {
    let at = "2026-10-10T00:00:00Z";
    let first = point("first.invalid", at, "20261010T000000.001Z", 1.0);
    let second = point("first.invalid", at, "20261010T000000.999Z", 2.0);
    for sources in [[&first, &second], [&second, &first]] {
        let history: Vec<_> = sources.into_iter().map(|s| Counters::from_snapshot(s).unwrap()).collect();
        let mut current = point("first.invalid", "2026-10-11T00:00:00Z", "20261011T000000.000Z", 2.0);
        trends::last_seen_from_scans(current.workload.as_mut().unwrap(), "shop", &current.taken_at, &history);
        assert_eq!(current.workload.as_ref().unwrap().queries[0].last_seen.as_deref(), Some(at));
        let refs = current.baseline_references();
        assert_eq!(refs.iter().filter(|r| r.purpose == BaselinePurpose::QueryActivity).count(), 2);
    }
}

#[test]
fn digest_replays_retained_input_and_leaves_archived_private_evidence_out() {
    let home = home("digest");
    let ws = Workspace::open_in(&home);
    std::fs::create_dir_all(&home).unwrap();
    let mut source = snapshot();
    source.workload.as_mut().unwrap().queries[0].text.push_str(" /* ARCHIVED_PRIVATE_SQL */");
    let mut report = culprit_checks::analyze(source.clone());
    report.evidence.as_mut().unwrap().collected = Some(Box::new(source));
    report.evidence.as_mut().unwrap().recorded_changes.push(culprit_core::ChangeEvent::new(
        culprit_core::EventKind::Table,
        culprit_core::EventSource::Database,
        "private",
        "2026-10-10T00:00:00Z",
        "ARCHIVED_PRIVATE_CHANGE",
    ));
    let path = home.join("current.json");
    std::fs::write(&path, serde_json::to_vec(&report).unwrap()).unwrap();
    let expected = ws.digest(None, Some(&path), None, 100, 100).unwrap();
    report.snapshot.workload = None;
    report.snapshot.tables.clear();
    report.findings.clear();
    std::fs::write(&path, serde_json::to_vec(&report).unwrap()).unwrap();
    let actual = ws.digest(None, Some(&path), None, 100, 100).unwrap();
    assert_eq!(serde_json::to_value(actual.clone()).unwrap(), serde_json::to_value(expected).unwrap());
    let json = serde_json::to_string(&actual).unwrap();
    assert!(!json.contains("ARCHIVED_PRIVATE_SQL"));
    assert!(!json.contains("ARCHIVED_PRIVATE_CHANGE"));
    std::fs::remove_dir_all(home).unwrap();
}
