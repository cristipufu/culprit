//! Regressions at evidence boundaries: missing entries, ambiguous schemas, display changes and partial groups.
use super::*;
use culprit_core::{
    Category, FindingLargeRows, FindingLogWrite, FindingPlanChange, Fix, Metric, QueryCosts, QueryRef,
    RefinementOutcome,
};

fn snapshot() -> Snapshot {
    let mut s: Snapshot = serde_json::from_str(include_str!("../../../../fixtures/mssql-demo.json")).unwrap();
    let w = s.workload.as_mut().unwrap();
    w.measurement = None;
    w.timeline = None;
    w.window_hours = 24.0;
    w.queries.truncate(2);
    for q in &mut w.queries {
        q.executions = 100.0;
        q.total_time_ms = 1_000_000.0;
        q.mean_time_ms = 10_000.0;
        q.first_seen = None;
        q.last_seen = None;
        q.trend = None;
        q.plan = None;
        q.costs = None;
    }
    w.total_time_ms = 2_000_000.0;
    s
}
fn finding(check: &str, key: &str, ids: &[&str]) -> Finding {
    let mut f = support::finding(check, key, Category::Queries, support::subject(SubjectKind::Table, "shop.orders"));
    f.queries = ids.iter().map(|id| QueryRef { id: (*id).into(), plan_node: None, note: None }).collect();
    f
}

#[test]
fn index_keys_are_not_parsed_from_ids_or_display_labels() {
    let s = snapshot();
    let id = &s.queries()[0].id;
    let mut scan = finding("filtered-scan", "shop.orders(unrelated)", &[id]);
    scan.facts.index_keys = vec!["state,code".into()];
    let mut request = finding("missing-index", "shop.orders(other)", &[id]);
    request.facts.index_keys = vec!["state,code".into()];
    let mut findings = vec![scan, request];
    let trace = indexes::Rule(indexes::Policy::MissingScan).refine_traced(&s, &mut findings);
    assert_eq!(findings.len(), 1);
    assert!(matches!(&trace[0].outcome,RefinementOutcome::Absorbed {into} if into==&findings[0].id));
}

#[test]
fn histogram_coverage_needs_an_actionable_leading_key() {
    let mut s = snapshot();
    let id = s.queries()[0].id.clone();
    let mut histogram = finding("missing-histogram", "shop.orders", &[&id]);
    histogram.facts.histogram_columns = vec!["state".into(), "region".into()];
    histogram.metrics = vec![Metric::text("Renamed column label", "unrelated")];
    let mut index = finding("missing-index", "shop.orders(unrelated)", &[&id]);
    index.fix = Some(Fix {
        summary: "Suggested index".into(),
        sql: Some("CREATE INDEX ix_orders ON shop.orders (state, region)".into()),
        cost: None,
    });
    index.facts.index_keys = vec!["state".into(), "region".into()];
    let mut findings = vec![histogram.clone(), index.clone()];
    let trace = indexes::Rule(indexes::Policy::Histogram).refine_traced(&s, &mut findings);
    assert_eq!(findings.len(), 2, "the second key doesn't cover a standalone histogram");
    assert!(
        matches!(&trace[0].outcome,RefinementOutcome::ColumnsCovered {columns,..} if columns==&vec!["state".to_string()])
    );
    histogram.facts.histogram_columns = vec!["state".into()];
    for sql in
        [None, Some("CREATE INDEX ix_orders ON shop.orders (state)\n/* filter */ WHERE state IS NOT NULL".into())]
    {
        index.fix.as_mut().unwrap().sql = sql;
        let mut findings = vec![histogram.clone(), index.clone()];
        assert!(indexes::Rule(indexes::Policy::Histogram).refine_traced(&s, &mut findings).is_empty());
        assert_eq!(findings.len(), 2);
    }
    index.fix.as_mut().unwrap().sql = Some("CREATE INDEX ix_orders ON shop.orders (state)".into());
    for q in &mut s.workload.as_mut().unwrap().queries {
        q.mean_time_ms = 0.001;
        q.total_time_ms = 0.1;
    }
    s.workload.as_mut().unwrap().total_time_ms = 0.2;
    let mut findings = vec![histogram, index];
    assert!(
        indexes::Rule(indexes::Policy::Histogram).refine_traced(&s, &mut findings).is_empty(),
        "a cheap candidate will not retain SQL to cover the histogram"
    );
}

