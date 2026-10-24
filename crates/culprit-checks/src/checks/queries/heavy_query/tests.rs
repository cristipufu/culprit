use super::rewrite::or_across_tables;
use super::*;
use culprit_core::{PlanKind, Properties};

fn node(id: u32, op: &str, kind: NodeKind, children: Vec<PlanNode>) -> PlanNode {
    PlanNode {
        facts: None,
        id,
        op: op.into(),
        kind,
        detail: None,
        object: None,
        index: None,
        seek_predicate: None,
        predicate: None,
        est_rows: 1.0,
        est_executions: 1.0,
        act_rows: None,
        act_executions: None,
        rows_read: None,
        table_rows: None,
        subtree_cost: 0.0,
        self_cost: 0.0,
        act_time_ms: None,
        runtime: None,
        warnings: vec![],
        props: Properties::new(),
        children,
    }
}

/// An app list as a PostgreSQL generic plan: apps and app_shares read in full, and `a.owner_id = $1 OR s.email =
/// $2` above the join, where the 5.9K apps the scan keeps become a row or two.
fn apps_plan() -> Plan {
    let mut apps = node(7, "Seq Scan", NodeKind::Scan, vec![]);
    apps.object = Some("public.apps".into());
    apps.props.insert("Alias".into(), "a".into());
    apps.predicate = Some("(a.deleted_at IS NULL)".into());
    (apps.est_rows, apps.rows_read, apps.table_rows) = (5_880.0, Some(6_285.0), Some(6_285.0));
    (apps.self_cost, apps.subtree_cost) = (186.43, 186.43);
    let mut shares = node(5, "Seq Scan", NodeKind::Scan, vec![]);
    shares.object = Some("public.app_shares".into());
    shares.props.insert("Alias".into(), "s".into());
    (shares.est_rows, shares.rows_read, shares.table_rows) = (845.0, Some(845.0), Some(845.0));
    (shares.self_cost, shares.subtree_cost) = (19.45, 19.45);
    let hash = node(6, "Hash", NodeKind::Other, vec![apps]);
    let mut join = node(4, "Hash Join", NodeKind::Join, vec![shares, hash]);
    join.predicate = Some("(((a.owner_id)::text = $1) OR ((s.email)::text = $2))".into());
    (join.est_rows, join.self_cost, join.subtree_cost) = (11.0, 75.72, 281.6);
    let mut root = node(0, "Sort", NodeKind::Sort, vec![join]);
    (root.est_rows, root.self_cost, root.subtree_cost) = (11.0, 0.21, 286.19);
    Plan {
        facts: None,
        view: None,
        engine: Engine::Postgres,
        kind: PlanKind::Generic,
        statement: None,
        total_cost: 286.19,
        root,
        warnings: vec![],
        index_hints: vec![],
        props: Properties::new(),
        raw: None,
        insights: None,
    }
}

const APPS_QUERY: &str = "SELECT a.id, a.name FROM apps a LEFT JOIN app_shares s ON s.app_id = a.id WHERE (a.owner_id = $1 OR s.email = $2) AND a.deleted_at IS NULL GROUP BY a.id, a.name ORDER BY a.name";

