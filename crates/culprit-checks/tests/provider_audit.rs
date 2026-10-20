use culprit_core::operations::*;
use culprit_core::{Actionability, CheckStatus, CollectorNote, Engine, FileKind, FileScope, Snapshot};

fn snapshot(engine: Engine) -> Snapshot {
    let name = match engine {
        Engine::Mssql => "mssql-demo",
        Engine::Postgres => "postgres-demo",
        Engine::Mysql => "mysql-demo",
        Engine::Sqlite => "sqlite-demo",
    };
    let mut s: Snapshot = serde_json::from_str(
        &std::fs::read_to_string(format!("{}/../../fixtures/{name}.json", env!("CARGO_MANIFEST_DIR"))).unwrap(),
    )
    .unwrap();
    s.taken_at = "2026-10-20T12:00:00Z".into();
    s.operations = Default::default();
    s
}

#[test]
fn unavailable_indexes_do_not_prove_missing_foreign_key_indexes() {
    let mut s = snapshot(Engine::Postgres);
    s.indexes.clear();
    s.notes.push(CollectorNote {
        collector: "indexes".into(),
        message: "Reading indexes failed: permission denied".into(),
    });
    let r = culprit_checks::analyze(s);
    assert!(!r.findings.iter().any(|f| f.check == "unindexed-fk"));
    assert_eq!(r.assessments.iter().find(|c| c.check == "unindexed-fk").unwrap().status, CheckStatus::Unavailable);
    assert!(culprit_checks::index_advice(&r).candidates.iter().all(|c| c.sql.is_none()));
}

#[test]
fn missing_query_data_is_not_assessed_healthy() {
    let mut s = snapshot(Engine::Postgres);
    s.workload = None;
    let r = culprit_checks::analyze(s);
    assert_eq!(r.assessments.iter().find(|c| c.check == "regression").unwrap().status, CheckStatus::Unavailable);
}

#[test]
fn file_capacity_separates_growth_room_hard_limits_and_shared_context() {
    let mut s = snapshot(Engine::Mssql);
    let space = SpaceUsage {
        parts: vec![],
        name: "log".into(),
        kind: FileKind::Log,
        scope: FileScope::Database,
        total_bytes: 100.0,
        used_bytes: 98.0,
        growth_enabled: Some(true),
        limit_bytes: None,
        reuse_wait: Some("ACTIVE_TRANSACTION".into()),
    };
    s.operations.spaces.push(space);
    let r = culprit_checks::analyze(s.clone());
    assert_eq!(
        r.findings.iter().find(|f| f.check == "storage-capacity").unwrap().actionability,
        Actionability::Information
    );
    s.operations.spaces[0].growth_enabled = Some(false);
    let r = culprit_checks::analyze(s.clone());
    assert_eq!(
        r.findings.iter().find(|f| f.check == "storage-capacity").unwrap().actionability,
        Actionability::NeedsAttention
    );
    s.operations.spaces[0].scope = FileScope::Server;
    let r = culprit_checks::analyze(s);
    assert_eq!(
        r.findings.iter().find(|f| f.check == "storage-capacity").unwrap().actionability,
        Actionability::Information
    );
}

#[test]
fn bigint_limits_are_exact_and_cycling_is_not_called_exhaustion() {
    let mut s = snapshot(Engine::Mssql);
    let mut seq = SequenceCapacity {
        name: "dbo.orders.id".into(),
        table: Some("dbo.orders".into()),
        current: "9223372036854775806".into(),
        minimum: "-9223372036854775808".into(),
        maximum: "9223372036854775807".into(),
        increment: "1".into(),
        cycles: false,
        allocated: true,
    };
    assert_eq!(seq.remaining().unwrap().0, 1);
    s.operations.sequences.push(seq.clone());
    assert!(culprit_checks::analyze(s.clone()).findings.iter().any(|f| f.check == "sequence-capacity"));
    seq.cycles = true;
    s.operations.sequences = vec![seq];
    assert!(!culprit_checks::analyze(s).findings.iter().any(|f| f.check == "sequence-capacity"));
}

#[test]
fn multixact_age_is_independent_of_ordinary_xid_age() {
    let mut s = snapshot(Engine::Postgres);
    s.operations.multixacts.push(FreezeAge {
        subject: "public.events".into(),
        age: 1_500_000_000.0,
        freeze_max_age: Some(400_000_000.0),
    });
    let r = culprit_checks::analyze(s);
    assert!(r.findings.iter().any(|f| f.check == "multixact-age"));
}

