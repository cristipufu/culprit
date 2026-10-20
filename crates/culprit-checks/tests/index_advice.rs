//! The index advisor and the checks behind it, on the three demo snapshots with planted conditions.

use culprit_checks::{analyze, index_advice};
use culprit_core::{
    AutomaticTuningOption, CleanupKind, Finding, ForcedPlan, MissingIndex, PlanCacheUse, PlanNode, QueryFamily,
    RecommendedIndex, Setting, Severity, Snapshot, TableAdviceKind, TuningAdvice, TuningRecommendation,
};

fn snapshot(name: &str) -> Snapshot {
    let text =
        std::fs::read_to_string(format!("{}/../../fixtures/{name}", env!("CARGO_MANIFEST_DIR"))).expect("fixture");
    serde_json::from_str(&text).expect("snapshot")
}

/// Counters that have been running for ten days, so per-day figures are meaningful. PostgreSQL's age is the one its
/// scan measured (its counters outlive a restart), the others' the time since the server started.
fn ten_days(mut s: Snapshot) -> Snapshot {
    s.server.started_at = Some("2026-10-01T12:00:00Z".into());
    s.taken_at = "2026-10-11T12:00:00Z".into();
    if s.server.engine == culprit_core::Engine::Postgres {
        s.pressure.get_or_insert_with(Default::default).uptime_hours = Some(240.0);
    }
    crate::counter_age::digests_counted_since_the_start(&mut s);
    s
}

fn setting(name: &str, value: &str, scope: &str) -> Setting {
    Setting { name: name.into(), value: value.into(), default_value: None, scope: scope.into() }
}

fn ids(s: Snapshot) -> Vec<String> {
    analyze(s).findings.into_iter().map(|f| f.id).collect()
}

#[test]
fn postgres_tables_read_in_full_are_flagged_and_advised() {
    let mut s = ten_days(snapshot("postgres-demo.json"));
    // A large table nobody's captured plan explains: read in full 400 times a day.
    let t = s.tables.iter_mut().find(|t| t.rows.unwrap_or(0.0) >= 50_000.0).expect("a large table");
    let rows = t.rows.unwrap();
    t.name = "audit_log".into();
    t.seq_scans = Some(4000.0);
    t.seq_rows_read = Some(4000.0 * rows);
    let report = analyze(s);
    let finding = report
        .findings
        .iter()
        .find(|f| f.id.starts_with("scan-heavy:") && f.id.ends_with("audit_log"))
        .expect("scan-heavy");
    assert!(finding.title.contains("read in full about 400 times a day"), "{}", finding.title);
    let advice = index_advice(&report);
    let table =
        advice.tables.iter().find(|t| t.kind == TableAdviceKind::ScanHeavy && t.table == "audit_log").expect("advised");
    assert_eq!(table.finding_id.as_deref(), Some(finding.id.as_str()));
    assert!(advice.notes.iter().any(|n| n.contains("PostgreSQL doesn't record missing-index requests")));
}

// The advisor lists a table read in full only where the scan-heavy finding does, with that finding's statements: full
// reads that all come from a statement with no WHERE (a report over every row) have no filter to index, so neither the
// finding nor the advisor speaks.
#[test]
fn the_advisor_lists_a_table_read_in_full_only_with_its_finding() {
    let planted = |where_clause: bool| {
        let mut s = ten_days(snapshot("postgres-demo.json"));
        let t = s.tables.iter_mut().find(|t| t.name == "events").expect("events");
        let rows = t.rows.unwrap();
        t.seq_scans = Some(4000.0);
        t.seq_rows_read = Some(4000.0 * rows);
        let w = s.workload.as_mut().unwrap();
        let mut q = w.queries.iter().find(|q| q.id == "-3722457391793582113").unwrap().clone();
        q.id = "4242".into();
        q.text = if where_clause {
            "SELECT count(*) FROM events WHERE kind = $1".into()
        } else {
            "SELECT kind, count(*) FROM events GROUP BY kind".into()
        };
        q.executions = 4000.0;
        fn retarget(n: &mut PlanNode) {
            if n.object.is_some() {
                n.object = Some("public.events".into());
                n.predicate = None;
            }
            n.children.iter_mut().for_each(retarget);
        }
        retarget(&mut q.plan.as_mut().unwrap().root);
        w.queries.push(q);
        analyze(s)
    };
    let report = planted(true);
    let finding = report.findings.iter().find(|f| f.id == "scan-heavy:public.events").expect("scan-heavy");
    let advice = index_advice(&report);
    let table =
        advice.tables.iter().find(|t| t.kind == TableAdviceKind::ScanHeavy && t.table == "events").expect("listed");
    assert_eq!(table.finding_id.as_deref(), Some(finding.id.as_str()));
    assert_eq!(
        table.queries.iter().map(|q| q.id.as_str()).collect::<Vec<_>>(),
        finding.queries.iter().map(|q| q.id.as_str()).collect::<Vec<_>>()
    );
    assert!(
        table.reason.ends_with("Explain the statement listed with it to check its access path, then measure rows kept in representative runs."),
        "{}",
        table.reason
    );

    let report = planted(false);
    assert!(!report.findings.iter().any(|f| f.id == "scan-heavy:public.events"), "{:?}", ids_of(&report));
    assert!(!index_advice(&report).tables.iter().any(|t| t.kind == TableAdviceKind::ScanHeavy && t.table == "events"));
}

fn ids_of(report: &culprit_core::Report) -> Vec<&str> {
    report.findings.iter().map(|f| f.id.as_str()).collect()
}

#[test]
fn mysql_rows_read_without_an_index_count_as_full_reads() {
    let mut s = ten_days(snapshot("mysql-demo.json"));
    let t = s.tables.iter_mut().find(|t| t.rows.unwrap_or(0.0) >= 50_000.0).expect("a large table");
    t.name = "events".into();
    t.seq_scans = None;
    t.seq_rows_read = Some(t.rows.unwrap() * 2000.0);
    let report = analyze(s);
    assert!(report.findings.iter().any(|f| f.id.ends_with("events") && f.id.starts_with("scan-heavy:")));
}

#[test]
fn overlapping_and_over_indexed_tables_on_every_engine() {
    for name in ["mssql-demo.json", "postgres-demo.json", "mysql-demo.json"] {
        let mut s = ten_days(snapshot(name));
        let (schema, table) = {
            let t = s.tables.iter().max_by(|a, b| a.rows.unwrap_or(0.0).total_cmp(&b.rows.unwrap_or(0.0))).unwrap();
            (t.schema.clone(), t.name.clone())
        };
        let template = s.indexes.iter().find(|i| i.schema == schema && i.table == table).cloned().expect("an index");
        let mut add = |name: &str, columns: &[&str], reads: f64| {
            let mut i = template.clone();
            i.name = name.into();
            i.columns = columns.iter().map(|c| c.to_string()).collect();
            i.include.clear();
            i.filter = None;
            i.is_primary_key = false;
            i.is_unique = false;
            i.is_clustered = false;
            i.is_disabled = false;
            i.reads = Some(reads);
            i.writes = Some(5_000_000.0);
            i.size_bytes = Some(64.0 * 1024.0 * 1024.0);
            s.indexes.push(i);
        };
        // ix_busy holds every key of ix_rare, so it can serve ix_rare's queries. ix_endpoint only shares the tenant
        // column: in a multi-tenant table that says nothing about whether ix_busy could replace it.
        add("ix_busy", &["zz_tenant", "zz_created", "zz_status"], 900_000.0);
        add("ix_rare", &["zz_tenant", "zz_status"], 12.0);
        add("ix_endpoint", &["zz_tenant", "zz_endpoint"], 12.0);
        for n in 0..7 {
            add(&format!("ix_extra_{n}"), &[&format!("zz_col_{n}")], 5000.0);
        }
        let t = s.tables.iter_mut().find(|t| t.schema == schema && t.name == table).unwrap();
        t.inserts = Some(9_000_000.0);
        t.updates = Some(1_000_000.0);
        t.deletes = Some(0.0);
        for i in s
            .indexes
            .iter_mut()
            .filter(|i| i.schema == schema && i.table == table && (i.is_clustered || i.is_primary_key))
        {
            i.writes = Some(10_000_000.0);
        }
        let report = analyze(s);
        let found: Vec<&str> = report.findings.iter().map(|f| f.id.as_str()).collect();
        assert!(
            found.iter().any(|id| id.starts_with("overlapping-index:") && id.ends_with("ix_rare")),
            "{name}: {found:?}"
        );
        assert!(
            !found.iter().any(|id| id.starts_with("overlapping-index:") && id.ends_with("ix_endpoint")),
            "{name}: {found:?}"
        );
        assert!(found.iter().any(|id| id.starts_with("over-indexed:")), "{name}: {found:?}");
        let advice = index_advice(&report);
        assert!(
            advice.cleanups.iter().any(|c| c.kind == CleanupKind::Overlap
                && c.index == "ix_rare"
                && c.other.as_deref() == Some("ix_busy"))
        );
        assert!(advice.tables.iter().any(|t| t.kind == TableAdviceKind::OverIndexed));
    }
}