fn postgres_demo() -> Snapshot {
    serde_json::from_str(include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../fixtures/postgres-demo.json")))
        .expect("fixture")
}

/// An index of the demo's on `public.<table>` (`columns`), as the catalog would list it.
fn index(snapshot: &Snapshot, table: &str, name: &str, columns: &[&str]) -> culprit_core::IndexStat {
    let mut index = snapshot.indexes[0].clone();
    (index.schema, index.table, index.name) = ("public".into(), table.into(), name.into());
    index.columns = columns.iter().map(|c| c.to_string()).collect();
    (index.include, index.filter, index.is_disabled) = (vec![], None, false);
    index
}

/// The fix for the scan of apps, for runs that return `rows` each.
fn scan_fix_on(snapshot: &Snapshot, rows: f64) -> Fix {
    let plan = apps_plan();
    let mut q = snapshot.workload.as_ref().expect("workload").queries[0].clone();
    q.text = APPS_QUERY.into();
    q.rows = Some(rows * q.executions);
    q.plan = Some(plan.clone());
    let scan = plan.nodes().into_iter().find(|n| n.id == 7).expect("the scan of apps");
    heavy_fix_and_lever(snapshot, Some((&plan, scan)), None, &q, statement_kind(&q.text), false).0
}

fn scan_fix(rows: f64) -> String {
    scan_fix_on(&postgres_demo(), rows).summary
}

#[test]
fn a_scan_narrowed_later_is_not_too_many_rows_for_an_index() {
    // The scan keeps 94% of apps, but a run returns two rows: the OR above the join narrows them.
    let fix = scan_fix(2.0);
    assert!(!fix.contains("too many for an index"), "{fix}");
    assert!(fix.contains("returns about 2 rows a run: they are narrowed later in the plan"), "{fix}");
    assert!(fix.contains("OR spans different tables") && fix.contains("UNION rewrite"), "{fix}");
    // Runs that return about what the scan keeps need those rows: no index serves them.
    let fix = scan_fix(5_000.0);
    assert!(fix.contains("keeps about 94% of the rows, too many for an index to help"), "{fix}");
    // The aptabase-us reading: "returns about 3 rows a run" beside 2.5 in the evidence. Rounded as it is.
    assert!(scan_fix(2.5).contains("the statement returns about 2.5 rows a run"), "{}", scan_fix(2.5));
    assert!(scan_fix(1.96).contains("the statement returns about 2 rows a run"), "{}", scan_fix(1.96));
}

// The fresh PostgreSQL demo: `SELECT … FOR UPDATE` that waited 38 s for a row lock was a read, told to read fewer
// rows. A read that locks what it finds waits as a write does.
#[test]
fn rewrite_observation_is_shared_without_premature_index_sql() {
    let s = postgres_demo();
    let mut q = s.queries()[0].clone();
    q.text = APPS_QUERY.into();
    q.executions = 1000.0;
    q.rows = Some(2000.0);
    q.total_time_ms = 1.0;
    q.mean_time_ms = 0.001;
    q.plan = Some(apps_plan());
    let p = q.plan.as_ref().unwrap();
    let insight = crate::query_plan_insights(&s, &q, p).items.into_iter().find(|i| i.rule == "cross-table-or").unwrap();
    assert_eq!(insight.area, culprit_core::insights::InsightArea::Query);
    assert_eq!(insight.severity, Severity::Low);
    assert!(insight.fix_sql.is_none());
    assert!(insight.fix.contains("permission checks") && insight.verify.contains("selectivity"));
    let f = heavy_query_finding(&s, &q);
    assert_eq!(f.actionability, culprit_core::Actionability::Information);
    assert!(f.fix.unwrap().sql.is_none());
    assert_eq!(f.queries[0].plan_node, insight.nodes.first().copied());
    for text in [format!("{APPS_QUERY} FOR UPDATE"), "UPDATE apps SET name = $1".into()] {
        let mut writes = q.clone();
        writes.text = text;
        assert!(
            late_filter_rewrite(&s, &writes, p).is_none(),
            "read rewrites do not change locking or write semantics"
        );
    }
    let mut with_query = s.clone();
    with_query.workload.as_mut().unwrap().queries = vec![q.clone()];
    let report = crate::analyze(with_query);
    let advisor = crate::index_advice(&report);
    assert!(advisor.notes.iter().any(|note| note.contains("Query rewrites to evaluate") && note.contains(&q.id)));
    // Larger tables keep the structural concern, without claiming that a rewrite has measured benefit.
    let mut large = p.clone();
    large.root.children[0].children[1].children[0].table_rows = Some(6_285_000.0);
    large.root.children[0].children[1].children[0].rows_read = Some(6_285_000.0);
    large.root.children[0].children[1].children[0].est_rows = 5_880_000.0;
    assert!(late_filter_rewrite(&s, &q, &large).is_some());
    let mut without_rows = q.clone();
    without_rows.rows = None;
    let observation = late_filter_rewrite(&s, &without_rows, &large).unwrap();
    assert!(!observation.fix.contains("statement returns"), "no invented row count");
    // Engine-owned objects are never candidates for a rewrite or index change.
    for n in &mut large.root.children[0].children {
        n.object = Some("pg_catalog.pg_class".into());
        for c in &mut n.children {
            c.object = Some("pg_catalog.pg_class".into());
        }
    }
    assert!(late_filter_rewrite(&s, &q, &large).is_none());
}

#[test]
fn a_read_that_locks_its_rows_is_a_locking_statement() {
    for text in [
        "SELECT id FROM orders WHERE id = $1 FOR UPDATE",
        "SELECT id FROM jobs WHERE state = $1 LIMIT $2 FOR UPDATE SKIP LOCKED",
        "SELECT id FROM orders WHERE id = $1 FOR NO KEY UPDATE",
        "SELECT * FROM accounts WHERE id = ? LOCK IN SHARE MODE",
        "SELECT [id] FROM [shop].[orders] WITH (UPDLOCK, ROWLOCK) WHERE [id] = @P1",
    ] {
        assert_eq!(statement_kind(text), StatementKind::Locking, "{text}");
    }
    for (text, kind) in [
        ("SELECT updated_for, for_update FROM t WHERE x = $1", StatementKind::Read),
        ("SELECT count(*) FROM orders WHERE status = $1", StatementKind::Aggregate),
        (
            "WITH b AS (SELECT id FROM jobs LIMIT 10) DELETE FROM jobs USING b WHERE jobs.id = b.id",
            StatementKind::Write,
        ),
    ] {
        assert_eq!(statement_kind(text), kind, "{text}");
    }
    // A plan that locks the rows it returns says so whatever the text shows.
    let mut q = postgres_demo().workload.unwrap().queries[0].clone();
    q.text = "SELECT id FROM orders WHERE id = $1".into();
    let mut plan = apps_plan();
    plan.root.op = "LockRows".into();
    q.plan = Some(plan);
    assert_eq!(kind_of(&q), StatementKind::Locking);
}

#[test]
fn each_side_of_a_union_names_the_index_it_has_or_the_one_it_needs() {
    // apps has an index on owner_id; app_shares' key leads with app_id, so the email side has none.
    let mut snapshot = postgres_demo();
    let owner = index(&snapshot, "apps", "idx_apps_owner", &["owner_id"]);
    let key = index(&snapshot, "app_shares", "PK_app_shares", &["app_id", "email"]);
    snapshot.indexes.extend([owner, key]);
    let fix = scan_fix_on(&snapshot, 2.0);
    assert!(!fix.summary.contains("an index of its own"), "{}", fix.summary);
    assert!(fix.summary.contains("idx_apps_owner on public.apps leads with owner_id"), "{}", fix.summary);
    assert!(fix.summary.contains("public.app_shares has no index that leads with email"), "{}", fix.summary);
    assert!(fix.sql.is_none(), "measure branch selectivity after a rewrite before proposing DDL");
    assert!(fix.summary.contains("Preserve the original joins, permission checks"));
    let mut unavailable = snapshot.clone();
    unavailable.server.capabilities.retain(|c| *c != Capability::IndexUsage);
    unavailable.notes.push(culprit_core::CollectorNote { collector: "indexes".into(), message: "unavailable".into() });
    let unknown = scan_fix_on(&unavailable, 2.0);
    assert!(!unknown.summary.contains("has no index"), "unavailable definitions do not prove absence");
    // With an index on email too, each side names its own and there is nothing to create.
    snapshot.indexes.push(index(&snapshot, "app_shares", "app_shares_email_idx", &["email"]));
    let fix = scan_fix_on(&snapshot, 2.0);
    assert!(fix.summary.contains("app_shares_email_idx on public.app_shares leads with email"), "{}", fix.summary);
    assert!(fix.sql.is_none(), "{:?}", fix.sql);
}

#[test]
fn a_whole_table_read_under_the_cost_bar_gets_the_scan_steps_advice() {
    // Costed at 177, under the 250 a step's share needs, yet not cheap: it reads all of apps on every run. The fix
    // comes from that scan (the OR as a UNION), not "kept up to date instead of recomputed" for a 2-row lookup.
    let mut snapshot = postgres_demo();
    snapshot.indexes.push(index(&snapshot, "apps", "idx_apps_owner", &["owner_id"]));
    let mut plan = apps_plan();
    (plan.total_cost, plan.root.subtree_cost) = (177.44, 177.44);
    let q = &mut snapshot.workload.as_mut().expect("workload").queries[0];
    q.text = APPS_QUERY.into();
    (q.mean_time_ms, q.executions) = (1.44, q.total_time_ms / 1.44);
    (q.rows, q.logical_reads) = (Some(2.5 * q.executions), Some(60.0 * q.executions));
    q.plan = Some(plan);
    let id = q.id.clone();
    let f = heavy_query(&snapshot).into_iter().find(|f| f.queries[0].id == id).expect("the heavy query");
    let fix = f.fix.expect("a fix");
    assert!(fix.summary.contains("UNION rewrite"), "{}", fix.summary);
    assert!(!fix.summary.contains("kept up to date"), "{}", fix.summary);
    assert!(!f.summary.contains("of its estimated cost"), "no share under the bar: {}", f.summary);
    // The summary names the scan the fix is about.
    assert!(
        f.summary.ends_with(
            "Every run includes a seq scan of public.apps that reads 6.3K rows: a scan reads the whole table, and costs more as the table grows."
        ),
        "{}",
        f.summary
    );
    assert_eq!(f.queries[0].plan_node, Some(7), "the step the fix is about");
}

#[test]
fn a_plan_that_reads_a_whole_table_is_not_called_cheap() {
    // Costed under what reading 10K rows costs, a millisecond a run and ten pages per row: cheap only while no
    // step reads a table in full, which costs more as the table grows.
    let snapshot = postgres_demo();
    let mut plan = apps_plan();
    (plan.total_cost, plan.root.subtree_cost) = (100.0, 100.0);
    let mut q = snapshot.workload.as_ref().expect("workload").queries[0].clone();
    (q.mean_time_ms, q.logical_reads, q.rows) = (1.0, Some(20.0 * q.executions), Some(2.0 * q.executions));
    assert!(!culprit_core::insights::runs_cheaply(&plan, &q));
    // The same plan finding its rows through indexes.
    fn seek(n: &mut PlanNode) {
        if n.kind == NodeKind::Scan {
            n.kind = NodeKind::Seek;
        }
        n.children.iter_mut().for_each(seek);
    }
    seek(&mut plan.root);
    assert!(culprit_core::insights::runs_cheaply(&plan, &q));
}

#[test]
fn an_or_across_tables_is_told_from_one_within_a_table() {
    assert!(or_across_tables("(((a.owner_id)::text = $1) OR ((s.email)::text = $2))"));
    assert!(or_across_tables(
        "[db].[app].[messages].[tenant_id]=[@par0] OR [db].[app].[conversations].[account_id]=[@par1]"
    ));
    assert!(!or_across_tables("((a.status = 1) OR (a.status = 2)) AND b.id = a.owner_id"));
    assert!(!or_across_tables("(s.label = 'black or white') AND (a.id = s.app_id)"));
    assert!(!or_across_tables("(a.deleted_at IS NULL)"));
}

#[test]
fn a_pools_statements_get_the_pools_own_lever() {
    // PostgreSQL's DISCARD ALL is the pool's reset: its setting, not transactions or pings.
    let discard = session_fix(Engine::Postgres, "DISCARD ALL");
    assert!(
        discard.contains("do not identify its caller or pooling mode") && discard.contains("Keep resets"),
        "{discard}"
    );
    assert!(!discard.contains("sp_reset_connection"), "{discard}");
    assert!(discard.contains("Npgsql") && discard.contains("PgBouncer") && discard.contains("benchmark"), "{discard}");
    for engine in [Engine::Postgres, Engine::Yugabyte] {
        for reset in ["DISCARD ALL", "RESET ALL", "/* pooled */ DISCARD ALL"] {
            let advice = session_fix(engine, reset);
            assert!(advice.contains("Keep resets") && advice.contains("do not identify its caller"), "{advice}");
            assert!(!advice.contains("sp_reset_connection"), "{advice}");
        }
    }
    // sp_reset_connection is SQL Server's.
    let set = session_fix(Engine::Mssql, "SET TRANSACTION ISOLATION LEVEL READ COMMITTED");
    assert!(set.contains("sp_reset_connection"), "{set}");
    let commit = session_fix(Engine::Postgres, "COMMIT");
    assert!(commit.contains("transaction of its own") && !commit.contains("sp_reset_connection"), "{commit}");
    let ping = session_fix(Engine::Mysql, "/* ping */ SELECT 1");
    assert!(ping.contains("still alive"), "{ping}");
}

#[test]
fn write_row_labels_use_the_declared_counter_meaning() {
    for (engine, expected) in [
        (Engine::Postgres, Some("Rows changed per run")),
        (Engine::Yugabyte, Some("Rows changed per run")),
        (Engine::Mssql, Some("Rows per run")),
        (Engine::Mysql, None),
    ] {
        assert_eq!(rows_label(engine, StatementKind::Write).as_deref(), expected);
        assert_eq!(rows_label(engine, StatementKind::Read).as_deref(), Some("Rows returned per run"));
    }
}
