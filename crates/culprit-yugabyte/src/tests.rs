use super::*;
use culprit_postgres::wire::{INTERACTIVE, query_one, tagged};

// These opt-in cases share a disposable database. Role DDL invalidates active transactions' catalog snapshots;
// serialize fixture mutations instead of adding production retries for an artificial cross-test race.
static LIVE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn local_spec() -> ConnectionSpec {
    let mut spec = ConnectionSpec {
        engine: Some(Engine::Yugabyte),
        host: "127.0.0.1".into(),
        port: Some(std::env::var("CULPRIT_YSQL_TEST_PORT").expect("set CULPRIT_YSQL_TEST_PORT").parse().unwrap()),
        user: Some("yugabyte".into()),
        database: Some("culprit_yugabyte".into()),
        environment: culprit_core::Environment::Local,
        ..Default::default()
    };
    spec.options.insert("sslmode".into(), "disable".into());
    spec
}

#[test]
fn provider_rejects_foreign_engines_and_cloud_bearer_credentials() {
    let mut spec =
        ConnectionSpec { host: "db.example.com".into(), engine: Some(Engine::Yugabyte), ..Default::default() };
    for auth in [AuthKind::Entra, AuthKind::AwsIam] {
        spec.auth = auth;
        spec.token = Some("never-send-this-token".into());
        let error = YugabyteProvider::new(&spec).err().unwrap();
        assert!(!error.message.contains("never-send-this-token"));
        assert_eq!(error.code, culprit_core::ErrorCode::InvalidInput);
    }
    spec.auth = AuthKind::Password;
    spec.token = None;
    let defaults = YugabyteProvider::new(&spec).unwrap();
    assert_eq!(defaults.connector.port, 5433);
    assert_eq!(defaults.spec.user.as_deref(), Some("yugabyte"));
    assert_eq!(defaults.spec.database.as_deref(), Some("yugabyte"));
    spec.engine = Some(Engine::Postgres);
    assert!(YugabyteProvider::new(&spec).is_err());
}

#[tokio::test]
async fn production_measurements_reject_native_mutations_before_connecting() {
    let provider = YugabyteProvider::new(&ConnectionSpec {
        engine: Some(Engine::Yugabyte),
        host: "invalid.example".into(),
        environment: culprit_core::Environment::Production,
        ..Default::default()
    })
    .unwrap();
    for statement in ["SELECT yb_create_global_view('public','pg_settings')", "UPDATE orders SET total=0"] {
        assert_eq!(
            provider.explain(statement, ExplainMode::Analyze).await.unwrap_err().code,
            culprit_core::ErrorCode::PermissionDenied
        );
    }
}

