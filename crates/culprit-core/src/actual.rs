//! What actually happened when a statement ran: runtime counters on plan operators (actual plans, the last actual plan
//! SQL Server kept, live progress), and the progress of a statement that is still running.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::model::Engine;
use crate::plan::{Plan, PlanKind, PlanNode};

/// Runtime counters an operator reports beyond rows, executions and time. Every field is optional: engines report
/// different subsets (Postgres has buffers but no CPU per operator, MySQL has neither).
#[derive(Serialize, Deserialize, TS, Clone, Debug, Default, PartialEq)]
#[ts(export)]
pub struct NodeRuntime {
    pub cpu_ms: Option<f64>,
    /// Pages (SQL Server) or buffers (Postgres) read, from memory or disk.
    pub logical_reads: Option<f64>,
    /// The part of `logical_reads` that had to come from disk.
    pub physical_reads: Option<f64>,
    /// Rows each worker produced for parallel operators; uneven numbers mean one worker did most of the work.
    #[serde(default)]
    pub thread_rows: Vec<f64>,
    /// Pages written to and read back from tempdb / temp files: the operator ran out of memory.
    pub spill_pages: Option<f64>,
}

impl NodeRuntime {
    pub fn is_empty(&self) -> bool {
        self == &NodeRuntime::default()
    }
}

/// Whether an operator of a running statement has started, is producing rows, or has finished.
#[derive(Serialize, Deserialize, TS, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
#[ts(export)]
pub enum NodeState {
    Pending,
    Running,
    Done,
}

/// One operator of a running statement, so far.
#[derive(Serialize, Deserialize, TS, Clone, Debug, PartialEq)]
#[ts(export)]
pub struct NodeProgress {
    pub node_id: u32,
    pub state: NodeState,
    /// Rows produced so far, across executions and threads.
    pub rows: f64,
    /// Rows the optimizer expected in total; `rows / est_rows` is how far along the operator is, when the estimate holds.
    pub est_rows: f64,
    pub elapsed_ms: Option<f64>,
    pub cpu_ms: Option<f64>,
    pub logical_reads: Option<f64>,
}

impl NodeProgress {
    /// 0..1 when the estimate holds; `None` before the operator starts producing rows against a zero estimate.
    pub fn fraction(&self) -> Option<f64> {
        match self.state {
            NodeState::Done => Some(1.0),
            NodeState::Pending => Some(0.0),
            NodeState::Running if self.est_rows >= 1.0 => Some((self.rows / self.est_rows).min(0.99)),
            NodeState::Running => None,
        }
    }
}

/// A statement that is running right now, as far along as the engine can say.
#[derive(Serialize, Deserialize, TS, Clone, Debug)]
#[ts(export)]
pub struct QueryProgress {
    pub session: String,
    pub engine: Engine,
    pub taken_at: String,
    /// False once the session has no statement running (it finished, failed or was cancelled).
    pub running: bool,
    pub state: Option<String>,
    pub wait: Option<String>,
    pub wait_ms: Option<f64>,
    pub elapsed_ms: Option<f64>,
    pub cpu_ms: Option<f64>,
    pub logical_reads: Option<f64>,
    /// Rows the statement has examined so far (MySQL).
    pub rows_examined: Option<f64>,
    pub statement: Option<String>,
    /// The plan being executed. SQL Server fills in the actual counters so far; Postgres and MySQL give the estimated
    /// plan, sent once per statement (`with_plan`).
    pub plan: Option<Plan>,
    /// Per-operator progress; empty when the engine doesn't report it.
    pub nodes: Vec<NodeProgress>,
    /// How far along the whole statement is, 0..1, and what that number is based on.
    pub percent: Option<f64>,
    pub percent_basis: Option<String>,
    /// The operator doing most of the work right now.
    pub hot_node: Option<u32>,
    /// A maintenance step the engine reports progress for (Postgres CREATE INDEX/VACUUM/COPY…, MySQL stages).
    pub phase: Option<String>,
    pub phase_percent: Option<f64>,
    /// Why the view is coarser than full per-operator progress on this engine or server, in plain language.
    pub note: Option<String>,
}

