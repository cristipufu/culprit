//! Lessons from a busy multi-tenant SaaS database on Azure SQL: soft deletes behind filtered indexes, a tenant id leading
//! every index, polling workers, document columns and a week of history spanning several deploys. Each test plants one
//! condition on the SQL Server demo snapshot.

use culprit_checks::{analyze, index_advice};
use culprit_core::{
    CandidateSource, Finding, MissingIndex, Pressure, Severity, Signal, SignalScope, SignalUnit, Snapshot,
    TuningAdvice, TuningRecommendation,
};
use serde_json::json;

fn snapshot() -> Snapshot {
    let text = std::fs::read_to_string(format!("{}/../../fixtures/mssql-demo.json", env!("CARGO_MANIFEST_DIR")))
        .expect("fixture");
    counted_for_ten_days(serde_json::from_str(&text).expect("snapshot"))
}

/// The demo's counters are minutes old; an index is only called unused after a week of them.
fn counted_for_ten_days(mut s: Snapshot) -> Snapshot {
    s.server.started_at = Some("2026-10-01T12:00:00Z".into());
    if let Some(p) = s.pressure.as_mut() {
        p.uptime_hours = Some(240.0);
    }
    s
}

fn finding<'a>(findings: &'a [Finding], id: &str) -> Option<&'a Finding> {
    findings.iter().find(|f| f.id == id)
}

#[test]
fn advice_about_a_query_that_stopped_running_is_marked_inactive() {
    let mut s = snapshot();
    let w = s.workload.as_mut().unwrap();
    let id = w.queries[0].id.clone();
    // Ran every second from the start of the window, then not once in its last four hours.
    w.queries[0].last_seen = Some("2026-10-11T08:00:00Z".into());
    s.tuning = Some(TuningAdvice {
        recommendations: vec![TuningRecommendation {
            name: "PR_1".into(),
            kind: "FORCE_LAST_GOOD_PLAN".into(),
            state: Some("Active".into()),
            query_id: Some(id.clone()),
            regressed_plan_id: Some("2".into()),
            recommended_plan_id: Some("1".into()),
            estimated_gain: Some(0.9),
            ..Default::default()
        }],
        ..Default::default()
    });
    let report = analyze(s);
    let f = finding(&report.findings, &format!("tuning-advice:force:{id}")).expect("tuning advice");
    assert!(f.inactive, "{:?}", f.caveats);
    assert!(f.caveats[0].contains("not run since"), "{:?}", f.caveats);
}

#[test]
fn a_filtered_index_does_not_serve_a_foreign_key() {
    let s = snapshot();
    let id = analyze(s.clone())
        .findings
        .into_iter()
        .find(|f| f.check == "unindexed-fk")
        .map(|f| f.id)
        .expect("the demo has an unindexed foreign key");
    let fk = s
        .foreign_keys
        .iter()
        .find(|fk| id == format!("unindexed-fk:{}.{}({})", fk.schema, fk.table, fk.columns.join(",")))
        .unwrap()
        .clone();
    let with_index = |filter: Option<&str>| {
        let mut s = s.clone();
        let mut i = s.indexes.iter().find(|i| i.table == fk.table).cloned().expect("an index");
        i.name = format!("ix_{}_live", fk.table);
        i.columns = fk.columns.clone();
        i.include.clear();
        i.filter = filter.map(String::from);
        i.is_primary_key = false;
        i.is_unique = false;
        i.is_clustered = false;
        s.indexes.push(i);
        analyze(s).findings.iter().any(|f| f.id == id)
    };
    assert!(with_index(Some("([is_deleted]=(0))")), "a filtered index can't find soft-deleted children");
    assert!(!with_index(None));
}