#[tokio::test]
#[ignore = "requires dev/yugabyte and CULPRIT_YSQL_TEST_PORT"]
async fn live_native_catalog_workload_sessions_and_stale_identity_guards() {
    let _fixture = LIVE.lock().await;
    let wrong =
        culprit_postgres::PostgresProvider::new(&ConnectionSpec { engine: Some(Engine::Postgres), ..local_spec() })
            .unwrap()
            .probe()
            .await
            .unwrap_err();
    assert_eq!(wrong.code, culprit_core::ErrorCode::InvalidInput);
    assert!(wrong.hint.as_deref().unwrap().contains("--engine yugabyte"));
    let provider = YugabyteProvider::new(&local_spec()).unwrap();
    let probe = provider.probe().await.unwrap();
    assert_eq!(probe.engine, Engine::Yugabyte);
    assert!(probe.database_size_bytes.is_none());
    let snapshot = provider.snapshot("yugabyte-native-test", &SnapshotOptions::default()).await.unwrap();
    assert_eq!(snapshot.identity.as_ref().unwrap().engine, Engine::Yugabyte);
    assert!(matches!(snapshot.workload_measurement().unwrap().database.location,
        culprit_core::DatabaseLocation::Server { port, .. } if port == provider.connector.port));
    let orders = snapshot.tables.iter().find(|t| t.name == "orders").unwrap();
    assert_eq!(orders.rows, Some(20_000.0));
    assert!(orders.data_bytes.is_some_and(|n| n > 0.0));
    assert!(orders.dead_rows.is_none() && orders.xid_age.is_none());
    assert!(orders.storage.as_ref().unwrap().docdb.is_some());
    let index = snapshot.indexes.iter().find(|i| i.name == "orders_created_idx").unwrap();
    assert_eq!(index.docdb.as_ref().unwrap().hash_columns, vec!["created_at"]);
    assert!(index.size_bytes.is_some_and(|n| n > 0.0));
    let workload = snapshot.workload.as_ref().unwrap();
    assert!(snapshot.server.has(culprit_core::Capability::PgStatStatements));
    assert!(!snapshot.server.has(culprit_core::Capability::IndexUsage));
    assert!(workload.counter_set.as_ref().unwrap().epochs_known());
    assert!(workload.queries.iter().any(|q| q.plan.as_ref().is_some_and(|p| p.engine == Engine::Yugabyte)));
    assert!(
        workload
            .queries
            .iter()
            .all(|q| q.logical_reads.is_none() && q.physical_reads.is_none() && q.cpu_time_ms.is_none())
    );

    // Preview configuration is changed only on this disposable test session, never by the provider.
    let global = provider.connector.connect_observed(COLLECTOR).await.unwrap();
    global.client.batch_execute(&tagged("SET yb_enable_global_views=on")).await.unwrap();
    let probe = crate::probe::read(&global.client).await.unwrap();
    let mut context = snapshot.clone();
    context.sources.retain(|source| source.source != culprit_core::source_id::DOCDB_INDEX_USAGE);
    let mut coverage = culprit_core::Coverage::default();
    crate::index_usage::read(&global, &probe, &mut context, &mut coverage).await;
    coverage.append_to(&mut context);
    assert!(context.server.has(culprit_core::Capability::IndexUsage));
    assert!(context.server.missing.iter().all(|missing| missing.capability != culprit_core::Capability::IndexUsage));
    assert!(context.indexes.iter().all(|index| index.reads.is_some() && index.counters_since.is_none()));

    let client = provider.connector.connect(INTERACTIVE).await.unwrap();
    let (bounded, truncated) =
        culprit_postgres::wire::query_bounded(&client, "SELECT n::text FROM generate_series(1,20) n", 3).await.unwrap();
    assert_eq!(bounded.len(), 3);
    assert!(truncated);
    let oversized = format!(
        "SELECT CASE WHEN n=1 THEN repeat('x',{}) ELSE 'small' END FROM generate_series(1,2) n",
        culprit_core::limits::MAX_RESULT_BYTES + 1
    );
    let (bounded, truncated) = culprit_postgres::wire::query_bounded(&client, &oversized, 10).await.unwrap();
    assert!(bounded.is_empty() && truncated, "a small row after a rejected large row must not break prefix retention");
    assert!(
        culprit_postgres::wire::query_bounded(&client, "SELECT 100/(20-n) FROM generate_series(1,20) n", 3)
            .await
            .is_err(),
        "a late error must survive collector truncation"
    );
    let own_identity = query_one(&client, &format!(
        "SELECT replace(yb_get_local_tserver_uuid()::text,'-','') AS node,pid::text,{} AS started FROM pg_stat_activity WHERE pid=pg_backend_pid()",
        crate::probe::timestamp("backend_start")
    )).await.unwrap().unwrap();
    let monitor =
        format!("{}/{}/{}", own_identity.string("node"), own_identity.string("pid"), own_identity.string("started"));
    assert_eq!(
        provider.interrupt_session(&monitor, culprit_core::SessionAction::Terminate).await.unwrap_err().code,
        culprit_core::ErrorCode::NotFound
    );
    assert!(query_one(&client, "SELECT 1 AS monitor_still_alive").await.unwrap().is_some());
    client.batch_execute(&tagged("SET application_name='culprit_native_session_test'")).await.unwrap();
    let own = query_one(&client, "SELECT pg_backend_pid()::text AS pid").await.unwrap().unwrap().string("pid");
    let live = provider.live().await.unwrap();
    let observed = live
        .sessions
        .iter()
        .find(|s| s.application.as_deref() == Some("culprit_native_session_test") && s.id.contains(&format!("/{own}/")))
        .unwrap();
    assert_eq!(observed.id.split('/').count(), 3);
    let mut stale: Vec<_> = observed.id.split('/').map(str::to_string).collect();
    stale[2] = "2000-01-01T00:00:00Z".into();
    assert_eq!(
        provider.interrupt_session(&stale.join("/"), culprit_core::SessionAction::Cancel).await.unwrap_err().code,
        culprit_core::ErrorCode::NotFound
    );
    stale[0] = "00000000000000000000000000000000".into();
    assert!(provider.interrupt_session(&stale.join("/"), culprit_core::SessionAction::Terminate).await.is_err());
    assert!(query_one(&client, "SELECT 1 AS still_alive").await.unwrap().is_some());
    let active = provider.connector.connect(INTERACTIVE).await.unwrap();
    active.batch_execute(&tagged("SET application_name='culprit_native_progress_test'")).await.unwrap();
    let pid = query_one(&active, "SELECT pg_backend_pid()::text AS pid").await.unwrap().unwrap().string("pid");
    let sleeper = tokio::spawn(async move { active.batch_execute(&tagged("SELECT pg_sleep(15)")).await });
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    let live = provider.live().await.unwrap();
    let running = live
        .sessions
        .iter()
        .find(|s| {
            s.application.as_deref() == Some("culprit_native_progress_test") && s.id.contains(&format!("/{pid}/"))
        })
        .unwrap();
    let progress = provider.query_progress(&running.id, true).await.unwrap();
    assert!(progress.running && progress.elapsed_ms.is_some());
    assert!(
        progress
            .plan
            .as_ref()
            .is_some_and(|plan| plan.engine == Engine::Yugabyte && plan.kind != culprit_core::PlanKind::Actual)
    );
    assert!(progress.nodes.is_empty() && progress.percent.is_none());
    let sample = provider.wait_sample(1).await.unwrap();
    assert_eq!(sample.engine, Engine::Yugabyte);
    assert!(sample.seconds >= 1.0 && sample.active_sessions.is_some_and(|n| n >= 1.0));
    provider.interrupt_session(&running.id, culprit_core::SessionAction::Cancel).await.unwrap();
    assert!(sleeper.await.unwrap().is_err(), "only this test's verified sleeping backend is canceled");
    let mut production = local_spec();
    production.environment = culprit_core::Environment::Production;
    let production = YugabyteProvider::new(&production).unwrap();
    assert!(production.live().await.unwrap().session_actions.is_empty());
    assert_eq!(
        production.interrupt_session(&observed.id, culprit_core::SessionAction::Cancel).await.unwrap_err().code,
        culprit_core::ErrorCode::PermissionDenied
    );
}