// CX-13: a staging SQL Server reading forced a plan by hand with FORCE_LAST_GOOD_PLAN on and said nothing of it, and
// SQL Server's own recommendation asked to turn automatic plan correction on while it was on. Every fix that forces a
// plan says what automatic correction does where it is on, and asks for it only where it is off.
#[test]
fn a_plan_forced_by_hand_says_what_automatic_plan_correction_does() {
    let fix = |actual: &str| {
        let mut s = snapshot("mssql-demo.json");
        let query = s.workload.as_ref().unwrap().queries[0].id.clone();
        s.tuning = Some(TuningAdvice {
            automatic_tuning: vec![AutomaticTuningOption {
                name: "FORCE_LAST_GOOD_PLAN".into(),
                desired: actual.into(),
                actual: actual.into(),
                reason: None,
            }],
            recommendations: vec![TuningRecommendation {
                name: "PR_1".into(),
                kind: "FORCE_LAST_GOOD_PLAN".into(),
                state: Some("Active".into()),
                query_id: Some(query.clone()),
                regressed_plan_id: Some("685".into()),
                recommended_plan_id: Some("815".into()),
                estimated_gain: Some(0.73),
                ..Default::default()
            }],
            ..Default::default()
        });
        let id = format!("tuning-advice:force:{query}");
        analyze(s).findings.into_iter().find(|f| f.id == id).expect("the recommendation").fix.unwrap().summary
    };
    let on = fix("ON");
    assert!(
        on.contains("Automatic plan correction (FORCE_LAST_GOOD_PLAN) is on for this database: SQL Server forces the last good plan itself"),
        "{on}"
    );
    assert!(on.contains("A plan forced by hand stays forced until someone unforces it"), "{on}");
    assert!(!on.contains("turn on automatic plan correction"), "{on}");
    let off = fix("OFF");
    assert!(off.ends_with("To let SQL Server do this on its own, turn on automatic plan correction."), "{off}");
    assert!(!off.contains("is on for this database"), "{off}");
}

#[test]
fn sql_server_tuning_advice_and_forced_plan_failures() {
    let mut s = snapshot("mssql-demo.json");
    let query = s.workload.as_ref().unwrap().queries[0].id.clone();
    s.tuning = Some(TuningAdvice {
        automatic_tuning: vec![AutomaticTuningOption {
            name: "FORCE_LAST_GOOD_PLAN".into(),
            desired: "OFF".into(),
            actual: "OFF".into(),
            reason: None,
        }],
        recommendations: vec![TuningRecommendation {
            name: "PR_1".into(),
            kind: "FORCE_LAST_GOOD_PLAN".into(),
            state: Some("Active".into()),
            query_id: Some(query.clone()),
            regressed_plan_id: Some("685".into()),
            recommended_plan_id: Some("815".into()),
            estimated_gain: Some(0.73),
            script: Some(format!("exec sp_query_store_force_plan @query_id = {query}, @plan_id = 815")),
            ..Default::default()
        }],
        forced_plans: vec![ForcedPlan {
            query_id: "16129".into(),
            plan_id: "40".into(),
            failures: 34.0,
            last_failure_reason: Some("NO_INDEX".into()),
            last_execution: None,
            forcing: None,
        }],
        hints: Vec::new(),
    });
    let report = analyze(s);
    let titles: Vec<&str> =
        report.findings.iter().filter(|f| f.check == "tuning-advice").map(|f| f.title.as_str()).collect();
    assert!(titles.iter().any(|t| t.contains("suggests forcing plan 815") && t.contains("73%")), "{titles:?}");
    assert!(titles.iter().any(|t| t.contains("stopped applying 34 times (NO_INDEX)")), "{titles:?}");
    assert!(titles.iter().any(|t| t.contains("Automatic plan correction is off")), "{titles:?}");
    let force = report.findings.iter().find(|f| f.id == format!("tuning-advice:force:{query}")).unwrap();
    assert!(force.fix.as_ref().unwrap().sql.as_deref().unwrap().contains("sp_query_store_force_plan"));
}

// Automatic tuning forced plan 9 for query 9 on Oct 3 and is verifying it: that is no longer a suggestion. The plan
// scans order_items for an order id; once an index leads with order_id, the forcing is what keeps the query scanning.
#[test]
fn forced_plans_are_listed_and_one_that_scans_past_a_newer_index_stands_out() {
    let mut s = snapshot("mssql-demo.json");
    s.tuning = Some(TuningAdvice {
        recommendations: vec![TuningRecommendation {
            name: "PR_9".into(),
            kind: "FORCE_LAST_GOOD_PLAN".into(),
            state: Some("Verifying".into()),
            query_id: Some("9".into()),
            regressed_plan_id: Some("3".into()),
            recommended_plan_id: Some("9".into()),
            applied_at: Some("2026-10-03T08:00:00Z".into()),
            ..Default::default()
        }],
        forced_plans: vec![
            ForcedPlan {
                query_id: "9".into(),
                plan_id: "9".into(),
                forcing: Some("AUTO".into()),
                ..Default::default()
            },
            ForcedPlan {
                query_id: "90001".into(),
                plan_id: "4".into(),
                forcing: Some("MANUAL".into()),
                last_execution: Some("2026-10-02T06:00:00Z".into()),
                ..Default::default()
            },
        ],
        ..Default::default()
    });
    let tuning = |s: &Snapshot| -> Vec<Finding> {
        analyze(s.clone()).findings.into_iter().filter(|f| f.check == "tuning-advice").collect()
    };

    let findings = tuning(&s);
    assert!(findings.iter().all(|f| f.id != "tuning-advice:force:9"), "Verifying isn't a suggestion");
    let listed = findings.iter().find(|f| f.id == "tuning-advice:forced-plans").expect("the forced plans");
    assert_eq!(listed.title, "2 plans are forced in Query Store, 1 by automatic tuning");
    assert_eq!(listed.severity, Severity::Low);
    // CX-10a: what forcing is for, in a sentence that parses.
    assert!(
        listed.summary.starts_with(
            "SQL Server runs these queries with the plan forced for each instead of choosing one, even after an index \
             is added or the data grows. A forced plan keeps a query on the plan it ran fast with, so a new plan can't \
             slow it down."
        ),
        "{}",
        listed.summary
    );
    let list = &listed.caveats[0];
    assert!(list.contains(": plan 9, forced by automatic tuning on Oct 3."), "{list}");
    assert!(list.contains("Query 90001: plan 4, forced by hand; not run since Oct 2."), "{list}");
    // CX-L: with automatic plan correction off (or unknown), nothing checks a forced plan.
    assert!(
        listed.summary.ends_with(
            "Once the query or its indexes change, a forced plan can become the slower one without anything saying so."
        ),
        "{}",
        listed.summary
    );
    // On, it keeps checking the plan it forced; only the one forced by hand goes unchecked.
    let mut on = s.clone();
    on.tuning.as_mut().unwrap().automatic_tuning = vec![AutomaticTuningOption {
        name: "FORCE_LAST_GOOD_PLAN".into(),
        desired: "ON".into(),
        actual: "ON".into(),
        reason: None,
    }];
    let checked = tuning(&on).into_iter().find(|f| f.id == "tuning-advice:forced-plans").expect("the forced plans");
    assert!(
        checked.summary.ends_with(
            "Automatic plan correction forced one of them after its query got slower with a new plan. It is still on, \
             so it keeps checking the plans it forced and unforces one that turns out slower. The other plan stays \
             forced until someone unforces it: once its query or indexes change, it can become the slower one \
             without anything saying so."
        ),
        "{}",
        checked.summary
    );
    on.tuning.as_mut().unwrap().forced_plans.retain(|p| p.query_id == "9");
    let all = tuning(&on).into_iter().find(|f| f.id == "tuning-advice:forced-plans").expect("the forced plan");
    assert!(!all.summary.contains("without anything saying so"), "{}", all.summary);
    assert!(all.summary.ends_with("unforces one that turns out slower."), "{}", all.summary);
    assert!(
        listed
            .fix
            .as_ref()
            .unwrap()
            .sql
            .as_deref()
            .unwrap()
            .contains("sp_query_store_unforce_plan @query_id = 9, @plan_id = 9")
    );

    // An index that leads with order_id arrives: the forced plan still scans.
    let mut index = s.indexes.iter().find(|i| i.name == "ix_order_items_product_id").unwrap().clone();
    index.name = "ix_order_items_order_id".into();
    index.columns = vec!["order_id".into()];
    s.indexes.push(index);
    let findings = tuning(&s);
    let scan = findings.iter().find(|f| f.id == "tuning-advice:forced-scan:9").expect("the scanning forced plan");
    assert!(scan.title.ends_with("scan order_items though ix_order_items_order_id could seek it"), "{}", scan.title);
    assert!(scan.summary.contains("Plan 9 is forced by automatic tuning on Oct 3 for query 9."), "{}", scan.summary);
    assert!(scan.summary.contains("reads all of shop.order_items (1.5M rows)"), "{}", scan.summary);
    // Severity needs an amount: the demo's query takes 35 s a day, so this is Low; at an hour a day it stands out.
    assert_eq!(scan.severity, Severity::Low);
    let mut busy = s.clone();
    let w = busy.workload.as_mut().unwrap();
    w.total_time_ms *= 100.0;
    for q in &mut w.queries {
        q.total_time_ms *= 100.0;
        q.mean_time_ms *= 100.0;
    }
    let busy_scan =
        tuning(&busy).into_iter().find(|f| f.id == "tuning-advice:forced-scan:9").expect("the scanning forced plan");
    assert!(busy_scan.severity >= Severity::Medium, "{:?}", busy_scan.severity);
    assert_eq!(scan.queries[0].plan_node, Some(1));
    assert_eq!(
        scan.fix.as_ref().unwrap().sql.as_deref(),
        Some("EXEC sp_query_store_unforce_plan @query_id = 9, @plan_id = 9;")
    );
    // The other one is still listed.
    let listed = findings.iter().find(|f| f.id == "tuning-advice:forced-plans").expect("the forced plans");
    assert_eq!(listed.title, "A plan is forced in Query Store");
}