#[test]
fn an_unread_filtered_index_is_unused_but_a_filtered_unique_one_is_a_constraint() {
    let mut s = snapshot();
    let template = s.indexes.iter().find(|i| i.table == "support_tickets").cloned().expect("an index");
    for (name, unique, key) in [("ix_tickets_open", false, "zz_opened"), ("ux_tickets_open_number", true, "zz_number")]
    {
        let mut i = template.clone();
        i.name = name.into();
        i.columns = vec!["zz_tenant".into(), key.into()];
        i.include.clear();
        i.filter = Some("([is_deleted]=(0))".into());
        i.is_primary_key = false;
        i.is_clustered = false;
        i.is_disabled = false;
        i.is_unique = unique;
        i.reads = Some(0.0);
        i.writes = Some(40_000.0);
        i.size_bytes = Some(8.0 * 1024.0 * 1024.0);
        s.indexes.push(i);
    }
    let report = analyze(s);
    let f = finding(&report.findings, "unused-index:shop.support_tickets").expect("unused index");
    assert!(f.summary.contains("ix_tickets_open"), "{}", f.summary);
    assert!(!f.summary.contains("ux_tickets_open_number"), "{}", f.summary);
}

// A deploy adds an index for a query that ships with it, or hasn't run yet: no reads says nothing until it has existed
// for a while. The scan dates it (`created_at_latest`) when an earlier scan saw its table without it, or the database
// dated a change to its table that no earlier scan was there to see.
#[test]
fn an_index_added_this_week_is_not_unused_yet() {
    let mut s = snapshot();
    // Six weeks of usage counters.
    s.server.started_at = Some("2026-08-31T12:00:00Z".into());
    fn index(s: &mut Snapshot) -> &mut culprit_core::IndexStat {
        s.indexes.iter_mut().find(|i| i.name == "ix_support_tickets_customer_id").unwrap()
    }
    let unused = |s: &Snapshot| {
        let report = analyze(s.clone());
        let cleaned = index_advice(&report).cleanups.iter().any(|c| c.index == "ix_support_tickets_customer_id");
        (finding(&report.findings, "unused-index:shop.support_tickets").cloned(), cleaned)
    };

    // Created two and a half days before the scan.
    index(&mut s).created_at_latest = Some("2026-10-09T00:51:00Z".into());
    let (f, cleaned) = unused(&s);
    assert!(f.is_none(), "{:?}", f.map(|f| f.summary));
    assert!(!cleaned);

    // Twelve days old, it is unused, over the days it has existed rather than the counters' six weeks.
    index(&mut s).created_at_latest = Some("2026-09-29T12:00:00Z".into());
    let (f, cleaned) = unused(&s);
    let f = f.expect("unused after a week");
    assert!(
        f.summary.contains(
            "No query has read ix_support_tickets_customer_id in the 12 days since Sep 29, before which it may not have existed"
        ),
        "{}",
        f.summary
    );
    assert!(f.metrics.iter().any(|m| m.label == "Counting for" && m.value.round() == 12.0));
    assert!(cleaned);

    // Nothing says it is new: as old as the counters.
    index(&mut s).created_at_latest = None;
    let f = unused(&s).0.expect("unused");
    assert!(f.summary.contains("in the 41 days since the server last restarted"), "{}", f.summary);
}

#[test]
fn a_poll_and_a_heartbeat_are_not_called_n_plus_one() {
    let mut s = snapshot();
    let w = s.workload.as_mut().unwrap();
    let extra = 2.0 * 30_000.0 - w.queries[1].executions - w.queries[2].executions;
    w.total_executions += extra;
    let (poll, beat) = (w.queries[1].id.clone(), w.queries[2].id.clone());
    let plant = |q: &mut culprit_core::QueryStat, text: &str, rows: f64| {
        q.text = text.into();
        q.object_name = None;
        q.executions = 30_000.0;
        q.rows = Some(rows);
        q.mean_time_ms = 0.1;
        q.total_time_ms = 3_000.0;
    };
    plant(&mut w.queries[1], "SELECT TOP (10) id FROM shop.jobs WHERE status = 'pending' ORDER BY id", 0.0);
    plant(&mut w.queries[2], "UPDATE shop.worker_lease SET expires_at = @p1 WHERE name = @p2", 30_000.0);
    let report = analyze(s);
    let p = finding(&report.findings, &format!("chatty-query:{poll}")).expect("poll");
    assert!(p.title.contains("most runs find nothing"), "{}", p.title);
    assert!(p.fix.as_ref().unwrap().summary.contains("back off"));
    let b = finding(&report.findings, &format!("chatty-query:{beat}")).expect("heartbeat");
    assert!(b.title.contains("one row each time"), "{}", b.title);
    assert!(!b.fix.as_ref().unwrap().summary.contains("before the loop"));
}

