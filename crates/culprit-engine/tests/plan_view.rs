#[path = "support/plan_view_cases.rs"]
mod inputs;

use async_trait::async_trait;
use culprit_core::{
    Engine, Error, ExplainMode, LiveState, Plan, Provider, Result, ResultSet, ServerInfo, Snapshot, SnapshotOptions,
};

#[derive(Clone)]
struct PlanReader(Plan);

#[async_trait]
impl Provider for PlanReader {
    fn engine(&self) -> Engine {
        Engine::Mssql
    }
    async fn probe(&self) -> Result<ServerInfo> {
        Err(Error::invalid("not needed"))
    }
    async fn snapshot(&self, _: &str, _: &SnapshotOptions) -> Result<Snapshot> {
        Err(Error::invalid("not needed"))
    }
    async fn explain(&self, _: &str, _: ExplainMode) -> Result<Plan> {
        Ok(self.0.clone())
    }
    async fn live(&self) -> Result<LiveState> {
        Err(Error::invalid("not needed"))
    }
    async fn run(&self, _: &str, _: usize) -> Result<ResultSet> {
        Err(Error::invalid("not needed"))
    }
    async fn stored_plan(&self, _: &str, _: &str) -> Result<Option<Plan>> {
        Ok(Some(self.0.clone()))
    }
    async fn last_actual_plan(&self, _: &str, _: Option<&str>) -> Result<Plan> {
        Ok(self.0.clone())
    }
    async fn query_progress(&self, session: &str, _: bool) -> Result<culprit_core::QueryProgress> {
        let mut progress = culprit_core::QueryProgress::idle(session, Engine::Mssql, "2026-10-10T00:00:00Z".into());
        progress.running = true;
        progress.plan = Some(self.0.clone());
        Ok(progress)
    }
}

#[tokio::test]
async fn workspace_plan_endpoints_project_the_selected_variant_and_execution_state() {
    let home = std::env::temp_dir().join(format!("culprit-plan-view-{}", std::process::id()));
    let mut case = inputs::cases().into_iter().find(|c| c.name == "slow-variant").unwrap();
    let query = case.query.take().unwrap();
    let query_id = query.id.clone();
    // Deliberately stale input: all endpoints must replace it with their own context.
    case.plan.view.as_mut().unwrap().cost_note = Some("obsolete".into());
    let reader = PlanReader(case.plan.clone());
    let ws = culprit_engine::Workspace::open_in(&home)
        .with_provider(Engine::Mssql, std::sync::Arc::new(move |_| Ok(Box::new(reader.clone()))));
    ws.add_connection(culprit_engine::NewConnection {
        name: "replayed".into(),
        input: "mssql://reader@replay.invalid/culprit_demo".into(),
        auth: Some(culprit_core::AuthKind::None),
        environment: culprit_core::Environment::Development,
        ..Default::default()
    })
    .unwrap();
    let mut snapshot: Snapshot = serde_json::from_str(include_str!("../../../fixtures/mssql-demo.json")).unwrap();
    snapshot.connection = "replayed".into();
    snapshot.workload.as_mut().unwrap().queries = vec![query];
    let report = culprit_checks::analyze(snapshot);
    culprit_engine::reports::save(&ws.paths().reports_dir("replayed"), &report).unwrap();
    for id in ["slow", "fast", "unmeasured"] {
        for plan in [
            ws.query_plan_at("replayed", &query_id, Some(id), Some(&report.snapshot.id)).await.unwrap(),
            ws.last_actual_plan_at("replayed", &query_id, Some(id), Some(&report.snapshot.id)).await.unwrap(),
        ] {
            let view = plan.view.unwrap();
            if id == "unmeasured" {
                assert!(view.query.is_none());
                assert!(plan.insights.unwrap().note.unwrap().contains("No matching measurements"));
            } else {
                assert_eq!(view.query.unwrap().plan_id.as_deref(), Some(id));
                let note = view.cost_note.unwrap_or_default();
                assert_eq!(note.contains("322 pages a run"), id == "slow", "{id}: {note}");
                assert_ne!(note, "obsolete");
            }
        }
    }
    let explained = ws.explain("replayed", "SELECT 1", ExplainMode::Estimated, false).await.unwrap();
    assert!(explained.view.is_some());
    assert!(explained.view.unwrap().query.is_none());
    let progress = ws.query_progress("replayed", "52", true).await.unwrap().plan.unwrap();
    let view = progress.view.unwrap();
    assert_eq!(view.kind, culprit_core::PlanKind::Estimated);
    assert!(view.actual.is_none());
    std::fs::remove_dir_all(home).unwrap();
}

#[test]
fn measured_variant_context_and_outliers_survive_the_shared_projection() {
    let cases = inputs::cases();
    let view = |name: &str| cases.iter().find(|c| c.name == name).unwrap().plan.view.as_ref().unwrap();
    assert!(view("wide-read").cost_note.as_ref().unwrap().contains("72 pages a run"));
    assert!(view("slow-variant").cost_note.as_ref().unwrap().contains("322 pages a run"));
    let fast = view("fast-variant").cost_note.as_ref().unwrap();
    assert!(!fast.contains("322 pages") && !fast.contains("20 ms"));
    assert!(view("slow-duration").cost_note.as_ref().unwrap().contains("averaged 20 ms"));
    let outlier = view("outlier").cost_note.as_ref().unwrap();
    assert!(outlier.contains("4 runs took 2.4 s in all, with the slowest taking 2.4 s"));
    let small = &view("small-scan").nodes[0];
    assert!(small.no_share_reason.as_ref().unwrap().contains("as a whole is cheap"));
    assert!(small.finding_no_share_reason.as_ref().unwrap().contains("all 3.9K rows of shop.widgets"));
}

