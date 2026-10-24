//! Opt-in verification against dev/yugabyte/cluster.compose.yml, never an application database.
use super::*;
use culprit_core::{CounterScope, Coverage, SourceState};
use culprit_postgres::wire::{INTERACTIVE, query_one, tagged};
use std::collections::BTreeSet;

static CLUSTER: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
const WORK: &str = "/* native-cluster-workload */ SELECT sum(id) FROM public.cluster_probe WHERE id>0";

fn spec(port: u16, user: &str) -> ConnectionSpec {
    assert_eq!(
        std::env::var("CULPRIT_YSQL_CLUSTER_TEST").as_deref(),
        Ok("1"),
        "Start and seed dev/yugabyte/cluster.compose.yml, then set CULPRIT_YSQL_CLUSTER_TEST=1"
    );
    let mut spec = ConnectionSpec {
        engine: Some(Engine::Yugabyte),
        host: "127.0.0.1".into(),
        port: Some(port),
        user: Some(user.into()),
        database: Some("culprit_cluster".into()),
        environment: culprit_core::Environment::Local,
        ..Default::default()
    };
    spec.options.insert("sslmode".into(), "disable".into());
    spec
}

async fn reader() -> culprit_postgres::wire::ObservedSession {
    Connector::new(&spec(55436, "culprit_cluster_monitor")).unwrap().connect_observed(COLLECTOR).await.unwrap()
}

async fn workload(session: &culprit_postgres::wire::ObservedSession) -> culprit_core::Workload {
    let p = probe::read(&session.client).await.unwrap();
    assert!(p.cluster && p.global);
    let mut coverage = Coverage::default();
    crate::workload::read(session, &p, 100, &mut coverage).await.unwrap()
}