#[test]
fn a_filtered_statistic_is_measured_against_the_whole_table() {
    let mut s = snapshot();
    // A statistic over the 20K soft-deleted rows of a million-row table, its counter at 35K.
    s.statistics.push(
        serde_json::from_value(json!({
            "schema": "shop", "table": "orders", "name": "ix_orders_purge", "auto_created": false,
            "last_updated": "2026-10-10T03:00:00Z", "rows": 20000.0, "rows_sampled": 20000.0, "modifications": 35000.0
        }))
        .unwrap(),
    );
    let report = analyze(s);
    let wrong = report
        .findings
        .iter()
        .filter(|f| f.check == "stale-stats")
        .any(|f| f.metrics.iter().any(|m| m.label == "Rows in table" && m.value == 20_000.0));
    assert!(!wrong, "35K changes are 3.5% of a million rows, not 175% of the table");
}

#[test]
fn document_columns_stay_out_of_suggested_includes() {
    let mut s = snapshot();
    for (name, data_type) in [("zz_payload", "nvarchar(max)"), ("zz_note", "nvarchar(200)")] {
        s.columns.push(
            serde_json::from_value(json!({
                "schema": "shop", "table": "ticket_events", "name": name, "data_type": data_type,
                "nullable": true, "default": null, "generated": false, "ordinal": 90
            }))
            .unwrap(),
        );
    }
    s.missing_indexes.push(MissingIndex {
        schema: "shop".into(),
        table: "ticket_events".into(),
        equality: vec!["zz_channel".into()],
        inequality: Vec::new(),
        include: vec!["zz_payload".into(), "zz_note".into()],
        seeks: 9_000.0,
        scans: 0.0,
        avg_query_cost: 12.0,
        avg_impact_pct: 80.0,
        last_used: None,
        query_hashes: Vec::new(),
    });
    let report = analyze(s);
    let f = finding(&report.findings, "missing-index:shop.ticket_events(zz_channel)").expect("missing index");
    let sql = f.fix.as_ref().unwrap().sql.as_deref().unwrap();
    assert!(sql.contains("zz_note") && !sql.contains("zz_payload"), "{sql}");
    assert!(f.caveats.iter().any(|c| c.contains("zz_payload")), "{:?}", f.caveats);
    let advice = index_advice(&report);
    let c = advice.candidates.iter().find(|c| c.table == "ticket_events" && c.keys == ["zz_channel"]).unwrap();
    assert_eq!(c.include, ["zz_note"]);
}

#[test]
fn a_foreign_key_index_does_not_claim_the_database_time_of_queries_that_name_the_key() {
    let report = analyze(snapshot());
    let advice = index_advice(&report);
    let dashboard = culprit_checks::dashboard(&report);
    let measured: f64 = advice
        .candidates
        .iter()
        .filter(|c| c.source != CandidateSource::ForeignKey)
        .flat_map(|c| &c.queries)
        .filter_map(|q| q.share)
        .sum();
    let tile = dashboard.tiles.iter().find(|t| t.key == culprit_core::TileKey::Indexes).unwrap();
    if measured < 0.01 {
        assert!(!tile.takeaway.contains("of database time"), "{}", tile.takeaway);
    }
}

#[test]
fn one_request_seen_waiting_for_a_worker_is_worth_watching_not_high() {
    let mut s = snapshot();
    let signal = |value: f64| {
        Signal::new("threadpool-waiting", "Tasks without a worker", value, SignalUnit::Count, SignalScope::Now)
    };
    s.pressure = Some(Pressure { uptime_hours: Some(120.0), signals: vec![signal(1.0)], ..Default::default() });
    let report = analyze(s.clone());
    let f = finding(&report.findings, "worker-pressure:server")
        .or_else(|| report.findings.iter().find(|f| f.check == "worker-pressure"));
    let f = f.expect("worker pressure");
    assert_eq!(f.severity, Severity::Medium);
    s.pressure.as_mut().unwrap().signals = vec![signal(6.0)];
    let report = analyze(s);
    assert_eq!(report.findings.iter().find(|f| f.check == "worker-pressure").unwrap().severity, Severity::High);
}

