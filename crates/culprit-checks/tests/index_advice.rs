//! The index advisor and the checks behind it, on the three demo snapshots with planted conditions.

use culprit_checks::{analyze, index_advice};
use culprit_core::{
    AutomaticTuningOption, CleanupKind, ForcedPlan, PlanCacheUse, QueryFamily, Setting, Snapshot, TableAdviceKind,
    TuningAdvice, TuningRecommendation,
};

fn snapshot(name: &str) -> Snapshot {
    let text =
        std::fs::read_to_string(format!("{}/../../fixtures/{name}", env!("CARGO_MANIFEST_DIR"))).expect("fixture");
    serde_json::from_str(&text).expect("snapshot")
}

/// Counters that have been running for ten days, so per-day figures are meaningful.
fn ten_days(mut s: Snapshot) -> Snapshot {
    s.server.started_at = Some("2026-10-01T12:00:00Z".into());
    s.taken_at = "2026-10-11T12:00:00Z".into();
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
        add("ix_busy", &["zz_tenant", "zz_created"], 900_000.0);
        add("ix_rare", &["zz_tenant", "zz_status"], 12.0);
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