#[test]
fn retained_errors_and_shared_redo_waits_are_information() {
    for (engine, kind, scope) in [
        (Engine::Postgres, OperationalCounterKind::SubscriptionApplyErrors, FileScope::Database),
        (Engine::Mysql, OperationalCounterKind::RedoLogWaits, FileScope::Server),
    ] {
        let mut s = snapshot(engine);
        s.operations.counters.push(OperationalCounter {
            kind,
            subject: "worker".into(),
            scope,
            count: 10000.0,
            since: Some("2025-01-01T00:00:00Z".into()),
            last_event_at: None,
            before: None,
        });
        let r = culprit_checks::analyze(s);
        let f = r.findings.iter().find(|f| f.check == "operational-errors").unwrap();
        assert_eq!(f.actionability, Actionability::Information);
        assert!(f.summary.contains("historical information") || f.summary.contains("whole server"));
    }
}

#[test]
fn counter_comparisons_require_same_reset_and_increasing_counts() {
    let mut a = snapshot(Engine::Postgres);
    a.taken_at = "2026-10-20T10:00:00Z".into();
    a.operations.counters.push(OperationalCounter {
        kind: OperationalCounterKind::StandbyConflicts,
        subject: "db".into(),
        scope: FileScope::Database,
        count: 10.0,
        since: Some("2026-10-01T00:00:00Z".into()),
        last_event_at: None,
        before: None,
    });
    let mut b = a.clone();
    b.taken_at = "2026-10-20T12:00:00Z".into();
    b.operations.counters[0].count = 25.0;
    compare(&a, &mut b);
    assert_eq!(b.operations.counters[0].before.as_ref().unwrap().count, 10.0);
    b.operations.counters[0].since = Some("2026-10-20T11:00:00Z".into());
    compare(&a, &mut b);
    assert!(b.operations.counters[0].before.is_none());
    b.operations.counters[0].since = a.operations.counters[0].since.clone();
    b.operations.counters[0].count = 5.0;
    compare(&a, &mut b);
    assert!(b.operations.counters[0].before.is_none());
}

#[test]
fn galera_donor_and_historical_flow_control_do_not_become_outages() {
    let mut s = snapshot(Engine::Mysql);
    s.operations.cluster = Some(ClusterHealth {
        ready: Some(true),
        connected: Some(true),
        primary_component: Some(true),
        local_state: Some("Donor/Desynced".into()),
        flow_control_paused: Some(0.5),
    });
    let r = culprit_checks::analyze(s.clone());
    assert_eq!(
        r.findings.iter().find(|f| f.check == "cluster-health").unwrap().actionability,
        Actionability::Information
    );
    s.operations.cluster.as_mut().unwrap().ready = Some(false);
    let r = culprit_checks::analyze(s);
    assert_eq!(
        r.findings.iter().find(|f| f.check == "cluster-health").unwrap().actionability,
        Actionability::NeedsAttention
    );
}

#[test]
fn a_large_wal_is_not_proof_of_a_blocked_checkpoint() {
    let mut s = snapshot(Engine::Sqlite);
    s.operations.sqlite = Some(SqliteFileHealth { wal_bytes: Some(512.0 * 1024.0 * 1024.0), before: None });
    let r = culprit_checks::analyze(s);
    let f = r.findings.iter().find(|f| f.check == "sqlite-wal").unwrap();
    assert_eq!(f.actionability, Actionability::Information);
    assert!(f.fix.as_ref().unwrap().sql.is_none());
    assert!(f.summary.contains("does not establish"));
}

#[test]
fn a_young_small_integer_range_is_not_near_exhaustion() {
    let mut s = snapshot(Engine::Mysql);
    s.operations.sequences.push(SequenceCapacity {
        name: "items.id".into(),
        table: Some("items".into()),
        current: "10".into(),
        increment: "1".into(),
        minimum: "0".into(),
        maximum: "255".into(),
        cycles: false,
        allocated: true,
    });
    assert!(!culprit_checks::analyze(s.clone()).findings.iter().any(|f| f.check == "sequence-capacity"));
    s.operations.sequences[0].current = "250".into();
    assert!(culprit_checks::analyze(s).findings.iter().any(|f| f.check == "sequence-capacity"));
}

#[test]
fn dashboard_health_cannot_call_a_nearly_exhausted_identity_healthy() {
    let mut s = snapshot(Engine::Mssql);
    s.operations.sequences.push(SequenceCapacity {
        name: "items.id".into(),
        table: Some("items".into()),
        current: "2147483646".into(),
        increment: "1".into(),
        minimum: "-2147483648".into(),
        maximum: "2147483647".into(),
        cycles: false,
        allocated: true,
    });
    let r = culprit_checks::analyze(s);
    let d = culprit_checks::dashboard(&r);
    let tile = d.tile(culprit_core::TileKey::Health).unwrap();
    assert_eq!(tile.status, culprit_core::TileStatus::Watch);
    assert!(tile.takeaway.contains("numeric limit"));
}