#[test]
fn a_query_that_runs_in_bursts_is_not_taken_for_one_that_stopped() {
    // The demo's timeline has 15-minute buckets from 07:30; its data ends at 12:13. Query 0 is the probe.
    let probe = |active: &[usize], last_seen: &str| {
        let mut s = snapshot();
        let w = s.workload.as_mut().unwrap();
        let id = w.queries[0].id.clone();
        let buckets = w.timeline.as_ref().unwrap().starts.len();
        let trend = w.queries[0].trend.as_mut().unwrap();
        trend.executions = (0..buckets).map(|i| Some(if active.contains(&i) { 40.0 } else { 0.0 })).collect();
        trend.time_ms = trend.executions.clone();
        w.queries[0].last_seen = Some(last_seen.into());
        s.tuning = Some(TuningAdvice {
            recommendations: vec![TuningRecommendation {
                name: "PR_1".into(),
                kind: "FORCE_LAST_GOOD_PLAN".into(),
                query_id: Some(id.clone()),
                regressed_plan_id: Some("2".into()),
                recommended_plan_id: Some("1".into()),
                estimated_gain: Some(0.9),
                ..Default::default()
            }],
            ..Default::default()
        });
        let report = analyze(s);
        finding(&report.findings, &format!("tuning-advice:force:{id}")).expect("tuning advice").inactive
    };
    // Paused 2¼ hours (07:45–10:15) and came back: 1.9 hours of quiet since is its rhythm.
    assert!(!probe(&[0, 1, 11], "2026-10-11T10:20:00Z"));
    // Ran steadily until 09:45, then nothing for 2.4 hours: stopped.
    assert!(probe(&[0, 1, 2, 3, 4, 5, 6, 7, 8, 9], "2026-10-11T09:50:00Z"));
}

#[test]
fn advice_for_a_query_outside_the_window_that_has_not_run_for_days_is_marked_inactive() {
    let mut s = snapshot();
    s.tuning = Some(TuningAdvice {
        recommendations: vec![TuningRecommendation {
            name: "PR_9".into(),
            kind: "FORCE_LAST_GOOD_PLAN".into(),
            query_id: Some("90001".into()),
            regressed_plan_id: Some("2".into()),
            recommended_plan_id: Some("1".into()),
            last_execution: Some("2026-10-08T17:20:00Z".into()),
            ..Default::default()
        }],
        ..Default::default()
    });
    let report = analyze(s);
    let f = finding(&report.findings, "tuning-advice:force:90001").expect("tuning advice");
    assert!(f.inactive && f.caveats[0].contains("Query 90001 has not run since 2026-10-08 17:20"), "{:?}", f.caveats);
}

/// The demo plus small child tables of `shop.customers`, each with a foreign key and no index for it; the first one's
/// key is covered only by a filtered unique index.
fn with_small_children(count: usize) -> Snapshot {
    let mut s = snapshot();
    let template = s.indexes.iter().find(|i| i.table == "customers").cloned().expect("an index");
    for n in 0..count {
        let table = format!("customer_note_{n}");
        s.tables.push(
            serde_json::from_value(json!({
                "schema": "shop", "name": table, "rows": 500.0 + n as f64, "total_bytes": 65536.0,
                "data_bytes": 65536.0, "index_bytes": 0.0, "has_primary_key": true, "is_heap": false
            }))
            .unwrap(),
        );
        s.foreign_keys.push(
            serde_json::from_value(json!({
                "name": format!("fk_{table}_customer"), "schema": "shop", "table": table, "columns": ["customer_id"],
                "ref_schema": "shop", "ref_table": "customers", "ref_columns": ["id"]
            }))
            .unwrap(),
        );
        if n == 0 {
            let mut i = template.clone();
            i.name = format!("ux_{table}_customer_live");
            i.table = table.clone();
            i.columns = vec!["customer_id".into(), "kind".into()];
            i.include.clear();
            i.filter = Some("([is_deleted]=(0))".into());
            i.is_primary_key = false;
            i.is_clustered = false;
            i.is_unique = true;
            s.indexes.push(i);
        }
    }
    s
}