fn node_mut(node: &mut PlanNode, id: u32) -> Option<&mut PlanNode> {
    if node.id == id {
        return Some(node);
    }
    node.children.iter_mut().find_map(|child| node_mut(child, id))
}

// A scan of a filtered index reads only that index's rows (a queue's pending rows, empty most of the time), however
// large the table: the plan's table cardinality is the whole table's, and must not make it read as a full scan.
#[test]
fn a_scan_of_a_small_filtered_index_is_not_a_read_of_the_whole_table() {
    let mut s = snapshot("mssql-demo.json");
    s.tuning = Some(TuningAdvice {
        forced_plans: vec![ForcedPlan {
            query_id: "9".into(),
            plan_id: "9".into(),
            forcing: Some("AUTO".into()),
            ..Default::default()
        }],
        ..Default::default()
    });
    // An index that could seek it, so a forced scan of order_items stands out.
    let mut seekable = s.indexes.iter().find(|i| i.name == "ix_order_items_product_id").unwrap().clone();
    seekable.name = "ix_order_items_order_id".into();
    seekable.columns = vec!["order_id".into()];
    s.indexes.push(seekable.clone());
    let mut pending = seekable;
    pending.name = "ix_order_items_pending".into();
    pending.columns = vec!["product_id".into()];
    pending.filter = Some("([quantity]=(0))".into());
    s.indexes.push(pending);
    // The forced plan's scan is of that filtered index, and reads its one row.
    let query = s.workload.as_mut().unwrap().queries.iter_mut().find(|q| q.id == "9").unwrap();
    let node = node_mut(&mut query.plan.as_mut().unwrap().root, 1).unwrap();
    node.index = Some("[ix_order_items_pending]".into());
    node.rows_read = Some(1.0);
    let forced_scan =
        |s: &Snapshot| analyze(s.clone()).findings.into_iter().any(|f| f.id == "tuning-advice:forced-scan:9");

    assert!(!forced_scan(&s), "reading one row of a filtered index is no scan of the table");
    // The same scan of an index without a filter is taken at the table's size.
    s.indexes.iter_mut().find(|i| i.name == "ix_order_items_pending").unwrap().filter = None;
    assert!(forced_scan(&s), "an unfiltered index scan reads the whole table");
}

// Azure SQL Database recommends creating and dropping indexes too. One the optimizer asks for already has a
// missing-index finding; one an index added since covers is out of date; one nothing else flags is a finding.
#[test]
fn azure_index_recommendations_are_findings_unless_another_finding_has_them() {
    let mut s = snapshot("mssql-demo.json");
    let recommend = |kind: &str, table: &str, name: &str, columns: &[&str]| TuningRecommendation {
        name: format!("IR_{name}"),
        kind: kind.into(),
        state: Some("Active".into()),
        score: Some(64.0),
        valid_since: Some("2026-10-06T00:00:00Z".into()),
        index: Some(RecommendedIndex {
            schema: "shop".into(),
            table: table.into(),
            name: Some(name.into()),
            columns: columns.iter().map(|c| c.to_string()).collect(),
            include: Vec::new(),
        }),
        ..Default::default()
    };
    s.tuning = Some(TuningAdvice {
        recommendations: vec![
            recommend("CreateIndex", "order_items", "nci_wi_order_items_1", &["order_id"]),
            recommend("CreateIndex", "orders", "nci_wi_orders_2", &["customer_id"]),
            recommend("CreateIndex", "orders", "nci_wi_orders_3", &["channel", "created_at"]),
            {
                let mut r = recommend("CreateIndex", "customers", "nci_wi_customers_4", &["email"]);
                r.index.as_mut().unwrap().include = vec!["segment".into()];
                r
            },
            recommend("DropIndex", "orders", "ix_orders_channel", &[]),
            recommend("DropIndex", "orders", "ix_orders_status", &[]),
        ],
        ..Default::default()
    });
    let report = analyze(s);
    let ids: Vec<&str> = report.findings.iter().filter(|f| f.check == "tuning-advice").map(|f| f.id.as_str()).collect();

    // The optimizer asks for it too: one finding, which says so.
    assert!(!ids.iter().any(|id| id.contains("order_items")), "{ids:?}");
    let missing = report.findings.iter().find(|f| f.id == "missing-index:shop.order_items(order_id)").unwrap();
    assert!(
        missing.summary.contains("Azure SQL's automatic tuning recommends the same index, scoring it 64 out of 100.")
    );

    // An index a migration added since covers it: nothing to do, and Azure lets it expire.
    assert!(!ids.iter().any(|id| id.contains("(customer_id)")), "{ids:?}");

    let new =
        report.findings.iter().find(|f| f.id == "tuning-advice:create-index:shop.orders(channel,created_at)").unwrap();
    assert!(new.title.starts_with("Azure SQL recommends an index on orders ("), "{}", new.title);
    assert!(new.summary.contains(", since Oct 6, and scores it 64 out of 100."), "{}", new.summary);
    assert_eq!(new.actionability, culprit_core::Actionability::Information);
    assert!(new.fix.as_ref().unwrap().sql.is_none());

    // ix_customers_email leads with email but lacks the column the recommendation includes: widening it does the same.
    let widen = report.findings.iter().find(|f| f.id == "tuning-advice:create-index:shop.customers(email)").unwrap();
    assert!(
        widen.caveats[0].starts_with("ix_customers_email already leads with email; the recommended index adds segment"),
        "{:?}",
        widen.caveats
    );

    // ix_orders_channel is read now and then, so nothing else flags it; ix_orders_status already has a finding.
    let drop =
        report.findings.iter().find(|f| f.id == "tuning-advice:drop-index:shop.orders.ix_orders_channel").unwrap();
    assert_eq!(drop.title, "Azure SQL recommends dropping ix_orders_channel on orders");
    assert!(drop.fix.as_ref().unwrap().sql.as_deref().unwrap().contains("DISABLE"));
    assert!(!ids.iter().any(|id| id.contains("ix_orders_status")), "{ids:?}");
}

#[test]
fn literal_families_and_single_use_plans() {
    let mut s = snapshot("mssql-demo.json");
    s.query_families = vec![QueryFamily {
        key: "0x8A1F".into(),
        sample: "SELECT * FROM shop.orders WHERE id = 1041".into(),
        variants: 1240.0,
        executions: 1240.0,
        total_time_ms: 5000.0,
        cached_bytes: Some(480.0 * 1024.0 * 1024.0),
        query_ids: vec![],
    }];
    s.plan_cache = Some(PlanCacheUse {
        single_use_plans: 30_000.0,
        single_use_bytes: 1.5 * 1024.0 * 1024.0 * 1024.0,
        total_plans: 40_000.0,
        total_bytes: 2.0 * 1024.0 * 1024.0 * 1024.0,
    });
    let report = analyze(s);
    let family =
        report.findings.iter().find(|f| f.check == "query-family" && f.id != "query-family:plan-cache").unwrap();
    assert!(family.title.contains("1,240 versions") || family.title.contains("1.2K versions"), "{}", family.title);
    assert!(family.summary.contains("plan cache"));
    let cache = report.findings.iter().find(|f| f.id == "query-family:plan-cache").unwrap();
    assert!(cache.title.contains("75%"), "{}", cache.title);

    let mut pg = snapshot("postgres-demo.json");
    pg.query_families = vec![QueryFamily {
        key: "abc".into(),
        sample: "SELECT * FROM orders WHERE id IN ($1, $2, $3)".into(),
        variants: 37.0,
        executions: 900.0,
        total_time_ms: 10.0,
        cached_bytes: None,
        query_ids: vec![],
    }];
    let report = analyze(pg);
    let family = report.findings.iter().find(|f| f.check == "query-family").unwrap();
    assert!(family.title.contains("tracked as 37 separate statements"), "{}", family.title);
}