#[test]
fn logs_with_observations_are_not_disabled_by_their_success_note() {
    let mut s = snapshot(Engine::Sqlite);
    s.server.capabilities.push(culprit_core::Capability::QueryLog);
    s.notes.push(CollectorNote { collector: "query log".into(), message: "Read structured error records".into() });
    s.operations.log_failures.push(LoggedFailure {
        query_id: "shape".into(),
        statement: "INSERT INTO items VALUES (?)".into(),
        code: "SQLITE_BUSY_SNAPSHOT".into(),
        count: 3.0,
        first_at: None,
        last_at: None,
    });
    let r = culprit_checks::analyze(s);
    let f = r.findings.iter().find(|f| f.check == "logged-errors").unwrap();
    assert_eq!(f.actionability, Actionability::Information);
    assert!(f.fix.as_ref().unwrap().summary.contains("complete transaction"));
}

#[test]
fn a_new_untimed_storage_error_cannot_inherit_an_old_successful_runs_history() {
    let mut s = snapshot(Engine::Sqlite);
    s.server.capabilities.push(culprit_core::Capability::QueryLog);
    let mut w = snapshot(Engine::Postgres).workload.unwrap();
    w.queries.truncate(1);
    let q = &mut w.queries[0];
    q.id = "shape".into();
    q.executions = 1.0;
    q.first_seen = Some("2026-10-01T10:00:00Z".into());
    q.last_seen = q.first_seen.clone();
    q.plan = None;
    w.log = Some("JSON lines".into());
    s.workload = Some(w);
    s.operations.log_failures.push(LoggedFailure {
        query_id: "shape".into(),
        statement: "INSERT INTO items VALUES (?)".into(),
        code: "SQLITE_CORRUPT".into(),
        count: 1.0,
        first_at: Some("2026-10-20T11:59:00Z".into()),
        last_at: Some("2026-10-20T11:59:00Z".into()),
    });
    let r = culprit_checks::analyze(s);
    let f = r.findings.iter().find(|f| f.check == "logged-errors").unwrap();
    assert_eq!(f.actionability, Actionability::NeedsAttention);
    assert!(!f.inactive);
    assert_eq!(
        culprit_checks::dashboard(&r).tile(culprit_core::TileKey::Failures).unwrap().status,
        culprit_core::TileStatus::Watch
    );
}

#[test]
fn catalog_only_holds_do_not_explain_application_dead_rows() {
    let mut s = snapshot(Engine::Postgres);
    s.pressure.get_or_insert_with(Default::default).oldest_transaction = Some(culprit_core::OpenTransaction {
        session: "decoder".into(),
        holder: Some(culprit_core::CleanupHolder::ReplicationSlot),
        xid_age: Some(150_000_000.0),
        catalog_only: true,
        ..Default::default()
    });
    for table in &mut s.tables {
        table.rows = Some(100_000.0);
        table.dead_rows = Some(1_000_000.0);
        table.last_vacuum = Some("2026-10-20T11:45:00Z".into());
    }
    let r = culprit_checks::analyze(s);
    let f = r.findings.iter().find(|f| f.check == "held-cleanup").unwrap();
    assert!(f.title.contains("system-catalog cleanup"));
    for f in r.findings.iter().filter(|f| f.check == "dead-rows") {
        assert!(!f.summary.contains("A cleanup holder was observed"));
        assert!(f.fix.as_ref().unwrap().sql.is_none());
    }
}

#[test]
fn new_errors_need_a_comparison_within_the_selected_window() {
    let mut s = snapshot(Engine::Postgres);
    s.options = Some(culprit_core::SnapshotOptions { window_minutes: Some(60), ..Default::default() });
    s.operations.counters.push(OperationalCounter {
        kind: OperationalCounterKind::SubscriptionApplyErrors,
        subject: "feed".into(),
        scope: FileScope::Database,
        count: 100.0,
        since: Some("2026-10-01T00:00:00Z".into()),
        last_event_at: None,
        before: Some(CounterReading { at: "2026-10-20T10:00:00Z".into(), count: 50.0 }),
    });
    let r = culprit_checks::analyze(s.clone());
    assert_eq!(
        r.findings.iter().find(|f| f.check == "operational-errors").unwrap().actionability,
        Actionability::Information
    );
    s.operations.counters[0].before.as_mut().unwrap().at = "2026-10-20T11:15:00Z".into();
    let r = culprit_checks::analyze(s);
    assert_eq!(
        r.findings.iter().find(|f| f.check == "operational-errors").unwrap().actionability,
        Actionability::NeedsAttention
    );
}