impl QueryProgress {
    pub fn idle(session: &str, engine: Engine, taken_at: String) -> QueryProgress {
        QueryProgress {
            session: session.to_string(),
            engine,
            taken_at,
            running: false,
            state: None,
            wait: None,
            wait_ms: None,
            elapsed_ms: None,
            cpu_ms: None,
            logical_reads: None,
            rows_examined: None,
            statement: None,
            plan: None,
            nodes: Vec::new(),
            percent: None,
            percent_basis: None,
            hot_node: None,
            phase: None,
            phase_percent: None,
            note: None,
        }
    }

    /// Overall progress from per-operator progress, weighting each operator by its estimated cost, and the operator
    /// working hardest right now (most time of its own among those still running).
    pub fn summarize_nodes(&mut self) {
        let Some(plan) = &self.plan else { return };
        if self.nodes.is_empty() {
            return;
        }
        let costs: std::collections::HashMap<u32, f64> =
            plan.nodes().iter().map(|n| (n.id, n.self_cost.max(plan.total_cost.max(1e-9) * 0.001))).collect();
        let (mut done, mut weight) = (0.0, 0.0);
        for node in &self.nodes {
            let w = costs.get(&node.node_id).copied().unwrap_or(0.0);
            if let Some(f) = node.fraction() {
                done += w * f;
                weight += w;
            }
        }
        if weight > 0.0 {
            self.percent = Some((done / weight).clamp(0.0, 0.99));
            self.percent_basis =
                Some("Rows each operator has produced against the optimizer's estimate, weighted by cost".to_string());
        }
        let own = self_times_from_progress(plan, &self.nodes);
        self.hot_node = self
            .nodes
            .iter()
            .filter(|n| n.state == NodeState::Running)
            .filter_map(|n| own.get(&n.node_id).map(|t| (n.node_id, *t)))
            .max_by(|a, b| a.1.total_cmp(&b.1))
            .map(|(id, _)| id);
    }
}

fn self_times_from_progress(plan: &Plan, nodes: &[NodeProgress]) -> std::collections::HashMap<u32, f64> {
    let elapsed: std::collections::HashMap<u32, f64> =
        nodes.iter().filter_map(|n| n.elapsed_ms.or(n.cpu_ms).map(|t| (n.node_id, t))).collect();
    own_times(&plan.root, &|node| elapsed.get(&node.id).copied())
}

/// SQL Server batch-mode operators report their own time; row-mode operators (and every Postgres and MySQL operator)
/// report time including their inputs.
fn batch_mode(node: &PlanNode) -> bool {
    let facts = node.facts();
    matches!(
        facts.actual_execution_mode.as_ref().or(facts.estimated_execution_mode.as_ref()),
        Some(crate::PlanExecutionMode::Batch)
    )
}

/// Each operator's own time from reported times that may include inputs (row mode), exclude them (batch mode), or be
/// missing (SQL Server doesn't time Compute Scalar): an operator without a time counts as the sum of its inputs.
fn own_times(root: &PlanNode, time: &dyn Fn(&PlanNode) -> Option<f64>) -> std::collections::HashMap<u32, f64> {
    fn visit(
        node: &PlanNode,
        time: &dyn Fn(&PlanNode) -> Option<f64>,
        out: &mut std::collections::HashMap<u32, f64>,
    ) -> f64 {
        let inputs: f64 = node.children.iter().map(|c| visit(c, time, out)).sum();
        match time(node) {
            Some(t) if batch_mode(node) => {
                out.insert(node.id, t);
                t + inputs
            }
            Some(t) => {
                out.insert(node.id, (t - inputs).max(0.0));
                t.max(inputs)
            }
            None => {
                out.insert(node.id, 0.0);
                inputs
            }
        }
    }
    let mut out = std::collections::HashMap::new();
    visit(root, time, &mut out);
    out
}