#[test]
fn actual_facts_share_one_clock_and_keep_missing_values_unknown() {
    let cases = inputs::cases();
    let view = |name: &str| cases.iter().find(|c| c.name == name).unwrap().plan.view.as_ref().unwrap();
    let triggers = view("triggers").actual.as_ref().unwrap();
    assert_eq!(
        (triggers.elapsed_ms, triggers.memory_granted_kb, triggers.memory_used_kb),
        (Some(10.0), Some(1024.0), None)
    );
    assert_eq!((triggers.triggers.len(), triggers.triggers[0].time_ms), (1, 120.5));
    let rows = view("rows-only");
    assert_eq!(rows.timed_total_ms, None);
    let actual = rows.actual.as_ref().unwrap();
    assert!(!actual.timed);
    assert_eq!((actual.elapsed_ms, actual.cpu_ms), (None, None));
    assert!(rows.states_shares, "unknown zero time must not claim a costed plan was measured cheap");
    let batch = view("batch-mode");
    assert_eq!(batch.timed_total_ms, Some(47.0));
    assert_eq!(batch.nodes.iter().find(|n| n.id == 2).unwrap().own_ms, Some(15.0));
    let compute = batch.nodes.iter().find(|n| n.id == 1).unwrap();
    assert_eq!((compute.own_ms, compute.time_share), (None, None));
    assert!(batch.time_note.as_ref().unwrap().contains("untimed"));
    let worst = view("estimate-origin").actual.as_ref().unwrap();
    assert_eq!((worst.worst_estimate_node, worst.worst_estimate), (Some(1), Some(5000.0)));
}

#[test]
fn partial_execution_is_not_projected_as_a_completed_actual_run() {
    let cases = inputs::cases();
    let progress = cases.iter().find(|c| c.name == "in-progress").unwrap();
    assert_eq!(progress.plan.kind, culprit_core::PlanKind::Actual, "the collected plan stays intact");
    let view = progress.plan.view.as_ref().unwrap();
    assert_eq!(view.kind, culprit_core::PlanKind::Estimated);
    assert!(view.actual.is_none());
    assert!(view.timed_total_ms.is_none());
}

#[test]
fn imported_plan_json_cannot_supply_a_stale_or_forged_projection() {
    let mut case = inputs::cases().remove(0);
    case.plan.view.as_mut().unwrap().cost_note = Some("A stale explanation".into());
    case.plan.view.as_mut().unwrap().nodes[0].cost_share = Some(999.0);
    let text = serde_json::to_string(&case.plan).unwrap();
    let parsed = culprit_engine::plans::parse_plan(&text).unwrap();
    let view = parsed.view.unwrap();
    assert_ne!(view.cost_note.as_deref(), Some("A stale explanation"));
    assert_ne!(view.nodes[0].cost_share, Some(999.0));

    // Cached projections can change shape between analyzer versions. Only raw plan facts are input.
    for cached in [serde_json::json!({"old_schema": true}), serde_json::json!("obsolete"), serde_json::Value::Null] {
        let mut value = serde_json::to_value(&case.plan).unwrap();
        value["view"] = cached;
        let parsed = culprit_engine::plans::parse_plan(&value.to_string()).unwrap();
        assert!(parsed.view.is_some(), "the current projection must be recomputed");
        assert_eq!(parsed.root.id, case.plan.root.id);
    }
}

#[test]
fn projected_runs_keep_zero_and_parallel_execution_semantics() {
    let mut plan = inputs::cases().remove(0).plan;
    plan.root.act_executions = Some(0.0);
    let view = culprit_checks::plan_view(&plan, None);
    assert_eq!((view.nodes[0].runs, view.nodes[0].estimated_rows_for_runs), (0.0, 0.0));
    plan.root.act_executions = Some(4.0);
    plan.root.props.insert(culprit_core::plan::PARALLEL_THREADS.into(), "4".into());
    let view = culprit_checks::plan_view(&plan, None);
    assert_eq!((view.nodes[0].runs, view.nodes[0].estimated_rows_for_runs), (1.0, plan.root.est_rows));
}

#[test]
fn browser_plan_view_contracts_match_current_rust_analysis() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/plan-view.sample.json");
    let saved: serde_json::Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    crate::invariants::assert_browser_json(
        &saved,
        &serde_json::to_value(inputs::cases()).unwrap(),
        "refresh plan view samples",
    );

    let actual: serde_json::Value = serde_json::from_str(include_str!("../../../fixtures/actual.sample.json")).unwrap();
    for queries in actual["last_actual"].as_object().unwrap().values() {
        for saved in queries.as_object().unwrap().values() {
            let plan: Plan = serde_json::from_value(saved.clone()).unwrap();
            crate::invariants::assert_browser_json(
                &saved["view"],
                &serde_json::to_value(culprit_checks::plan_view(&plan, None)).unwrap(),
                "refresh recorded actual plan projections",
            );
        }
    }
    for frames in actual["progress"].as_object().unwrap().values() {
        for frame in frames.as_array().unwrap().iter().filter(|f| !f["plan"].is_null()) {
            let mut plan: Plan = serde_json::from_value(frame["plan"].clone()).unwrap();
            culprit_checks::present_progress_plan(&mut plan);
            crate::invariants::assert_browser_json(
                &frame["plan"]["view"],
                &serde_json::to_value(plan.view).unwrap(),
                "refresh recorded progress projections",
            );
        }
    }
}
