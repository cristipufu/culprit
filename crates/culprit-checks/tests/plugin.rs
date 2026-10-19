//! A check defined outside the crate, using only the public API, behaves exactly like a built-in one.

use std::sync::Arc;

use culprit_core::{
    Capability, Category, Check, CheckInfo, Confidence, Engine, Finding, Fix, Impact, Metric, Severity, Snapshot,
    Subject, SubjectKind, Unit,
};

struct BigTables;

impl Check for BigTables {
    fn info(&self) -> CheckInfo {
        CheckInfo {
            id: "acme.big-table".into(),
            title: "Tables over a million rows".into(),
            category: Category::Maintenance,
            engines: vec![Engine::Postgres, Engine::Mssql],
            requires: vec![Capability::TableStats],
            description: "Team policy: tables this big need a retention plan.".into(),
        }
    }

    fn run(&self, snapshot: &Snapshot) -> Vec<Finding> {
        snapshot
            .tables
            .iter()
            .filter(|t| t.rows.unwrap_or(0.0) >= 1_000_000.0)
            .map(|t| Finding {
                id: format!("acme.big-table:{}", t.qualified()),
                check: "acme.big-table".into(),
                category: Category::Maintenance,
                severity: Severity::Medium,
                actionability: Default::default(),
                confidence: Confidence::High,
                title: format!("{} has no retention plan", t.name),
                summary: "Our policy asks for one above a million rows.".into(),
                subject: Subject { kind: SubjectKind::Table, name: t.qualified() },
                impact: Impact { score: 99.0, db_time_share: None, time_ms_per_day: None, label: "Policy".into() },
                metrics: vec![Metric::new("Rows", t.rows.unwrap_or(0.0), Unit::Rows)],
                queries: Vec::new(),
                fix: Some(Fix { summary: "Add a purge job.".into(), sql: None, cost: None }),
                verify: Vec::new(),
                verification_basis: Default::default(),
                caveats: Vec::new(),
                last_seen: None,
                inactive: false,
            })
            .collect()
    }
}

struct NeedsQueryStore;

impl Check for NeedsQueryStore {
    fn info(&self) -> CheckInfo {
        CheckInfo {
            id: "acme.qs-only".into(),
            title: "Something only Query Store can answer".into(),
            category: Category::Queries,
            engines: vec![Engine::Postgres],
            requires: vec![Capability::QueryStore],
            description: String::new(),
        }
    }

    fn run(&self, _: &Snapshot) -> Vec<Finding> {
        panic!("a locked check must never run");
    }
}

fn demo_snapshot() -> Snapshot {
    serde_json::from_str(include_str!("../../../fixtures/postgres-demo.json")).expect("fixture is a snapshot")
}

#[test]
fn plugged_in_checks_run_rank_and_lock_like_built_ins() {
    let extra: Vec<Arc<dyn Check>> = vec![Arc::new(BigTables), Arc::new(NeedsQueryStore)];
    let report = culprit_checks::analyze_with(demo_snapshot(), &extra);

    let first = &report.findings[0];
    assert_eq!(first.id, "acme.big-table:public.orders", "a high-scoring plugged-in finding ranks first");
    assert!(report.summary.opportunities as usize >= 2);
    assert!(report.locked.iter().any(|l| l.check.id == "acme.qs-only" && l.missing == vec![Capability::QueryStore]));

    let ids: Vec<String> = culprit_checks::infos_with(&extra).into_iter().map(|i| i.id).collect();
    assert!(ids.contains(&"acme.big-table".to_string()) && ids.contains(&"missing-index".to_string()));
}

#[test]
fn a_plugged_in_check_cannot_replace_a_built_in_id() {
    struct Impostor;
    impl Check for Impostor {
        fn info(&self) -> CheckInfo {
            CheckInfo {
                id: "unused-index".into(),
                title: "Impostor".into(),
                category: Category::Indexes,
                engines: vec![Engine::Postgres],
                requires: Vec::new(),
                description: String::new(),
            }
        }
        fn run(&self, _: &Snapshot) -> Vec<Finding> {
            panic!("an impostor must never run");
        }
    }
    let extra: Vec<Arc<dyn Check>> = vec![Arc::new(Impostor)];
    let report = culprit_checks::analyze_with(demo_snapshot(), &extra);
    assert!(report.findings.iter().all(|f| f.title != "Impostor"));
    assert_eq!(culprit_checks::infos_with(&extra).iter().filter(|i| i.id == "unused-index").count(), 1);
}