/// Milliseconds each operator spent on its own work in an actual plan (its time minus its inputs' time), when the plan
/// carries per-operator timing for most operators. `None` for estimated plans and for actual plans that only kept row
/// counts (SQL Server's last actual plan).
pub fn self_times(plan: &Plan) -> Option<std::collections::HashMap<u32, f64>> {
    if plan.kind != PlanKind::Actual {
        return None;
    }
    let nodes = plan.nodes();
    let timed = nodes.iter().filter(|n| n.act_time_ms.is_some()).count();
    if timed == 0 || timed * 2 < nodes.len() {
        return None;
    }
    let times = own_times(&plan.root, &|n| n.act_time_ms);
    let total: f64 = nodes.iter().filter_map(|n| times.get(&n.id)).sum();
    (total.is_finite() && total > 0.0).then_some(times)
}

/// The headline numbers of an actual plan, for the CLI and agents.
#[derive(Serialize, Deserialize, TS, Clone, Debug, PartialEq)]
#[ts(export)]
pub struct ActualSummary {
    #[serde(default)]
    pub triggers: Vec<TriggerTiming>,
    pub elapsed_ms: Option<f64>,
    pub cpu_ms: Option<f64>,
    pub logical_reads: Option<f64>,
    pub physical_reads: Option<f64>,
    pub rows: Option<f64>,
    pub memory_granted_kb: Option<f64>,
    pub memory_used_kb: Option<f64>,
    pub spilled: bool,
    /// The operator that spent the most time of its own, and that time.
    pub slowest_node: Option<u32>,
    pub slowest_ms: Option<f64>,
    /// The operator whose row count was furthest from the estimate (×, signed like `PlanNode::misestimate`).
    pub worst_estimate_node: Option<u32>,
    pub worst_estimate: Option<f64>,
    #[serde(default)]
    pub timed: bool,
    pub operator_time_ms: Option<f64>,
    pub spill_pages: Option<f64>,
    pub skew: Option<WorkerSkew>,
    pub last_run_at: Option<String>,
    pub workers: Option<f64>,
}

#[derive(Serialize, Deserialize, TS, Clone, Debug, PartialEq)]
#[ts(export)]
pub struct WorkerSkew {
    pub node: u32,
    pub busiest: f64,
    pub threads: usize,
}

/// A descriptive actual-run callout; detector urgency additionally weighs the amount and shape of work.
pub fn worker_skew(plan: &Plan) -> Option<WorkerSkew> {
    let mut found: Option<WorkerSkew> = None;
    for node in plan.nodes() {
        let Some(runtime) = &node.runtime else { continue };
        let rows = &runtime.thread_rows;
        if rows.len() < 2 || rows.iter().any(|v| !v.is_finite() || *v < 0.0) {
            continue;
        }
        let total: f64 = rows.iter().sum();
        if !total.is_finite() || total < 10_000.0 {
            continue;
        }
        let busiest = rows.iter().copied().fold(0.0, f64::max) / total;
        if busiest >= 0.5 && busiest > 1.8 / rows.len() as f64 && found.as_ref().is_none_or(|old| busiest > old.busiest)
        {
            found = Some(WorkerSkew { node: node.id, busiest, threads: rows.len() });
        }
    }
    found
}