#[test]
fn most_foreign_keys_without_an_index_are_reported_as_a_habit() {
    let report = analyze(with_small_children(4));
    let f = finding(&report.findings, "unindexed-fk:schema").expect("schema-wide finding");
    // The demo's two large unindexed keys plus the four small ones, of its five keys plus the four new ones.
    assert_eq!(f.title, "6 of 9 foreign keys have no index that can serve them");
    assert!(
        f.summary.starts_with(
            "4 of these keys are on tables under 10K rows (2 on larger tables have findings of their own); they are"
        ),
        "{}",
        f.summary
    );
    assert!(f.summary.contains("ux_customer_note_0_customer_live"), "{}", f.summary);
    let sql = f.fix.as_ref().unwrap().sql.as_deref().unwrap();
    assert!((0..4).all(|n| sql.contains(&format!("[customer_note_{n}]"))), "{sql}");
    // Small tables get no finding of their own; the advisor points at the one that lists them.
    assert!(!report.findings.iter().any(|f| f.id.starts_with("unindexed-fk:shop.customer_note")));
    let advice = index_advice(&report);
    assert!(
        advice.notes.iter().any(|n| n.starts_with("4 more foreign keys") && n.contains(&f.title)),
        "{:?}",
        advice.notes
    );
}

#[test]
fn an_index_filtered_on_its_key_being_not_null_serves_the_key() {
    let mut s = with_small_children(4);
    for i in s.indexes.iter_mut().filter(|i| i.name == "ux_customer_note_0_customer_live") {
        i.filter = Some("([customer_id] IS NOT NULL)".into());
    }
    let report = analyze(s);
    let f = finding(&report.findings, "unindexed-fk:schema").expect("schema-wide finding");
    assert_eq!(f.title, "5 of 9 foreign keys have no index that can serve them");
    assert!(!f.summary.contains("filtered"), "{}", f.summary);
}

#[test]
fn a_key_repeated_or_led_by_another_gets_one_index() {
    let mut s = with_small_children(4);
    let first = s.foreign_keys.iter().find(|fk| fk.table == "customer_note_1").cloned().unwrap();
    let mut twin = first.clone();
    twin.name = "fk_customer_note_1_customer_again".into();
    let mut wider = first.clone();
    wider.name = "fk_customer_note_1_customer_kind".into();
    wider.columns = vec!["customer_id".into(), "kind".into()];
    s.foreign_keys.push(twin);
    s.foreign_keys.push(wider);
    let report = analyze(s);
    let sql = finding(&report.findings, "unindexed-fk:schema").unwrap().fix.as_ref().unwrap().sql.clone().unwrap();
    assert_eq!(sql.matches("[customer_note_1]").count(), 1, "{sql}");
    assert!(sql.contains("[customer_id], [kind]"), "{sql}");
}

#[test]
fn a_table_of_unknown_size_is_not_called_small() {
    let mut s = with_small_children(4);
    s.tables.iter_mut().filter(|t| t.name == "customer_note_3").for_each(|t| t.rows = None);
    let report = analyze(s);
    let f = finding(&report.findings, "unindexed-fk:schema").expect("schema-wide finding");
    assert!(f.summary.contains("1 is on a table whose size isn't known"), "{}", f.summary);
    assert!(!f.fix.as_ref().unwrap().sql.as_deref().unwrap().contains("[customer_note_3]"));
}

#[test]
fn every_key_unindexed_reads_as_none() {
    let mut s = with_small_children(4);
    s.foreign_keys.retain(|fk| fk.table.starts_with("customer_note"));
    let f = analyze(s).findings.into_iter().find(|f| f.id == "unindexed-fk:schema").expect("schema-wide finding");
    assert_eq!(f.title, "None of the 4 foreign keys has an index that can serve it");
    assert!(f.summary.starts_with("All 4 are on tables under 10K rows; they are"), "{}", f.summary);
}

#[test]
fn a_few_unindexed_keys_on_small_tables_are_not_a_habit() {
    assert!(finding(&analyze(snapshot()).findings, "unindexed-fk:schema").is_none());
    assert!(finding(&analyze(with_small_children(2)).findings, "unindexed-fk:schema").is_none());
}
