//! Shared evidence rules, with arbitrary application SQL and all providers. Both quiet and costly controls matter.
use culprit_core::{
    Actionability, Category, Check, CheckInfo, Engine, Finding, Fix, IndexContention, QueryRef, Severity, Snapshot,
};
use std::sync::Arc;

fn snapshot(engine: Engine) -> Snapshot {
    let mut s: Snapshot = serde_json::from_str(include_str!("../../../fixtures/mssql-demo.json")).unwrap();
    s.server.engine = engine;
    s.taken_at = "2026-10-18T12:00:00Z".into();
    s.server.started_at = Some("2026-10-01T00:00:00Z".into());
    s.options = Some(culprit_core::SnapshotOptions::default().with_window_minutes(120));
    if let Some(w) = &mut s.workload {
        w.window_end = Some(s.taken_at.clone());
        w.window_start = None;
        w.timeline = None;
        for q in &mut w.queries {
            q.first_seen = None;
            q.last_seen = None;
            q.trend = None;
        }
    }
    s
}

struct Probe(Finding);
impl Check for Probe {
    fn info(&self) -> CheckInfo {
        CheckInfo {
            id: "example.index".into(),
            title: "Example index".into(),
            category: Category::Indexes,
            engines: vec![Engine::Mssql, Engine::Postgres, Engine::Mysql, Engine::Sqlite],
            requires: vec![],
            description: String::new(),
        }
    }
    fn run(&self, _: &Snapshot) -> Vec<Finding> {
        vec![self.0.clone()]
    }
}

fn candidate(s: &Snapshot) -> Finding {
    let mut f = culprit_checks::analyze(s.clone()).findings.into_iter().next().expect("demo findings");
    f.id = "example.index:shop.orders(customer_id)".into();
    f.check = "example.index".into();
    f.subject.kind = culprit_core::SubjectKind::Query;
    f.subject.name = s.queries()[0].id.clone();
    f.queries = vec![QueryRef { id: s.queries()[0].id.clone(), plan_node: None, note: None }];
    f.inactive = false;
    f.actionability = Actionability::OptionalImprovement;
    f.severity = Severity::Low;
    f.fix = Some(Fix {
        summary: "Evaluate an index.".into(),
        sql: Some("CREATE INDEX ix_orders_customer ON shop.orders(customer_id);".into()),
        cost: None,
    });
    f
}

#[test]
fn measured_index_gate_is_provider_independent_and_keeps_costly_controls() {
    for engine in [Engine::Mssql, Engine::Postgres, Engine::Mysql, Engine::Sqlite] {
        for (runs, mean, actionable) in [(2.0, 7.0, false), (2.0, 50.0, true), (1_000_000.0, 0.4, true)] {
            let mut s = snapshot(engine);
            let w = s.workload.as_mut().unwrap();
            w.windowed = true;
            w.window_hours = 2.0;
            w.timeline = None;
            w.queries.truncate(1);
            let q = &mut w.queries[0];
            q.text = "SELECT * FROM shop.orders WHERE customer_id = 1".into();
            q.plan = None;
            q.first_seen = None;
            q.last_seen = None;
            q.trend = None;
            q.executions = runs;
            q.total_time_ms = runs * mean;
            q.mean_time_ms = mean;
            w.total_time_ms = runs * mean;
            w.total_executions = runs;
            let f = candidate(&s);
            let r = culprit_checks::analyze_with(s, &[Arc::new(Probe(f))]);
            let f = r.findings.iter().find(|f| f.check == "example.index").unwrap();
            assert_eq!(f.is_actionable(), actionable, "{engine:?}, {runs} x {mean}: {}", f.summary);
            assert_eq!(f.fix.as_ref().unwrap().sql.is_some(), actionable);
            assert_eq!(r.summary.opportunities as usize, r.findings.iter().filter(|f| f.is_actionable()).count());
            assert_eq!(
                r.summary.by_severity.iter().map(|s| s.count as usize).sum::<usize>(),
                r.summary.opportunities as usize
            );
        }
    }
}

#[test]
fn explicit_information_stays_visible_without_an_opportunity_or_digest_alert() {
    let s = snapshot(Engine::Mssql);
    let mut f = candidate(&s);
    f.actionability = Actionability::Information;
    f.severity = Severity::High;
    f.fix.as_mut().unwrap().sql = None;
    let r = culprit_checks::analyze_with(s, &[Arc::new(Probe(f))]);
    let f = r.findings.iter().find(|f| f.check == "example.index").unwrap();
    assert_eq!(f.actionability, Actionability::Information);
    assert!(!f.is_actionable());
    let digest = culprit_checks::digest(&r, None, 100, 10);
    assert!(!digest.top.iter().any(|f| f.id.starts_with("example.index:")));
    assert!(digest.findings.iter().any(|f| f.id.starts_with("example.index:")));
    let brief = culprit_checks::agent_brief(&r, f);
    assert!(brief.contains("requests no immediate database change"));
}