#[test]
fn health_settings_per_engine() {
    let mut ms = snapshot("mssql-demo.json");
    ms.settings.retain(|s| !s.name.starts_with("query_store_options.") && s.name != "LEGACY_CARDINALITY_ESTIMATION");
    ms.settings.extend([
        setting("query_store_options.actual_state_desc", "READ_WRITE", "database"),
        setting("query_store_options.current_storage_size_mb", "960", "database"),
        setting("query_store_options.max_storage_size_mb", "1000", "database"),
        setting("query_store_options.query_capture_mode_desc", "ALL", "database"),
        setting("query_store_options.wait_stats_capture_mode_desc", "OFF", "database"),
        setting("LEGACY_CARDINALITY_ESTIMATION", "1", "database"),
    ]);
    let found = ids(ms);
    for id in [
        "config:query-store-size",
        "config:query-store-capture",
        "config:query-store-waits",
        "config:legacy-cardinality-estimation",
    ] {
        assert!(found.iter().any(|f| f == id), "missing {id}: {found:?}");
    }

    let mut pg = snapshot("postgres-demo.json");
    pg.settings.retain(|s| !["autovacuum", "pg_stat_statements.track"].contains(&s.name.as_str()));
    pg.settings.extend([
        setting("autovacuum", "off", "server"),
        setting("pg_stat_statements.track", "none", "server"),
        setting("pg_stat_statements.dealloc", "4200", "status"),
        setting("pg_stat_statements.max", "5000", "server"),
    ]);
    let found = ids(pg);
    for id in ["config:autovacuum", "config:pg_stat_statements.track", "config:pg_stat_statements.max"] {
        assert!(found.iter().any(|f| f == id), "missing {id}: {found:?}");
    }

    let mut my = snapshot("mysql-demo.json");
    my.settings.extend([
        setting("performance_schema_digest_lost", "812", "status"),
        setting("performance_schema.consumer.statements_digest", "NO", "status"),
    ]);
    let found = ids(my);
    for id in ["config:performance_schema_digests_size", "config:statements_digest"] {
        assert!(found.iter().any(|f| f == id), "missing {id}: {found:?}");
    }
}

const ASKED: &str = "missing-index:shop.support_tickets(customer_id,is_deleted)";

/// The demo where the optimizer asked for an index on support_tickets (`equality`, then `inequality`, including
/// `include`) for one query, which compares is_deleted with `comparison`. `filtered` gives the table an index on those
/// keys for the rows where is_deleted = 0.
fn asked(
    equality: &[&str],
    inequality: &[&str],
    include: &[&str],
    comparison: &str,
    filtered: Option<&[&str]>,
) -> Snapshot {
    let names = |v: &[&str]| v.iter().map(|c| c.to_string()).collect::<Vec<_>>();
    let mut s = ten_days(snapshot("mssql-demo.json"));
    let hash = "0x00C0FFEE00C0FFEE";
    s.missing_indexes.push(MissingIndex {
        schema: "shop".into(),
        table: "support_tickets".into(),
        equality: names(equality),
        inequality: names(inequality),
        include: names(include),
        seeks: 400.0,
        scans: 0.0,
        avg_query_cost: 0.5,
        avg_impact_pct: 95.0,
        last_used: None,
        query_hashes: vec![hash.into()],
    });
    let w = s.workload.as_mut().expect("workload");
    let mut q = w.queries.iter().find(|q| q.plan.is_none()).cloned().unwrap_or_else(|| w.queries[0].clone());
    q.id = "4242".into();
    q.query_hash = Some(hash.into());
    q.plan = None;
    q.text = format!(
        "(@par0 int,@par1 bit)select [id], [status] from [shop].[support_tickets] where (([customer_id] = @par0) and ({comparison}))"
    );
    w.queries.push(q);
    if let Some(keys) = filtered {
        let mut i = s.indexes.iter().find(|i| i.name == "ix_support_tickets_customer_id").expect("an index").clone();
        i.name = "ix_support_tickets_open".into();
        i.columns = names(keys);
        i.filter = Some("([is_deleted]=(0))".into());
        s.indexes.push(i);
    }
    s
}

fn asked_by_a_query(filtered: bool, comparison: &str) -> Snapshot {
    let index: &[&str] = &["customer_id", "created_at"];
    asked(&["customer_id", "is_deleted"], &[], &[], comparison, filtered.then_some(index))
}

#[test]
fn a_missing_index_names_the_query_that_asked_for_it() {
    let report = analyze(asked(&["customer_id", "is_deleted"], &[], &["status"], "[is_deleted] = @par1", None));
    let f = report.findings.iter().find(|f| f.id == ASKED).expect("the request");
    assert_eq!(f.queries.iter().map(|q| q.id.as_str()).collect::<Vec<_>>(), ["4242"]);
    assert!(f.fix.as_ref().unwrap().sql.as_deref().unwrap().contains("CREATE NONCLUSTERED INDEX"));
    let advice = index_advice(&report);
    let c = advice.candidates.iter().find(|c| c.table == "support_tickets" && c.keys.len() == 2).expect("to create");
    assert_eq!(c.queries.iter().map(|q| q.id.as_str()).collect::<Vec<_>>(), ["4242"]);

    // When no query in the scan asked for it, the finding says where the request came from.
    let mut s = asked_by_a_query(false, "[is_deleted] = @par1");
    s.workload.as_mut().unwrap().queries.retain(|q| q.id != "4242");
    let report = analyze(s);
    let f = report.findings.iter().find(|f| f.id == ASKED).expect("the request");
    assert!(f.queries.is_empty());
    assert!(
        f.summary
            .contains("None of the queries in this scan asked for it: the requests came from plans that ran before"),
        "{}",
        f.summary
    );
}

// The index exists for is_deleted = 0, but the query passes is_deleted as a parameter, which SQL Server can't match to
// a filtered index: the fix is the literal, not a second index.
#[test]
fn a_filtered_index_missed_through_a_parameter_is_a_rewrite_not_a_new_index() {
    let report = analyze(asked_by_a_query(true, "[is_deleted] = @par1"));
    let f = report.findings.iter().find(|f| f.id == ASKED).expect("the request");
    assert_eq!(
        f.title,
        "A query on support_tickets passes is_deleted as a parameter, so ix_support_tickets_open can't serve it"
    );
    assert!(
        f.summary.contains(
            "but ix_support_tickets_open (customer_id and created_at) already serves these lookups where is_deleted = 0"
        ),
        "{}",
        f.summary
    );
    let fix = f.fix.as_ref().unwrap();
    assert!(fix.sql.is_none());
    assert_eq!(
        fix.summary,
        "Write the filter as the index does, is_deleted = 0, instead of a parameter in the query below: ix_support_tickets_open \
         then serves it, and no new index is needed."
    );
    assert_eq!((f.severity, f.confidence), (Severity::Low, culprit_core::Confidence::Medium));
    assert_eq!(f.queries.iter().map(|q| q.id.as_str()).collect::<Vec<_>>(), ["4242"]);
    // Nor does the index advisor, or the digest's list to create, suggest it.
    let advice = index_advice(&report);
    assert!(!advice.candidates.iter().any(|c| c.table == "support_tickets" && c.keys.len() == 2));

    // The parameter on either side of the comparison.
    let report = analyze(asked_by_a_query(true, "@par1 = [support_tickets].[is_deleted]"));
    assert!(report.findings.iter().find(|f| f.id == ASKED).unwrap().fix.as_ref().unwrap().sql.is_none());
    // A literal already matches the index: whatever asked, it isn't the parameter, so the request stands.
    let report = analyze(asked_by_a_query(true, "[is_deleted] = 0"));
    assert!(report.findings.iter().find(|f| f.id == ASKED).unwrap().fix.as_ref().unwrap().sql.is_some());

    // Included columns the index lacks: the literal first, then the index rebuilt with them, keeping its filter.
    let index: &[&str] = &["customer_id", "created_at"];
    let report = analyze(asked(&["customer_id", "is_deleted"], &[], &["status"], "[is_deleted] = @par1", Some(index)));
    let fix = report.findings.iter().find(|f| f.id == ASKED).unwrap().fix.clone().unwrap();
    assert!(fix.summary.contains("If reading status from the table for each row still costs too much after that, include it in ix_support_tickets_open"), "{}", fix.summary);
    let sql = fix.sql.unwrap();
    assert!(sql.contains("INCLUDE ([status])\n    WHERE ([is_deleted]=(0))\n    WITH (DROP_EXISTING = ON"), "{sql}");
}

// The index finds the rows by the request's equality columns but has no key for its range: the literal first, then,
// if the range still matters, the rest of the request as an index filtered the same way. Never an index keyed on the
// filter's column.
#[test]
fn a_filtered_index_that_only_partly_serves_the_request_leaves_a_filtered_index_to_create() {
    let s = asked(
        &["customer_id", "priority", "is_deleted"],
        &["due_at"],
        &[],
        "[is_deleted] = @par1",
        Some(&["priority", "customer_id"]),
    );
    let report = analyze(s);
    let id = "missing-index:shop.support_tickets(customer_id,priority,is_deleted,due_at)";
    let f = report.findings.iter().find(|f| f.id == id).expect("the request");
    assert_eq!(
        f.title,
        "A query on support_tickets passes is_deleted as a parameter, so ix_support_tickets_open can't serve it"
    );
    assert!(
        f.summary.contains(
            "but ix_support_tickets_open (priority and customer_id) already finds these rows where is_deleted = 0 by priority and customer_id, \
             though not by due_at."
        ),
        "{}",
        f.summary
    );
    assert_eq!((f.severity, f.confidence), (Severity::Low, culprit_core::Confidence::Low));
    let fix = f.fix.as_ref().unwrap();
    assert!(
        fix.summary.starts_with(
            "Write the filter as the index does, is_deleted = 0, instead of a parameter in the query below: ix_support_tickets_open then \
             finds its rows by priority and customer_id. If the range on due_at still costs too much after that, create the index below: the request \
             without is_deleted, filtered the same way, so it holds only the rows where is_deleted = 0. It starts with ix_support_tickets_open's keys"
        ),
        "{}",
        fix.summary
    );
    let sql = fix.sql.as_deref().unwrap();
    assert!(
        sql.contains("ON [shop].[support_tickets] ([priority], [customer_id], [due_at])\n    WHERE ([is_deleted]=(0))"),
        "{sql}"
    );
    assert!(
        !sql.contains("[is_deleted]]") && !sql.contains("[is_deleted], ") && !sql.contains(", [is_deleted]"),
        "{sql}"
    );

    // The index advisor, and the digest's list to create, list the filtered index, not the request.
    let advice = index_advice(&report);
    let c = advice.candidates.iter().find(|c| c.finding_id.as_deref() == Some(id)).expect("to create");
    assert_eq!(c.keys, ["priority", "customer_id", "due_at"]);
    assert!(c.sql.as_deref().unwrap().contains("WHERE ([is_deleted]=(0))"));
    assert!(c.reason.contains("This index is the rest of the request, filtered the same way."), "{}", c.reason);
    let digest = culprit_checks::digest(&report, None, 10, 5);
    let create = digest.index_advice.unwrap().create;
    let listed = create.iter().find(|c| c.finding.as_deref() == Some(id)).expect("in the digest");
    assert_eq!(listed.keys, ["priority", "customer_id", "due_at"]);
    assert!(listed.sql.as_deref().unwrap().contains("WHERE ([is_deleted]=(0))"));
}

