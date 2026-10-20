//! Shared inputs for native regression tests and the web renderer's generated contract fixtures.
use culprit_core::{Engine, NodeKind, Plan, PlanKind, PlanNode, PlanVariant, QueryStat, Snapshot};
use serde::Serialize;

#[derive(Serialize)]
pub struct Case {
    pub name: String,
    pub plan: Plan,
    pub query: Option<QueryStat>,
}

fn base() -> (Plan, QueryStat) {
    let snapshot: Snapshot = serde_json::from_str(include_str!("../../../../fixtures/mssql-demo.json")).unwrap();
    let mut query = snapshot.workload.unwrap().queries.remove(0);
    let mut plan = query.plan.take().unwrap();
    plan.raw = None;
    plan.view = None;
    plan.insights = None;
    plan.index_hints.clear();
    plan.warnings.clear();
    plan.props.clear();
    plan.kind = PlanKind::Estimated;
    plan.total_cost = 0.001;
    plan.statement = Some("SELECT id FROM shop.widgets WHERE id = @id".into());
    let n = &mut plan.root;
    n.id = 0;
    n.children.clear();
    n.kind = NodeKind::Seek;
    n.op = "Index Seek".into();
    n.detail = None;
    n.object = None;
    n.index = None;
    n.predicate = None;
    n.seek_predicate = None;
    n.props.clear();
    n.warnings.clear();
    n.est_rows = 1.0;
    n.est_executions = 1.0;
    n.subtree_cost = 0.001;
    n.self_cost = 0.001;
    n.act_rows = None;
    n.act_executions = None;
    n.act_time_ms = None;
    n.runtime = None;
    n.table_rows = None;
    n.rows_read = None;
    query.text = plan.statement.clone().unwrap();
    query.plan_id = None;
    query.plans.clear();
    query.executions = 100.0;
    query.rows = Some(100.0);
    query.logical_reads = Some(7200.0);
    query.total_time_ms = 100.0;
    query.mean_time_ms = 1.0;
    query.max_time_ms = None;
    query.stddev_time_ms = None;
    query.cpu_time_ms = None;
    query.physical_reads = None;
    query.temp_writes = None;
    query.first_seen = None;
    query.last_seen = None;
    query.rows_affected = None;
    query.costs = None;
    query.spread = None;
    query.requests = None;
    query.resolved_write_target = None;
    query.parameters = None;
    query.object_name = None;
    query.waits.clear();
    query.trend = None;
    (plan, query)
}

fn variant(id: &str, mean: f64, reads: f64) -> PlanVariant {
    serde_json::from_value(serde_json::json!({
        "id":id,"executions":10.0,"mean_time_ms":mean,"total_time_ms":mean*10.0,
        "logical_reads":reads*10.0,"first_seen":null,"last_seen":null,"is_forced":false,
        "min_time_ms":null,"max_time_ms":null,"stddev_time_ms":null,"compiled":null,"variant_query":null,"cpu_time_ms":null
    })).unwrap()
}

fn finish(name: &str, mut plan: Plan, query: Option<QueryStat>) -> Case {
    culprit_core::insights::annotate(&mut plan);
    culprit_checks::present_plan(&mut plan, query.as_ref());
    Case { name: name.into(), plan, query }
}

fn timed(template: &PlanNode, id: u32, ms: Option<f64>, children: Vec<PlanNode>) -> PlanNode {
    let mut node = template.clone();
    node.id = id;
    node.act_time_ms = ms;
    node.act_rows = Some(1.0);
    node.act_executions = Some(1.0);
    node.children = children;
    node.self_cost = 1.0;
    node.subtree_cost = 1.0;
    node
}