#[test]
fn timed_lock_counters_distinguish_current_history_missing_and_reset() {
    let mut s = snapshot(Engine::Mssql);
    for i in &mut s.indexes {
        i.contention = None;
    }
    s.indexes[0].contention =
        Some(IndexContention { row_lock_waits: 1.0, row_lock_wait_ms: 13_000.0, ..Default::default() });
    let mut old = s.clone();
    old.taken_at = "2026-10-18T10:00:00Z".into();
    let classify =
        |s: Snapshot| culprit_checks::analyze(s).findings.into_iter().find(|f| f.check == "hot-rows").unwrap();
    assert_eq!(classify(s.clone()).actionability, Actionability::Information);
    s.contention_before = culprit_core::ContentionBaseline::from_scan(&old, &s);
    let f = classify(s.clone());
    assert_eq!(f.actionability, Actionability::History);
    assert!(f.title.starts_with("No new lock waits"));
    s.indexes[0].contention.as_mut().unwrap().row_lock_waits = 101.0;
    s.indexes[0].contention.as_mut().unwrap().row_lock_wait_ms = 1_213_000.0;
    let f = classify(s.clone());
    assert_eq!(f.actionability, Actionability::NeedsAttention);
    assert_eq!(f.severity, Severity::High);
    let r = culprit_checks::analyze(s.clone());
    assert!(!culprit_checks::gone_after_rescan(&r, r.findings.iter().find(|f| f.check == "hot-rows").unwrap()));
    for minutes in [1440, 7 * 1440, 30 * 1440] {
        s.options = Some(culprit_core::SnapshotOptions::default().with_window_minutes(minutes));
        assert_eq!(classify(s.clone()).actionability, Actionability::NeedsAttention);
    }
    // The same growth over a period extending outside the selected window is undated context.
    s.options = Some(culprit_core::SnapshotOptions::default().with_window_minutes(60));
    assert_eq!(classify(s.clone()).actionability, Actionability::Information);
    s.contention_before.as_mut().unwrap().indexes.values_mut().for_each(|c| c.waits = 1000.0);
    assert!(s.contention_before.as_ref().unwrap().since(&s.indexes[0]).is_none());
    assert_eq!(classify(s.clone()).actionability, Actionability::Information);
    old.server.started_at = None;
    assert!(culprit_core::ContentionBaseline::from_scan(&old, &s).is_none());
    old.server.started_at = Some("2026-10-18T09:00:00Z".into());
    assert!(culprit_core::ContentionBaseline::from_scan(&old, &s).is_none());
    old.server.started_at = s.server.started_at.clone();
    old.taken_at = "2026-10-18T13:00:00Z".into();
    assert!(culprit_core::ContentionBaseline::from_scan(&old, &s).is_none());
}

#[test]
fn selected_window_changes_the_amount_behind_index_advice() {
    for engine in [Engine::Mssql, Engine::Postgres, Engine::Mysql, Engine::Sqlite] {
        for (hours, actionable) in [(1.0, true), (24.0, true), (168.0, false), (720.0, false)] {
            let mut s = snapshot(engine);
            s.options = Some(culprit_core::SnapshotOptions::default().with_window_minutes((hours * 60.0) as u32));
            let w = s.workload.as_mut().unwrap();
            w.windowed = true;
            w.window_hours = hours;
            w.timeline = None;
            w.queries.truncate(1);
            let q = &mut w.queries[0];
            q.text = "SELECT * FROM shop.orders WHERE customer_id = 1".into();
            q.plan = None;
            q.first_seen = None;
            q.last_seen = None;
            q.trend = None;
            q.executions = 240_000.0;
            q.total_time_ms = 120_000.0;
            q.mean_time_ms = 0.5;
            w.total_time_ms = q.total_time_ms;
            w.total_executions = q.executions;
            let r = culprit_checks::analyze_with(s.clone(), &[Arc::new(Probe(candidate(&s)))]);
            let f = r.findings.iter().find(|f| f.check == "example.index").unwrap();
            assert_eq!(f.is_actionable(), actionable, "{engine:?}, {hours}h");
        }
    }
}

#[test]
fn schema_policy_without_query_evidence_keeps_its_own_justification() {
    let s = snapshot(Engine::Mssql);
    let mut f = candidate(&s);
    f.subject.kind = culprit_core::SubjectKind::Table;
    f.subject.name = "shop.orders".into();
    f.queries.clear();
    let r = culprit_checks::analyze_with(s, &[Arc::new(Probe(f))]);
    let f = r.findings.iter().find(|f| f.check == "example.index").unwrap();
    assert!(f.is_actionable());
    assert!(f.fix.as_ref().unwrap().sql.is_some());
}