// An index with the request's keys is widened only when the queries can use it: a filtered one only when they ask for
// the rows it holds, written as its filter is.
#[test]
fn a_filtered_index_is_widened_only_for_queries_that_fit_its_filter() {
    let id = "missing-index:shop.support_tickets(customer_id,created_at)";
    let filtered: &[&str] = &["customer_id", "created_at"];
    let request = |comparison: &str, index: Option<&[&str]>| {
        asked(&["customer_id"], &["created_at"], &["status"], comparison, index)
    };
    let fix = |s: Snapshot| analyze(s).findings.into_iter().find(|f| f.id == id).expect("the request").fix.unwrap();

    // The query asks for is_deleted = 0, as the index's filter does: widen it, filter kept.
    let widen = fix(request("[support_tickets].[is_deleted] = (0)", Some(filtered)));
    let sql = widen.sql.unwrap();
    assert!(sql.starts_with("CREATE NONCLUSTERED INDEX [ix_support_tickets_open]"), "{sql}");
    assert!(sql.contains("INCLUDE ([status])\n    WHERE ([is_deleted]=(0))\n    WITH (DROP_EXISTING = ON"), "{sql}");

    // The query doesn't ask for only those rows: a new index, and why the filtered one isn't it.
    let create = fix(request("[priority] = @par1", Some(filtered)));
    assert!(
        create
            .sql
            .unwrap()
            .contains("ON [shop].[support_tickets] ([customer_id], [created_at])\n    INCLUDE ([status])")
    );
    assert!(
        create.summary.ends_with(
            " ix_support_tickets_open has these keys but holds only the rows where is_deleted = 0, which the query below doesn't ask for."
        ),
        "{}",
        create.summary
    );

    // No query in the selected scan asked: retain the candidate without recommending DDL.
    let mut s = request("[is_deleted] = 0", Some(filtered));
    s.workload.as_mut().unwrap().queries.retain(|q| q.id != "4242");
    let create = fix(s);
    assert!(create.sql.is_none());
    assert!(create.summary.contains("does not establish enough measured work"));

    // An unfiltered index with the keys is widened as before.
    let id = "missing-index:shop.support_tickets(customer_id)";
    let report = analyze(asked(&["customer_id"], &[], &["status"], "[priority] = @par1", None));
    let sql = report.findings.iter().find(|f| f.id == id).expect("the request").fix.clone().unwrap().sql.unwrap();
    assert!(sql.starts_with("CREATE NONCLUSTERED INDEX [ix_support_tickets_customer_id]"), "{sql}");
    assert!(sql.contains("WITH (DROP_EXISTING = ON") && !sql.contains("WHERE"), "{sql}");
}

/// One check's findings on a snapshot, before the pipeline refines them.
fn run(check: &str, s: &Snapshot) -> Vec<Finding> {
    culprit_checks::all().into_iter().find(|c| c.info().id == check).expect("a check").run(s)
}

// A one-off monitoring statement (`SELECT TOP 15 … FROM sys.query_store_*`) is the operator's, not the application's:
// the plan-insights check passes it over, as the other checks do, however its plan reads.
#[test]
fn a_monitoring_statement_gets_no_plan_insight_findings() {
    let mut s = snapshot("mssql-demo.json");
    // Query 10 looks up 20K tickets one at a time: nearly all of its plan's work.
    let q = s.workload.as_mut().unwrap().queries.iter_mut().find(|q| q.id == "10").unwrap();
    let plan = q.plan.as_mut().unwrap();
    (plan.total_cost, plan.root.subtree_cost) = (61.0, 61.0);
    let lookup = node_mut(&mut plan.root, 4).unwrap();
    lookup.est_executions = 20_000.0;
    (lookup.self_cost, lookup.subtree_cost) = (60.0, 60.0);
    let on_10 = |s: &Snapshot| run("plan-insights", s).into_iter().any(|f| f.queries.iter().any(|r| r.id == "10"));
    assert!(on_10(&s), "the application's statement");
    let q = s.workload.as_mut().unwrap().queries.iter_mut().find(|q| q.id == "10").unwrap();
    q.text = format!("SELECT TOP 15 q.query_id FROM sys.query_store_query q CROSS APPLY ({}) t", q.text);
    assert!(!on_10(&s), "a monitoring statement");
}

// An index that already finds the request's rows by all of its columns but one is widened with that one, not joined
// by a near twin. One column of two is too little to go on.
#[test]
fn a_request_one_column_past_an_existing_index_widens_it() {
    let id = "missing-index:shop.support_tickets(customer_id,priority,is_deleted)";
    let with_index = |mut s: Snapshot, name: &str, columns: &[&str], filter: Option<&str>| {
        let mut i = s.indexes.iter().find(|i| i.name == "ix_support_tickets_customer_id").unwrap().clone();
        i.name = name.into();
        i.columns = columns.iter().map(|c| c.to_string()).collect();
        i.filter = filter.map(str::to_string);
        s.indexes.push(i);
        s
    };
    let request = || asked(&["customer_id", "priority", "is_deleted"], &[], &["status"], "[is_deleted] = 0", None);
    let fix = |s: Snapshot| analyze(s).findings.into_iter().find(|f| f.id == id).expect("the request").fix.unwrap();

    let s = with_index(request(), "ix_support_tickets_priority_customer", &["priority", "customer_id"], None);
    let report = analyze(s);
    let widen = report.findings.iter().find(|f| f.id == id).expect("the request").fix.clone().unwrap();
    assert!(
        widen.summary.starts_with(
            "ix_support_tickets_priority_customer already finds these rows by priority and customer_id: all the \
             request asks for but is_deleted. Widen it rather than add an index"
        ),
        "{}",
        widen.summary
    );
    let sql = widen.sql.unwrap();
    assert!(
        sql.starts_with(
            "CREATE NONCLUSTERED INDEX [ix_support_tickets_priority_customer]\n    ON [shop].[support_tickets] ([priority], [customer_id])\n    INCLUDE ([is_deleted], [status])\n    WITH (DROP_EXISTING = ON"
        ),
        "{sql}"
    );
    let advice = index_advice(&report);
    let c = advice.candidates.iter().find(|c| c.finding_id.as_deref() == Some(id)).expect("advised");
    assert_eq!(c.widens.as_deref(), Some("ix_support_tickets_priority_customer"));

    // A filtered index serves the queries that ask for the rows it holds: its filter stands for is_deleted.
    let filtered =
        || with_index(request(), "ix_support_tickets_open", &["customer_id", "priority"], Some("([is_deleted]=(0))"));
    let widen = fix(filtered());
    assert!(
        widen.summary.starts_with(
            "ix_support_tickets_open already finds these rows by customer_id and priority, in the rows where \
             is_deleted = 0. Rebuild it with status included"
        ),
        "{}",
        widen.summary
    );
    assert!(
        widen.sql.unwrap().contains("INCLUDE ([status])\n    WHERE ([is_deleted]=(0))\n    WITH (DROP_EXISTING = ON")
    );

    // Without its captured query, the candidate stays informational and has no DDL.
    let mut s = filtered();
    s.workload.as_mut().unwrap().queries.retain(|q| q.id != "4242");
    let create = fix(s);
    assert!(create.sql.is_none());
    assert!(create.summary.contains("does not establish enough measured work"));
}

