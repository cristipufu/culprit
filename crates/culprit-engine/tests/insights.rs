//! Plan insights on real plans captured from the three demo databases (dev/mssql, dev/postgres, dev/mysql): each plan was
//! written to show one problem, and the matching rule must find it, name it plainly, point at the right operator and say
//! how to fix it, on every engine.

use culprit_core::insights::{PlanInsight, coverage_note, insights};
use culprit_core::{Engine, Severity};
use culprit_engine::plans::parse_plan;

fn load(name: &str) -> culprit_core::Plan {
    load_from("tests/insights", name)
}

fn load_from(dir: &str, name: &str) -> culprit_core::Plan {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(dir).join(name);
    parse_plan(&std::fs::read_to_string(&path).unwrap()).unwrap_or_else(|e| panic!("{name}: {}", e.message))
}

/// What the app adds to a PostgreSQL estimate from `pg_class.reltuples` (culprit-postgres `tables::fill`): the table's
/// size on every scan of it, and the rows a sequential scan reads.
fn with_table_size(mut plan: culprit_core::Plan, table: &str, rows: f64) -> culprit_core::Plan {
    fn apply(n: &mut culprit_core::PlanNode, table: &str, rows: f64) {
        if n.object.as_deref() == Some(table) {
            n.table_rows = Some(rows);
            if n.op.contains("Seq Scan") && n.rows_read.is_none() && n.act_rows.is_none() {
                n.rows_read = Some(rows);
            }
        }
        n.children.iter_mut().for_each(|c| apply(c, table, rows));
    }
    apply(&mut plan.root, table, rows);
    plan
}

fn rules_of(plan: &culprit_core::Plan) -> Vec<String> {
    insights(plan).into_iter().map(|i| i.rule).collect()
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
    // The plan can't rebuild ix_orders_status safely (its other keys, included columns, options): words, no script.
    assert!(lookups.fix.contains("as included columns") && lookups.fix_sql.is_none(), "{lookups:?}");

    let conversion = find("mssql-implicit.sqlplan", "implicit-conversion");
    assert!(conversion.title.contains("customers.email"), "{}", conversion.title);
    assert!(!rules("mssql-implicit.sqlplan").contains(&"scan-filter".into()), "the conversion explains the scan");

    assert!(find("mssql-nonsargable.sqlplan", "non-sargable").title.starts_with("DATEPART() around created_at"));
    // Every address ends in @example.com: SQL Server expects the filter to keep all 200K rows, and no index on
    // email (full-text or otherwise) would read fewer.
    assert!(!rules("mssql-like.sqlplan").contains(&"leading-wildcard".into()));
    assert!(find("mssql-notin.sqlplan", "not-in").title.contains("NULL"));
    assert!(!rules("mssql-notin.sqlplan").contains(&"nested-loops".into()), "the spool loop is NOT IN's doing");
    assert!(find("mssql-spool.sqlplan", "eager-spool").fix_sql.is_some());
    // An estimated plan leaves the function's body out of its costs; 443K calls a run is the evidence, worth Medium.
    let udf = find("mssql-udf.sqlplan", "scalar-udf");
    assert_eq!(udf.severity, Severity::Medium);
    assert!(udf.title.starts_with("shop.fn_age_label runs once for every row"));
    assert!(find("mssql-tablevar.sqlplan", "table-variable").title.contains("@t"));
    // A cost of 31 is below the cost threshold for parallelism many servers use (50): whether it would split depends
    // on the server's setting, which the plan doesn't carry.
    let maxdop = find("mssql-maxdop1.sqlplan", "parallelism-blocked");
    assert!(maxdop.title.contains("MAXDOP") && maxdop.severity == Severity::Low, "{maxdop:?}");
    let hint = find("mssql-missing.sqlplan", "missing-index-hint");
    assert!(hint.fix_sql.as_deref().unwrap().starts_with("CREATE NONCLUSTERED INDEX"));
    assert!(find("mssql-sort-spill.sqlplan", "spill").title.contains("tempdb"));
    assert!(find("mssql-grant.sqlplan", "memory-grant").title.starts_with("Reserves 41.5 MB"));
    // GrantWaitTime is in seconds (showplan schema): these runs waited 3 s for their grant.
    let wait = insights(&load("mssql-sort-spill.sqlplan")).into_iter().find(|i| i.title.starts_with("Waited"));
    assert_eq!(wait.map(|i| i.title), Some("Waited 3.0 s for memory before it could start".into()));
    let loops = find("mssql-loops.sqlplan", "nested-loops");
    assert!(loops.fix.contains("forces a loop join"), "{}", loops.fix);
    // The orders scan returns 30K of the 1M rows it expected because the hash join's bitmap filtered it, which the
    // scan's estimate leaves out: not a misestimate.
    assert!(!rules("mssql-loops.sqlplan").contains(&"misestimate".into()));
    // TOP 10 over a cross join costs next to nothing: it's said, but Low.
    let cross = find("mssql-cross.sqlplan", "no-join-predicate");
    assert_eq!(cross.severity, Severity::Low);
    // "GoodEnoughPlanFound" is the optimizer finishing normally, not a timeout.
    assert!(!rules("mssql-cross.sqlplan").contains(&"optimizer-timeout".into()));
}