#[test]
fn scan_causality_uses_the_recorded_plan_transition() {
    let mut s = snapshot();
    let q = &mut s.workload.as_mut().unwrap().queries[0];
    let mut before = q.plans.first().unwrap().clone();
    before.id = "before".into();
    before.executions = 20.0;
    before.logical_reads = Some(20.0 * 100.0);
    before.mean_time_ms = 10.0;
    let mut after = before.clone();
    after.id = "after".into();
    after.logical_reads = Some(20.0 * 110.0);
    after.mean_time_ms = 50.0;
    let mut unrelated = before.clone();
    unrelated.id = "unrelated-fast".into();
    unrelated.logical_reads = Some(20.0);
    unrelated.mean_time_ms = 1.0;
    q.plan_id = Some("after".into());
    q.plans = vec![before, after, unrelated];
    let id = q.id.clone();
    let mut scan = finding("filtered-scan", "shop.orders(state)", &[&id]);
    scan.queries[0].plan_node = Some(2);
    let mut regression = finding("regression", &id, &[&id]);
    regression.facts.plan_change = Some(FindingPlanChange { from: "before".into(), to: "after".into() });
    let mut findings = vec![regression.clone(), scan.clone()];
    assert!(
        queries::Rule(queries::Policy::ScanRegression).refine_traced(&s, &mut findings).is_empty(),
        "an unrelated fast plan cannot establish that the transition caused a scan"
    );
    s.workload.as_mut().unwrap().queries[0].plans[1].logical_reads = Some(20.0 * 1000.0);
    let mut findings = vec![regression.clone(), scan.clone()];
    assert_eq!(queries::Rule(queries::Policy::ScanRegression).refine_traced(&s, &mut findings).len(), 1);
    assert!(findings[0].summary.contains("plan before read about 100 pages"));
    regression.facts.plan_change = None;
    regression.metrics.push(Metric::text("Plan change", "before → after"));
    let mut findings = vec![regression, scan];
    assert!(
        queries::Rule(queries::Policy::ScanRegression).refine_traced(&s, &mut findings).is_empty(),
        "a display label cannot supply missing transition evidence"
    );
}

#[test]
fn plan_story_preserves_sensitivity_impact_and_unrelated_query_evidence() {
    let s = snapshot();
    let (id, other) = (&s.queries()[0].id, &s.queries()[1].id);
    let mut rec = finding("tuning-advice", &format!("force:{id}"), &[id]);
    rec.facts.recommended_plan =
        Some(culprit_core::FindingPlanRecommendation { plan: "42".into(), cpu_saved_share: Some(0.5) });
    rec.impact.db_time_share = Some(0.8);
    let mut sensitive = finding("parameter-sensitive", id, &[id]);
    sensitive.impact.db_time_share = Some(0.1);
    sensitive.facts.usual_fast_plan = Some("43".into());
    let grouped = finding("regression", "grouped", &[id, other]);
    let mut findings = vec![rec, sensitive, grouped];
    let trace = queries::Rule(queries::Policy::PlanStory).refine_traced(&s, &mut findings);
    assert_eq!(trace.len(), 1);
    assert_eq!(findings.len(), 2);
    assert_eq!(findings[0].impact.db_time_share, Some(0.1));
    assert!(
        findings[0].caveats[0]
            .contains("forcing plan 42 (estimated 50% less CPU per run), while most calls run fast with plan 43")
    );
    assert_eq!(findings[1].queries.len(), 2);
}

#[test]
fn write_merges_keep_schemas_distinct_and_leave_ambiguous_names_alone() {
    let mut s = snapshot();
    let mut a = s.tables[0].clone();
    a.schema = "tenant_one".into();
    a.name = "orders".into();
    let mut b = a.clone();
    b.schema = "tenant_two".into();
    s.tables = vec![a, b];
    s.columns.clear();
    let q = &mut s.workload.as_mut().unwrap().queries[0];
    q.rows = Some(100.0);
    q.costs = Some(QueryCosts { log_bytes: Some(10_000_000.0), ..Default::default() });
    let id = q.id.clone();
    let mut log = finding("log-volume", &id, &[&id]);
    log.facts.log_write = Some(FindingLogWrite { bytes: 10_000_000.0, per_row: Some(100_000.0) });
    let mut large = finding("large-rows", "tenant_one.orders", &[]);
    large.subject.name = "tenant_one.orders".into();
    large.facts.large_rows = Some(FindingLargeRows { rows: 100.0, size: 100_000_000.0, large: 80_000_000.0 });
    for text in ["UPDATE tenant_two.orders SET payload = @p0", "UPDATE orders SET payload = @p0"] {
        s.workload.as_mut().unwrap().queries[0].text = text.into();
        let mut findings = vec![large.clone(), log.clone()];
        assert!(writes::Rule(writes::Policy::LargeValues).refine_traced(&s, &mut findings).is_empty(), "{text}");
        assert_eq!(findings.len(), 2);
    }
    s.workload.as_mut().unwrap().queries[0].text = "UPDATE tenant_one.orders SET payload = @p0".into();
    let mut findings = vec![large, log];
    assert_eq!(writes::Rule(writes::Policy::LargeValues).refine_traced(&s, &mut findings).len(), 1);
    assert_eq!(findings[0].queries.len(), 1);
    assert!(findings[0].summary.contains("The statement below writes them"));
}

