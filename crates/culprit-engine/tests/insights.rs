//! Plan insights on real plans captured from the three demo databases (dev/mssql, dev/postgres, dev/mysql): each plan was
//! written to show one problem, and the matching rule must find it, name it plainly, point at the right operator and say
//! how to fix it, on every engine.

use culprit_core::insights::{PlanInsight, coverage_note, insights};
use culprit_core::{Engine, Severity};
use culprit_engine::plans::parse_plan;

fn load(name: &str) -> culprit_core::Plan {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/insights").join(name);
    parse_plan(&std::fs::read_to_string(&path).unwrap()).unwrap_or_else(|e| panic!("{name}: {}", e.message))
}

fn find(name: &str, rule: &str) -> PlanInsight {
    let plan = load(name);
    let all = insights(&plan);
    let found = all.iter().find(|i| i.rule == rule).cloned();
    found.unwrap_or_else(|| {
        panic!("{name}: no {rule} insight; got {:?}", all.iter().map(|i| &i.rule).collect::<Vec<_>>())
    })
}

fn rules(name: &str) -> Vec<String> {
    insights(&load(name)).into_iter().map(|i| i.rule).collect()
}

/// Every insight is complete: a sentence, a fix, a verify step and an operator to show.
fn complete(name: &str) {
    for i in insights(&load(name)) {
        assert!(
            !i.title.is_empty() && !i.detail.is_empty() && !i.fix.is_empty() && !i.verify.is_empty(),
            "{name}: {i:?}"
        );
        assert!(!i.nodes.is_empty(), "{name}: {} points at no operator", i.rule);
        assert!((0.0..=1.0).contains(&i.impact), "{name}: impact {}", i.impact);
    }
}

#[test]
fn every_real_plan_gives_complete_insights() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/insights");
    for entry in std::fs::read_dir(dir).unwrap() {
        complete(&entry.unwrap().file_name().to_string_lossy());
    }
}

#[test]
fn sql_server_shapes() {
    let lookups = find("mssql-lookups.sqlplan", "key-lookups");
    assert_eq!(lookups.title, "Looks up 20K rows in shop.orders one at a time");
    assert!(lookups.fix_sql.as_deref().unwrap().contains("INCLUDE"), "{:?}", lookups.fix_sql);

    let conversion = find("mssql-implicit.sqlplan", "implicit-conversion");
    assert!(conversion.title.contains("customers.email"), "{}", conversion.title);
    assert!(!rules("mssql-implicit.sqlplan").contains(&"scan-filter".into()), "the conversion explains the scan");

    assert!(find("mssql-nonsargable.sqlplan", "non-sargable").title.starts_with("DATEPART() around created_at"));
    assert!(find("mssql-like.sqlplan", "leading-wildcard").title.contains("email"));
    assert!(find("mssql-notin.sqlplan", "not-in").title.contains("NULL"));
    assert!(!rules("mssql-notin.sqlplan").contains(&"nested-loops".into()), "the spool loop is NOT IN's doing");
    assert!(find("mssql-spool.sqlplan", "eager-spool").fix_sql.is_some());
    assert_eq!(find("mssql-udf.sqlplan", "scalar-udf").severity, Severity::High);
    assert!(find("mssql-udf.sqlplan", "scalar-udf").title.starts_with("shop.fn_age_label runs once for every row"));
    assert!(find("mssql-tablevar.sqlplan", "table-variable").title.contains("@t"));
    assert!(find("mssql-maxdop1.sqlplan", "parallelism-blocked").title.contains("MAXDOP"));
    let hint = find("mssql-missing.sqlplan", "missing-index-hint");
    assert!(hint.fix_sql.as_deref().unwrap().starts_with("CREATE NONCLUSTERED INDEX"));
    assert!(find("mssql-sort-spill.sqlplan", "spill").title.contains("tempdb"));
    assert!(find("mssql-grant.sqlplan", "memory-grant").title.starts_with("Reserves 41.5 MB"));
    let loops = find("mssql-loops.sqlplan", "nested-loops");
    assert!(loops.fix.contains("forces a loop join"), "{}", loops.fix);
    assert_eq!(find("mssql-cross.sqlplan", "no-join-predicate").severity, Severity::High);
    // "GoodEnoughPlanFound" is the optimizer finishing normally, not a timeout.
    assert!(!rules("mssql-cross.sqlplan").contains(&"optimizer-timeout".into()));
}