#[test]
fn postgres_shapes() {
    assert_eq!(find("pg-seqscan-filter.json", "scan-filter").title, "Reads 2.0M rows of orders to keep 14");
    // A parallel scan's Plan Rows are one process's share (4,167 of 2 workers and the leader): scaled to the ~10K the
    // scan returns in all, next to the 2M rows the table holds.
    let function = with_table_size(load("pg-function.json"), "public.orders", 2_000_000.0);
    let scan = &function.root.children[0];
    assert!((scan.est_rows - 4_167.0 * 2.4).abs() < 1.0, "{}", scan.est_rows);
    let nonsargable = insights(&function).into_iter().find(|i| i.rule == "non-sargable").expect("non-sargable");
    assert!(nonsargable.title.starts_with("DATE_TRUNC() around created_at"), "{}", nonsargable.title);
    // Without the table's size the estimate can't say how much of it the filter keeps: no index advice.
    assert!(!rules("pg-function.json").contains(&"non-sargable".into()));
    // Every customer's address ends in @example.com: PostgreSQL expects the filter to keep 199,980 of 200K rows, so a
    // trigram index wouldn't read fewer.
    assert!(rules_of(&with_table_size(load("pg-like.json"), "public.customers", 200_000.0)).is_empty());
    assert!(find("pg-notin.json", "not-in").title.contains("anti-join"));
    assert!(!rules("pg-notin.json").contains(&"correlated-subquery".into()), "NOT IN's subplan isn't reported twice");
    assert!(find("pg-sort-spill.json", "spill").title.starts_with("The sort writes"));
    assert!(rules("pg-hash-batches.json").iter().filter(|r| *r == "spill").count() >= 2, "sort and hash both spill");
    let mis = find("pg-misestimate.json", "misestimate");
    assert!(mis.title.starts_with("Expected 438 rows from customers"), "{}", mis.title);
    // The filter is on `id % 4`, an expression: multi-column statistics on plain columns wouldn't help.
    assert!(!mis.fix_sql.as_deref().unwrap().contains("CREATE STATISTICS"), "{:?}", mis.fix_sql);
    assert!(mis.fix.contains("expression"), "{}", mis.fix);
    // 400K one-row index lookups on an outer side estimated at 501K: the optimizer weighed a hash join against this loop
    // with the right numbers and chose the loop.
    assert!(!rules("pg-loops.json").contains(&"nested-loops".into()));
    // 8K heap fetches in a 2.7 ms run: said, but Low.
    let fetches = find("pg-heap-fetches.json", "heap-fetches");
    assert!(fetches.fix_sql.as_deref().unwrap().starts_with("VACUUM"));
    assert_eq!(fetches.severity, Severity::Low);
    assert!(find("pg-cross.json", "no-join-predicate").title.contains("customers and products"));
    assert!(!rules("pg-cross.json").contains(&"nested-loops".into()), "a cross join isn't also a loop problem");

    // Below a Limit, 20 rows of an expected 400K is the Limit stopping the scan, not a misestimate.
    assert!(!rules_of(&load_from("tests/plans", "pg-analyze.txt")).contains(&"misestimate".into()));
    // An InitPlan that never ran: no rows to compare, and its Gather launched no workers because it never asked.
    let subplans = rules_of(&load_from("tests/plans", "pg-subplans.txt"));
    assert!(!subplans.contains(&"misestimate".into()) && !subplans.contains(&"workers-short".into()), "{subplans:?}");
}

#[test]
fn mysql_shapes() {
    // `filtered: 10.00` is MySQL's fixed guess for an equality on a column without statistics, not a measured tenth:
    // the plan can't say an index would pay. EXPLAIN ANALYZE measures it.
    assert!(!rules("mysql-scan-filter.json").contains(&"scan-filter".into()));
    assert_eq!(find("mysql-analyze-scan.txt", "scan-filter").title, "Reads 500K rows of orders to keep 0");
    // MySQL made no estimate for the cast (`filtered: 100.00`) and guessed 11.11% for the LIKE: neither says how much
    // the filter keeps, so neither rewrite can be shown to let an index pay.
    assert!(!rules("mysql-function.json").contains(&"non-sargable".into()));
    assert!(!rules("mysql-like.json").contains(&"leading-wildcard".into()));
    // MySQL 8.0.17+ runs this NOT IN as an antijoin (`Not exists`) against the subquery materialized once.
    let notin = rules("mysql-notin.json");
    assert!(!notin.contains(&"not-in".into()) && !notin.contains(&"nested-loops".into()), "{notin:?}");
    assert!(find("mysql-filesort-temp.json", "temp-table-sort").title.contains("temporary table"));
    assert!(find("mysql-join-buffer.json", "join-buffer").title.contains("without an index"));
    assert!(find("mysql-dependent.json", "correlated-subquery").title.starts_with("A subquery runs 199K times"));
    // 200K index lookups on an outer side estimated at 199K: MySQL's plan, with the right numbers.
    assert!(!rules("mysql-analyze-join.txt").contains(&"nested-loops".into()));
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
/// desktop app computes them live). Run with `python3 dev/refresh-demo.py`.
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
        let report = culprit_engine::reading::open(&root.join(&name)).unwrap();
        let mut by_query = serde_json::Map::new();
        for q in report.snapshot.workload.iter().flat_map(|w| &w.queries) {
            if let Some(plan) = &q.plan {
                let set = culprit_checks::query_plan_insights(&report.snapshot, q, plan);
                by_query.insert(q.id.clone(), serde_json::to_value(set).unwrap());
            }
        }
        out.insert(report.snapshot.connection.clone(), serde_json::Value::Object(by_query));
    }
    let text = serde_json::to_string_pretty(&serde_json::Value::Object(out)).unwrap();
    std::fs::write(root.join("insights.sample.json"), text + "\n").unwrap();
}