#[test]
fn blocking_chain_must_cover_the_whole_lock_episode() {
    let mut s = snapshot();
    for q in &mut s.workload.as_mut().unwrap().queries {
        q.waits =
            vec![culprit_core::WaitStat { category: "Lock".into(), wait_type: None, wait_ms: 100_000.0, tasks: None }];
    }
    let (id, other) = (&s.queries()[0].id, &s.queries()[1].id);
    let lock = finding("lock-time", "episode", &[id, other]);
    let hot = finding("hot-rows", "shop.orders", &[id]);
    let mut findings = vec![hot.clone(), lock.clone()];
    assert!(locks::Rule(locks::Policy::Chain).refine_traced(&s, &mut findings).is_empty());
    let mut hot = hot;
    hot.queries.push(QueryRef { id: other.clone(), plan_node: None, note: None });
    let mut findings = vec![hot, lock];
    assert_eq!(locks::Rule(locks::Policy::Chain).refine_traced(&s, &mut findings).len(), 1);
}

#[test]
fn absorption_keeps_each_querys_recorded_note() {
    let s = snapshot();
    let (id, other) = (&s.queries()[0].id, &s.queries()[1].id);
    let target = finding("lock-time", id, &[id]);
    let mut source = finding("lock-time", other, &[other]);
    source.queries[0].note = Some("Its own measured wait and period.".into());
    let mut findings = vec![target, source];
    let decision = merge::Absorption::decide(&s, &findings, 0, 1);
    decision.apply_state(&mut findings);
    decision.render(&mut findings, Some("Another statement's wait.".into()));
    decision.finish(&mut findings);
    assert_eq!(findings[0].queries[1].note.as_deref(), Some("Its own measured wait and period."));
}

#[test]
fn partial_automatic_index_transfer_remeasures_the_remaining_queries() {
    let mut s: Snapshot = serde_json::from_str(include_str!("../../../../fixtures/sqlite-demo.json")).unwrap();
    let mut original = s.queries().iter().find(|q| q.text.contains("LEFT OUTER JOIN")).unwrap().clone();
    original.id = "customer-join".into();
    original.executions = 100.0;
    original.total_time_ms = 100_000.0;
    original.mean_time_ms = 1000.0;
    let mut other = original.clone();
    other.id = "region-join".into();
    other.total_time_ms = 200_000.0;
    other.mean_time_ms = 2000.0;
    other.text = other.text.replace("customer_id", "region_id");
    fn change_key(n: &mut culprit_core::PlanNode) {
        if let Some(key) = n.props.get_mut("Automatic Index") {
            *key = key.replace("customer_id", "region_id");
        }
        if let Some(detail) = &mut n.detail {
            *detail = detail.replace("customer_id", "region_id");
        }
        if let Some(predicate) = &mut n.seek_predicate {
            *predicate = predicate.replace("customer_id", "region_id");
        }
        for child in &mut n.children {
            change_key(child);
        }
    }
    change_key(&mut other.plan.as_mut().unwrap().root);
    let w = s.workload.as_mut().unwrap();
    w.queries = vec![original, other];
    w.total_time_ms = 300_000.0;
    let group =
        crate::queries::plan_insights(&s).into_iter().find(|f| f.id == "plan-insights:automatic-index:orders").unwrap();
    assert_eq!(group.queries.len(), 2);
    let mut fk = finding("unindexed-fk", "main.orders(customer_id)", &[]);
    fk.subject.name = "main.orders".into();
    fk.facts.index_keys = vec!["customer_id".into()];
    let mut findings = vec![group.clone(), fk.clone()];
    let trace = indexes::Rule(indexes::Policy::AutomaticForeignKey).refine_traced(&s, &mut findings);
    assert!(
        matches!(&trace[0].outcome,RefinementOutcome::EvidenceTransferred {queries,..} if queries==&vec!["customer-join".to_string()])
    );
    let remaining = &findings[0];
    assert_eq!(remaining.queries.len(), 1);
    assert_eq!(remaining.queries[0].id, "region-join");
    assert!((remaining.impact.db_time_share.unwrap() - 2.0 / 3.0).abs() < 1e-10);
    assert_eq!(remaining.metrics.iter().find(|m| m.label == "Average duration").unwrap().value, 2000.0);
    assert!(!remaining.summary.contains("same problem shows up in 2 queries"));
    assert_eq!(findings[1].queries[0].id, "customer-join");
    let mut incomplete = group;
    incomplete.queries.push(QueryRef { id: "missing-query".into(), plan_node: None, note: None });
    let mut findings = vec![incomplete, fk];
    assert!(
        indexes::Rule(indexes::Policy::AutomaticForeignKey).refine_traced(&s, &mut findings).is_empty(),
        "a partial merge cannot silently drop an unresolved reference"
    );
}