// Confidence is how sure, severity how much. The optimizer's 95% of runs that read 2,000 pages each of a 300K-row
// table is certain however little time they took; of runs that read 3 pages it is a guess about very little.
#[test]
fn a_missing_index_is_as_confident_as_what_its_runs_read() {
    let request = |ms: Option<f64>, pages_a_run: f64| {
        let mut s = asked(&["customer_id", "is_deleted"], &[], &["status"], "[is_deleted] = @par1", None);
        let q = s.workload.as_mut().unwrap().queries.iter_mut().find(|q| q.id == "4242").unwrap();
        q.logical_reads = Some(q.executions * pages_a_run);
        if let Some(ms) = ms {
            q.total_time_ms = ms;
        }
        analyze(s).findings.into_iter().find(|f| f.id == ASKED).expect("the request")
    };
    let busy = request(None, 2_000.0);
    assert!(busy.severity >= Severity::Medium, "{:?}", busy.severity);
    assert_eq!(busy.confidence, culprit_core::Confidence::High);
    let quiet = request(Some(27.0), 2_000.0);
    assert_eq!((quiet.severity, quiet.confidence), (Severity::Low, culprit_core::Confidence::High));
    let cheap = request(None, 3.0);
    assert_eq!(cheap.confidence, culprit_core::Confidence::Medium);

    // The demo's lookup by order_id scans the 1.5M-row clustered index for 1.5 rows (7,993 pages a run), and SQL
    // Server estimates 99% cheaper: certain, though it takes little time.
    let report = analyze(snapshot("mssql-demo.json"));
    let f = report.findings.iter().find(|f| f.id == "missing-index:shop.order_items(order_id)").expect("the request");
    assert_eq!(f.confidence, culprit_core::Confidence::High);
    // One key: no word about the order of keys.
    assert_eq!(f.fix.as_ref().unwrap().summary, "Create an index on order_id.");
}

// A request on a table too small for an index to pay (160 rows, 27 ms of queries in the scan) says there's nothing to
// create, with no SQL, and the index advisor doesn't list it; the same request on a large table is advice.
#[test]
fn a_table_too_small_for_an_index_gets_none() {
    let small = |rows: f64| {
        let mut s = asked(&["customer_id", "is_deleted"], &[], &["status"], "[is_deleted] = @par1", None);
        s.tables.iter_mut().find(|t| t.name == "support_tickets").unwrap().rows = Some(rows);
        s.workload.as_mut().unwrap().queries.iter_mut().find(|q| q.id == "4242").unwrap().total_time_ms = 27.0;
        analyze(s)
    };
    let report = small(160.0);
    let f = report.findings.iter().find(|f| f.id == ASKED).expect("the request");
    assert_eq!(f.title, "SQL Server asks for an index on support_tickets, which is too small for one to pay off");
    let fix = f.fix.as_ref().unwrap();
    assert!(fix.sql.is_none(), "{:?}", fix.sql);
    assert!(
        fix.summary.starts_with(
            "Nothing to create while shop.support_tickets stays this small: it holds 160 rows, and the query that asked for it took 27 ms in "
        ),
        "{}",
        fix.summary
    );
    assert!(!index_advice(&report).candidates.iter().any(|c| c.table == "support_tickets" && c.keys.len() == 2));

    // An index there already narrows these lookups by most of their columns (a staging database's *_active indexes, filtered
    // to is_deleted = 0): the fix names it.
    let index: &[&str] = &["customer_id"];
    let mut s = asked(&["customer_id", "is_deleted"], &["opened_at"], &[], "[is_deleted] = 0", Some(index));
    s.tables.iter_mut().find(|t| t.name == "support_tickets").unwrap().rows = Some(1_200.0);
    s.workload.as_mut().unwrap().queries.iter_mut().find(|q| q.id == "4242").unwrap().total_time_ms = 27.0;
    let report = analyze(s);
    let f = report
        .findings
        .iter()
        .find(|f| f.id == "missing-index:shop.support_tickets(customer_id,is_deleted,opened_at)")
        .expect("the request");
    let fix = f.fix.as_ref().unwrap();
    assert!(fix.sql.is_none(), "{:?}", fix.sql);
    assert!(
        fix.summary.contains(
            " ix_support_tickets_open already leads with customer_id, for the rows where is_deleted = 0. Create one only \
             if the table grows past 10K rows"
        ),
        "{}",
        fix.summary
    );

    let large = small(300_000.0);
    let f = large.findings.iter().find(|f| f.id == ASKED).expect("the request");
    assert!(f.fix.as_ref().unwrap().sql.as_deref().is_some_and(|sql| sql.starts_with("CREATE")), "{:?}", f.fix);
}

// A request with a range (pending_generation) on a table whose filtered index already finds its rows by the equality
// columns (tenant_id and account_id, where is_deleted = 0) names that index, as one without a range does.
#[test]
fn an_index_that_lacks_only_the_range_is_named() {
    const RANGED: &str = "missing-index:shop.support_tickets(customer_id,is_deleted,opened_at)";
    let index: &[&str] = &["customer_id"];
    // No query in the scan to show they ask for those rows.
    let mut s = asked(&["customer_id", "is_deleted"], &["opened_at"], &[], "[is_deleted] = 0", Some(index));
    s.workload.as_mut().unwrap().queries.retain(|q| q.id != "4242");
    let report = analyze(s);
    let fix = report.findings.iter().find(|f| f.id == RANGED).expect("the request").fix.clone().unwrap();
    assert!(fix.sql.is_none());
    assert!(fix.summary.contains("does not establish enough measured work"));
}

// A widen rebuilds an existing index under its own name: the verify names it, not "the new index".
#[test]
fn a_widen_is_verified_on_the_index_it_rebuilds() {
    let index: &[&str] = &["customer_id", "created_at"];
    let report = analyze(asked(&["customer_id", "is_deleted"], &[], &["status"], "[is_deleted] = 0", Some(index)));
    let f = report.findings.iter().find(|f| f.id == ASKED).expect("the request");
    assert!(f.fix.as_ref().unwrap().sql.as_deref().unwrap().contains("DROP_EXISTING"), "{:?}", f.fix);
    assert_eq!(
        f.verify,
        vec![
            "After testing the rebuild of ix_support_tickets_open, capture a newly compiled plan under representative load. A stored suggestion can remain in old plan XML.".to_string(),
            "Check whether the queries listed here use ix_support_tickets_open and compare reads and execution time under equivalent parameters and load, including index storage and write cost.".to_string(),
        ]
    );
    let advice = index_advice(&report);
    let c = advice.candidates.iter().find(|c| c.widens.as_deref() == Some("ix_support_tickets_open")).expect("advised");
    assert!(
        c.verify
            .iter()
            .any(|v| v == "Check whether the queries listed use ix_support_tickets_open and compare reads and time per run under equivalent parameters and load."),
        "{:?}",
        c.verify
    );
}

// A column whose type can hold large values stays out of INCLUDE; the caveat says the table holds large values only
// where its LOB and row-overflow pages say so (a staging SQL Server reading said "it holds large values" of a table with none).
#[test]
fn large_values_are_said_only_where_the_table_keeps_them() {
    let caveat = |lob: Option<f64>| {
        let mut s = asked(&["customer_id", "is_deleted"], &[], &["subject"], "[is_deleted] = @par1", None);
        s.columns.iter_mut().find(|c| c.table == "support_tickets" && c.name == "subject").unwrap().data_type =
            "nvarchar(max)".into();
        let t = s.tables.iter_mut().find(|t| t.name == "support_tickets").unwrap();
        t.lob_bytes = lob;
        t.overflow_bytes = lob.map(|_| 0.0);
        let report = analyze(s);
        let f = report.findings.iter().find(|f| f.id == ASKED).expect("the request");
        f.caveats.iter().find(|c| c.starts_with("Left subject out of INCLUDE")).cloned().expect("the caveat")
    };
    assert_eq!(
        caveat(Some(0.0)),
        "Left subject out of INCLUDE: its type can hold large values, though shop.support_tickets keeps none apart from \
         its rows now, and including it would copy whatever it holds into the index. The rows the index finds go back to \
         the table for it."
    );
    assert_eq!(
        caveat(Some(1.5 * 1024.0 * 1024.0)),
        "Left subject out of INCLUDE: its type can hold large values, and shop.support_tickets keeps 1.5 MB of large \
         values apart from its rows. Including it could copy that data into the index; the rows the index finds go back \
         to the table for it, which costs less."
    );
    assert!(caveat(None).starts_with("Left subject out of INCLUDE: its type can hold large values, and including it"));
}