pub fn cases() -> Vec<Case> {
    let (plan, query) = base();
    let mut cases = vec![finish("wide-read", plan.clone(), Some(query.clone()))];
    let mut variants = query.clone();
    variants.plans = vec![variant("fast", 1.0, 2.0), variant("slow", 20.0, 322.0)];
    variants.plan_id = Some("slow".into());
    cases.push(finish("slow-variant", plan.clone(), Some(variants.clone())));
    variants.plan_id = Some("fast".into());
    cases.push(finish("fast-variant", plan.clone(), Some(variants.clone())));
    variants.plans[1].logical_reads = Some(20.0);
    variants.plan_id = Some("slow".into());
    cases.push(finish("slow-duration", plan.clone(), Some(variants)));
    let mut outlier = query.clone();
    outlier.executions = 4.0;
    outlier.rows = Some(4.0);
    outlier.logical_reads = Some(4.0);
    outlier.total_time_ms = 2404.0;
    outlier.mean_time_ms = 601.0;
    outlier.max_time_ms = Some(2400.0);
    cases.push(finish("outlier", plan.clone(), Some(outlier.clone())));
    let mut small = plan.clone();
    small.root.kind = NodeKind::Scan;
    small.root.op = "Table Scan".into();
    small.root.object = Some("shop.widgets".into());
    small.root.table_rows = Some(3900.0);
    small.root.rows_read = Some(3900.0);
    outlier.mean_time_ms = 1.0;
    outlier.total_time_ms = 4.0;
    outlier.max_time_ms = None;
    cases.push(finish("small-scan", small, Some(outlier)));
    cases.push(finish("retained-old", plan.clone(), Some(query.clone())));
    let mut newer = query;
    newer.logical_reads = Some(75_000.0);
    newer.mean_time_ms = 50.0;
    newer.total_time_ms = 5000.0;
    cases.push(finish("retained-new", plan.clone(), Some(newer)));

    let mut trigger = plan.clone();
    trigger.engine = Engine::Postgres;
    trigger.kind = PlanKind::Actual;
    trigger.root = timed(&plan.root, 0, Some(10.0), vec![]);
    trigger.root.kind = NodeKind::Modify;
    trigger.root.op = "Update".into();
    trigger.total_cost = 1.0;
    trigger.props.insert("MemoryGrantedKB".into(), "1024".into());
    trigger.props.insert(
        "Triggers".into(),
        r#"[{"Trigger Name":"audit","Relation":"items","Time":120.5,"Calls":10},{"Trigger Name":"invalid","Time":-1}]"#
            .into(),
    );
    cases.push(finish("triggers", trigger, None));
    let mut rows_only = plan.clone();
    rows_only.kind = PlanKind::Actual;
    rows_only.total_cost = 1.0;
    rows_only.root = timed(&plan.root, 0, Some(0.0), vec![]);
    rows_only.props.insert("CpuMs".into(), "0".into());
    cases.push(finish("rows-only", rows_only, None));

    let mut workers = plan.clone();
    workers.kind = PlanKind::Actual;
    workers.total_cost = 1.0;
    workers.root = timed(&plan.root, 0, Some(10.0), vec![]);
    workers.root.est_rows = 100_000.0;
    workers.root.act_rows = Some(100_000.0);
    workers.root.act_executions = Some(2.0);
    workers.root.props.insert(culprit_core::plan::PARALLEL_THREADS.into(), "2".into());
    workers.root.runtime =
        Some(culprit_core::NodeRuntime { thread_rows: vec![95_000.0, 5_000.0], ..Default::default() });
    workers.props.insert("UsedThreads".into(), "2".into());
    cases.push(finish("workers", workers, None));

    let mut batch = plan.clone();
    batch.kind = PlanKind::Actual;
    batch.total_cost = 10.0;
    let mut join = timed(
        &plan.root,
        3,
        Some(7.0),
        vec![timed(&plan.root, 4, Some(10.0), vec![]), timed(&plan.root, 5, Some(6.0), vec![])],
    );
    for node in &mut join.children {
        node.props.insert("ActualExecutionMode".into(), "Batch".into());
    }
    join.props.insert("ActualExecutionMode".into(), "Batch".into());
    let mut hash = timed(&plan.root, 2, Some(15.0), vec![join]);
    hash.props.insert("ActualExecutionMode".into(), "Batch".into());
    let mut compute = timed(&plan.root, 1, None, vec![hash]);
    compute.kind = NodeKind::Compute;
    compute.op = "Compute Scalar".into();
    batch.root = timed(&plan.root, 0, Some(47.0), vec![compute]);
    cases.push(finish("batch-mode", batch, None));

    let mut error = plan.clone();
    error.engine = Engine::Postgres;
    error.kind = PlanKind::Actual;
    error.total_cost = 10.0;
    let mut scan = timed(&plan.root, 1, Some(90.0), vec![]);
    scan.op = "Seq Scan".into();
    scan.kind = NodeKind::Scan;
    scan.est_rows = 10.0;
    scan.act_rows = Some(50_000.0);
    scan.self_cost = 9.0;
    error.root = timed(&plan.root, 0, Some(100.0), vec![scan]);
    error.root.op = "Aggregate".into();
    error.root.kind = NodeKind::Aggregate;
    cases.push(finish("estimate-origin", error, None));

    let mut partial = plan;
    partial.kind = PlanKind::Actual;
    partial.total_cost = 10.0;
    let template = partial.root.clone();
    partial.root = timed(
        &template,
        0,
        Some(100.0),
        vec![timed(&template, 1, None, vec![]), timed(&template, 2, Some(20.0), vec![])],
    );
    cases.push(finish("partial-times", partial.clone(), None));
    culprit_checks::present_progress_plan(&mut partial);
    cases.push(Case { name: "in-progress".into(), plan: partial, query: None });
    cases
}