pub fn actual_summary(plan: &Plan) -> Option<ActualSummary> {
    if plan.kind != PlanKind::Actual {
        return None;
    }
    let nodes = plan.nodes();
    let facts = plan.facts();
    let sum = |f: &dyn Fn(&PlanNode) -> Option<f64>| {
        let values: Vec<f64> = nodes.iter().filter_map(|n| f(n)).filter(|v| v.is_finite() && *v >= 0.0).collect();
        (!values.is_empty()).then(|| values.iter().sum::<f64>()).filter(|v| v.is_finite())
    };
    let own = self_times(plan);
    // Tree order makes ties deterministic, matching the operator the UI opens rather than HashMap iteration.
    let slowest = own.as_ref().and_then(|times| {
        nodes.iter().filter_map(|n| times.get(&n.id).map(|ms| (n.id, *ms))).fold(
            None,
            |best: Option<(u32, f64)>, current| {
                if best.is_some_and(|b| b.1 >= current.1) { best } else { Some(current) }
            },
        )
    });
    let known = |value: Option<f64>| value.filter(|v| v.is_finite() && *v >= 0.0 && (*v != 0.0 || own.is_some()));
    // Misestimates by the rule behind the Misestimate warning (`PlanNode::misestimates`): an operator a Limit stopped
    // early read fewer rows than its estimate for a full run, which is no misestimate. A bad estimate repeats in every
    // operator above the one where it starts; report where it starts (the deepest operator within a factor of two of
    // the worst), since that is what a fix has to reach.
    let found: std::collections::HashMap<u32, f64> = plan.root.misestimates().into_iter().collect();
    let factor = |n: &PlanNode| found.get(&n.id).copied();
    let largest = found.values().map(|f| f.abs()).fold(0.0, f64::max);
    fn origin(n: &PlanNode, at_least: f64, factor: &dyn Fn(&PlanNode) -> Option<f64>) -> Option<(u32, f64)> {
        n.children
            .iter()
            .find_map(|c| origin(c, at_least, factor))
            .or_else(|| factor(n).filter(|f| f.abs() >= at_least).map(|f| (n.id, f)))
    }
    let worst = (!found.is_empty()).then(|| origin(&plan.root, largest / 2.0, &factor)).flatten();
    Some(ActualSummary {
        triggers: trigger_timings(plan),
        elapsed_ms: known(facts.elapsed_ms.or(plan.root.act_time_ms)),
        cpu_ms: known(facts.cpu_ms.or_else(|| sum(&|n| n.runtime.as_ref().and_then(|r| r.cpu_ms)))),
        logical_reads: sum(&|n| n.runtime.as_ref().and_then(|r| r.logical_reads)),
        physical_reads: sum(&|n| n.runtime.as_ref().and_then(|r| r.physical_reads)),
        rows: plan.root.act_rows,
        memory_granted_kb: facts.memory_granted_kb,
        memory_used_kb: facts.memory_max_used_kb,
        spilled: plan.has_warning(crate::plan::PlanWarning::Spill)
            || nodes.iter().any(|n| n.runtime.as_ref().and_then(|r| r.spill_pages).is_some_and(|p| p > 0.0)),
        slowest_node: slowest.map(|s| s.0),
        slowest_ms: slowest.map(|s| s.1),
        worst_estimate_node: worst.map(|w| w.0),
        worst_estimate: worst.map(|w| w.1),
        timed: own.is_some(),
        operator_time_ms: own.as_ref().map(|times| nodes.iter().filter_map(|n| times.get(&n.id)).sum()),
        spill_pages: sum(&|n| n.runtime.as_ref().and_then(|r| r.spill_pages)),
        skew: worker_skew(plan),
        last_run_at: plan.props.get("LastRunAt").cloned(),
        workers: facts.used_threads.map(f64::from),
    })
}

#[derive(Serialize, Deserialize, TS, Clone, Debug, PartialEq)]
#[ts(export)]
pub struct TriggerTiming {
    pub name: String,
    pub relation: Option<String>,
    pub time_ms: f64,
    pub calls: Option<f64>,
}

