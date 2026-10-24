//! One interpretation for plan renderers: Rust's timings, estimates, cost shares and explanations.
use culprit_core::{NodeKind, Plan, PlanInsightShare, PlanKind, PlanNode, PlanOperatorView, PlanView, QueryStat};
use std::collections::BTreeMap;

pub fn plan_view(plan: &Plan, query: Option<&QueryStat>) -> PlanView {
    with_insights(plan, query, plan.insights.as_ref())
}

pub(super) fn with_insights(
    plan: &Plan,
    query: Option<&QueryStat>,
    insights: Option<&culprit_core::insights::PlanInsights>,
) -> PlanView {
    let nodes = plan.nodes();
    let weighed = plan.weighed();
    let cost_basis = if weighed { culprit_core::plan_diff::plan_cost_basis(plan) } else { 1.0 };
    let states_shares = plan.states_shares();
    let real_work = culprit_core::insights::does_real_work(plan);
    let guessed: BTreeMap<_, _> = plan.guesses().into_iter().collect();
    let uncosted: Vec<_> = plan.uncosted_steps().iter().map(|n| n.id).collect();
    let actual = culprit_core::actual_summary(plan);
    let timed_total_ms = actual.as_ref().and_then(|a| a.operator_time_ms);
    let times = timed_total_ms.and_then(|_| culprit_core::self_times(plan));
    let mut subtree = BTreeMap::new();
    fn sum(
        node: &PlanNode,
        times: &std::collections::HashMap<u32, f64>,
        out: &mut BTreeMap<u32, (f64, bool)>,
    ) -> (f64, bool) {
        let mut total = times.get(&node.id).copied().unwrap_or(0.0);
        let mut complete = node.act_time_ms.is_some();
        for child in &node.children {
            let (value, known) = sum(child, times, out);
            total += value;
            complete &= known;
        }
        out.insert(node.id, (total, complete));
        (total, complete)
    }
    if let Some(times) = &times {
        sum(&plan.root, times, &mut subtree);
    }
    let hottest_time = times.as_ref().filter(|_| real_work).and_then(|times| {
        nodes
            .iter()
            .filter(|n| n.kind != NodeKind::Parallelism && n.act_time_ms.is_some())
            .filter_map(|n| times.get(&n.id).map(|ms| (n.id, *ms)))
            .max_by(|a, b| a.1.total_cmp(&b.1))
            .map(|(id, _)| id)
    });
    let hottest_cost = plan
        .hottest()
        .filter(|n| real_work && states_shares && !guessed.contains_key(&n.id) && n.self_cost > 0.0)
        .map(|n| n.id);
    let operators = nodes
        .iter()
        .map(|n| {
            let own_ms = times.as_ref().and_then(|t| t.get(&n.id)).copied().filter(|_| n.act_time_ms.is_some());
            let guess = guessed.get(&n.id).copied();
            let cost_share = (states_shares && n.self_cost > 0.0 && guess.is_none()).then(|| plan.cost_share(n));
            PlanOperatorView {
                id: n.id,
                runs: (n.act_executions.is_some() || !n.facts().estimated_rows_unavailable).then(|| n.runs()),
                estimated_rows: (!n.facts().estimated_rows_unavailable).then(|| n.est_rows_total()),
                estimated_rows_for_runs: (!n.facts().estimated_rows_unavailable).then(|| n.est_rows_for_runs()),
                rows_out: n.act_rows.or_else(|| (!n.facts().estimated_rows_unavailable).then(|| n.est_rows_total())),
                rows_read: n.rows_read_total(),
                misestimate: n.misestimate(),
                guess,
                uncosted: uncosted.contains(&n.id),
                layout_own_share: n.self_cost / cost_basis,
                layout_subtree_share: n.subtree_cost / cost_basis,
                cost_share,
                subtree_cost_share: (states_shares && guess.is_none())
                    .then(|| (n.subtree_cost / cost_basis).clamp(0.0, 1.0)),
                own_ms,
                time_share: own_ms.zip(timed_total_ms).map(|(ms, total)| ms / total),
                subtree_time_share: subtree
                    .get(&n.id)
                    .filter(|(_, complete)| *complete)
                    .zip(timed_total_ms)
                    .map(|((ms, _), total)| ms / total),
                no_share_reason: crate::no_share_reason(plan, n, query),
                finding_no_share_reason: crate::finding_no_share_reason(plan, n, query),
            }
        })
        .collect();
    PlanView {
        kind: plan.kind,
        query: query.map(|q| culprit_core::plan_view::PlanMeasurementRef {
            query_id: q.id.clone(),
            plan_id: q.plan_id.clone(),
        }),
        focus_checks: super::brief::ABOUT_THE_PLAN.iter().map(|id| (*id).into()).collect(),
        cost_basis,
        weighed,
        states_shares,
        real_work,
        hottest_cost,
        hottest_time,
        timed_total_ms,
        cost_note: crate::plan_shares_note(plan, query),
        time_note: (times.is_some() && nodes.iter().any(|n| n.act_time_ms.is_none()))
            .then(|| "Only recorded operator times contribute; untimed steps have no measured share.".into()),
        actual,
        nodes: operators,
        insights: insights
            .into_iter()
            .flat_map(|i| &i.items)
            .map(|i| PlanInsightShare {
                rule: i.rule.clone(),
                nodes: i.nodes.clone(),
                share: culprit_core::insights::stated_share(plan, i),
            })
            .collect(),
    }
}

pub fn present_plan(plan: &mut Plan, query: Option<&QueryStat>) {
    plan.view = Some(plan_view(plan, query));
}

/// In-flight rows and timing are not a completed run. Its layout uses estimated cost, with progress overlaid separately.
pub fn present_progress_plan(plan: &mut Plan) {
    let mut estimated = plan.clone();
    estimated.kind = PlanKind::Estimated;
    estimated.view = None;
    plan.view = Some(plan_view(&estimated, None));
}
