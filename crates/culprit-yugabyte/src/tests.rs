use super::*;
use culprit_postgres::wire::{INTERACTIVE, query_one, tagged};

// These opt-in cases share a disposable database. Role DDL invalidates active transactions' catalog snapshots;
// serialize fixture mutations instead of adding production retries for an artificial cross-test race.
static LIVE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[tokio::test]
#[ignore = "requires dev/yugabyte and CULPRIT_YSQL_TEST_PORT"]
async fn live_hypothetical_indexes_use_native_plans_without_persistent_indexes_or_heap_measurements() {
    let _fixture = LIVE.lock().await;
    let provider = YugabyteProvider::new(&local_spec()).unwrap();
    let admin = provider.connector.connect(INTERACTIVE).await.unwrap();
    admin.batch_execute(&tagged("SET default_transaction_read_only=off")).await.unwrap();
    let existing=query_one(&admin,"SELECT n.nspname AS schema FROM pg_extension e JOIN pg_namespace n ON n.oid=e.extnamespace WHERE e.extname='hypopg'").await.unwrap();
    let pid = query_one(&admin, "SELECT pg_backend_pid()::text AS pid").await.unwrap().unwrap().string("pid");
    let schema = format!("culprit_hypothetical_{pid}");
    admin.batch_execute(&tagged(&format!("CREATE SCHEMA {schema}"))).await.unwrap();
    if existing.is_none() {
        let missing = provider
            .explain_hypothetical(
                "SELECT * FROM public.orders",
                &["CREATE INDEX ON public.orders (customer_id ASC)".into()],
            )
            .await
            .unwrap_err();
        assert_eq!(missing.code, culprit_core::ErrorCode::Unsupported);
        assert!(missing.hint.as_deref().unwrap().contains("CREATE EXTENSION hypopg"));
        admin.batch_execute(&tagged(&format!("CREATE EXTENSION hypopg WITH SCHEMA {schema}"))).await.unwrap();
    }
    let observed = async {
        let sql = "SELECT * FROM public.orders WHERE customer_id=42";
        let index = "CREATE INDEX culprit_virtual_customer ON public.orders (customer_id ASC)".to_string();
        let plan = provider.explain_hypothetical(sql, std::slice::from_ref(&index)).await?;
        let parameterized = provider
            .explain_hypothetical("SELECT * FROM public.orders WHERE customer_id=$1", std::slice::from_ref(&index))
            .await?;
        let invalid = provider.explain_hypothetical(sql, &["DROP TABLE public.orders".into()]).await;
        let multiple = provider.explain_hypothetical(sql, &[format!("{index}; DROP TABLE public.orders")]).await;
        let sql_error = provider
            .explain_hypothetical("SELECT missing_column FROM public.orders", std::slice::from_ref(&index))
            .await;
        let unmodified = provider.explain(sql, culprit_core::ExplainMode::Estimated).await?;
        let persistent =
            query_one(&admin, "SELECT to_regclass('public.culprit_virtual_customer') IS NOT NULL AS found")
                .await?
                .unwrap()
                .flag("found");
        Ok::<_, Error>((plan, parameterized, invalid, multiple, sql_error, unmodified, persistent))
    }
    .await;
    if existing.is_none() {
        admin.batch_execute(&tagged("DROP EXTENSION hypopg")).await.unwrap();
    }
    admin.batch_execute(&tagged(&format!("DROP SCHEMA {schema}"))).await.unwrap();
    let (plan, parameterized, invalid, multiple, sql_error, unmodified, persistent) = observed.unwrap();
    assert_eq!(plan.engine, Engine::Yugabyte);
    assert_eq!(parameterized.engine, Engine::Yugabyte);
    assert_eq!(parameterized.kind, culprit_core::PlanKind::Generic);
    assert!(plan.props.get("VirtualIndexes").is_some_and(|names| names.contains("hypo") || names.contains("lsm")));
    assert_eq!(crate::plan::reparse(&plan).unwrap().props.get("VirtualIndexes"), plan.props.get("VirtualIndexes"));
    assert!(plan.nodes().iter().any(|n| n.index.as_deref().is_some_and(|i| i.contains("lsm"))));
    assert!(
        plan.nodes().iter().all(|n| n.table_rows.is_none() && n.facts().heap_fetches.is_none() && n.runtime.is_none())
    );
    assert!(invalid.is_err() && multiple.is_err() && sql_error.is_err());
    assert!(!persistent);
    assert!(!unmodified.props.contains_key("VirtualIndexes"));
}