#[tokio::test]
#[ignore = "requires the seeded disposable three-node cluster and CULPRIT_YSQL_CLUSTER_TEST=1"]
async fn live_cluster_aggregation_resets_resources_and_pooled_session_guards() {
    let _fixture = CLUSTER.lock().await;
    let mut nodes = Vec::new();
    for port in [55436, 55437, 55438] {
        let session = Connector::new(&spec(port, "yugabyte")).unwrap().connect_observed(COLLECTOR).await.unwrap();
        session
            .client
            .batch_execute(&tagged("SET default_transaction_read_only=off; SELECT pg_stat_statements_reset()"))
            .await
            .unwrap();
        nodes.push(session);
    }
    let monitor = reader().await;
    let inventory = global::nodes(&monitor).await.unwrap();
    assert_eq!(inventory.len(), 3);
    let mut ids = BTreeSet::new();
    for (i, node) in nodes.iter().enumerate() {
        for _ in 0..=i {
            node.client.simple_query(WORK).await.unwrap();
        }
        let row = query_one(&node.client, "SELECT queryid::text AS id,calls::text AS calls FROM pg_stat_statements WHERE query LIKE '%native-cluster-workload%' AND query NOT LIKE '%/* culprit */%'").await.unwrap().unwrap();
        assert_eq!(row.string("calls").parse::<usize>().unwrap(), i + 1);
        ids.insert(row.string("id"));
    }
    assert_eq!(ids.len(), 1, "identical database/role SQL must have one native query identity");
    let id = ids.first().unwrap();
    let before = workload(&monitor).await;
    assert_eq!(before.queries.iter().find(|q| &q.id == id).unwrap().executions, 6.0);
    let epochs = before.counter_set.as_ref().unwrap();
    assert_eq!(epochs.scope, CounterScope::Cluster);
    assert_eq!(epochs.members.keys().cloned().collect::<BTreeSet<_>>(), inventory);
    assert!(epochs.epochs_known());
    assert!(before.window_start.is_none(), "independent reset dates are not one common history window");
    assert_eq!(workload(&monitor).await.counter_set, before.counter_set);
    nodes[1].client.batch_execute(&tagged("SELECT pg_stat_statements_reset()")).await.unwrap();
    nodes[1].client.simple_query(WORK).await.unwrap();
    let after = workload(&monitor).await;
    let later = after.counter_set.as_ref().unwrap();
    assert!(later.epochs_known());
    assert_eq!(epochs.members.keys().collect::<Vec<_>>(), later.members.keys().collect::<Vec<_>>());
    assert_eq!(epochs.members.iter().filter(|(node, at)| later.members.get(*node) != Some(*at)).count(), 1);
    assert_eq!(after.queries.iter().find(|q| &q.id == id).unwrap().executions, 5.0);
    let identity =
        culprit_core::DatabaseIdentity::server(Engine::Yugabyte, "culprit_cluster", "127.0.0.1", 55436, None);
    let measurement = |w: &culprit_core::Workload| {
        culprit_core::MeasurementContext::workload(identity.clone(), w, w.window_end.as_deref().unwrap(), None)
    };
    assert_eq!(measurement(&before).counter_transition(&measurement(&after)), culprit_core::CounterTransition::Reset);
    let resources = resources::read(&monitor, 1000).await.unwrap();
    assert!(!resources.partial && resources.nodes_expected == Some(3));
    assert_eq!(resources.nodes.iter().map(|n| n.node.clone()).collect::<BTreeSet<_>>(), inventory);
    assert!(resources.nodes.iter().all(|n| n.available));

    // A pooled physical backend must never be signaled or planned as a logical client.
    let pool = Connector::new(&spec(55439, "culprit_cluster_app")).unwrap().connect(INTERACTIVE).await.unwrap();
    pool.batch_execute(&tagged("SET application_name='culprit_cluster_pooled_client'")).await.unwrap();
    let provider = YugabyteProvider::new(&spec(55436, "culprit_cluster_monitor")).unwrap();
    let observe = async {
        let mut found = None;
        for _ in 0..40 {
            let live = provider.live().await.unwrap();
            if let Some(session) = live
                .sessions
                .into_iter()
                .find(|s| s.application.as_deref() == Some("culprit_cluster_pooled_client") && s.state == "active")
            {
                found = Some(session);
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        let session = found.expect("pooled application backend was not visible through global views");
        let progress = provider.query_progress(&session.id, true).await.unwrap();
        assert!(progress.running && progress.plan.is_none() && progress.phase.is_none());
        assert!(progress.note.as_deref().unwrap().contains("reusable Connection Manager worker"));
        // Connect directly to that same node, so a wrong-node guard cannot hide a missing worker guard.
        let local = YugabyteProvider::new(&spec(55438, "yugabyte")).unwrap();
        assert_eq!(
            local.interrupt_session(&session.id, culprit_core::SessionAction::Cancel).await.unwrap_err().code,
            culprit_core::ErrorCode::NotFound
        );
    };
    let sleep = tagged("SELECT pg_sleep(4)");
    let (done, ()) = tokio::join!(pool.batch_execute(&sleep), observe);
    done.unwrap();
    assert!(query_one(&pool, "SELECT 1 AS still_connected").await.unwrap().is_some());
    let snapshot =
        provider.snapshot("cluster-verification", &SnapshotOptions { plans: 0, ..Default::default() }).await.unwrap();
    assert_eq!(snapshot.server.engine, Engine::Yugabyte);
    assert!(snapshot.server.database_size_bytes.is_none());
    assert!(snapshot.tables.iter().all(|t| t.dead_rows.is_none() && t.xid_age.is_none()));
    assert_eq!(snapshot.workload.as_ref().unwrap().counter_set.as_ref().unwrap().members.len(), 3);
    assert!(snapshot.server.has(culprit_core::Capability::IndexUsage));
    assert!(snapshot.indexes.iter().all(|i| i.reads.is_some() && i.counters_since.is_none()));
    assert!(
        snapshot.operations.activity_history.as_ref().is_some_and(|h| !h.period.partial && h.period.nodes.len() == 3)
    );
    assert!(snapshot.operations.termination_history.as_ref().is_some_and(|h| !h.partial && h.nodes.len() == 3));
}

struct PausedNode(String);

impl Drop for PausedNode {
    fn drop(&mut self) {
        // Resume this fixture even if an assertion unwinds. Never select containers by a loose name prefix.
        let result = std::process::Command::new("docker").args(["unpause", &self.0]).output();
        if !result.is_ok_and(|r| r.status.success()) {
            eprintln!("Could not resume owned YugabyteDB fixture {}", self.0);
        }
    }
}

fn pause_fixture_node() -> PausedNode {
    let result = std::process::Command::new("docker")
        .args([
            "ps",
            "--filter",
            "label=com.docker.compose.project=culprit-yugabyte-cluster",
            "--filter",
            "label=com.docker.compose.service=yb3",
            "--filter",
            "label=dev.culprit.fixture=yugabyte-cluster",
            "--format",
            "{{.ID}}",
        ])
        .output()
        .unwrap();
    assert!(result.status.success());
    let id = String::from_utf8(result.stdout).unwrap().trim().to_string();
    assert!(id.len() >= 12 && id.bytes().all(|b| b.is_ascii_hexdigit()), "exactly one owned fixture must match");
    let guard = PausedNode(id);
    assert!(std::process::Command::new("docker").args(["pause", &guard.0]).output().unwrap().status.success());
    guard
}

#[tokio::test]
#[ignore = "pauses only the labelled disposable cluster node; requires CULPRIT_YSQL_CLUSTER_TEST=1"]
async fn live_cluster_single_session_progress_does_not_contact_an_unrelated_failed_node() {
    let _fixture = CLUSTER.lock().await;
    let monitor = reader().await;
    let active = Connector::new(&spec(55436, "culprit_cluster_app")).unwrap().connect(INTERACTIVE).await.unwrap();
    let identity=query_one(&active,&format!("SELECT replace(yb_get_local_tserver_uuid()::text,'-','') AS node,pid::text,{} AS started FROM pg_stat_activity WHERE pid=pg_backend_pid()",probe::timestamp("backend_start"))).await.unwrap().unwrap();
    let id = format!("{}/{}/{}", identity.string("node"), identity.string("pid"), identity.string("started"));
    active.batch_execute(&tagged("SET default_transaction_read_only=off")).await.unwrap();
    active.batch_execute(&tagged("CREATE TEMP TABLE culprit_prune_probe(id int)")).await.unwrap();
    let paused = pause_fixture_node();
    let before = monitor.notices();
    let observe = async {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let (progress, _) = crate::progress::read_session(&monitor, &id, false).await.unwrap();
        assert!(progress.running);
        assert_eq!(
            monitor.notices(),
            before,
            "a single-node session observation must prune unrelated global-view nodes"
        );
    };
    let sql = tagged("SELECT pg_sleep(4)");
    let (done, ()) = tokio::join!(active.batch_execute(&sql), observe);
    done.unwrap();
    // Check the command-progress view's own UUID predicate as well as its current-activity identity subquery.
    // No rows are written; holding COPY open provides a stable command phase without a timing-dependent DDL.
    let copy = active.copy_in::<_, &'static [u8]>(&tagged("COPY culprit_prune_probe FROM STDIN")).await.unwrap();
    tokio::pin!(copy);
    let (progress, _) = crate::progress::read_session(&monitor, &id, false).await.unwrap();
    assert!(progress.phase.as_deref().is_some_and(|p| p.starts_with("COPY FROM PIPE: IN PROGRESS")));
    assert_eq!(monitor.notices(), before, "command progress must also prune unrelated global-view nodes");
    assert_eq!(copy.as_mut().finish().await.unwrap(), 0);
    drop(paused);
}

#[tokio::test]
#[ignore = "pauses only the labelled disposable cluster node; requires CULPRIT_YSQL_CLUSTER_TEST=1"]
async fn live_cluster_skipped_node_withdraws_complete_counters_and_marks_native_history_partial() {
    let _fixture = CLUSTER.lock().await;
    let monitor = reader().await;
    assert_eq!(global::nodes(&monitor).await.unwrap().len(), 3);
    let connected = Connector::new(&spec(55436, "yugabyte")).unwrap().connect(INTERACTIVE).await.unwrap();
    connected.simple_query(WORK).await.unwrap();
    let healthy = workload(&monitor).await;
    assert!(healthy.counter_set.as_ref().unwrap().epochs_known());
    let provider = YugabyteProvider::new(&spec(55436, "culprit_cluster_monitor")).unwrap();
    let mut snapshot = provider
        .snapshot("cluster-failure-verification", &SnapshotOptions { plans: 0, ..Default::default() })
        .await
        .unwrap();
    let paused = pause_fixture_node();
    let observed = crate::deadline(std::time::Duration::from_secs(45), async {
        let began = std::time::Instant::now();
        let before = monitor.notices();
        let p = probe::read(&monitor.client).await?;
        let mut coverage = Coverage::default();
        let mut workload = crate::workload::read(&monitor, &p, 100, &mut coverage).await.unwrap();
        eprintln!("Cluster failure workload read: {:?}", began.elapsed());
        assert!(!workload.counter_set.as_ref().unwrap().complete);
        assert!(!workload.counter_set.as_ref().unwrap().epochs_known());
        assert!(workload.window_start.is_none() && workload.window_hours == 0.0);
        assert!(monitor.notices() > before, "the native skipped-node WARNING must reach the reader");
        assert!(
            coverage.outcomes().iter().any(|o| o.source == sources::WORKLOAD.name && o.state == SourceState::Partial)
        );
        snapshot.indexes = crate::catalog::indexes(&monitor.client, &p).await?;
        crate::index_usage::read(&monitor, &p, &mut snapshot, &mut coverage).await;
        assert!(snapshot.indexes.iter().all(|i| i.reads.is_none()));
        assert!(
            coverage
                .outcomes()
                .iter()
                .any(|o| o.source == sources::INDEX_USAGE.name && o.state == SourceState::Partial)
        );
        let options = SnapshotOptions { plans: 0, window_minutes: Some(60), ..Default::default() };
        let activity = crate::ash::read(&monitor, &p, &options, 10_000).await?;
        eprintln!("Cluster failure activity read: {:?}", began.elapsed());
        assert!(activity.period.partial);
        let terminations = crate::terminations::read(&monitor, &p, &options, 1000).await?;
        eprintln!("Cluster failure termination read: {:?}", began.elapsed());
        assert!(terminations.partial);
        crate::history::collect(&monitor, &p, &mut workload, &mut coverage).await;
        eprintln!("Cluster failure plan-history read: {:?}", began.elapsed());
        assert!(
            coverage
                .outcomes()
                .iter()
                .any(|o| o.source == sources::PLAN_HISTORY.name && o.state == SourceState::Partial)
        );
        let resources = crate::resources::read(&monitor, 1000).await?;
        eprintln!("Cluster failure resource read: {:?}", began.elapsed());
        assert!(resources.partial);
        assert!(resources.nodes.iter().any(|n| !n.available));
        Ok::<_, culprit_core::Error>(())
    })
    .await;
    drop(paused);
    observed.unwrap();
    let restored = workload(&reader().await).await;
    assert!(restored.counter_set.as_ref().unwrap().epochs_known());
}
