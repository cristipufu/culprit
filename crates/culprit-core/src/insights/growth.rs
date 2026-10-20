//! Structural scaling observations. They preserve pagination/partition semantics and never infer an index from shape.
use super::*;

pub(super) fn deep_pagination(cx: &Cx) -> Vec<Draft> {
    let Some(offset) =
        cx.plan.statement.as_deref().and_then(|s| crate::sql::offset_count(s, cx.engine())).filter(|v| *v >= 10_000.0)
    else {
        return vec![];
    };
    let n = cx
        .nodes
        .iter()
        .copied()
        .find(|n| n.op.to_ascii_lowercase().starts_with("limit") || n.op == "Top")
        .unwrap_or(&cx.plan.root);
    let mut d = Draft::new("deep-pagination", InsightArea::Query, cx.subtree(n), vec![n.id]);
    d.subtree = true;
    d.cap = Severity::Medium;
    d.title = format!("Pagination asks to skip {} rows before returning this page", format::count(offset));
    d.detail="The statement's literal OFFSET requires skipping the preceding matches before returning a later page. A small table or an early end can make it cheap today; later pages can read much more as matching data grows. This does not establish the cost of other parameter values or every run.".into();
    d.fix="If the product permits cursor pagination, test continuing from the last ordered key using a deterministic order with a unique tie-breaker. Preserve permissions, filters, NULL ordering and behavior under concurrent changes. Keep OFFSET when arbitrary page jumps are required; an ordered index can help find rows but does not remove the skipped matches.".into();
    d.verify="Compare identical page results and ordering, then rows read and duration for early and late pages on representative data. Confirm cursor behavior under inserts/deletes and equal sort keys; a changed operator alone is not success.".into();
    vec![d]
}

pub(super) fn partition_filtering(cx: &Cx) -> Vec<Draft> {
    if cx.engine() != Engine::Postgres || cx.plan.kind != PlanKind::Actual {
        return vec![];
    }
    let mut groups: std::collections::BTreeMap<String, Vec<&PlanNode>> = Default::default();
    for n in &cx.nodes {
        if n.act_executions.is_some_and(|v| v > 0.0)
            && n.predicate.is_some()
            && n.act_rows.is_some()
            && let Some(parent) = n.facts().partition_parent.as_ref()
        {
            groups.entry(parent.clone()).or_default().push(n);
        }
    }
    groups.into_iter().filter_map(|(parent,nodes)|{
        let unique:std::collections::HashSet<_>=nodes.iter().filter_map(|n|n.object.as_deref()).collect();
        let read:f64=nodes.iter().map(|n|rows_read(n)).sum();let kept:f64=nodes.iter().map(|n|rows_out(n)).sum();
        if unique.len()<16||read<100_000.0||kept/read.max(1.0)>INDEX_KEEPS_AT_MOST{return None;}
        let mut d=Draft::new("partition-filtering",InsightArea::Query,0.1,nodes.iter().map(|n|n.id).collect());d.fixed_weight=true;d.cap=Severity::Low;
        d.title=format!("This run visits {} partitions of {} and filters most rows afterwards",unique.len(),parent);
        d.detail=format!("Measured partition scans read {} rows and returned {} after their filters. This establishes broad partition access in this run, not failed pruning: the filter may intentionally span these partitions or use a different key.",format::count(read),format::count(kept));
        d.fix="Check the partition key and whether the query's intended range can constrain it directly. Review casts, functions and parameter types only where they prevent that constraint. Do not add a narrower range that changes results; some reports correctly need every partition. Check per-partition indexes separately.".into();
        d.verify="Compare equivalent results, actual partitions executed, rows read and execution time for representative values. Runtime pruning can skip partitions listed in a generic plan, so compare measured executions rather than counting planned branches.".into();Some(d)
    }).collect()
}