#[tokio::test]
#[ignore = "requires dev/yugabyte and CULPRIT_YSQL_TEST_PORT"]
async fn live_copy_progress_keeps_native_counters_and_rejects_retained_results_and_stale_sessions() {
    use futures_util::SinkExt;

    let _fixture = LIVE.lock().await;
    let provider = YugabyteProvider::new(&local_spec()).unwrap();
    let active = provider.connector.connect(INTERACTIVE).await.unwrap();
    active.batch_execute(&tagged("SET default_transaction_read_only=off")).await.unwrap();
    let identity = query_one(&active, &format!(
        "SELECT replace(yb_get_local_tserver_uuid()::text,'-','') AS node,pg_backend_pid()::text AS pid,{} AS started FROM pg_stat_activity WHERE pid=pg_backend_pid()",
        crate::probe::timestamp("backend_start")
    )).await.unwrap().unwrap();
    let pid = identity.string("pid");
    let id = format!("{}/{pid}/{}", identity.string("node"), identity.string("started"));
    // Temporary tables use PostgreSQL-local storage; use a bounded, owned DocDB table for native batch counters.
    let table = format!("public.culprit_progress_copy_{pid}");
    active.batch_execute(&tagged(&format!("CREATE TABLE {table} (id int PRIMARY KEY, payload text)"))).await.unwrap();
    let role = format!("culprit_progress_reader_{pid}");
    active.batch_execute(&tagged(&format!("CREATE ROLE {role} LOGIN"))).await.unwrap();
    let copy = active
        .copy_in::<_, std::io::Cursor<Vec<u8>>>(&tagged(&format!(
            "/* leading application comment */ COPY {table} FROM STDIN WITH (ROWS_PER_TRANSACTION 2)",
        )))
        .await
        .unwrap();
    tokio::pin!(copy);
    let data = (1..=16).map(|i| format!("{i}\t{}\n", "x".repeat(8192))).collect::<String>();
    copy.as_mut().send(std::io::Cursor::new(data.into_bytes())).await.unwrap();
    copy.as_mut().flush().await.unwrap();
    let mut observed = None;
    let mut last = None;
    for _ in 0..40 {
        let progress = provider.query_progress(&id, true).await.unwrap();
        if progress.phase.as_deref().is_some_and(|p| p.contains("COPY FROM PIPE: IN PROGRESS") && !p.contains("0 rows"))
        {
            observed = Some(progress);
            break;
        }
        last = Some(progress);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    let progress = observed.unwrap_or_else(|| {
        panic!(
            "native COPY progress was not visible while its stream was held open: {}",
            serde_json::to_string(&last).unwrap()
        )
    });
    assert!(progress.running && progress.statement.as_deref().unwrap().starts_with("/* leading"));
    assert!(progress.phase.as_deref().unwrap().contains("rows reported processed"));
    assert!(progress.note.as_deref().unwrap().contains("completed transactions"));
    assert!(progress.percent.is_none() && progress.phase_percent.is_none() && progress.nodes.is_empty());
    assert!(progress.plan.is_none(), "native command progress does not require an estimated query plan");
    assert!(!progress.phase.as_deref().unwrap().contains("source file"), "STDIN has no known file size");
    let global = provider.connector.connect_observed(INTERACTIVE).await.unwrap();
    global.client.batch_execute(&tagged("SET yb_enable_global_views=on")).await.unwrap();
    let (cluster, _) = crate::progress::read_session(&global, &id, false).await.unwrap();
    assert!(cluster.phase.as_deref().is_some_and(|p| p.starts_with("COPY FROM PIPE: IN PROGRESS")));
    assert!(cluster.percent.is_none());
    let mut spec = local_spec();
    spec.user = Some(role.clone());
    let limited = YugabyteProvider::new(&spec).unwrap().query_progress(&id, false).await.unwrap();
    assert!(
        limited.phase.is_none() && limited.statement.is_none(),
        "hidden activity cannot authorize reading another role's command counters"
    );
    let stale = id.rsplit_once('/').map(|(head, _)| format!("{head}/2000-01-01T00:00:00Z")).unwrap();
    let stale = provider.query_progress(&stale, false).await.unwrap();
    assert!(!stale.running && stale.phase.is_none());
    let foreign = format!("00000000000000000000000000000000/{pid}/{}", identity.string("started"));
    assert!(provider.query_progress(&foreign, false).await.unwrap().phase.is_none());
    assert_eq!(copy.as_mut().finish().await.unwrap(), 16);
    let retained = query_one(
        &active,
        &format!("SELECT yb_status,tuples_processed::text AS rows FROM pg_stat_progress_copy WHERE pid={pid}"),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(retained.string("yb_status"), "SUCCESS");
    assert_eq!(retained.string("rows"), "16");
    let idle = provider.query_progress(&id, false).await.unwrap();
    assert!(!idle.running && idle.phase.is_none());
    // A failed COPY is retained too; it cannot become progress for a later statement on this backend.
    let copy = active.copy_in::<_, &'static [u8]>(&tagged(&format!("COPY {table} FROM STDIN"))).await.unwrap();
    tokio::pin!(copy);
    copy.as_mut().send(b"invalid-integer\tx\n").await.unwrap();
    assert!(copy.as_mut().finish().await.is_err());
    let retained = query_one(&active, &format!("SELECT yb_status FROM pg_stat_progress_copy WHERE pid={pid}"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(retained.string("yb_status"), "ERROR");
    let sleeping = async {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let progress = provider.query_progress(&id, false).await.unwrap();
        assert!(progress.running && progress.statement.as_deref().unwrap().contains("pg_sleep"));
        assert!(progress.phase.is_none(), "retained COPY must not describe a later SELECT");
    };
    let sleep_sql = tagged("SELECT pg_sleep(2)");
    let (done, ()) = tokio::join!(active.batch_execute(&sleep_sql), sleeping);
    done.unwrap();
    active.batch_execute(&tagged(&format!("DROP TABLE {table}"))).await.unwrap();
    active.batch_execute(&tagged(&format!("DROP ROLE {role}"))).await.unwrap();
}

#[tokio::test]
#[ignore = "requires dev/yugabyte and CULPRIT_YSQL_TEST_PORT"]
async fn live_index_progress_reports_backfill_rows_without_a_guessed_percentage() {
    let _fixture = LIVE.lock().await;
    let provider = YugabyteProvider::new(&local_spec()).unwrap();
    let active = provider.connector.connect(INTERACTIVE).await.unwrap();
    active.batch_execute(&tagged("SET default_transaction_read_only=off")).await.unwrap();
    let identity = query_one(&active, &format!(
        "SELECT replace(yb_get_local_tserver_uuid()::text,'-','') AS node,pg_backend_pid()::text AS pid,{} AS started FROM pg_stat_activity WHERE pid=pg_backend_pid()",
        crate::probe::timestamp("backend_start")
    )).await.unwrap().unwrap();
    let pid = identity.string("pid");
    let id = format!("{}/{pid}/{}", identity.string("node"), identity.string("started"));
    let table = format!("public.culprit_progress_index_{pid}");
    active.batch_execute(&tagged(&format!("CREATE TABLE {table} (id int PRIMARY KEY, value int); INSERT INTO {table} SELECT n,n%10 FROM generate_series(1,20000) n; ANALYZE {table}"))).await.unwrap();
    let sql = tagged(&format!(
        "/* application comment */ CREATE INDEX culprit_progress_idx_{pid} ON {table}(value) WHERE value=1"
    ));
    let poll = async {
        let mut seen = Vec::new();
        for _ in 0..100 {
            let progress = provider.query_progress(&id, false).await.unwrap();
            if progress.phase.is_some() {
                assert!(progress.percent.is_none() && progress.phase_percent.is_none() && progress.nodes.is_empty());
                let backfilling = progress.phase.as_deref().unwrap().contains("table rows processed");
                seen.push(progress);
                if backfilling {
                    break;
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        }
        seen
    };
    let (built, observed) = tokio::join!(active.batch_execute(&sql), poll);
    active.batch_execute(&tagged(&format!("DROP TABLE {table}"))).await.unwrap();
    built.unwrap();
    assert!(
        observed.iter().any(|p| p.phase.as_deref().unwrap().contains("table rows processed")),
        "native backfill rows were not observed: {}",
        serde_json::to_string(&observed).unwrap()
    );
    assert!(observed.iter().any(|p| p.note.as_deref().unwrap().contains("including for partial indexes")));
}

#[tokio::test]
#[ignore = "requires dev/yugabyte and CULPRIT_YSQL_TEST_PORT"]
async fn live_node_resources_are_shared_native_samples_with_their_own_admin_permissions() {
    let _fixture = LIVE.lock().await;
    let provider = YugabyteProvider::new(&local_spec()).unwrap();
    let admin = provider.connector.connect_observed(INTERACTIVE).await.unwrap();
    admin.client.batch_execute(&tagged("SET yb_enable_global_views=off")).await.unwrap();
    let p = crate::probe::read(&admin.client).await.unwrap();
    let resources = crate::resources::read(&admin, 1000).await.unwrap();
    assert!(!resources.partial);
    assert_eq!(resources.nodes_expected, Some(1));
    assert_eq!(resources.nodes.len(), 1);
    let node = &resources.nodes[0];
    assert!(node.available);
    assert_eq!(Some(&node.node), p.node.as_ref());
    for fraction in [node.cpu_user_fraction, node.cpu_system_fraction] {
        assert!(fraction.is_some_and(|n| (0.0..=1.0).contains(&n)));
    }
    assert!(node.host_memory_total_bytes.is_some_and(|n| n > 0.0));
    let memory = node.process_memory.as_ref().unwrap();
    assert!(memory.tracked_bytes.is_some_and(|n| n >= 0.0));
    assert!(memory.hard_limit.is_some() && memory.soft_limit.is_some());
    assert!(
        culprit_core::format::epoch_seconds(&resources.observed_until)
            >= culprit_core::format::epoch_seconds(&resources.observed_from)
    );
    assert!(resources.notes.iter().any(|n| n.contains("container")));
    let pid = query_one(&admin.client, "SELECT pg_backend_pid()::text AS pid").await.unwrap().unwrap().string("pid");
    let role = format!("culprit_resource_reader_{pid}");
    admin.client.batch_execute(&tagged("SET default_transaction_read_only=off")).await.unwrap();
    admin.client.batch_execute(&tagged(&format!("CREATE ROLE {role} LOGIN"))).await.unwrap();
    let access = async {
        let mut spec = local_spec();
        spec.user = Some(role.clone());
        let reader = YugabyteProvider::new(&spec)?;
        let session = reader.connector.connect_observed(INTERACTIVE).await?;
        let denied = crate::resources::read(&session, 1000).await;
        admin
            .client
            .batch_execute(&tagged(&format!("GRANT pg_read_all_stats TO {role}")))
            .await
            .map_err(culprit_postgres::wire::map_error)?;
        let monitor_denied = crate::resources::read(&session, 1000).await;
        admin
            .client
            .batch_execute(&tagged(&format!("GRANT yb_db_admin TO {role}")))
            .await
            .map_err(culprit_postgres::wire::map_error)?;
        let permitted = crate::resources::read(&session, 1000).await;
        admin
            .client
            .batch_execute(&tagged(&format!("REVOKE yb_db_admin FROM {role}")))
            .await
            .map_err(culprit_postgres::wire::map_error)?;
        let revoked = crate::resources::read(&session, 1000).await;
        Ok::<_, Error>((denied, monitor_denied, permitted, revoked))
    }
    .await;
    admin.client.batch_execute(&tagged(&format!("DROP ROLE {role}"))).await.unwrap();
    let (denied, monitor_denied, permitted, revoked) = access.unwrap();
    for result in [denied, monitor_denied, revoked] {
        let error = result.unwrap_err();
        assert_eq!(error.code, culprit_core::ErrorCode::PermissionDenied);
        assert!(error.hint.unwrap().contains("pg_read_all_stats alone does not grant access"));
    }
    assert!(permitted.unwrap().nodes.iter().any(|n| n.available));
}

#[tokio::test]
#[ignore = "requires dev/yugabyte and CULPRIT_YSQL_TEST_PORT"]
async fn live_terminations_preserve_visibility_period_nodes_and_omit_raw_sql() {
    let _fixture = LIVE.lock().await;
    let provider = YugabyteProvider::new(&local_spec()).unwrap();
    let session = provider.connector.connect_observed(INTERACTIVE).await.unwrap();
    let fail = "SELECT count(*) FROM (SELECT generate_series(1,20000) AS n ORDER BY n DESC) native_termination_test /* PRIVATE-TERMINATION-SQL */";
    session.client.batch_execute(&tagged("SET work_mem='64kB'")).await.unwrap();
    session.client.simple_query(fail).await.unwrap();
    // Aliases do not change the native fingerprint; PGSS can retain the first alias seen for the same shape.
    let query_id=query_one(&session.client,"SELECT queryid::text AS id FROM pg_stat_statements WHERE query LIKE 'SELECT count(*) FROM (SELECT generate_series%' AND userid=(SELECT oid FROM pg_roles WHERE rolname=current_user) LIMIT 1").await.unwrap().unwrap().string("id");
    let began = crate::probe::read(&session.client).await.unwrap();
    let began = culprit_core::format::epoch_seconds(&began.now).unwrap();
    let mut foreign_spec = local_spec();
    foreign_spec.database = Some("yugabyte".into());
    let foreign = YugabyteProvider::new(&foreign_spec).unwrap().connector.connect(INTERACTIVE).await.unwrap();
    foreign.batch_execute(&tagged("SET work_mem='64kB'; SET temp_file_limit=0")).await.unwrap();
    assert!(foreign.simple_query(fail).await.is_err());
    session.client.batch_execute(&tagged("SET work_mem='64kB'; SET temp_file_limit=0")).await.unwrap();
    for _ in 0..2 {
        let error = session.client.simple_query(fail).await.unwrap_err();
        assert_eq!(error.as_db_error().unwrap().code().code(), "53400");
    }
    // The collector's shared buffer refresh is asynchronous; wait for these owned records, not a fixed long delay.
    let options = SnapshotOptions { window_minutes: Some(60), ..Default::default() };
    let mut observed = None;
    for _ in 0..50 {
        let p = crate::probe::read(&session.client).await.unwrap();
        let h = crate::terminations::read(&session, &p, &options, 1000).await.unwrap();
        if h.events
            .iter()
            .filter(|e| {
                e.reason == culprit_core::TerminationReason::TempFileLimit
                    && culprit_core::format::epoch_seconds(&e.ended_at).unwrap() >= began
            })
            .count()
            >= 2
        {
            observed = Some((p, h));
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    let (probe, local) = observed.expect("owned temporary-file-limit failures were not retained");
    assert!(!local.partial);
    assert!(
        local
            .events
            .iter()
            .filter(|e| e.query_id.as_deref() == Some(query_id.as_str())
                && culprit_core::format::epoch_seconds(&e.ended_at).unwrap() >= began)
            .count()
            >= 2
    );
    assert!(local.events.iter().all(|e| e.node == probe.node));
    let encoded = serde_json::to_string(&local).unwrap();
    assert!(
        !encoded.contains("PRIVATE-TERMINATION-SQL")
            && !encoded.contains("query_text")
            && !encoded.contains("backend_pid")
    );
    let expected=query_one(&session.client,&format!("SELECT count(*)::text AS n FROM pg_catalog.yb_terminated_queries WHERE databasename=current_database() AND query_end_time>='{}'::timestamptz AND query_end_time<'{}'::timestamptz",local.requested_start,local.requested_end)).await.unwrap().unwrap().string("n").parse::<usize>().unwrap();
    assert_eq!(local.events.len(), expected);
    let foreign_records=query_one(&session.client,&format!("SELECT count(*)::text AS n FROM pg_catalog.yb_terminated_queries WHERE databasename='yugabyte' AND query_end_time>='{}'::timestamptz AND query_end_time<'{}'::timestamptz",local.requested_start,local.requested_end)).await.unwrap().unwrap().string("n").parse::<usize>().unwrap();
    assert!(foreign_records > 0, "the database filter needs real foreign-database counterexamples");
    assert!(crate::terminations::read(&session, &probe, &options, 1).await.unwrap().partial);
    session.client.batch_execute(&tagged("SET yb_enable_global_views=on")).await.unwrap();
    let mut global_probe = crate::probe::read(&session.client).await.unwrap();
    global_probe.now = probe.now.clone();
    let global = crate::terminations::read(&session, &global_probe, &options, 1000).await.unwrap();
    assert_eq!(global.events, local.events);
    assert_eq!(global.scope, culprit_core::CounterScope::Cluster);
    assert!(!global.partial);
    let pid = query_one(&session.client, "SELECT pg_backend_pid()::text AS pid").await.unwrap().unwrap().string("pid");
    let role = format!("culprit_termination_reader_{pid}");
    session.client.batch_execute(&tagged("SET default_transaction_read_only=off")).await.unwrap();
    session
        .client
        .batch_execute(&tagged(&format!("CREATE ROLE {role} LOGIN; ALTER ROLE {role} SET temp_file_limit=0; ALTER ROLE {role} SET yb_enable_global_views=on")))
        .await
        .unwrap();
    let own = async {
        let mut spec = local_spec();
        spec.user = Some(role.clone());
        let reader = YugabyteProvider::new(&spec)?;
        let own = reader.connector.connect_observed(INTERACTIVE).await?;
        let p = crate::probe::read(&own.client).await?;
        let before = crate::terminations::read(&own, &p, &options, 1000).await?;
        own.client.batch_execute(&tagged("SET work_mem='64kB'")).await.map_err(culprit_postgres::wire::map_error)?;
        if own.client.simple_query(fail).await.is_ok() {
            return Err(Error::invalid("The limited-role sort unexpectedly completed"));
        }
        let mut retained = None;
        for _ in 0..50 {
            let p = crate::probe::read(&own.client).await?;
            let h = crate::terminations::read(&own, &p, &options, 1000).await?;
            if !h.events.is_empty() {
                retained = Some(h);
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        session
            .client
            .batch_execute(&tagged(&format!("GRANT pg_read_all_stats TO {role}")))
            .await
            .map_err(culprit_postgres::wire::map_error)?;
        let widened = crate::terminations::read(&own, &p, &options, 1000).await;
        let full = crate::probe::read(&own.client).await?;
        let monitored = crate::terminations::read(&own, &full, &options, 1000).await?;
        session
            .client
            .batch_execute(&tagged(&format!("REVOKE pg_read_all_stats FROM {role}")))
            .await
            .map_err(culprit_postgres::wire::map_error)?;
        let narrowed = crate::terminations::read(&own, &full, &options, 1000).await;
        Ok::<_, Error>((before, retained, monitored, widened, narrowed))
    }
    .await;
    session.client.batch_execute(&tagged(&format!("DROP ROLE {role}"))).await.unwrap();
    let (before, retained, monitored, widened, narrowed) = own.unwrap();
    assert!(before.events.is_empty(), "foreign roles' records entered a new login's native visibility");
    assert_eq!(before.scope, culprit_core::CounterScope::Node);
    assert!(before.notes.iter().any(|n| n.contains("Global termination reads require")));
    assert_eq!(retained.unwrap().events.len(), 1);
    assert!(
        monitored.events.len() > local.events.len(),
        "an effective monitoring grant should include other logins' records and the new own record"
    );
    assert_eq!(monitored.scope, culprit_core::CounterScope::Cluster);
    assert_eq!(widened.unwrap_err().code, culprit_core::ErrorCode::PermissionDenied);
    assert_eq!(narrowed.unwrap_err().code, culprit_core::ErrorCode::PermissionDenied);
}

#[tokio::test]
async fn invalid_retained_plan_identifiers_are_rejected_before_connecting() {
    let provider = YugabyteProvider::new(&ConnectionSpec {
        engine: Some(Engine::Yugabyte),
        host: "invalid.example".into(),
        ..Default::default()
    })
    .unwrap();
    for (query, plan) in [("1; SELECT 1", "2"), ("1", ""), ("1", "9223372036854775808")] {
        assert_eq!(provider.stored_plan(query, plan).await.unwrap_err().code, culprit_core::ErrorCode::InvalidInput);
    }
}

#[tokio::test]
#[ignore = "requires dev/yugabyte and CULPRIT_YSQL_TEST_PORT"]
async fn live_qpm_history_retains_independent_periods_and_redacted_plans() {
    let _fixture = LIVE.lock().await;
    let provider = YugabyteProvider::new(&local_spec()).unwrap();
    let admin = provider.connector.connect(INTERACTIVE).await.unwrap();
    let statement = "SELECT sum(total),max(id) FROM public.orders WHERE id=42 /* native_plan_history_test */";
    // Synthetic application reads intentionally carry no collector tag, so the native engine records both plans.
    admin.simple_query(statement).await.unwrap();
    for sql in ["SET enable_indexscan=off", "SET enable_bitmapscan=off"] {
        admin.batch_execute(&tagged(sql)).await.unwrap();
    }
    admin.simple_query(statement).await.unwrap();
    for sql in ["RESET enable_indexscan", "RESET enable_bitmapscan"] {
        admin.batch_execute(&tagged(sql)).await.unwrap();
    }
    let snapshot = provider
        .snapshot("native-qpm-test", &SnapshotOptions { top_queries: 1000, plans: 0, ..Default::default() })
        .await
        .unwrap();
    let q = snapshot.queries().iter().find(|q| q.text.contains("native_plan_history_test")).unwrap();
    assert!(q.plans.len() >= 2, "two engine-selected plans should be retained: {:?}", snapshot.notes);
    let history = q.plan_history.as_ref().unwrap();
    assert_eq!(history.kind, culprit_core::PlanHistoryKind::Cache);
    assert!(history.captured_plan.is_none() && q.plan.is_none(), "plans=0 retains metadata without fetching bodies");
    assert!(q.workload_plans().is_empty());
    assert!(q.plans.iter().all(|p| p.executions > 0.0 && p.first_seen.is_some() && p.last_seen.is_some()));
    for variant in &q.plans {
        let plan = provider.stored_plan(&q.id, &variant.id).await.unwrap().unwrap();
        assert_eq!(plan.engine, Engine::Yugabyte);
        assert_eq!(plan.kind, culprit_core::PlanKind::Estimated);
        assert!(plan.facts().constants_redacted);
        assert!(plan.nodes().iter().all(|n| n.act_rows.is_none() && n.act_time_ms.is_none()));
        assert!(crate::plan::reparse(&plan).unwrap().facts().constants_redacted);
    }
    assert!(provider.stored_plan(&q.id, "-9223372036854775808").await.is_ok(), "signed ids reach the native lookup");
    admin.batch_execute(&tagged("SET default_transaction_read_only=off")).await.unwrap();
    let pid = query_one(&admin, "SELECT pg_backend_pid()::text AS pid").await.unwrap().unwrap().string("pid");
    let role = format!("culprit_qpm_reader_{pid}");
    admin.batch_execute(&tagged(&format!("CREATE ROLE {role} LOGIN NOINHERIT"))).await.unwrap();
    let mut limited_spec = local_spec();
    limited_spec.user = Some(role.clone());
    let limited = YugabyteProvider::new(&limited_spec).unwrap();
    let hidden = limited.stored_plan(&q.id, &q.plans[0].id).await;
    admin.batch_execute(&tagged(&format!("GRANT pg_read_all_stats TO {role}"))).await.unwrap();
    let unprivileged_member = limited.stored_plan(&q.id, &q.plans[0].id).await;
    admin.batch_execute(&tagged(&format!("ALTER ROLE {role} INHERIT"))).await.unwrap();
    let visible = limited.stored_plan(&q.id, &q.plans[0].id).await;
    admin.batch_execute(&tagged(&format!("DROP ROLE {role}"))).await.unwrap();
    assert_eq!(hidden.unwrap_err().code, culprit_core::ErrorCode::PermissionDenied);
    assert_eq!(unprivileged_member.unwrap_err().code, culprit_core::ErrorCode::PermissionDenied);
    assert!(
        visible.unwrap().unwrap().facts().constants_redacted,
        "the monitoring grant permits safe cache reads without superuser"
    );
    // Collector statements opt out of QPM without changing the server's tracking configuration.
    let collector =
        "SELECT sum(id)+count(*)+max(customer_id) FROM public.orders WHERE customer_id=72 /* native_qpm_skip_test */";
    admin.simple_query(&tagged(collector)).await.unwrap();
    let tracked = query_one(
        &admin,
        "SELECT count(*)::text AS count FROM yb_pg_stat_plans p JOIN pg_stat_statements s
        ON p.queryid=s.queryid AND p.dbid=s.dbid AND p.userid=s.userid
        WHERE s.query LIKE '%native_qpm_skip_test%' AND s.query NOT LIKE '%SELECT count(*)::text AS count%'",
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(tracked.string("count"), "0");
    let session = provider.connector.connect_observed(culprit_postgres::wire::COLLECTOR).await.unwrap();
    let probe = crate::probe::read(&session.client).await.unwrap();
    let mut workload = snapshot.workload.clone().unwrap();
    workload.queries.retain(|entry| entry.id == q.id);
    let mut coverage = culprit_core::Coverage::default();
    crate::collect::capture_plans(&provider, &session, &probe, &mut workload, 1, &mut coverage).await;
    let captured = workload.queries[0].plan_history.as_ref().unwrap();
    assert!(captured.captured_plan.is_some(), "a retained body should be captured alongside the current estimate");
    assert!(workload.queries[0].plan.is_some());
    session.client.batch_execute(&tagged("SET yb_enable_global_views=on")).await.unwrap();
    let global_probe = crate::probe::read(&session.client).await.unwrap();
    crate::history::collect(&session, &global_probe, &mut workload, &mut coverage).await;
    assert!(workload.queries[0].plan_history.as_ref().unwrap().source.contains("cluster"));
    let local: std::collections::BTreeMap<_, _> = q.plans.iter().map(|p| (&p.id, p.executions)).collect();
    let global: std::collections::BTreeMap<_, _> =
        workload.queries[0].plans.iter().map(|p| (&p.id, p.executions)).collect();
    assert_eq!(local, global, "one-node global views retain the same cache counts");
}

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

#[tokio::test]
#[ignore = "requires dev/yugabyte and CULPRIT_YSQL_TEST_PORT"]
async fn live_ash_history_has_its_own_weights_period_bounds_and_role_scope() {
    let _fixture = LIVE.lock().await;
    let provider = YugabyteProvider::new(&local_spec()).unwrap();
    let session = provider.connector.connect_observed(COLLECTOR).await.unwrap();
    // Disposable application stimulus: a long enough query to cross several native sampling ticks.
    session.client.simple_query("SELECT pg_sleep(2.2),891 AS native_ash_history_test").await.unwrap();
    let probe = crate::probe::read(&session.client).await.unwrap();
    let options = SnapshotOptions { window_minutes: Some(60), ..Default::default() };
    let local = crate::ash::read(&session, &probe, &options, 10_000).await.unwrap();
    assert!(local.period.observations >= 2.0 && local.period.weighted_observations >= local.period.observations);
    let query = query_one(
        &session.client,
        "SELECT queryid::text AS id FROM pg_stat_statements
        WHERE query LIKE 'SELECT pg_sleep%' AND query LIKE '%native_ash_history_test%'
        AND userid=(SELECT oid FROM pg_roles WHERE rolname=current_user) LIMIT 1",
    )
    .await
    .unwrap()
    .unwrap()
    .string("id");
    assert!(
        local.events.iter().any(|e| e.wait_event == "PgSleep"
            && e.component == "YSQL"
            && e.query_id.as_deref() == Some(query.as_str()))
    );
    assert!(local.events.iter().all(|e| e.observations > 0.0 && e.weighted_observations > 0.0));
    assert_eq!(local.period.scope, culprit_core::CounterScope::Node);
    assert!(local.events.iter().all(|e| e.node == probe.node));
    assert!(!local.period.partial);
    let short = crate::ash::read(&session, &probe, &options, 1).await.unwrap();
    assert!(short.period.partial && short.period.observations == 1.0);
    session.client.batch_execute(&tagged("SET yb_enable_global_views=on")).await.unwrap();
    let mut global_probe = crate::probe::read(&session.client).await.unwrap();
    global_probe.now = probe.now.clone(); // Compare the same closed range, despite work since the first read.
    let global = crate::ash::read(&session, &global_probe, &options, 10_000).await.unwrap();
    assert_eq!(global.period.scope, culprit_core::CounterScope::Cluster);
    assert!(global.events.iter().all(|e| e.node == probe.node));
    assert_eq!(global.period.observations, local.period.observations, "one-node views retain the same observations");
    assert_eq!(global.period.weighted_observations, local.period.weighted_observations);
    assert!(!global.period.partial);
    session.client.batch_execute(&tagged("SET default_transaction_read_only=off")).await.unwrap();
    let pid = query_one(&session.client, "SELECT pg_backend_pid()::text AS pid").await.unwrap().unwrap().string("pid");
    let role = format!("culprit_ash_reader_{pid}");
    session.client.batch_execute(&tagged(&format!("CREATE ROLE {role} LOGIN"))).await.unwrap();
    let own=async {
        let mut spec=local_spec();spec.user=Some(role.clone());
        let reader=YugabyteProvider::new(&spec)?;
        let own=reader.connector.connect_observed(COLLECTOR).await?;
        own.client.simple_query("SELECT pg_sleep(2.2) /* native_ash_role_test */").await.map_err(culprit_postgres::wire::map_error)?;
        let p=crate::probe::read(&own.client).await?;
        let history=crate::ash::read(&own,&p,&options,10_000).await?;
        let id=p.statement_role.ok_or_else(||culprit_core::Error::invalid("The test role unexpectedly has full statistics access"))?;
        let expected=query_one(&own.client,&format!(
            "SELECT count(*)::text AS n FROM pg_catalog.yb_active_session_history
            WHERE ysql_dbid={} AND ysql_userid={id} AND sample_time>='{}'::timestamptz AND sample_time<'{}'::timestamptz",
            p.oid,history.period.requested_start,history.period.requested_end
        )).await?.unwrap().string("n").parse::<f64>().unwrap();
        session.client.batch_execute(&tagged(&format!("GRANT pg_read_all_stats TO {role}"))).await.map_err(culprit_postgres::wire::map_error)?;
        let stale=crate::probe::read(&own.client).await?;
        if stale.statement_role.is_some(){return Err(culprit_core::Error::invalid("The test monitoring grant did not apply"));}
        session.client.batch_execute(&tagged(&format!("REVOKE pg_read_all_stats FROM {role}"))).await.map_err(culprit_postgres::wire::map_error)?;
        let denied_activity=crate::ash::read(&own,&stale,&options,10_000).await;
        // A stale probe must not enter QPM's native foreign-role redaction path, which crashes this release.
        let denied_cache=crate::history::stored(&own,&stale,"1","1").await;
        Ok::<_,culprit_core::Error>((history,expected,id,denied_activity,denied_cache))
    }.await;
    session.client.batch_execute(&tagged(&format!("DROP ROLE {role}"))).await.unwrap();
    let (history, expected, id, denied_activity, denied_cache) = own.unwrap();
    assert!(history.period.source.ends_with(&format!("role {id} only")));
    assert!(history.period.observations >= 2.0);
    assert_eq!(history.period.observations, expected, "other roles' observations must not enter this history");
    assert_eq!(denied_activity.unwrap_err().code, culprit_core::ErrorCode::PermissionDenied);
    assert_eq!(denied_cache.unwrap_err().code, culprit_core::ErrorCode::PermissionDenied);
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
    assert!(snapshot.sources.iter().any(|source| source.source == culprit_core::source_id::DOCDB_PLAN_HISTORY
        && source.state == culprit_core::SourceState::Unavailable));
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

#[tokio::test]
#[ignore = "requires dev/yugabyte and CULPRIT_YSQL_TEST_PORT"]
async fn live_deferred_checks_and_foreign_keys_are_collected_until_validation() {
    let _fixture = LIVE.lock().await;
    let provider = YugabyteProvider::new(&local_spec()).unwrap();
    let admin = provider.connector.connect(INTERACTIVE).await.unwrap();
    admin.batch_execute(&tagged("SET default_transaction_read_only=off")).await.unwrap();
    let pid = query_one(&admin, "SELECT pg_backend_pid()::text AS pid").await.unwrap().unwrap().string("pid");
    let schema = format!("culprit_native_constraints_{pid}");
    admin.batch_execute(&tagged(&format!("CREATE SCHEMA {schema}"))).await.unwrap();
    let observed = async {
        use culprit_postgres::wire::map_error;
        for sql in [
            format!("CREATE TABLE {schema}.parent(id integer PRIMARY KEY)"),
            format!("CREATE TABLE {schema}.child(id integer PRIMARY KEY, parent_id integer, amount integer)"),
            format!("INSERT INTO {schema}.child VALUES (1,999,-1)"),
            format!("ALTER TABLE {schema}.child ADD CONSTRAINT \"Positive amount\" CHECK (amount>=0) NOT VALID"),
            format!("ALTER TABLE {schema}.child ADD CONSTRAINT \"Parent FK\" FOREIGN KEY (parent_id) REFERENCES {schema}.parent(id) NOT VALID"),
        ] {
            admin.batch_execute(&tagged(&sql)).await.map_err(map_error)?;
        }
        let options = SnapshotOptions { plans: 0, ..Default::default() };
        let before = provider.snapshot("deferred-constraint-test", &options).await?;
        let invalid_rows = admin.batch_execute(&tagged(&format!("ALTER TABLE {schema}.child VALIDATE CONSTRAINT \"Positive amount\""))).await.is_err();
        admin.batch_execute(&tagged(&format!("DELETE FROM {schema}.child"))).await.map_err(map_error)?;
        for name in ["Positive amount", "Parent FK"] {
            admin.batch_execute(&tagged(&format!("ALTER TABLE {schema}.child VALIDATE CONSTRAINT {}", Engine::Yugabyte.dialect().quote(name)))).await.map_err(map_error)?;
        }
        let after = provider.snapshot("validated-constraint-test", &options).await?;
        Ok::<_, Error>((before, after, invalid_rows))
    }.await;
    admin.batch_execute(&tagged(&format!("DROP SCHEMA {schema} CASCADE"))).await.unwrap();
    let (before, after, invalid_rows) = observed.unwrap();
    assert!(invalid_rows, "NOT VALID must not be treated as proof that existing rows satisfy the constraint");
    let constraints: Vec<_> = before.constraints.iter().filter(|c| c.schema == schema).collect();
    assert_eq!(constraints.len(), 2);
    assert!(constraints.iter().any(|c| c.kind == culprit_core::ConstraintKind::Check && c.name == "Positive amount"));
    assert!(constraints.iter().any(|c| c.kind == culprit_core::ConstraintKind::ForeignKey && c.name == "Parent FK"));
    assert!(constraints.iter().all(|c| !c.disabled && !c.inheritance_child));
    assert!(
        before.sources.iter().any(
            |s| s.source == culprit_core::source_id::CONSTRAINTS && s.state == culprit_core::SourceState::Collected
        )
    );
    assert!(
        !after.constraints.iter().any(|c| c.schema == schema),
        "validated constraints must disappear on the next scan"
    );
}