/// Trigger totals are separate from operator timings; BEFORE triggers overlap their modifying node's time.
pub fn trigger_timings(plan: &Plan) -> Vec<TriggerTiming> {
    if plan.engine != Engine::Postgres {
        return vec![];
    }
    plan.facts()
        .trigger_timings
        .iter()
        .filter(|t| t.time_ms.is_finite() && t.time_ms >= 0.0)
        .cloned()
        .map(|mut t| {
            t.calls = t.calls.and_then(crate::plan_facts::whole);
            t
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Properties;
    use crate::plan::NodeKind;

    fn node(id: u32, time: f64, est: f64, act: f64, cost: f64, children: Vec<PlanNode>) -> PlanNode {
        PlanNode {
            facts: None,
            id,
            op: format!("op{id}"),
            kind: NodeKind::Other,
            detail: None,
            object: None,
            index: None,
            seek_predicate: None,
            predicate: None,
            est_rows: est,
            est_executions: 1.0,
            act_rows: Some(act),
            act_executions: Some(1.0),
            rows_read: None,
            table_rows: None,
            subtree_cost: cost,
            self_cost: cost,
            act_time_ms: Some(time),
            runtime: None,
            warnings: vec![],
            props: Properties::new(),
            children,
        }
    }

    fn plan(root: PlanNode) -> Plan {
        Plan {
            facts: None,
            view: None,
            engine: Engine::Postgres,
            kind: PlanKind::Actual,
            statement: None,
            total_cost: 10.0,
            root,
            warnings: vec![],
            index_hints: vec![],
            props: Properties::new(),
            raw: None,
            insights: None,
        }
    }

    #[test]
    fn own_time_subtracts_inputs() {
        let p = plan(node(0, 100.0, 1.0, 1.0, 1.0, vec![node(1, 90.0, 10.0, 50_000.0, 9.0, vec![])]));
        let own = self_times(&p).unwrap();
        assert_eq!(own[&0], 10.0);
        assert_eq!(own[&1], 90.0);
        let s = actual_summary(&p).unwrap();
        assert_eq!((s.slowest_node, s.elapsed_ms), (Some(1), Some(100.0)));
        assert_eq!(s.worst_estimate_node, Some(1), "50,000 rows against an estimate of 10");
    }

    #[test]
    fn a_scan_a_limit_stopped_is_not_the_worst_estimate() {
        // The scan's 50,000 rows are its estimate for a full run; the Limit took 10 and stopped it.
        let mut limit = node(0, 10.0, 10.0, 10.0, 1.0, vec![node(1, 9.0, 50_000.0, 10.0, 9.0, vec![])]);
        limit.op = "Limit".into();
        let s = actual_summary(&plan(limit)).unwrap();
        assert_eq!((s.worst_estimate_node, s.worst_estimate), (None, None));
    }

    #[test]
    fn batch_mode_and_untimed_operators_are_not_double_counted() {
        let mut join = node(
            3,
            7.0,
            1.0,
            1.0,
            1.0,
            vec![node(4, 10.0, 1.0, 1.0, 1.0, vec![]), node(5, 6.0, 1.0, 1.0, 1.0, vec![])],
        );
        let mut hash = node(2, 15.0, 1.0, 1.0, 1.0, vec![]);
        for n in [&mut join, &mut hash] {
            n.props.insert("ActualExecutionMode".into(), "Batch".into());
        }
        for child in &mut join.children {
            child.props.insert("ActualExecutionMode".into(), "Batch".into());
        }
        hash.children.push(join);
        let mut compute = node(1, 0.0, 1.0, 1.0, 1.0, vec![hash]);
        compute.act_time_ms = None;
        let root = node(0, 47.0, 1.0, 1.0, 1.0, vec![compute]);
        let own = self_times(&plan(root)).unwrap();
        assert_eq!(own[&0], 9.0, "the exchange only waited for its batch-mode inputs (38 ms)");
        assert_eq!((own[&1], own[&2], own[&3]), (0.0, 15.0, 7.0));
        assert_eq!(own.values().sum::<f64>(), 47.0);
    }

    #[test]
    fn progress_weights_operators_by_cost() {
        let p = plan(node(0, 0.0, 1.0, 0.0, 1.0, vec![node(1, 0.0, 1000.0, 0.0, 9.0, vec![])]));
        let mut progress = QueryProgress::idle("52", Engine::Mssql, String::new());
        progress.running = true;
        progress.plan = Some(p);
        progress.nodes = vec![
            NodeProgress {
                node_id: 0,
                state: NodeState::Running,
                rows: 0.0,
                est_rows: 1.0,
                elapsed_ms: Some(400.0),
                cpu_ms: None,
                logical_reads: None,
            },
            NodeProgress {
                node_id: 1,
                state: NodeState::Running,
                rows: 500.0,
                est_rows: 1000.0,
                elapsed_ms: Some(390.0),
                cpu_ms: None,
                logical_reads: None,
            },
        ];
        progress.summarize_nodes();
        let pct = progress.percent.unwrap();
        assert!((pct - 0.45).abs() < 0.01, "{pct}");
        assert_eq!(progress.hot_node, Some(1), "the scan does the work; the root only waits on it");
    }

    #[test]
    fn rows_only_actuals_do_not_report_unknown_timings_as_zero() {
        let mut p = plan(node(0, 0.0, 1.0, 1.0, 1.0, vec![]));
        p.props.insert("CpuMs".into(), "0".into());
        p.props.insert("MemoryGrantedKB".into(), "1024".into());
        let actual = actual_summary(&p).unwrap();
        assert!(!actual.timed);
        assert_eq!((actual.elapsed_ms, actual.cpu_ms, actual.slowest_node), (None, None, None));
        assert_eq!(actual.rows, Some(1.0));
        assert_eq!((actual.memory_granted_kb, actual.memory_used_kb), (Some(1024.0), None));
        assert_eq!(actual.spill_pages, None);
        p.root.act_time_ms = Some(10.0);
        assert_eq!(actual_summary(&p).unwrap().cpu_ms, Some(0.0), "a timed run can measure zero CPU");
    }

    #[test]
    fn actual_summary_ties_follow_tree_order_and_keep_trigger_time_separate() {
        let mut p = plan(node(
            0,
            10.0,
            1.0,
            1.0,
            1.0,
            vec![node(2, 5.0, 1.0, 1.0, 1.0, vec![]), node(1, 5.0, 1.0, 1.0, 1.0, vec![])],
        ));
        p.props.insert("Triggers".into(), r#"[{"Trigger Name":"audit","Time":120.5,"Calls":10},{"Time":-1}]"#.into());
        for _ in 0..10 {
            let actual = actual_summary(&p).unwrap();
            assert_eq!(actual.slowest_node, Some(2));
            assert_eq!(actual.operator_time_ms, Some(10.0));
            assert_eq!(actual.elapsed_ms, Some(10.0));
            assert_eq!(actual.triggers.len(), 1);
            assert_eq!(actual.triggers[0].time_ms, 120.5);
        }
    }

    #[test]
    fn actual_worker_distribution_requires_real_nonnegative_counts() {
        let mut p = plan(node(0, 10.0, 1.0, 1.0, 1.0, vec![]));
        p.root.runtime = Some(NodeRuntime { thread_rows: vec![95_000.0, 5_000.0], ..Default::default() });
        let skew = actual_summary(&p).unwrap().skew.unwrap();
        assert_eq!((skew.node, skew.threads, skew.busiest), (0, 2, 0.95));
        p.root.runtime.as_mut().unwrap().thread_rows[1] = -1.0;
        assert!(actual_summary(&p).unwrap().skew.is_none());
        p.props.insert("ElapsedMs".into(), "NaN".into());
        p.props.insert("CpuMs".into(), "inf".into());
        let actual = actual_summary(&p).unwrap();
        assert_eq!(actual.elapsed_ms, Some(10.0));
        assert_eq!(actual.cpu_ms, None);
    }
}