// Azure's index recommendations, read like the optimizer's requests: large values stay out of INCLUDE, an index that
// already leads with the keys is named with its own key order and widened rather than twinned, a low score says the
// index is hardly worth having, and an index that leads with some of the keys is pointed out.
#[test]
fn azure_index_recommendations_are_weighed_against_the_indexes_there() {
    let mut s = snapshot("mssql-demo.json");
    s.columns.iter_mut().find(|c| c.table == "support_tickets" && c.name == "subject").unwrap().data_type =
        "nvarchar(max)".into();
    let mut index = s.indexes.iter().find(|i| i.name == "ix_support_tickets_customer_id").unwrap().clone();
    index.name = "ix_support_tickets_customer_priority".into();
    index.columns = vec!["customer_id".into(), "priority".into()];
    s.indexes.push(index);
    let recommend = |name: &str, columns: &[&str], include: &[&str], score: f64| {
        let quoted = |v: &[&str]| v.iter().map(|c| format!("[{c}]")).collect::<Vec<_>>().join(", ");
        TuningRecommendation {
            name: format!("IR_{name}"),
            kind: "CreateIndex".into(),
            state: Some("Active".into()),
            score: Some(score),
            script: Some(format!(
                "CREATE NONCLUSTERED INDEX [{name}] ON [shop].[support_tickets] ({}){} WITH (ONLINE = ON)",
                quoted(columns),
                if include.is_empty() { String::new() } else { format!(" INCLUDE ({})", quoted(include)) }
            )),
            index: Some(RecommendedIndex {
                schema: "shop".into(),
                table: "support_tickets".into(),
                name: Some(name.into()),
                columns: columns.iter().map(|c| c.to_string()).collect(),
                include: include.iter().map(|c| c.to_string()).collect(),
            }),
            ..Default::default()
        }
    };
    s.tuning = Some(TuningAdvice {
        recommendations: vec![
            recommend("nci_1", &["priority", "customer_id"], &["subject"], 2.0),
            recommend("nci_2", &["customer_id", "priority"], &["subject", "status"], 40.0),
            recommend("nci_3", &["opened_at"], &[], 3.0),
            recommend("nci_4", &["customer_id", "status"], &[], 60.0),
        ],
        ..Default::default()
    });
    let found = run("tuning-advice", &s);
    let advice = |keys: &str| {
        found
            .iter()
            .find(|f| f.id == format!("tuning-advice:create-index:shop.support_tickets({keys})"))
            .unwrap_or_else(|| panic!("{keys}: {:?}", found.iter().map(|f| &f.id).collect::<Vec<_>>()))
    };

    // The index there differs only by the large column: nothing to create, said in the index's own key order, by the
    // fix alone (CX-10d: no caveat repeats it).
    let only_large = advice("priority,customer_id");
    assert!(
        !only_large.caveats.iter().any(|c| c.contains("already leads with")),
        "the fix says it: {:?}",
        only_large.caveats
    );
    // CX-K: nothing to create, so nothing was left out of an index either.
    assert!(
        !only_large.caveats.iter().any(|c| c.starts_with("Left subject out of INCLUDE")),
        "{:?}",
        only_large.caveats
    );
    let fix = only_large.fix.as_ref().unwrap();
    assert!(fix.sql.is_none(), "{:?}", fix.sql);
    assert!(
        fix.summary.starts_with(
            "Azure scores it 2 out of 100: it expects the index to save little next to what it costs every write, so \
             it's usually not worth having. Nothing to create: ix_support_tickets_customer_priority already finds"
        ),
        "{}",
        fix.summary
    );
    assert!(fix.summary.contains("rows by customer_id and priority"), "{}", fix.summary);

    // A column worth including: the index there is widened with it, and never with the large one.
    let widen = advice("customer_id,priority");
    assert!(
        widen.caveats[0].starts_with(
            "ix_support_tickets_customer_priority already leads with customer_id and priority; the recommended index \
             adds status"
        ),
        "{:?}",
        widen.caveats
    );
    let sql = widen.fix.as_ref().unwrap().sql.as_deref().unwrap();
    assert!(
        sql.starts_with(
            "CREATE NONCLUSTERED INDEX [ix_support_tickets_customer_priority]\n    ON [shop].[support_tickets] ([customer_id], [priority])\n    INCLUDE ([status])\n    WITH (DROP_EXISTING = ON"
        ),
        "{sql}"
    );
    assert!(!sql.contains("subject"), "{sql}");

    // Scored low, with nothing there: Azure's word that it's hardly worth it comes first.
    let low = advice("opened_at");
    let fix = low.fix.as_ref().unwrap();
    assert!(fix.summary.starts_with("Azure scores it 3 out of 100"), "{}", fix.summary);
    assert!(fix.summary.contains("If the queries that read support_tickets are slow all the same, create it online"));
    assert!(fix.sql.as_deref().unwrap().starts_with("CREATE NONCLUSTERED INDEX [nci_3]"));
    assert!(low.caveats.is_empty(), "{:?}", low.caveats);

    // An index leads with one of the keys: worth checking before adding another.
    let partly = advice("customer_id,status");
    assert_eq!(
        partly.caveats[0],
        "ix_support_tickets_customer_id already leads with customer_id, 1 of the 2 recommended keys: check whether \
         the queries that read support_tickets use it before adding an index beside it."
    );
    assert!(partly.fix.as_ref().unwrap().summary.starts_with("Create it online"));
}

/// The demo with Azure's recommendation of an index on `table` (`columns`, including `include`), scored `score`.
fn recommended(s: &mut Snapshot, table: &str, columns: &[&str], include: &[&str], score: f64) {
    s.tuning = Some(TuningAdvice {
        recommendations: vec![TuningRecommendation {
            name: "IR_low".into(),
            kind: "CreateIndex".into(),
            state: Some("Active".into()),
            score: Some(score),
            valid_since: Some("2026-10-06T00:00:00Z".into()),
            index: Some(RecommendedIndex {
                schema: "shop".into(),
                table: table.into(),
                name: Some(format!("nci_msft_{table}")),
                columns: columns.iter().map(|c| c.to_string()).collect(),
                include: include.iter().map(|c| c.to_string()).collect(),
            }),
            ..Default::default()
        }],
        ..Default::default()
    });
}

// CX-9, a staging SQL Server reading: "If the queries that read job_runs are slow all the same, create it" for an
// index on a bit column, including columns every claim rewrites, beside statements that average under a millisecond.
// The condition is one the scan answers: nothing to create.
#[test]
fn a_low_score_on_a_bit_key_beside_fast_statements_is_nothing_to_create() {
    let mut s = snapshot("mssql-demo.json");
    recommended(&mut s, "support_tickets", &["is_deleted"], &["status", "claimed_by"], 3.0);
    s.columns.push(
        serde_json::from_value(serde_json::json!({
            "schema": "shop", "table": "support_tickets", "name": "is_deleted", "data_type": "bit",
            "nullable": false, "default": null, "generated": false, "ordinal": 40
        }))
        .unwrap(),
    );
    let w = s.workload.as_mut().expect("workload");
    let mut q = w.queries.iter().find(|q| q.id == "10").expect("query 10").clone();
    (q.id, q.query_hash, q.plan, q.plans) = ("777".into(), None, None, Vec::new());
    q.text = "SELECT [id], [status] FROM [shop].[support_tickets] WHERE [is_deleted] = 0 AND [id] = @P1".into();
    (q.executions, q.total_time_ms, q.mean_time_ms) = (2000.0, 600.0, 0.3);
    w.queries.push(q);
    let f = analyze(s)
        .findings
        .into_iter()
        .find(|f| f.id == "tuning-advice:create-index:shop.support_tickets(is_deleted)")
        .expect("the recommendation");
    let fix = f.fix.unwrap();
    assert_eq!(fix.sql, None);
    assert!(
        fix.summary.contains(
            "Nothing to create: the statement this scan captured that compares is_deleted with a value averages 0.30 ms a run, so there is no slow read for it to speed up, and its key, is_deleted, is a bit column: it holds at most two values, so an index on it narrows the rows little; every write that changes one of the 2 columns it includes would update it too. Let the recommendation expire."
        ),
        "{}",
        fix.summary
    );
    assert!(f.verify[0].starts_with("Nothing to change"), "{:?}", f.verify);
}

// Azure scores an index low, but a heavy statement the scan measured filters on exactly its keys and reads 750 pages a
// run to return nothing: the finding cites it and doesn't call the index not worth having. Without such a statement it
// says the ones it was recommended for may be gone, and checking it is conditional, as the advice is.
#[test]
fn a_low_azure_score_gives_way_to_a_heavy_statement_on_its_keys() {
    const ID: &str = "tuning-advice:create-index:shop.support_tickets(customer_id,status)";
    let with_statement = |heavy: bool| {
        let mut s = snapshot("mssql-demo.json");
        recommended(&mut s, "support_tickets", &["customer_id", "status"], &[], 3.0);
        let w = s.workload.as_mut().expect("workload");
        let mut q = w.queries.iter().find(|q| q.id == "10").expect("query 10").clone();
        q.id = "777".into();
        q.query_hash = None;
        q.plan = None;
        q.plans = Vec::new();
        q.text =
            "SELECT [id], [subject] FROM [shop].[support_tickets] WHERE [customer_id] = @P1 AND [status] = @P2".into();
        q.executions = 2000.0;
        q.logical_reads = Some(1_500_000.0);
        q.rows = Some(0.0);
        q.total_time_ms = if heavy { 40_000.0 } else { 40.0 };
        q.mean_time_ms = q.total_time_ms / q.executions;
        if heavy {
            w.total_time_ms = 400_000.0;
        }
        w.queries.push(q);
        let report = analyze(s);
        report.findings.into_iter().find(|f| f.id == ID).expect("the recommendation")
    };

    let heavy = with_statement(true);
    let fix = heavy.fix.as_ref().unwrap().summary.clone();
    assert!(!fix.contains("usually not worth having"), "{fix}");
    assert!(
        fix.contains("but a read from support_tickets, query 777 below, filters on exactly these columns, takes 10% of database time and reads 750 pages a run to return nothing: an index on them would find its rows directly. Create it online"),
        "{fix}"
    );
    assert!(
        heavy
            .summary
            .contains("a read from support_tickets (below) compares all of customer_id and status with a value"),
        "{}",
        heavy.summary
    );
    assert!(heavy.queries.iter().any(|q| q.id == "777"), "{:?}", heavy.queries);
    assert_eq!(
        heavy.verify,
        vec![
            "Re-scan a day later: the recommendation is gone, and query 777 reads far fewer than 750 pages a run."
                .to_string()
        ]
    );

    // A statement on the keys that takes little time doesn't outweigh Azure's score; and its runs, measured fast,
    // answer the condition the low score leaves ("if the queries are slow all the same"): nothing to create.
    let light = with_statement(false);
    let fix = light.fix.as_ref().unwrap();
    assert!(fix.summary.contains("usually not worth having"), "{}", fix.summary);
    assert!(
        fix.summary.contains(
            "Nothing to create: the statement this scan captured that compares all of customer_id and status with a value averages 0.02 ms a run, so there is no slow read for it to speed up. Let the recommendation expire."
        ),
        "{}",
        fix.summary
    );
    assert!(fix.sql.is_none(), "{:?}", fix.sql);
    assert_eq!(
        light.verify,
        vec!["Nothing to change: once Azure lets the recommendation expire, this finding goes with it."]
    );

    // No statement on the keys: say so, rather than "check that those queries still run as they did".
    let mut s = snapshot("mssql-demo.json");
    recommended(&mut s, "support_tickets", &["customer_id", "status"], &[], 3.0);
    let none = analyze(s).findings.into_iter().find(|f| f.id == ID).expect("the recommendation");
    assert!(
        none.summary.ends_with("none of the statements this scan captured compares all of customer_id and status with a value: the ones it was recommended for may have changed, stopped running, or not be among them."),
        "{}",
        none.summary
    );
    assert!(!none.summary.contains("still run as they did"), "{}", none.summary);
}