#[test]
fn postgres_shapes() {
    assert_eq!(find("pg-seqscan-filter.json", "scan-filter").title, "Reads 2.0M rows of orders to keep 14");
    assert!(find("pg-function.json", "non-sargable").title.starts_with("DATE_TRUNC() around created_at"));
    let like = find("pg-like.json", "leading-wildcard");
    assert!(like.fix_sql.as_deref().unwrap().contains("gin_trgm_ops"));
    assert!(find("pg-notin.json", "not-in").title.contains("anti-join"));
    assert!(!rules("pg-notin.json").contains(&"correlated-subquery".into()), "NOT IN's subplan isn't reported twice");
    assert!(find("pg-sort-spill.json", "spill").title.starts_with("The sort writes"));
    assert!(rules("pg-hash-batches.json").iter().filter(|r| *r == "spill").count() >= 2, "sort and hash both spill");
    let mis = find("pg-misestimate.json", "misestimate");
    assert!(mis.title.starts_with("Expected 438 rows from customers"), "{}", mis.title);
    // The filter is on `id % 4`, an expression: multi-column statistics on plain columns wouldn't help.
    assert!(!mis.fix_sql.as_deref().unwrap().contains("CREATE STATISTICS"), "{:?}", mis.fix_sql);
    assert!(mis.fix.contains("expression"), "{}", mis.fix);
    assert!(find("pg-loops.json", "nested-loops").title.contains("400K times"));
    assert!(find("pg-heap-fetches.json", "heap-fetches").fix_sql.as_deref().unwrap().starts_with("VACUUM"));
    assert!(find("pg-cross.json", "no-join-predicate").title.contains("customers and products"));
    assert!(!rules("pg-cross.json").contains(&"nested-loops".into()), "a cross join isn't also a loop problem");
}

#[test]
fn mysql_shapes() {
    assert_eq!(find("mysql-scan-filter.json", "scan-filter").title, "Reads 499K rows of orders to keep 50K");
    assert_eq!(find("mysql-analyze-scan.txt", "scan-filter").title, "Reads 500K rows of orders to keep 0");
    assert!(find("mysql-function.json", "non-sargable").title.starts_with("A cast on created_at"));
    assert!(find("mysql-like.json", "leading-wildcard").title.contains("email"));
    assert!(find("mysql-notin.json", "not-in").title.contains("NOT IN"));
    assert!(!rules("mysql-notin.json").contains(&"nested-loops".into()));
    assert!(find("mysql-filesort-temp.json", "temp-table-sort").title.contains("temporary table"));
    assert!(find("mysql-join-buffer.json", "join-buffer").title.contains("without an index"));
    assert!(find("mysql-dependent.json", "correlated-subquery").title.starts_with("A subquery runs 199K times"));
    assert!(find("mysql-analyze-join.txt", "nested-loops").title.contains("200K times"));
}

#[test]
fn coverage_notes_say_what_each_plan_cannot_show() {
    assert!(coverage_note(&load("mssql-sort-spill.sqlplan")).is_none(), "an actual SQL Server plan has it all");
    assert!(coverage_note(&load("mssql-like.sqlplan")).unwrap().contains("actual plan"));
    assert!(coverage_note(&load("pg-like.json")).unwrap().contains("EXPLAIN ANALYZE"));
    assert!(coverage_note(&load("mysql-like.json")).unwrap().contains("EXPLAIN ANALYZE"));
    assert!(coverage_note(&load("mysql-analyze-join.txt")).unwrap().contains("spills"));
    let engines: Vec<Engine> =
        ["mssql-like.sqlplan", "pg-like.json", "mysql-like.json"].iter().map(|f| load(f).engine).collect();
    assert_eq!(engines, vec![Engine::Mssql, Engine::Postgres, Engine::Mysql]);
}

/// Regenerates `fixtures/insights.sample.json`, the browser demo's insights for the plans in the demo reports (the
/// desktop app computes them live). Run with `cargo test -p culprit-engine --test insights -- --ignored`.
#[test]
#[ignore]
fn write_demo_sidecar() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures");
    let mut out = serde_json::Map::new();
    let mut names: Vec<_> = std::fs::read_dir(&root)
        .unwrap()
        .filter_map(|e| e.ok().map(|e| e.file_name().to_string_lossy().into_owned()))
        .filter(|n| n.ends_with(".report.json"))
        .collect();
    names.sort();
    for name in names {
        let report: culprit_core::Report = serde_json::from_slice(&std::fs::read(root.join(&name)).unwrap()).unwrap();
        let mut by_query = serde_json::Map::new();
        for q in report.snapshot.workload.iter().flat_map(|w| &w.queries) {
            if let Some(plan) = &q.plan {
                let set = culprit_core::insights::PlanInsights { items: insights(plan), note: coverage_note(plan) };
                by_query.insert(q.id.clone(), serde_json::to_value(set).unwrap());
            }
        }
        out.insert(report.snapshot.connection.clone(), serde_json::Value::Object(by_query));
    }
    let text = serde_json::to_string_pretty(&serde_json::Value::Object(out)).unwrap();
    std::fs::write(root.join("insights.sample.json"), text + "\n").unwrap();
}