#[tokio::test]
#[ignore = "requires dev/yugabyte and CULPRIT_YSQL_TEST_PORT"]
async fn live_limited_login_aggregates_its_own_visible_role_without_hidden_query_groups() {
    let _fixture = LIVE.lock().await;
    let provider = YugabyteProvider::new(&local_spec()).unwrap();
    let admin = provider.connector.connect(INTERACTIVE).await.unwrap();
    admin.batch_execute(&tagged("SET default_transaction_read_only=off")).await.unwrap();
    let pid = query_one(&admin, "SELECT pg_backend_pid()::text AS pid").await.unwrap().unwrap().string("pid");
    let role = format!("culprit_native_scope_{pid}");
    for sql in
        [format!("CREATE ROLE {role} LOGIN"), format!("GRANT SELECT ON public.orders, public.customers TO {role}")]
    {
        admin.batch_execute(&tagged(&sql)).await.unwrap();
    }
    let observed = async {
        let mut spec = local_spec();
        spec.user = Some(role.clone());
        let restricted = YugabyteProvider::new(&spec)?;
        // Synthetic application activity; intentionally recorded without the collector tag.
        let client = restricted.connector.connect(INTERACTIVE).await?;
        client
            .simple_query("SELECT count(*) FROM public.orders WHERE customer_id=42 /* ysql_scope_test */")
            .await
            .map_err(culprit_postgres::wire::map_error)?;
        restricted.snapshot("yugabyte-restricted-test", &SnapshotOptions { plans: 0, ..Default::default() }).await
    }
    .await;
    for sql in [format!("REVOKE SELECT ON public.orders, public.customers FROM {role}"), format!("DROP ROLE {role}")] {
        admin.batch_execute(&tagged(&sql)).await.unwrap();
    }
    let snapshot = observed.unwrap();
    let workload = snapshot.workload.unwrap();
    assert!(workload.source.contains("role ") && workload.source.ends_with(" only"));
    assert!(!workload.queries.is_empty());
    assert!(workload.queries.iter().all(|q| q.text.contains("ysql_scope_test")));
    assert_eq!(workload.query_count, workload.queries.len() as f64);
    assert_eq!(workload.total_executions, workload.queries.iter().map(|q| q.executions).sum::<f64>());
}