// A staging scan's call_sessions (CX-A): an index leads with keys the heavy statement compares with a value, but its plan reads the
// table through another one, picked on its estimate. Its reading for nothing comes from that choice, which heavy-query
// is about: no evidence for creating Azure's index, so with Azure's low score there is nothing to create. The finding
// takes no share of database time, so the heavy-query finding stays.
#[test]
fn a_heavy_statement_names_the_index_its_plan_reads_through() {
    const ID: &str = "tuning-advice:create-index:shop.support_tickets(customer_id,status)";
    let with_plan = |plan: bool, score: f64, estimate: f64| {
        let mut s = snapshot("mssql-demo.json");
        recommended(&mut s, "support_tickets", &["customer_id", "status"], &[], score);
        let w = s.workload.as_mut().expect("workload");
        // Query 10's plan seeks ix_support_tickets_priority, then looks each row up in pk_support_tickets.
        let mut q = w.queries.iter().find(|q| q.id == "10").expect("query 10").clone();
        q.id = "777".into();
        q.query_hash = None;
        q.plans = Vec::new();
        if !plan {
            q.plan = None;
        }
        if let Some(seek) = q.plan.as_mut().and_then(|p| node_mut(&mut p.root, 2)) {
            seek.est_rows = estimate;
        }
        q.text =
            "SELECT [id], [subject] FROM [shop].[support_tickets] WHERE [customer_id] = @P1 AND [status] = @P2".into();
        q.executions = 2000.0;
        q.logical_reads = Some(1_500_000.0);
        q.rows = Some(0.0);
        q.total_time_ms = 40_000.0;
        w.total_time_ms = 400_000.0;
        q.mean_time_ms = q.total_time_ms / q.executions;
        w.queries.push(q);
        analyze(s).findings.into_iter().find(|f| f.id == ID).expect("the recommendation")
    };
    // A seek estimated at one row beside 750 pages a run: the pages establish no intermediate row count or cause.
    let f = with_plan(true, 3.0, 1.0);
    let fix = f.fix.as_ref().unwrap();
    assert!(fix.sql.is_none(), "{:?}", fix.sql);
    assert!(
        fix.summary.starts_with(
            "Azure scores it 3 out of 100: it expects the index to save little next to what it costs every write, so \
             it's usually not worth having. Nothing to create: ix_support_tickets_customer_id already leads with \
             customer_id, which query 777 compares with a value, but its plan picks ix_support_tickets_priority \
             expecting 1 row, and its runs read 750 pages each to return nothing."
        ),
        "{}",
        fix.summary
    );
    assert!(
        fix.summary.ends_with(
            "and its runs read 750 pages each to return nothing. The recorded pages do not establish why that plan \
             was chosen: inspect a representative actual plan before choosing a change (see heavy-query:777). \
             Let the recommendation expire."
        ),
        "{}",
        fix.summary
    );
    assert!(f.caveats.is_empty(), "{:?}", f.caveats);
    assert!(
        f.verify[0].starts_with("Nothing to create here: gather the actual-plan and IO evidence"),
        "{:?}",
        f.verify
    );
    assert!(f.verify[0].contains("After a change supported by that evidence"), "{:?}", f.verify);
    assert!(!f.verify[0].contains("a few pages a run"), "{:?}", f.verify);
    assert!(f.impact.db_time_share.is_none(), "{:?}", f.impact.db_time_share);

    // Azure scores it high: its own word stands, and the caveat says what the statement's plan chose.
    let f = with_plan(true, 60.0, 1.0);
    assert!(f.fix.as_ref().unwrap().sql.is_some());
    assert!(
        f.caveats[0].starts_with(
            "ix_support_tickets_customer_id already leads with customer_id, 1 of the 2 recommended keys, but query \
             777's plan reads support_tickets through ix_support_tickets_priority instead, expecting 1 row: \
             heavy-query:777 is about that choice."
        ),
        "{:?}",
        f.caveats
    );
    // A seek costed for about the rows it finds is no misestimate: the index it seeks narrows too little, and the
    // statement is evidence for the recommended one.
    let f = with_plan(true, 3.0, 288.0);
    assert!(f.fix.as_ref().unwrap().summary.contains("an index on them would find its rows directly"));
    assert_eq!(
        f.caveats[0],
        "ix_support_tickets_customer_id already leads with customer_id, 1 of the 2 recommended keys, but query 777's \
         plan reads support_tickets through ix_support_tickets_priority instead, so that index isn't serving it."
    );
    // Without the plan, nothing says which index it reads: the statement is evidence, and the caveat asks to check.
    let f = with_plan(false, 3.0, 1.0);
    assert!(f.fix.as_ref().unwrap().summary.contains("an index on them would find its rows directly"));
    assert!(
        f.caveats[0].ends_with(
            ": check whether the queries that read support_tickets use it before adding an index beside it."
        ),
        "{:?}",
        f.caveats
    );
}

// CX-I: the count says how many statements compare the key, the list shows the heaviest five, and a column of another
// table in a join isn't this table's. One key reads as the column alone.
#[test]
fn a_recommendation_counts_the_statements_on_its_own_columns() {
    let mut s = snapshot("mssql-demo.json");
    recommended(&mut s, "support_tickets", &["status"], &[], 3.0);
    let w = s.workload.as_mut().expect("workload");
    let template = w.queries.iter().find(|q| q.id == "10").expect("query 10").clone();
    for n in 0..7 {
        let mut q = template.clone();
        q.id = format!("90{n}");
        q.query_hash = None;
        q.plan = None;
        q.plans = Vec::new();
        q.text = format!("SELECT [id] FROM [shop].[support_tickets] WHERE [status] = @P1 AND [priority] = {n}");
        q.total_time_ms = 10.0 + n as f64;
        w.queries.push(q);
    }
    let mut join = template.clone();
    join.id = "999".into();
    join.query_hash = None;
    join.plan = None;
    join.plans = Vec::new();
    join.text = "SELECT [t].[id] FROM [shop].[support_tickets] [t] JOIN [shop].[customers] [c] ON [c].[id] = \
                 [t].[customer_id] WHERE [c].[status] = @P1"
        .into();
    w.queries.push(join);
    let f = analyze(s)
        .findings
        .into_iter()
        .find(|f| f.id == "tuning-advice:create-index:shop.support_tickets(status)")
        .expect("the recommendation");
    assert!(
        f.summary.ends_with(
            "7 statements this scan captured compare status with a value; the 5 that take the most time are below."
        ),
        "{}",
        f.summary
    );
    assert_eq!(f.queries.len(), 5, "{:?}", f.queries);
    assert!(f.queries.iter().all(|q| q.id != "999"), "{:?}", f.queries);
    assert_eq!(f.queries[0].id, "906", "{:?}", f.queries);
}

// An index already finds the rows by the recommended keys and the recommendation only adds a large column: nothing to
// create, so nothing to check either; the verify doesn't ask for fewer reads.
#[test]
fn nothing_to_create_has_nothing_to_verify() {
    let mut s = snapshot("mssql-demo.json");
    let mut notes = s.columns.iter().find(|c| c.table == "customers" && c.name == "name").expect("a column").clone();
    notes.name = "notes".into();
    notes.data_type = "nvarchar(max)".into();
    s.columns.push(notes);
    recommended(&mut s, "customers", &["email"], &["notes"], 2.0);
    let f = analyze(s)
        .findings
        .into_iter()
        .find(|f| f.id == "tuning-advice:create-index:shop.customers(email)")
        .expect("the recommendation");
    assert!(f.fix.as_ref().unwrap().summary.contains("Nothing to create"), "{:?}", f.fix);
    assert_eq!(
        f.verify,
        vec!["Nothing to change: once Azure lets the recommendation expire, this finding goes with it.".to_string()]
    );
}