#[test]
fn an_old_index_request_is_context_even_when_its_query_still_runs() {
    for (hours, actionable) in [(1.0, false), (24.0, true)] {
        let mut s = snapshot(Engine::Mssql);
        s.taken_at = "2026-10-18T12:00:00Z".into();
        s.options = Some(culprit_core::SnapshotOptions::default().with_window_minutes((hours * 60.0) as u32));
        s.indexes.retain(|i| i.table != "support_tickets" || i.is_primary_key);
        s.missing_indexes = vec![culprit_core::MissingIndex {
            schema: "shop".into(),
            table: "support_tickets".into(),
            equality: vec!["customer_id".into()],
            inequality: vec!["due_at".into()],
            include: vec!["status".into()],
            seeks: 400.0,
            scans: 0.0,
            avg_query_cost: 1.0,
            avg_impact_pct: 95.0,
            last_used: Some("2026-10-18T10:30:00Z".into()),
            query_hashes: vec!["0xABC0A11".into()],
        }];
        let w = s.workload.as_mut().unwrap();
        w.windowed = true;
        w.window_hours = hours;
        w.window_end = Some(s.taken_at.clone());
        w.timeline = None;
        w.queries.truncate(1);
        let q = &mut w.queries[0];
        q.text = "SELECT status FROM shop.support_tickets WHERE customer_id = @id AND due_at > @date".into();
        q.query_hash = Some("0xABC0A11".into());
        q.plan = None;
        q.plans.clear();
        q.first_seen = None;
        q.last_seen = None;
        q.trend = None;
        q.executions = 2000.0;
        q.total_time_ms = 40_000.0;
        q.mean_time_ms = 20.0;
        w.total_time_ms = q.total_time_ms;
        w.total_executions = q.executions;
        let r = culprit_checks::analyze(s);
        let f = r
            .findings
            .iter()
            .find(|f| f.id == "missing-index:shop.support_tickets(customer_id,due_at)")
            .expect("candidate");
        assert_eq!(f.is_actionable(), actionable, "{hours}h: {}", f.summary);
        assert!(!f.inactive, "the associated query still runs");
        assert_eq!(f.fix.as_ref().unwrap().sql.is_some(), actionable);
        assert_eq!(
            culprit_checks::index_advice(&r).candidates.iter().any(|c| c.finding_id.as_deref() == Some(&f.id)),
            actionable
        );
    }
}

#[test]
fn a_digest_cannot_call_missing_cumulative_evidence_fixed() {
    let s = snapshot(Engine::Mssql);
    let mut f = candidate(&s);
    f.fix.as_mut().unwrap().sql = None;
    f.verification_basis = culprit_core::VerificationBasis::CounterChange;
    let before = culprit_checks::analyze_with(s.clone(), &[Arc::new(Probe(f))]);
    let after = culprit_checks::analyze(s);
    let previous = culprit_checks::digest(&before, None, 100, 0);
    let next = culprit_checks::digest_since(&after, &previous, 100, 0);
    assert!(!next.since.unwrap().fixed.iter().any(|f| f.id.starts_with("example.index:")));
}

#[test]
fn query_lock_evidence_does_not_inherit_lifetime_wait_totals() {
    let mut s = snapshot(Engine::Mssql);
    for i in &mut s.indexes {
        i.contention = None;
    }
    let index = &mut s.indexes[0];
    index.contention =
        Some(IndexContention { row_lock_waits: 100_000.0, row_lock_wait_ms: 360_000_000.0, ..Default::default() });
    let table = format!("{}.{}", index.schema, index.table);
    let w = s.workload.as_mut().unwrap();
    w.windowed = true;
    w.window_hours = 2.0;
    w.timeline = None;
    w.queries.truncate(1);
    let q = &mut w.queries[0];
    q.text = format!("UPDATE {table} SET id = id WHERE id = @id");
    q.plan = None;
    q.plans.clear();
    q.costs = None;
    q.first_seen = None;
    q.last_seen = None;
    q.trend = None;
    q.executions = 10.0;
    q.total_time_ms = 25_000.0;
    q.mean_time_ms = 2500.0;
    q.rows = Some(10.0);
    q.waits = vec![culprit_core::WaitStat { category: "Lock".into(), wait_type: None, wait_ms: 20_000.0, tasks: None }];
    w.total_time_ms = q.total_time_ms;
    w.total_executions = q.executions;
    let r = culprit_checks::analyze(s);
    let f = r.findings.iter().find(|f| f.check == "hot-rows").expect("merged lock evidence");
    assert!(f.is_actionable());
    assert_eq!(
        f.impact.label, "20.0 s waiting on locks",
        "the workload measured 20 seconds, independently of 100 hours in historical counters"
    );
}