#[tokio::test]
#[ignore = "requires dev/yugabyte and CULPRIT_YSQL_TEST_PORT"]
async fn live_docdb_blocking_links_native_transaction_ids_to_scoped_sessions() {
    let _fixture = LIVE.lock().await;
    let provider = YugabyteProvider::new(&local_spec()).unwrap();
    let holder = provider.connector.connect(INTERACTIVE).await.unwrap();
    for sql in [
        "SET application_name='culprit_native_lock_holder'",
        "BEGIN READ WRITE",
        "UPDATE public.orders SET total=total WHERE id=42",
    ] {
        holder.batch_execute(&tagged(sql)).await.unwrap();
    }
    let waiter = provider.connector.connect(INTERACTIVE).await.unwrap();
    for sql in ["SET application_name='culprit_native_lock_waiter'", "BEGIN READ WRITE"] {
        waiter.batch_execute(&tagged(sql)).await.unwrap();
    }
    let blocked = tokio::spawn(async move {
        let result = waiter.batch_execute(&tagged("UPDATE public.orders SET total=total WHERE id=42")).await;
        let _ = waiter.batch_execute(&tagged("ROLLBACK")).await;
        result
    });
    // Native lock metadata follows transaction heartbeats; poll this known live fixture transaction, bounded by
    // its own five-second test deadline, rather than requiring it to appear in the first metadata snapshot.
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    let live = loop {
        let observed = provider.live().await;
        let linked = observed.as_ref().is_ok_and(|live| {
            live.sessions
                .iter()
                .any(|s| s.application.as_deref() == Some("culprit_native_lock_waiter") && s.blocked_by.is_some())
        });
        if linked || observed.is_err() || tokio::time::Instant::now() >= deadline {
            break observed;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    };
    holder.batch_execute(&tagged("ROLLBACK")).await.unwrap();
    blocked.await.unwrap().unwrap();
    let live = live.unwrap();
    let holder = live.sessions.iter().find(|s| s.application.as_deref() == Some("culprit_native_lock_holder")).unwrap();
    let waiter = live.sessions.iter().find(|s| s.application.as_deref() == Some("culprit_native_lock_waiter")).unwrap();
    assert_eq!(
        waiter.blocked_by.as_deref(),
        Some(holder.id.as_str()),
        "native DocDB lock evidence must reach the holder's node-qualified identity: {:?}",
        live.notes
    );
    assert!(live.blocking.is_some());
}

#[tokio::test]
#[ignore = "requires dev/yugabyte and CULPRIT_YSQL_TEST_PORT"]
async fn live_relocated_statement_extension_keeps_native_statistics_and_planning() {
    let _fixture = LIVE.lock().await;
    let provider = YugabyteProvider::new(&local_spec()).unwrap();
    let admin = provider.connector.connect(INTERACTIVE).await.unwrap();
    let before = crate::probe::read(&admin).await.unwrap().statement_schema.unwrap();
    let pid = query_one(&admin, "SELECT pg_backend_pid()::text AS pid").await.unwrap().unwrap().string("pid");
    let schema = format!("culprit_native_stats_{pid}");
    admin.batch_execute(&tagged("SET default_transaction_read_only=off")).await.unwrap();
    for sql in [format!("CREATE SCHEMA {schema}"), format!("ALTER EXTENSION pg_stat_statements SET SCHEMA {schema}")] {
        admin.batch_execute(&tagged(&sql)).await.unwrap();
    }
    let observed = provider.snapshot("yugabyte-relocated-test", &SnapshotOptions::default()).await;
    for sql in [
        format!("ALTER EXTENSION pg_stat_statements SET SCHEMA {}", Engine::Yugabyte.dialect().quote(&before)),
        format!("DROP SCHEMA {schema}"),
    ] {
        admin.batch_execute(&tagged(&sql)).await.unwrap();
    }
    let snapshot = observed.unwrap();
    let workload = snapshot.workload.unwrap();
    assert!(!workload.queries.is_empty());
    assert!(workload.counter_set.unwrap().epochs_known());
    assert!(workload.queries.iter().any(|q| q.plan.as_ref().is_some_and(|p| p.engine == Engine::Yugabyte)));
}
