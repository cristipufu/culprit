#[test]
fn names_in_scripts_cannot_break_out_of_their_quotes() {
    assert_eq!(quote(Engine::Mssql, "a]; DROP TABLE t; --"), "[a]]; DROP TABLE t; --]");
    assert_eq!(quote(Engine::Mysql, "a`; DROP TABLE t; --"), "`a``; DROP TABLE t; --`");
    assert_eq!(quote(Engine::Postgres, "a\"; DROP TABLE t; --"), "\"a\"\"; DROP TABLE t; --\"");
    assert_eq!(quote(Engine::Postgres, "orders"), "orders");
    // Names read back from plans are unescaped once, so quoting them again round-trips.
    assert_eq!(clean_identifier("[a]]b]"), "a]b");
    assert_eq!(clean_identifier("`a``b`"), "a`b");
    assert_eq!(clean_identifier("\"a\"\"b\""), "a\"b");
    assert_eq!(quote(Engine::Mssql, &clean_identifier("[a]]b]")), "[a]]b]");
}

use super::*;
use crate::model::Properties;

fn node(id: u32, op: &str, kind: NodeKind) -> PlanNode {
    PlanNode {
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
        subtree_cost: 1.0,
        self_cost: 1.0,
        act_time_ms: None,
        warnings: vec![],
        props: Properties::new(),
        children: vec![],
        runtime: None,
    }
}

fn plan(engine: Engine, root: PlanNode) -> Plan {
    let mut p = Plan {
        engine,
        kind: PlanKind::Estimated,
        statement: None,
        total_cost: 0.0,
        root,
        warnings: vec![],
        index_hints: vec![],
        props: Properties::new(),
        raw: None,
        insights: None,
    };
    fn sum(n: &mut PlanNode) -> f64 {
        let children: f64 = n.children.iter_mut().map(sum).sum();
        n.subtree_cost = n.self_cost + children;
        n.subtree_cost
    }
    p.total_cost = sum(&mut p.root);
    p
}

fn rules(p: &Plan) -> Vec<String> {
    insights(p).into_iter().map(|i| i.rule).collect()
}

#[test]
fn a_temporary_table_on_the_grouping_differs_from_sorting_the_groups() {
    let scan = || {
        let mut t = node(3, "Table scan", NodeKind::Scan);
        t.est_rows = 499_000.0;
        t.object = Some("shop.orders".into());
        t
    };
    let title = |p: &Plan| insights(p).into_iter().find(|i| i.rule == "temp-table-sort").map(|i| i.title);
    // MySQL: grouping reads an index in order; only the ORDER BY COUNT(*) sorts, in a temporary table.
    let mut group = node(2, "Group", NodeKind::Aggregate);
    group.est_rows = 499_000.0;
    group.children = vec![scan()];
    let mut sort = node(1, "Sort", NodeKind::Sort);
    sort.detail = Some("using filesort, using temporary table".into());
    sort.est_rows = 499_000.0;
    sort.children = vec![group.clone()];
    let t = title(&plan(Engine::Mysql, sort.clone())).expect("the sort of the groups");
    assert!(t.starts_with("Sorts the groups"), "{t}");

    // MySQL: the grouping itself needs a temporary table.
    group.detail = Some("using temporary table".into());
    sort.children = vec![group];
    sort.detail = Some("using filesort".into());
    let t = title(&plan(Engine::Mysql, sort)).expect("grouping in a temporary table");
    assert!(t.starts_with("Groups 499K rows"), "{t}");

    // MariaDB: Sort → Temporary table → table.
    let mut temp = node(2, "Temporary table", NodeKind::Other);
    temp.est_rows = 499_000.0;
    temp.children = vec![scan()];
    let mut sort = node(1, "Sort", NodeKind::Sort);
    sort.est_rows = 499_000.0;
    sort.children = vec![temp];
    let mut p = plan(Engine::Mysql, sort);
    p.props.insert("flavor".into(), "MariaDB".into());
    let found = insights(&p).into_iter().find(|i| i.rule == "temp-table-sort").expect("MariaDB's shape");
    assert!(found.title.contains("then sorts them"), "{}", found.title);
    assert!(found.detail.contains("MariaDB"), "{}", found.detail);
}

#[test]
fn converting_a_parameter_is_not_a_type_mismatch() {
    // SQL Server converts the parameter, not the column: the index works and estimates are fine.
    let mut scan = node(1, "Clustered Index Seek", NodeKind::Seek);
    scan.seek_predicate = Some("[db].[shop].[orders].[id]=CONVERT_IMPLICIT(bigint,[@1],0)".into());
    assert!(!rules(&plan(Engine::Mssql, scan.clone())).contains(&"implicit-conversion".to_string()));
    // `UPDATE shop.products SET price = price + 1 WHERE id = 1`, auto-parameterized.
    scan.seek_predicate = Some("[db].[shop].[products].[id]=CONVERT_IMPLICIT(int,[@2],0)".into());
    assert!(!rules(&plan(Engine::Mssql, scan.clone())).contains(&"implicit-conversion".to_string()));
    scan.seek_predicate =
        Some("[Expr1004]=CONVERT_IMPLICIT(int,[Expr1003],0) AND x=CONVERT_IMPLICIT(int,N'5',0)".into());
    assert!(!rules(&plan(Engine::Mssql, scan)).contains(&"implicit-conversion".to_string()));

    // The column is converted: that's the mismatch, named after the column even after a parameter convert.
    let mut scan = node(1, "Clustered Index Scan", NodeKind::Scan);
    scan.predicate = Some(
        "CONVERT_IMPLICIT(bigint,[@1],0)>0 AND CONVERT_IMPLICIT(nvarchar(200),[db].[shop].[customers].[email],0)=[@P1]"
            .into(),
    );
    let p = plan(Engine::Mssql, scan);
    let found = insights(&p).into_iter().find(|i| i.rule == "implicit-conversion").expect("the column is converted");
    assert!(found.title.contains("customers.email"), "{}", found.title);
    assert_eq!(convert_operands("CONVERT_IMPLICIT(nvarchar(36),[m].[c],0)=[@p1]"), vec!["[m].[c]"]);
}

#[test]
fn conversions_the_query_asks_for_are_not_a_type_mismatch() {
    // `SELECT CONVERT(varchar(36), o.id), CONVERT(varchar(30), o.created_at, 126) … WHERE o.archived = 0`: SQL Server
    // warns about each CONVERT in the select list, though nothing is compared with a value of another type.
    let mut scan = node(1, "Clustered Index Scan", NodeKind::Scan);
    scan.predicate = Some("[db].[shop].[orders].[archived] as [o].[archived]=(0)".into());
    let mut p = plan(Engine::Mssql, scan);
    p.warnings = vec![PlanWarning::ImplicitConversion];
    p.props.insert(
        "PlanAffectingConvert".into(),
        "Cardinality Estimate: CONVERT(varchar(36),[o].[id],0)\n\
         Cardinality Estimate: CONVERT(varchar(30),[o].[created_at],126)"
            .into(),
    );
    assert!(!rules(&p).contains(&"implicit-conversion".to_string()));
    p.props.insert("PlanAffectingConvert".into(), "Cardinality Estimate: CONVERT_IMPLICIT(varchar(64),[@P1],0)".into());
    assert!(!rules(&p).contains(&"implicit-conversion".to_string()));
}

#[test]
fn a_plan_captured_before_the_column_changed_type_has_no_mismatch() {
    // Query Store keeps a plan's first XML: after `jobs.name` became nvarchar(64), a recompile with the same shape
    // still shows the varchar-era conversion.
    let mut scan = node(1, "Clustered Index Scan", NodeKind::Scan);
    scan.predicate = Some("CONVERT_IMPLICIT(nvarchar(64),[db].[shop].[jobs].[name],0)=[@p1]".into());
    // A scan of 200K rows: enough work for the mismatch to matter.
    scan.rows_read = Some(200_000.0);
    let mut p = plan(Engine::Mssql, scan);
    p.props.insert(
        "PlanAffectingConvert".into(),
        "Seek Plan: CONVERT_IMPLICIT(nvarchar(64),[db].[shop].[jobs].[name],0)=[@p1]".into(),
    );
    let column = |data_type: &str| ColumnInfo {
        schema: "shop".into(),
        table: "jobs".into(),
        name: "name".into(),
        data_type: data_type.into(),
        nullable: false,
        default: None,
        generated: false,
        ordinal: 2,
    };
    let mut still_varchar = p.clone();
    mark_changed_columns(&mut still_varchar, &[column("varchar(64)")]);
    let found = insights(&still_varchar).into_iter().find(|i| i.rule == "implicit-conversion").expect("still varchar");
    assert!(found.severity >= Severity::Medium, "{:?}", found.severity);
    assert!(found.fix.contains("change jobs.name to nvarchar(64)"), "{}", found.fix);

    mark_changed_columns(&mut p, &[column("nvarchar(64)")]);
    assert_eq!(p.props.get(CHANGED_SINCE_CAPTURE).map(String::as_str), Some("jobs.name nvarchar(64)"));
    assert!(!rules(&p).contains(&"implicit-conversion".to_string()));
}

#[test]
fn only_an_implicitly_converted_column_is_a_mismatch() {
    assert!(converts_column("CONVERT_IMPLICIT(nvarchar(255),[db].[shop].[jobs].[run_key],0)=[@par4]"));
    assert!(converts_column("Seek Plan: CONVERT_IMPLICIT(nvarchar(36),[m].[CustomerId],0)=[@p1]"));
    assert!(converts_column("CONVERT_IMPLICIT(nvarchar(20),[status],0)"));
    // Written by the query, or of a value that isn't a column.
    assert!(!converts_column("Cardinality Estimate: CONVERT(varchar(36),[o].[id],0)"));
    assert!(!converts_column("CONVERT_IMPLICIT(varchar(36),[Union1006],0)"));
    assert!(!converts_column("Cardinality Estimate: CONVERT_IMPLICIT(varchar(8),[Expr1002],0)"));
    assert!(!converts_column("CONVERT_IMPLICIT(varchar(64),[@par2],0)"));
    assert!(!converts_column("x=CONVERT_IMPLICIT(int,N'5',0)"));
    assert!(!converts_column("no conversion here"));
}

#[test]
fn identifiers_skip_literals_types_and_parameters() {
    assert_eq!(identifiers("[db].[shop].[orders].[status]=[@P1]"), vec!["status"]);
    assert_eq!(
        identifiers("(date_trunc('day'::text, orders.created_at) = '2026-03-01'::timestamp)"),
        vec!["created_at"]
    );
    assert_eq!(identifiers("(`shop`.`o`.`promo_code` = 'SPRING3')"), vec!["promo_code"]);
    assert_eq!(filter_columns("(status = 'paid') AND (created_at > now())"), vec!["status", "created_at"]);
}

// The false flags: each of these filters gave "an index on (its first name)", which no B-tree index can seek on.
#[test]
fn only_conditions_an_index_can_seek_on_give_keys() {
    for filter in [
        "((status = 'x'::text) OR (region = 'eu'::text))",
        "(status <> 'done'::text)",
        "(shipped_at > created_at)",
        "((price * 1.1) > '100'::numeric)",
        "((created_at + '30 days'::interval) > now())",
        "((created_at)::date = $1)",
        "PROBE([Opt_Bitmap1005],[db].[shop].[orders].[customer_id],N'[IN ROW]')",
    ] {
        assert!(seekable_columns(filter).is_empty(), "{filter}: {:?}", seekable_columns(filter));
    }
    assert_eq!(
        seekable_columns("((status)::text = 'paid'::text) AND (created_at > (now() - '1 day'::interval))"),
        vec!["status", "created_at"]
    );
    assert_eq!(seekable_columns("((kind = 'a'::text) OR (kind = 'b'::text))"), vec!["kind"]);
    assert_eq!(seekable_columns("(amount BETWEEN 10 AND 20) AND (status = 'x')"), vec!["status", "amount"]);
    assert_eq!(seekable_columns("[db].[shop].[orders].[status]=[@P1]"), vec!["status"]);
}

// A CONVERT or CAST of a parameter keeps the column sargable: the type isn't a column.
#[test]
fn a_conversion_of_a_value_hides_no_column() {
    assert_eq!(wrapped_column("[orders].[created_at]>=CONVERT(datetime,[@from],0)", Engine::Mssql), None);
    assert_eq!(wrapped_column("(`orders`.`created_at` >= <cache>(cast(? as datetime)))", Engine::Mysql), None);
    assert!(wrapped_column("CONVERT(varchar(10),[orders].[created_at],120)=[@d]", Engine::Mssql).is_some());
}

#[test]
fn wrapped_columns_are_found_on_every_engine() {
    assert_eq!(
        wrapped_column("datepart(year,[db].[shop].[orders].[created_at])=(2026)", Engine::Mssql),
        Some(("DATEPART".into(), "created_at".into()))
    );
    assert_eq!(
        wrapped_column("(date_trunc('day'::text, orders.created_at) = '2026-03-01')", Engine::Postgres),
        Some(("DATE_TRUNC".into(), "created_at".into()))
    );
    assert_eq!(
        wrapped_column("(cast(`shop`.`orders`.`created_at` as date) = '2026-03-01')", Engine::Mysql),
        Some(("CAST".into(), "created_at".into()))
    );
    assert_eq!(
        wrapped_column("((o.total)::integer = 5)", Engine::Postgres),
        Some(("::integer".into(), "total".into()))
    );
    // varchar → text casts are how Postgres prints varchar comparisons; the index still works.
    assert_eq!(wrapped_column("((c.email)::text = 'x'::text)", Engine::Postgres), None);
    // A conversion of the parameter, not the column.
    assert_eq!(wrapped_column("[db].[shop].[orders].[status]=CONVERT(varchar(20),[@P1],0)", Engine::Mssql), None);
    // SQL Server seeks through a cast of a datetime column to date (a dynamic seek): nothing blocks the index.
    let to_date = "CONVERT(date,[db].[shop].[orders].[created_at],0)=CONVERT_IMPLICIT(date,[@1],0)";
    assert_eq!(wrapped_column(to_date, Engine::Mssql), None);
    assert!(wrapped_column("CONVERT(varchar(10),[db].[shop].[orders].[created_at],120)='x'", Engine::Mssql).is_some());
}

#[test]
fn lookups_need_volume_and_name_the_covering_index() {
    let mut seek = node(1, "Index Seek", NodeKind::Seek);
    seek.object = Some("shop.orders".into());
    seek.index = Some("ix_orders_status".into());
    seek.seek_predicate = Some("[db].[shop].[orders].[status] = 'pending'".into());
    let mut lookup = node(2, "Key Lookup", NodeKind::Lookup);
    lookup.object = Some("shop.orders".into());
    lookup.est_executions = 20_000.0;
    lookup.self_cost = 30.0;
    lookup.props.insert("OutputList".into(), "[db].[shop].[orders].[total], [db].[shop].[orders].[channel]".into());
    let mut join = node(0, "Nested Loops", NodeKind::Join);
    join.children = vec![seek, lookup];
    let p = plan(Engine::Mssql, join);
    let found = insights(&p);
    let i = found.iter().find(|i| i.rule == "key-lookups").expect("key lookups");
    assert_eq!(i.nodes[0], 2);
    assert!(i.title.contains("20K rows"), "{}", i.title);
    // Rebuilding the index from the seek's columns would drop its descending keys, included columns, uniqueness,
    // filter and compression: the fix names the index and the columns, and leaves the statement to the catalog.
    assert_eq!(i.fix_sql, None);
    assert!(i.fix.contains("total and channel to ix_orders_status"), "{}", i.fix);
    assert!(!rules(&p).contains(&"nested-loops".to_string()), "lookups aren't double-reported as loops");

    // A thousand lookups at a sliver of the plan stay Low: no floor lifts them.
    let mut small = p.clone();
    small.root.children[1].est_executions = 1_000.0;
    small.root.children[1].self_cost = 0.5;
    small.root.children[0].self_cost = 100.0;
    let small = plan(Engine::Mssql, small.root);
    let i = insights(&small).into_iter().find(|i| i.rule == "key-lookups").expect("still said");
    assert_eq!(i.severity, Severity::Low);
}

#[test]
fn spills_and_misestimates_explain_each_other() {
    let mut scan = node(2, "Seq Scan", NodeKind::Scan);
    scan.object = Some("public.customers".into());
    scan.predicate = Some("((region = 'eu') AND (segment = 'retail'))".into());
    scan.est_rows = 248.0;
    scan.act_rows = Some(49_997.0);
    scan.warnings = vec![PlanWarning::Misestimate];
    let mut sort = node(1, "Sort", NodeKind::Sort);
    sort.act_rows = Some(49_997.0);
    sort.warnings = vec![PlanWarning::Spill];
    sort.props.insert("Sort Space Type".into(), "Disk".into());
    sort.props.insert("Sort Space Used".into(), "5120".into());
    sort.children = vec![scan];
    let mut p = plan(Engine::Postgres, sort);
    p.kind = PlanKind::Actual;
    let found = insights(&p);
    let spill = found.iter().find(|i| i.rule == "spill").unwrap();
    assert!(spill.title.contains("5.0 MB"), "{}", spill.title);
    assert!(spill.detail.contains("underestimated"));
    let mis = found.iter().find(|i| i.rule == "misestimate").unwrap();
    assert!(mis.title.starts_with("Expected 248 rows from customers, got 50K"), "{}", mis.title);
    assert!(
        mis.fix_sql.as_deref().unwrap().contains("CREATE STATISTICS"),
        "two filtered columns → extended statistics"
    );
}

#[test]
fn coverage_is_said_plainly_per_engine() {
    let mut p = plan(Engine::Mysql, node(0, "Table scan", NodeKind::Scan));
    assert!(coverage_note(&p).unwrap().contains("EXPLAIN ANALYZE adds actual rows"));
    p.props.insert("server_version".into(), "5.7.44-log".into());
    assert!(coverage_note(&p).unwrap().contains("can't measure a run"), "5.7 has no EXPLAIN ANALYZE");
    p.props.insert("server_version".into(), "8.0.36".into());
    assert!(coverage_note(&p).unwrap().contains("EXPLAIN ANALYZE adds actual rows"));
    let mut actual = plan(Engine::Mssql, node(0, "Sort", NodeKind::Sort));
    actual.kind = PlanKind::Actual;
    assert!(coverage_note(&actual).is_none());
}

#[test]
fn severity_follows_impact_within_rule_limits() {
    let scan = |rows: f64, cost: f64| {
        let mut n = node(0, "Seq Scan", NodeKind::Scan);
        n.est_rows = rows;
        n.self_cost = cost;
        plan(Engine::Postgres, n)
    };
    let material = scan(50_000.0, 5_000.0);
    let cx = Cx::new(&material);
    let mut d = Draft::new("legacy-ce", InsightArea::Server, 0.9, vec![0]);
    d.cap = Severity::Low;
    assert_eq!(d.finish(&cx).severity, Severity::Low);
    let floored = || {
        let mut d = Draft::new("spill", InsightArea::Memory, 0.01, vec![0]);
        d.floor = Severity::Medium;
        d
    };
    assert_eq!(floored().finish(&cx).severity, Severity::Medium);
    assert_eq!(Draft::new("scan-filter", InsightArea::Index, 0.5, vec![0]).finish(&cx).severity, Severity::High);

    // The materiality gate comes before any floor: a cheap plan, or operators handling few rows, stay Low.
    let cheap = scan(50_000.0, 40.0);
    assert_eq!(floored().finish(&Cx::new(&cheap)).severity, Severity::Low);
    let few = scan(800.0, 5_000.0);
    assert_eq!(floored().finish(&Cx::new(&few)).severity, Severity::Low);
    // A measured run decides over the estimate: 3 ms is cheap whatever the plan expected.
    let mut fast = scan(50_000.0, 5_000.0);
    fast.props.insert("ElapsedMs".into(), "3".into());
    assert_eq!(floored().finish(&Cx::new(&fast)).severity, Severity::Low);
    // A problem measured in its own time is material by that time.
    let mut d = floored();
    d.cost_ms = Some(400.0);
    assert_eq!(d.finish(&Cx::new(&few)).severity, Severity::Medium);
    // About the whole plan: the busiest operator below counts.
    let mut root = node(0, "Aggregate", NodeKind::Aggregate);
    let mut child = node(1, "Seq Scan", NodeKind::Scan);
    child.est_rows = 80_000.0;
    child.self_cost = 5_000.0;
    root.children = vec![child];
    let whole = plan(Engine::Postgres, root);
    let mut d = floored();
    d.nodes = vec![0];
    assert_eq!(d.finish(&Cx::new(&whole)).severity, Severity::Medium, "the scan is the aggregate's input");
}

/// An actual plan of `root`, timed at `ms`, with costs summed bottom-up.
fn actual(engine: Engine, root: PlanNode, ms: f64) -> Plan {
    let mut p = plan(engine, root);
    p.kind = PlanKind::Actual;
    p.props.insert("ElapsedMs".into(), ms.to_string());
    p
}

fn measured(id: u32, op: &str, kind: NodeKind, est: f64, act: f64, runs: f64) -> PlanNode {
    let mut n = node(id, op, kind);
    n.est_rows = est;
    n.act_rows = Some(act);
    n.act_executions = Some(runs);
    n.est_executions = runs.max(1.0);
    n
}

fn found(p: &Plan) -> Vec<u32> {
    p.root.misestimates().into_iter().map(|(id, _)| id).collect()
}

#[test]
fn misestimates_compare_per_run_and_ignore_operators_stopped_early() {
    // Under a Limit, PostgreSQL's estimate is for a full run: 20 rows of an expected 5M is the Limit, not a
    // misestimate.
    let mut limit = measured(0, "Limit", NodeKind::Other, 20.0, 20.0, 1.0);
    limit.children = vec![measured(1, "Seq Scan", NodeKind::Scan, 5_000_000.0, 20.0, 1.0)];
    assert!(found(&plan(Engine::Postgres, limit.clone())).is_empty());
    // A sort under it hands over only what the Limit takes, but reads all its input: that count is real.
    let mut sort = measured(1, "Sort", NodeKind::Sort, 5_000_000.0, 20.0, 1.0);
    sort.children = vec![measured(2, "Seq Scan", NodeKind::Scan, 5_000_000.0, 3_000.0, 1.0)];
    limit.children = vec![sort];
    assert_eq!(found(&plan(Engine::Postgres, limit)), vec![2]);
    // More rows than expected is evidence anywhere.
    let mut top = measured(0, "Top", NodeKind::Other, 20.0, 20.0, 1.0);
    top.children = vec![measured(1, "Index Seek", NodeKind::Seek, 10.0, 5_000.0, 1.0)];
    assert_eq!(found(&plan(Engine::Mssql, top)), vec![1]);

    // The inner side of a semi or anti join stops at its first match; merge join inputs stop when the other ends.
    let mut semi = measured(0, "Nested Loop", NodeKind::Join, 900.0, 900.0, 1.0);
    semi.detail = Some("Anti join".into());
    semi.children = vec![
        measured(1, "Seq Scan", NodeKind::Scan, 1_000.0, 1_000.0, 1.0),
        measured(2, "Index Scan", NodeKind::Seek, 400.0, 1_000.0, 1_000.0),
    ];
    assert!(found(&plan(Engine::Postgres, semi)).is_empty());
    let mut merge = measured(0, "Merge Join", NodeKind::Join, 50.0, 50.0, 1.0);
    merge.children = vec![
        measured(1, "Index Scan", NodeKind::Seek, 2_000_000.0, 60.0, 1.0),
        measured(2, "Index Scan", NodeKind::Seek, 50.0, 50.0, 1.0),
    ];
    assert!(found(&plan(Engine::Postgres, merge)).is_empty());

    // An operator that never ran has nothing to compare.
    let never = measured(0, "Index Scan", NodeKind::Seek, 5_000.0, 0.0, 0.0);
    assert!(found(&plan(Engine::Postgres, never)).is_empty());

    // Nested loop: the outer side was misjudged (100 expected, 10K came); the inner seek returned the one row per
    // run it expected, so only the outer side is a misestimate.
    let mut nl = measured(0, "Nested Loops", NodeKind::Join, 100.0, 10_000.0, 1.0);
    let mut inner = measured(2, "Clustered Index Seek", NodeKind::Seek, 1.0, 10_000.0, 10_000.0);
    inner.est_executions = 100.0;
    nl.children = vec![measured(1, "Index Seek", NodeKind::Seek, 100.0, 10_000.0, 1.0), inner];
    let p = plan(Engine::Mssql, nl);
    assert_eq!(found(&p), vec![0, 1]);
    let mis = insights(&p).into_iter().filter(|i| i.rule == "misestimate").collect::<Vec<_>>();
    assert_eq!(mis.len(), 1, "{mis:?}");
    assert_eq!(mis[0].nodes, vec![1]);

    // A parallel scan runs once on each of 8 threads, which SQL Server's estimate counts as one execution.
    let mut parallel = measured(0, "Clustered Index Scan", NodeKind::Scan, 1_000_000.0, 1_000_000.0, 8.0);
    parallel.est_executions = 1.0;
    parallel.props.insert(crate::plan::PARALLEL_THREADS.into(), "8".into());
    assert_eq!(parallel.runs(), 1.0);
    assert!(found(&plan(Engine::Mssql, parallel)).is_empty());

    // A hash join's bitmap pushed into the probe-side scan isn't in the scan's estimate.
    let mut probe = measured(0, "Index Scan", NodeKind::Scan, 1_000_000.0, 30_000.0, 1.0);
    probe.predicate = Some("PROBE([Bitmap1005],[db].[shop].[orders].[id],N'[IN ROW]')".into());
    assert!(found(&plan(Engine::Mssql, probe)).is_empty());
}

#[test]
fn misestimate_advice_fits_the_table_and_the_server() {
    let scan = |object: &str| {
        let mut s = measured(1, "Table Scan", NodeKind::Scan, 10.0, 50_000.0, 1.0);
        s.object = Some(object.into());
        s.predicate = Some("[t].[a]=(1)".into());
        let mut join = measured(0, "Hash Match", NodeKind::Join, 10.0, 50_000.0, 1.0);
        join.children = vec![s];
        actual(Engine::Mssql, join, 400.0)
    };
    let fix = |p: &Plan| insights(p).into_iter().find(|i| i.rule == "misestimate").expect("misestimate");
    // A table variable estimated at one row is the table-variable rule's; never UPDATE STATISTICS on it.
    let mut one = scan("@t");
    one.root.children[0].est_rows = 1.0;
    assert!(!rules(&one).contains(&"misestimate".to_string()));
    assert!(rules(&one).contains(&"table-variable".to_string()));
    let tv = fix(&scan("@t"));
    assert_eq!(tv.fix_sql, None);
    assert!(tv.fix.contains("table variable"), "{}", tv.fix);
    assert_eq!(fix(&scan("#work")).fix_sql, None, "a temporary table's statistics belong to its session");
    assert!(fix(&scan("dbo.orders")).fix_sql.unwrap().starts_with("UPDATE STATISTICS [dbo].[orders]"));
    // No spill above the operator, so no "ran out of memory as a result".
    assert!(!fix(&scan("dbo.orders")).detail.contains("out of memory"));

    let mysql = |flavor: Option<&str>, version: Option<&str>| {
        let mut s = measured(1, "Table scan", NodeKind::Scan, 10.0, 50_000.0, 1.0);
        s.object = Some("shop.orders".into());
        s.predicate = Some("(shop.orders.status = 'x')".into());
        let mut join = measured(0, "Nested loop inner join", NodeKind::Join, 10.0, 50_000.0, 1.0);
        join.children = vec![s];
        let mut p = actual(Engine::Mysql, join, 400.0);
        if let Some(f) = flavor {
            p.props.insert("flavor".into(), f.into());
        }
        if let Some(v) = version {
            p.props.insert("server_version".into(), v.into());
        }
        fix(&p).fix_sql.unwrap()
    };
    assert!(mysql(None, Some("8.0.36")).contains("UPDATE HISTOGRAM ON `status`"));
    assert_eq!(mysql(None, Some("5.7.44")), "ANALYZE TABLE `shop`.`orders`;", "no histograms before 8.0");
    assert!(mysql(Some("MariaDB"), None).contains("PERSISTENT FOR COLUMNS (`status`) INDEXES ()"));

    // A misestimate nothing above depends on changed nothing in this plan.
    let mut root = measured(0, "Filter", NodeKind::Other, 50_000.0, 0.0, 1.0);
    let mut table = measured(1, "Table scan", NodeKind::Scan, 500_000.0, 500_000.0, 1.0);
    table.object = Some("shop.orders".into());
    root.predicate = Some("(orders.promo_code = 'X')".into());
    root.children = vec![table];
    let lone = fix(&actual(Engine::Mysql, root, 80.0));
    assert_eq!(lone.severity, Severity::Low);
    assert!(lone.title.contains("from orders"), "{}", lone.title);
}

#[test]
fn index_advice_from_a_scan_needs_a_scan_that_an_index_would_shrink() {
    let scan = |op: &str, kind: NodeKind, read: f64, kept: f64, filter: &str| {
        let mut n = node(1, op, kind);
        n.object = Some("dbo.orders".into());
        n.predicate = Some(filter.into());
        n.est_rows = kept;
        n.rows_read = Some(read);
        n.self_cost = 50.0;
        let mut p = plan(Engine::Mssql, n);
        p.root.warnings.push(PlanWarning::ResidualPredicate);
        p
    };
    let isnull = "isnull([db].[dbo].[orders].[deleted],(0))=(0)";
    // A seek reading 3 rows: the function blocks nothing worth having.
    assert!(rules(&scan("Index Seek", NodeKind::Seek, 3.0, 2.0, isnull)).is_empty());
    // A scan that keeps most of what it reads: an index wouldn't pay.
    assert!(rules(&scan("Clustered Index Scan", NodeKind::Scan, 2_000_000.0, 1_900_000.0, isnull)).is_empty());
    // A selective scan: now the function is what keeps the index out.
    let selective = scan("Clustered Index Scan", NodeKind::Scan, 2_000_000.0, 3_000.0, isnull);
    assert_eq!(rules(&selective), vec!["non-sargable"]);
    // SQL Server seeks through CONVERT(date, …): the scan needs an index, not a rewrite.
    let to_date = "CONVERT(date,[db].[dbo].[orders].[created_at],0)='2026-03-01'";
    assert_eq!(
        rules(&scan("Clustered Index Scan", NodeKind::Scan, 2_000_000.0, 3_000.0, to_date)),
        vec!["scan-filter"]
    );
    // A leading wildcard on a seek, or on a scan that keeps everything, isn't the problem.
    let like = "[db].[dbo].[orders].[email] like N'%@example.com'";
    assert!(rules(&scan("Index Seek", NodeKind::Seek, 2_000_000.0, 3_000.0, like)).is_empty());
    assert!(rules(&scan("Clustered Index Scan", NodeKind::Scan, 200_000.0, 200_000.0, like)).is_empty());
    let wildcard = insights(&scan("Clustered Index Scan", NodeKind::Scan, 2_000_000.0, 3_000.0, like));
    assert_eq!(wildcard.iter().map(|i| i.rule.as_str()).collect::<Vec<_>>(), vec!["leading-wildcard"]);
    assert!(
        wildcard[0].detail.contains("2,000,000 rows") || wildcard[0].detail.contains("2.0M"),
        "{}",
        wildcard[0].detail
    );
    // A function and a wildcard on one scan: one insight for it.
    let both = format!("upper([db].[dbo].[orders].[email]) like N'%X%' AND {isnull}");
    assert_eq!(
        rules(&scan("Clustered Index Scan", NodeKind::Scan, 2_000_000.0, 3_000.0, &both)),
        vec!["leading-wildcard"]
    );
    // A columnstore is read by segments, not seeks.
    let mut columnstore =
        scan("Clustered Index Scan", NodeKind::Scan, 2_000_000.0, 3_000.0, "[db].[dbo].[orders].[status]=(3)");
    assert_eq!(rules(&columnstore), vec!["scan-filter"]);
    columnstore.root.props.insert("Storage".into(), "ColumnStore".into());
    assert!(rules(&columnstore).is_empty());
    // Said as the plan shows it, not inferred: no claim about which indexes exist.
    let filtered =
        scan("Clustered Index Scan", NodeKind::Scan, 2_000_000.0, 3_000.0, "[db].[dbo].[orders].[status]=(3)");
    let i = insights(&filtered).into_iter().next().unwrap();
    assert!(!i.detail.contains("no index starts with"), "{}", i.detail);
}

#[test]
fn sqlite_says_when_it_builds_an_index_itself() {
    let mut auto = node(7, "Automatic Index", NodeKind::Other);
    auto.object = Some("visits".into());
    auto.props.insert("Automatic Index".into(), "visits(event_id)".into());
    let p = plan(Engine::Sqlite, auto);
    let found: Vec<PlanInsight> = insights(&p).into_iter().filter(|i| i.rule == "automatic-index").collect();
    assert_eq!(found.len(), 1, "{:?}", rules(&p));
    assert!(
        found[0].fix_sql.as_deref().unwrap_or("").starts_with("CREATE INDEX IF NOT EXISTS \"ix_visits_event_id\""),
        "{:?}",
        found[0].fix_sql
    );
    // No rows, no cost: nothing says how much it costs, so it stays Low.
    assert_eq!(found[0].severity, Severity::Low);
}

#[test]
fn mysql_guesses_are_not_evidence() {
    assert!(mysql_guessed_filtered(10.0, 1) && mysql_guessed_filtered(11.11, 1) && mysql_guessed_filtered(33.33, 1));
    assert!(mysql_guessed_filtered(1.0, 2) && mysql_guessed_filtered(3.33, 2) && mysql_guessed_filtered(1.23, 2));
    assert!(!mysql_guessed_filtered(1.0, 1), "one condition can't be two guesses multiplied");
    assert!(!mysql_guessed_filtered(0.5, 2) && !mysql_guessed_filtered(4.2, 1));
    let scan = |filtered: &str, possible: Option<&str>| {
        let mut n = node(1, "Table scan", NodeKind::Scan);
        n.object = Some("shop.orders".into());
        n.predicate = Some("(shop.o.promo_code = 'SPRING3')".into());
        n.rows_read = Some(498_573.0);
        n.est_rows = 498_573.0 * filtered.parse::<f64>().unwrap() / 100.0;
        n.self_cost = 50_000.0;
        n.props.insert("filtered".into(), filtered.into());
        if let Some(k) = possible {
            n.props.insert("possible_keys".into(), k.into());
        }
        n
    };
    // 10.00 is MySQL's guess for an equality it has no statistics on: nothing to advise from.
    assert!(rules(&plan(Engine::Mysql, scan("10.00", None))).is_empty());
    let measured_share = rules(&plan(Engine::Mysql, scan("0.50", None)));
    assert_eq!(measured_share, vec!["scan-filter"]);
    // A possible key on the filtered column: MySQL ignored an index; that's index-ignored's, not scan-filter's.
    assert_eq!(rules(&plan(Engine::Mysql, scan("0.50", Some("ix_orders_promo_code")))), vec!["index-ignored"]);
    // In a join, a possible key on the join column can't serve the filter.
    let mut join = node(0, "Nested loop", NodeKind::Join);
    let mut other = node(2, "Unique index lookup", NodeKind::Seek);
    other.object = Some("shop.customers".into());
    join.children = vec![scan("0.50", Some("fk_orders_customer")), other];
    let joined = rules(&plan(Engine::Mysql, join.clone()));
    assert!(!joined.contains(&"index-ignored".to_string()), "{joined:?}");
    assert!(joined.contains(&"scan-filter".to_string()), "{joined:?}");
    join.children[0] = scan("0.50", Some("fk_orders_customer, ix_orders_promo_code"));
    assert!(rules(&plan(Engine::Mysql, join.clone())).contains(&"index-ignored".to_string()));
    // With the catalog, a key counts by the column it leads with, whatever its name.
    let index = |name: &str, columns: &[&str]| {
        serde_json::from_value::<IndexStat>(serde_json::json!({
            "schema": "shop", "table": "orders", "name": name, "kind": "BTREE",
            "columns": columns, "include": [], "filter": null, "is_unique": false, "is_primary_key": false,
            "is_clustered": false, "is_disabled": false, "size_bytes": null, "reads": null, "writes": null,
            "seeks": null, "scans": null, "lookups": null, "last_read": null, "definition": null
        }))
        .unwrap()
    };
    let marked = |keys: &str, catalog: &[IndexStat]| {
        let mut j = join.clone();
        j.children[0] = scan("0.50", Some(keys));
        let mut p = plan(Engine::Mysql, j);
        mark_possible_keys(&mut p, catalog);
        rules(&p)
    };
    let promo = [index("idx_a", &["promo_code", "created_at"]), index("fk_orders_customer", &["customer_id"])];
    assert!(marked("fk_orders_customer, idx_a", &promo).contains(&"index-ignored".to_string()));
    // Named for promo_code, but it leads with customer_id: it can't serve a filter on promo_code.
    let misnamed = [index("ix_orders_promo_code", &["customer_id", "promo_code"])];
    let unusable = marked("ix_orders_promo_code", &misnamed);
    assert!(!unusable.contains(&"index-ignored".to_string()) && unusable.contains(&"scan-filter".to_string()));
    // Above a tenth kept, an index wouldn't pay either way.
    assert!(rules(&plan(Engine::Mysql, scan("15.00", Some("ix_orders_promo_code")))).is_empty());
}

// MySQL's rows produced count the join up to a table: on a hash-joined table they dwarf the rows it reads, and say
// nothing of its filter. Its own `filtered` is the share, wherever the share is asked for.
#[test]
fn the_share_kept_is_mysqls_own_estimate_unless_it_is_a_guess() {
    let mut items = node(1, "Table scan", NodeKind::Scan);
    items.predicate = Some("(shop.i.status = 'x')".into());
    items.rows_read = Some(800_000.0);
    items.est_rows = 40_000_000.0;
    items.props.insert("filtered".into(), "0.50".into());
    assert_eq!(items.kept_share(), Some(0.005));
    items.props.insert("filtered".into(), "10.00".into());
    assert_eq!(items.kept_share(), None, "MySQL's guess for an equality");
    // What the run measured outweighs any estimate.
    items.act_rows = Some(8_000.0);
    assert_eq!(items.kept_share(), Some(0.01));
}

#[test]
fn a_table_joined_through_a_buffer_gets_no_scan_filter_advice() {
    // A hash-joined table's condition holds the join's: the 0.5% MySQL expects it to keep is mostly the join's doing.
    let mut orders = node(1, "Table scan", NodeKind::Scan);
    orders.object = Some("shop.orders".into());
    orders.est_rows = 10.0;
    let mut items = node(2, "Table scan", NodeKind::Scan);
    items.object = Some("shop.items".into());
    items.predicate = Some("((shop.i.status = 'x') and (shop.i.order_id = shop.o.id))".into());
    items.props.insert("using_join_buffer".into(), "hash join".into());
    items.props.insert("filtered".into(), "0.50".into());
    items.rows_read = Some(800_000.0);
    items.est_rows = 40_000.0;
    items.self_cost = 80_000.0;
    let mut join = node(0, "Nested loop", NodeKind::Join);
    join.children = vec![orders, items];
    let found = rules(&plan(Engine::Mysql, join));
    assert!(found.contains(&"join-buffer".to_string()) && !found.contains(&"scan-filter".to_string()), "{found:?}");
}

#[test]
fn join_buffers_advise_an_index_only_over_a_large_read() {
    let buffered = |how: &str, kind: NodeKind, read: f64| {
        let mut orders = node(1, "Table scan", NodeKind::Scan);
        orders.object = Some("shop.orders".into());
        orders.est_rows = 10.0;
        let mut items = node(2, "Table scan", kind);
        items.object = Some("shop.items".into());
        items.predicate = Some("(shop.i.order_id = shop.o.id)".into());
        items.props.insert("using_join_buffer".into(), how.into());
        items.rows_read = Some(read);
        let mut join = node(0, "Nested loop", NodeKind::Join);
        join.children = vec![orders, items];
        rules(&plan(Engine::Mysql, join)).contains(&"join-buffer".to_string())
    };
    assert!(buffered("hash join", NodeKind::Scan, 800_000.0));
    // One read of 2,000 rows costs too little for an index to pay.
    assert!(!buffered("hash join", NodeKind::Scan, 2_000.0));
    // Batched Key Access looks each buffered row up through an index.
    assert!(!buffered("Batched Key Access", NodeKind::Seek, 800_000.0));
}

#[test]
fn nested_loops_speak_only_when_the_loop_was_not_the_right_choice() {
    let nl = |outer: PlanNode, inner: PlanNode| {
        let mut n = measured(0, "Nested Loop", NodeKind::Join, 400_000.0, 400_000.0, 1.0);
        n.children = vec![outer, inner];
        actual(Engine::Postgres, n, 300.0)
    };
    let outer = |est: f64| {
        let mut o = measured(1, "Seq Scan", NodeKind::Scan, est, 400_000.0, 1.0);
        o.object = Some("public.orders".into());
        o.self_cost = 100.0;
        o
    };
    let seek = || {
        let mut s = measured(2, "Index Scan", NodeKind::Seek, 1.0, 400_000.0, 400_000.0);
        s.object = Some("public.customers".into());
        s.seek_predicate = Some("(c.id = o.customer_id)".into());
        s.table_rows = Some(200_000.0);
        s.self_cost = 100.0;
        s
    };
    // An accurate outer estimate and a one-row seek per run: the optimizer weighed a hash join and chose this.
    assert!(!rules(&nl(outer(390_000.0), seek())).contains(&"nested-loops".to_string()));
    // It expected 400 rows and got 400K: the loop was chosen on wrong numbers.
    let wrong = insights(&nl(outer(400.0), seek()));
    let loops = wrong.iter().find(|i| i.rule == "nested-loops").expect("misjudged outer side");
    assert!(loops.detail.contains("expected 400 rows"), "{}", loops.detail);
    // Each run rereading most of a table is a problem whatever the estimate.
    let mut rescan = measured(2, "Seq Scan", NodeKind::Scan, 1.0, 400_000.0, 20_000.0);
    rescan.object = Some("public.regions".into());
    rescan.predicate = Some("(r.code = o.region_code)".into());
    rescan.rows_read = Some(20_000.0 * 900.0);
    rescan.table_rows = Some(1_000.0);
    rescan.self_cost = 100.0;
    let mut o = outer(20_000.0);
    o.act_rows = Some(20_000.0);
    let reread = insights(&nl(o, rescan));
    let r = reread.iter().find(|i| i.rule == "nested-loops").expect("rereads");
    assert!(r.fix.contains("Index regions"), "{}", r.fix);

    // Memoize answers repeats from its cache: only the 120 misses ran the index scan.
    let mut memo = measured(2, "Memoize", NodeKind::Spool, 1.0, 400_000.0, 400_000.0);
    let mut below = seek();
    below.act_executions = Some(120.0);
    below.est_executions = 120.0;
    below.act_rows = Some(120.0);
    memo.children = vec![below];
    assert!(!rules(&nl(outer(400.0), memo)).contains(&"nested-loops".to_string()));

    // SQL Server's lazy Table Spool reads its input once and replays it: never a reread of the table.
    let spool_plan = |predicate: Option<&str>| {
        let mut n = node(0, "Nested Loops", NodeKind::Join);
        n.predicate = predicate.map(str::to_string);
        n.self_cost = 6_000.0;
        let mut o = node(1, "Index Scan", NodeKind::Scan);
        o.object = Some("shop.order_items".into());
        o.est_rows = 1_500_000.0;
        let mut spool = node(2, "Table Spool", NodeKind::Spool);
        spool.detail = Some("Lazy Spool".into());
        spool.est_rows = 999.0;
        spool.est_executions = 1_500_001.0;
        let mut agg = node(3, "Hash Match", NodeKind::Aggregate);
        agg.est_rows = 999.0;
        let mut scan = node(4, "Index Scan", NodeKind::Scan);
        scan.object = Some("shop.order_items".into());
        scan.est_rows = 1_500_000.0;
        agg.children = vec![scan];
        spool.children = vec![agg];
        n.children = vec![o, spool];
        plan(Engine::Mssql, n)
    };
    let pairs = insights(&spool_plan(Some("[Expr1003]=[Expr1004]")));
    let p = pairs.iter().find(|i| i.rule == "nested-loops").expect("every pair tested");
    assert!(!p.title.contains("Repeats its read of order_items"), "{}", p.title);
    assert!(p.title.contains("1.5B pairs"), "{}", p.title);
    assert!(!rules(&spool_plan(None)).contains(&"nested-loops".to_string()), "a seekable inner side");

    // An eager index spool is built once per run: eager-spool's, not a repeated read.
    let mut eager = node(0, "Nested Loops", NodeKind::Join);
    let mut o = node(1, "Clustered Index Scan", NodeKind::Scan);
    o.est_rows = 50_000.0;
    let mut spool = node(2, "Index Spool", NodeKind::Spool);
    spool.detail = Some("Eager Spool".into());
    spool.est_executions = 50_000.0;
    let mut feed = node(3, "Clustered Index Scan", NodeKind::Scan);
    feed.object = Some("shop.ticket_events".into());
    feed.est_rows = 2_000_000.0;
    spool.children = vec![feed];
    eager.children = vec![o, spool];
    let eager = rules(&plan(Engine::Mssql, eager));
    assert!(eager.contains(&"eager-spool".to_string()) && !eager.contains(&"nested-loops".to_string()), "{eager:?}");

    // MySQL joins through an index whenever one fits: the advice is its estimate and join order, not a hash join.
    let mut mysql = measured(0, "Nested loop inner join", NodeKind::Join, 400_000.0, 400_000.0, 1.0);
    let mut c = measured(1, "Table scan", NodeKind::Scan, 400.0, 400_000.0, 1.0);
    c.object = Some("shop.orders".into());
    let mut lookup = measured(2, "Single-row index lookup", NodeKind::Seek, 1.0, 400_000.0, 400_000.0);
    lookup.object = Some("shop.customers".into());
    mysql.children = vec![c, lookup];
    let m = insights(&actual(Engine::Mysql, mysql, 300.0));
    let m = m.iter().find(|i| i.rule == "nested-loops").expect("misjudged outer side");
    assert!(!m.detail.contains("hash join") && !m.fix.contains("merge"), "{} / {}", m.detail, m.fix);
    assert!(m.fix.contains("ANALYZE TABLE"), "{}", m.fix);
}

#[test]
fn a_mariadb_join_buffer_is_no_repeated_read() {
    // MariaDB reads a table behind its join buffer (`block-nl-join`) once per buffer fill, not once for each of the
    // 50K orders.
    let mut orders = node(1, "Table scan", NodeKind::Scan);
    orders.object = Some("shop.orders".into());
    orders.est_rows = 50_000.0;
    let mut products = node(2, "Table scan", NodeKind::Scan);
    products.object = Some("shop.products".into());
    products.predicate = Some("(p.category = o.status)".into());
    products.rows_read = Some(20_000.0);
    products.est_rows = 20.0;
    products.props.insert("join_buffer_join_type".into(), "BNL".into());
    let mut join = node(0, "Nested loop", NodeKind::Join);
    join.children = vec![orders, products];
    let mut p = plan(Engine::Mysql, join);
    p.props.insert("flavor".into(), "MariaDB".into());
    assert!(!rules(&p).contains(&"nested-loops".to_string()), "{:?}", rules(&p));
}

#[test]
fn spills_say_what_spilled_and_how_much() {
    // An exchange spill resolves a deadlock between parallel threads: no memory-grant advice.
    let mut exchange = node(0, "Parallelism", NodeKind::Parallelism);
    exchange.props.insert(EXCHANGE_SPILL.into(), "true".into());
    exchange.props.insert("TempdbWritesPages".into(), "800".into());
    let p = actual(Engine::Mssql, exchange, 500.0);
    let i = insights(&p).into_iter().find(|i| i.rule == "spill").expect("exchange spill");
    assert_eq!(i.area, InsightArea::Parallelism);
    assert!(!i.fix.contains("grant") && !i.detail.contains("granted"), "{} / {}", i.detail, i.fix);

    // MariaDB's sort merge passes get MariaDB's words.
    let mut sort = node(0, "Sort", NodeKind::Sort);
    sort.warnings = vec![PlanWarning::Spill];
    sort.props.insert("r_sort_passes".into(), "3".into());
    let mut p = actual(Engine::Mysql, sort, 500.0);
    p.props.insert("flavor".into(), "MariaDB".into());
    let i = insights(&p).into_iter().find(|i| i.rule == "spill").expect("sort passes");
    assert!(i.title.contains("3 merge passes") && i.fix.contains("sort_buffer_size"), "{}", i.title);
    assert!(!i.title.contains("tempdb") && !i.fix.contains("memory grant"), "{}", i.fix);

    // PostgreSQL's hash aggregate: its own disk use, not the temp blocks its inputs wrote.
    let mut agg = node(0, "Aggregate", NodeKind::Aggregate);
    agg.warnings = vec![PlanWarning::Spill];
    agg.props.insert("HashAgg Batches".into(), "5".into());
    agg.props.insert("Disk Usage".into(), "7600".into());
    agg.props.insert("Temp Written Blocks".into(), "18432".into());
    let i = insights(&actual(Engine::Postgres, agg.clone(), 500.0)).into_iter().find(|i| i.rule == "spill").unwrap();
    assert!(i.title.contains("7.4 MB"), "{}", i.title);
    // Without BUFFERS or Disk Usage, no made-up size.
    agg.props.remove("Disk Usage");
    agg.props.remove("Temp Written Blocks");
    let i = insights(&actual(Engine::Postgres, agg, 500.0)).into_iter().find(|i| i.rule == "spill").unwrap();
    assert!(i.title.contains("5 batches") && !i.title.contains(" 0 B"), "{}", i.title);

    // A 64 KB spill is small next to the work around it.
    let mut small = node(0, "Sort", NodeKind::Sort);
    small.warnings = vec![PlanWarning::Spill];
    small.act_rows = Some(80_000.0);
    small.props.insert("TempdbWritesPages".into(), "8".into());
    let i = insights(&actual(Engine::Mssql, small, 500.0)).into_iter().find(|i| i.rule == "spill").unwrap();
    assert_eq!(i.severity, Severity::Low);
}

#[test]
fn a_spill_is_put_on_an_estimate_only_past_the_misestimate_bars() {
    // 40 rows where 2 were expected is twenty times off, and too few rows to have sized the sort's memory.
    let mut scan = measured(2, "Seq Scan", NodeKind::Scan, 2.0, 40.0, 1.0);
    scan.object = Some("public.customers".into());
    let mut sort = measured(1, "Sort", NodeKind::Sort, 2.0, 40.0, 1.0);
    sort.warnings = vec![PlanWarning::Spill];
    sort.props.insert("Sort Space Type".into(), "Disk".into());
    sort.props.insert("Sort Space Used".into(), "5120".into());
    sort.children = vec![scan];
    let spill = insights(&actual(Engine::Postgres, sort, 500.0)).into_iter().find(|i| i.rule == "spill").unwrap();
    assert!(!spill.detail.contains("underestimated"), "{}", spill.detail);
}

#[test]
fn a_lossy_bitmap_counts_rechecked_rows_over_its_loops() {
    // PostgreSQL gives rows removed by the recheck per loop, like its actual rows: 1,000 a loop over 50 loops.
    let mut heap = node(0, "Bitmap Heap Scan", NodeKind::Lookup);
    heap.act_executions = Some(50.0);
    heap.act_rows = Some(5_000.0);
    heap.props.insert("Lossy Heap Blocks".into(), "900".into());
    heap.props.insert("Exact Heap Blocks".into(), "40".into());
    heap.props.insert("Rows Removed by Index Recheck".into(), "1000".into());
    let lossy = insights(&actual(Engine::Postgres, heap, 50.0)).into_iter().find(|i| i.rule == "lossy-bitmap").unwrap();
    assert!(lossy.detail.contains("(50K rows thrown away)"), "{}", lossy.detail);
}

#[test]
fn not_in_is_only_row_by_row_probing() {
    let pg = |filter: &str| {
        let mut scan = node(1, "Seq Scan", NodeKind::Scan);
        scan.object = Some("public.customers".into());
        scan.predicate = Some(filter.into());
        scan.est_rows = 100_000.0;
        scan.self_cost = 1_000_000.0;
        let mut sub = node(2, "Seq Scan", NodeKind::Scan);
        sub.props.insert("Parent Relationship".into(), "SubPlan".into());
        sub.props.insert("Subplan Name".into(), "SubPlan 1".into());
        sub.est_rows = 400_000.0;
        scan.children = vec![sub];
        let mut p = plan(Engine::Postgres, scan);
        p.statement = Some("SELECT * FROM customers WHERE id NOT IN (SELECT customer_id FROM orders)".into());
        rules(&p)
    };
    // One build, one hash probe per row: PostgreSQL's hashed SubPlan, before and from 17.
    assert!(pg("(NOT (hashed SubPlan 1))").is_empty());
    assert!(pg("(NOT (ANY (customers.id = (hashed SubPlan 1).col1)))").is_empty());
    // Searched again for each row: that is NOT IN's cost.
    assert_eq!(pg("(NOT (SubPlan 1))"), vec!["not-in"]);
    assert_eq!(pg("(NOT (ANY (customers.id = (SubPlan 1).col1)))"), vec!["not-in"]);

    let mysql = |condition: &str, not_exists: bool| {
        let mut t = node(1, "Table scan", NodeKind::Scan);
        t.object = Some("shop.customers".into());
        t.predicate = Some(condition.into());
        t.est_rows = 199_000.0;
        t.self_cost = 20_000.0;
        if not_exists {
            t.props.insert("not_exists".into(), "true".into());
        }
        let mut p = plan(Engine::Mysql, t);
        p.statement = Some("SELECT * FROM customers WHERE id NOT IN (SELECT customer_id FROM orders)".into());
        rules(&p)
    };
    // MySQL 8.0.17+ runs it as an antijoin.
    assert!(
        mysql("<if>(is_not_null_compl(<subquery2>), <if>(found_match(<subquery2>), false, true), true)", true)
            .is_empty()
    );
    // Materialized once and looked up per row.
    assert!(mysql("(not(<in_optimizer>(customers.id,customers.id in ( <materialize> (select orders.customer_id from orders), <primary_index_lookup>(customers.id in <temporary table> on <auto_key>)))))", false).is_empty());
    // The subquery run for each row.
    assert_eq!(
        mysql(
            "(not(<in_optimizer>(customers.id,<exists>(<index_lookup>(<cache>(customers.id) in orders on fk_orders_customer)))))",
            false
        ),
        vec!["not-in"]
    );
}

#[test]
fn subqueries_count_the_runs_they_really_make() {
    // A hashed SubPlan runs once, however many rows probe it.
    let mut scan = node(1, "Seq Scan", NodeKind::Scan);
    scan.object = Some("public.orders".into());
    scan.est_rows = 500_000.0;
    scan.self_cost = 10_000.0;
    scan.predicate = Some("(ANY (customer_id = (hashed SubPlan 1).col1))".into());
    let mut sub = node(2, "Seq Scan", NodeKind::Scan);
    sub.object = Some("public.customers".into());
    sub.props.insert("Parent Relationship".into(), "SubPlan".into());
    sub.props.insert("Subplan Name".into(), "SubPlan 1".into());
    scan.children = vec![sub];
    let mut p = plan(Engine::Postgres, scan);
    assert!(!rules(&p).contains(&"correlated-subquery".to_string()));
    p.root.predicate = Some("(total > (SubPlan 1))".into());
    assert!(rules(&p).contains(&"correlated-subquery".to_string()));

    // MySQL: a subquery attached to a table's condition runs once per row of that table, not of a sibling.
    let mut table = node(1, "Table scan", NodeKind::Scan);
    table.object = Some("shop.customers".into());
    table.rows_read = Some(199_000.0);
    table.est_rows = 199_000.0;
    let dependent = |id: u32, rows: f64| {
        let mut d = node(id, "Dependent subquery", NodeKind::Other);
        d.props.insert("dependent".into(), "true".into());
        d.props.insert("cacheable".into(), "false".into());
        d.est_rows = rows;
        d.self_cost = 50.0;
        d
    };
    table.children = vec![dependent(2, 3.0), dependent(3, 5.0)];
    let found = insights(&plan(Engine::Mysql, table));
    let titles: Vec<&str> =
        found.iter().filter(|i| i.rule == "correlated-subquery").map(|i| i.title.as_str()).collect();
    assert!(
        !titles.is_empty() && titles.iter().all(|t| t.contains("199K times, once per row of customers")),
        "{titles:?}"
    );
}

#[test]
fn a_subquery_in_the_select_list_runs_once_per_row_returned() {
    // `SELECT o.id, (SELECT … WHERE c.id = o.customer_id) FROM orders o WHERE o.status = 'held'`: the scan reads 1M
    // rows and returns 50, and the SubPlan runs for those 50.
    let mut scan = node(1, "Seq Scan", NodeKind::Scan);
    scan.object = Some("public.orders".into());
    scan.predicate = Some("(status = 'held'::text)".into());
    scan.table_rows = Some(1_000_000.0);
    scan.est_rows = 50.0;
    scan.self_cost = 20_000.0;
    let mut sub = node(2, "Index Scan", NodeKind::Seek);
    sub.object = Some("public.customers".into());
    sub.props.insert("Parent Relationship".into(), "SubPlan".into());
    sub.props.insert("Subplan Name".into(), "SubPlan 1".into());
    scan.children = vec![sub];
    let mut p = plan(Engine::Postgres, scan);
    assert!(!rules(&p).contains(&"correlated-subquery".to_string()));
    let title = |p: &Plan| insights(p).into_iter().find(|i| i.rule == "correlated-subquery").map(|i| i.title);
    p.root.est_rows = 5_000.0;
    assert_eq!(title(&p).as_deref(), Some("A subquery runs 5.0K times, once per row of orders"));
    // In the scan's own condition it runs for each row the scan tests: at most every row it reads.
    p.root.predicate = Some("((status = 'held'::text) AND (total > (SubPlan 1)))".into());
    assert_eq!(title(&p).as_deref(), Some("A subquery runs up to 1.0M times, once per row of orders"));
    // SubPlan 12 is another subquery.
    p.root.predicate = Some("(total > (SubPlan 12))".into());
    assert_eq!(title(&p).as_deref(), Some("A subquery runs 5.0K times, once per row of orders"));
}

#[test]
fn smaller_rules_hold_for_this_plan() {
    // A Gather that never ran started no workers because it never asked for any.
    let mut gather = node(0, "Gather", NodeKind::Parallelism);
    gather.act_executions = Some(0.0);
    gather.act_rows = Some(0.0);
    gather.props.insert("Workers Planned".into(), "2".into());
    gather.props.insert("Workers Launched".into(), "0".into());
    assert!(!rules(&actual(Engine::Postgres, gather.clone(), 50.0)).contains(&"workers-short".to_string()));
    gather.act_executions = Some(1.0);
    assert!(rules(&actual(Engine::Postgres, gather, 50.0)).contains(&"workers-short".to_string()));

    // A BRIN bitmap is lossy by design.
    let mut heap = node(0, "Bitmap Heap Scan", NodeKind::Lookup);
    heap.props.insert("Lossy Heap Blocks".into(), "900".into());
    heap.props.insert("Exact Heap Blocks".into(), "0".into());
    assert!(!rules(&actual(Engine::Postgres, heap.clone(), 50.0)).contains(&"lossy-bitmap".to_string()));
    heap.props.insert("Exact Heap Blocks".into(), "40".into());
    assert!(rules(&actual(Engine::Postgres, heap, 50.0)).contains(&"lossy-bitmap".to_string()));

    // JIT that took most of the run can't have paid off; a fifth of it may have.
    let jit = |jit_ms: f64, run_ms: f64| {
        let mut p = actual(Engine::Postgres, node(0, "Seq Scan", NodeKind::Scan), run_ms);
        p.props.insert("Execution Time".into(), run_ms.to_string());
        p.props.insert("JIT".into(), format!("{{\"Timing\": {{\"Total\": {jit_ms}}}}}"));
        insights(&p).into_iter().find(|i| i.rule == "jit").expect("jit")
    };
    let most = jit(300.0, 400.0);
    assert_eq!(most.severity, Severity::Medium);
    assert!(most.fix_sql.as_deref().unwrap().starts_with("-- In your session"), "{:?}", most.fix_sql);
    let some = jit(100.0, 400.0);
    assert_eq!(some.severity, Severity::Low);
    assert!(!some.fix_sql.as_deref().unwrap().contains("ALTER ROLE"), "{:?}", some.fix_sql);

    // PostgreSQL's planning on Culprit's fresh connection includes loading table metadata.
    let mut p = actual(Engine::Postgres, node(0, "Seq Scan", NodeKind::Scan), 3.0);
    p.props.insert("Planning Time".into(), "30".into());
    p.props.insert("Execution Time".into(), "3".into());
    let compile = insights(&p).into_iter().find(|i| i.rule == "slow-compile").expect("slow compile");
    assert_eq!(compile.severity, Severity::Low);
    assert!(compile.detail.contains("twice in one session"), "{}", compile.detail);

    // The optimizer giving up on a cheap plan costs nothing.
    let mut cheap = plan(Engine::Mssql, node(0, "Clustered Index Seek", NodeKind::Seek));
    cheap.total_cost = 0.01;
    cheap.root.subtree_cost = 0.01;
    cheap.props.insert("StatementOptmEarlyAbortReason".into(), "TimeOut".into());
    let timeout = insights(&cheap).into_iter().find(|i| i.rule == "optimizer-timeout").expect("timeout");
    assert_eq!(timeout.severity, Severity::Low);

    // Below the commonly used cost threshold of 50, running serially may be the server's own choice.
    let blocked = |cost: f64| {
        let mut root = node(0, "Hash Match", NodeKind::Join);
        root.est_rows = 500_000.0;
        root.self_cost = cost;
        let mut p = plan(Engine::Mssql, root);
        p.props.insert("NonParallelPlanReason".into(), "MaxDOPSetToOne".into());
        insights(&p).into_iter().find(|i| i.rule == "parallelism-blocked")
    };
    assert!(blocked(3.0).is_none(), "below the default threshold nothing would go parallel");
    let mid = blocked(30.0).unwrap();
    assert_eq!(mid.severity, Severity::Low);
    assert!(!mid.detail.contains("would normally"), "{}", mid.detail);
    assert_eq!(blocked(200.0).unwrap().severity, Severity::Medium);

    // A cross join to a single row (a total, a setting) multiplies nothing.
    let mut cross = node(0, "Nested Loops", NodeKind::Join);
    cross.warnings = vec![PlanWarning::NoJoinPredicate];
    cross.est_rows = 200_000.0;
    let mut big = node(1, "Clustered Index Scan", NodeKind::Scan);
    big.object = Some("shop.orders".into());
    big.est_rows = 200_000.0;
    let mut total = node(2, "Stream Aggregate", NodeKind::Aggregate);
    total.est_rows = 1.0;
    total.est_executions = 1.0;
    cross.children = vec![big, total];
    assert!(!rules(&plan(Engine::Mssql, cross)).contains(&"no-join-predicate".to_string()));

    // A remote insert sends rows; it doesn't pull them.
    let mut remote = node(0, "Remote Insert", NodeKind::Modify);
    let mut rows = node(1, "Clustered Index Scan", NodeKind::Scan);
    rows.est_rows = 5_000.0;
    remote.children = vec![rows];
    let i = insights(&plan(Engine::Mssql, remote)).into_iter().find(|i| i.rule == "remote-query").unwrap();
    assert!(i.title.starts_with("Sends 5,000 rows") || i.title.starts_with("Sends 5.0K"), "{}", i.title);

    // A table variable never has statistics: missing-statistics leaves it to table-variable.
    let mut tv = node(0, "Table Scan", NodeKind::Scan);
    tv.object = Some("@t".into());
    tv.warnings = vec![PlanWarning::MissingStatistics];
    assert!(!rules(&plan(Engine::Mssql, tv.clone())).contains(&"missing-statistics".to_string()));
    tv.object = Some("dbo.orders".into());
    let stats = insights(&plan(Engine::Mssql, tv)).into_iter().find(|i| i.rule == "missing-statistics").unwrap();
    assert!(stats.fix_sql.unwrap().starts_with("SELECT name, is_auto_create_stats_on"));

    // MySQL's join buffer where possible keys exist: no claim that there is no index.
    let mut buffered = node(0, "Table scan", NodeKind::Scan);
    buffered.object = Some("shop.items".into());
    buffered.predicate = Some("(shop.i.price = shop.o.total)".into());
    buffered.props.insert("using_join_buffer".into(), "hash join".into());
    buffered.props.insert("possible_keys".into(), "ix_items_price".into());
    buffered.rows_read = Some(800_000.0);
    let i = insights(&plan(Engine::Mysql, buffered)).into_iter().find(|i| i.rule == "join-buffer").unwrap();
    assert!(!i.title.contains("without an index") && i.detail.contains("ix_items_price"), "{}", i.title);
    assert!(i.fix_sql.as_deref().unwrap().starts_with("ANALYZE TABLE"), "{:?}", i.fix_sql);
}

#[test]
fn every_rule_has_a_listing() {
    // Keep RULES in step with the rule functions: every id an insight can carry is listed.
    let ids: Vec<&str> = RULES.iter().map(|r| r.0).collect();
    for id in ["key-lookups", "memory-grant", "temp-table-sort", "index-ignored", "jit"] {
        assert!(ids.contains(&id));
    }
    assert!(RULES.len() >= 30);
}

// The false flag: a BRIN index's bitmap scan reports page ranges' worth of rows, and read as a misestimate. Marked with
// its kind from the catalog, it isn't one; a B-tree's still is.
#[test]
fn a_brin_bitmap_scan_is_no_misestimate() {
    let mut scan = node(1, "Bitmap Index Scan", NodeKind::Seek);
    scan.index = Some("events_at_brin".into());
    scan.object = Some("events".into());
    scan.est_rows = 100.0;
    scan.act_rows = Some(128_000.0);
    scan.act_executions = Some(1.0);
    let mut btree = scan.clone();
    btree.index = Some("events_at".into());
    assert!(scan.misestimate().is_some());
    mark_index_kinds(&mut scan, &[("events".into(), "events_at_brin".into(), "brin".into())]);
    assert_eq!(scan.misestimate(), None);
    mark_index_kinds(&mut btree, &[("events".into(), "events_at".into(), "btree".into())]);
    assert!(btree.misestimate().is_some());
}

// A monitoring query reads the engine's own catalog (Query Store's tables behind sys.query_store_*): there's no index
// to add to those and no view of them to rewrite. The same shapes on the application's tables are advice.
#[test]
fn no_index_or_rewrite_advice_on_the_engines_own_objects() {
    let lookups_on = |table: &str| {
        let mut seek = node(2, "Index Seek", NodeKind::Seek);
        seek.object = Some(table.into());
        seek.index = Some("plan_persist_runtime_stats_v2_idx1".into());
        let mut lookup = node(3, "Key Lookup", NodeKind::Lookup);
        lookup.object = Some(table.into());
        lookup.est_executions = 1_531.8;
        lookup.self_cost = 30.0;
        lookup.props.insert("OutputList".into(), "[count_executions], [total_duration]".into());
        let mut join = node(1, "Nested Loops", NodeKind::Join);
        join.children = vec![seek, lookup];
        let mut sort = node(0, "Sort", NodeKind::Sort);
        sort.children = vec![join];
        let mut p = plan(Engine::Mssql, sort);
        p.props.insert("StatementOptmEarlyAbortReason".into(), "TimeOut".into());
        p
    };
    let system = rules(&lookups_on("sys.plan_persist_runtime_stats_v2"));
    assert!(!system.contains(&"key-lookups".to_string()), "{system:?}");
    assert!(!system.contains(&"optimizer-timeout".to_string()), "{system:?}");
    let app = rules(&lookups_on("app.messages"));
    assert!(app.contains(&"key-lookups".to_string()) && app.contains(&"optimizer-timeout".to_string()), "{app:?}");

    for object in [
        "[mssqlsystemresource].[sys].[sysobjvalues]",
        "INFORMATION_SCHEMA.COLUMNS",
        "pg_catalog.pg_class",
        "performance_schema.events_statements_summary_by_digest",
        "`mysql`.`user`",
        "sqlite_master",
    ] {
        assert!(is_system_object(object), "{object}");
    }
    for object in ["app.messages", "public.orders", "shop.system_events", "orders"] {
        assert!(!is_system_object(object), "{object}");
    }
}

// A `SELECT … FROM sys.query_store_*` reads Query Store's internal table-valued functions, which SQL Server names
// without a schema (`<Object Table="[QUERY_STORE_QUERY_TEXT_IN_MEM]">`), next to its `sys.plan_persist_*` tables. In a
// plan whose every schema-qualified object is the catalog's, they are the catalog's too: no "declare it unique", no
// "replace nested views". Next to an application's table, the same shapes are advice.
#[test]
fn query_stores_unnamed_functions_are_the_catalogs_own() {
    let query_store = |table: &str| {
        let mut tvf = node(4, "Table-valued function", NodeKind::Other);
        tvf.object = Some("QUERY_STORE_QUERY_TEXT_IN_MEM".into());
        let mut scan = node(5, "Clustered Index Scan", NodeKind::Scan);
        scan.object = Some(table.into());
        scan.index = Some("plan_persist_query_text_cidx".into());
        let mut texts = node(3, "Concatenation", NodeKind::Other);
        texts.children = vec![tvf, scan];
        let mut queries = node(6, "Table-valued function", NodeKind::Other);
        queries.object = Some("QUERY_STORE_QUERY_IN_MEM".into());
        let mut join = node(2, "Merge Join", NodeKind::Join);
        join.props.insert("ManyToMany".into(), "true".into());
        join.children = vec![texts, queries];
        let mut sort = node(1, "Sort", NodeKind::Sort);
        sort.children = vec![join];
        let mut root = node(0, "Compute Scalar", NodeKind::Compute);
        root.children = vec![sort];
        let mut p = plan(Engine::Mssql, root);
        p.props.insert("StatementOptmEarlyAbortReason".into(), "TimeOut".into());
        rules(&p)
    };
    let system = query_store("sys.plan_persist_query_text");
    assert!(!system.contains(&"many-to-many".to_string()), "{system:?}");
    assert!(!system.contains(&"optimizer-timeout".to_string()), "{system:?}");
    let app = query_store("app.messages");
    assert!(app.contains(&"many-to-many".to_string()) && app.contains(&"optimizer-timeout".to_string()), "{app:?}");
}

// An estimated plan's lookups are the optimizer's expectation; a measured run's are what happened.
#[test]
fn estimated_lookups_are_said_as_an_estimate() {
    let lookups = |measured: bool| {
        let mut seek = node(1, "Index Seek", NodeKind::Seek);
        seek.object = Some("app.messages".into());
        seek.index = Some("ix_messages_conversation_id".into());
        let mut lookup = node(2, "Key Lookup", NodeKind::Lookup);
        lookup.object = Some("app.messages".into());
        lookup.est_executions = 1_500.0;
        if measured {
            lookup.act_executions = Some(1_500.0);
            lookup.act_rows = Some(1_500.0);
        }
        let mut join = node(0, "Nested Loops", NodeKind::Join);
        join.children = vec![seek, lookup];
        insights(&plan(Engine::Mssql, join)).into_iter().find(|i| i.rule == "key-lookups").expect("lookups").title
    };
    assert_eq!(lookups(false), "Estimated to look up 1.5K rows in app.messages one at a time");
    assert_eq!(lookups(true), "Looks up 1.5K rows in app.messages one at a time");
}

// SQL Server warned of no statistics on id, a key of the clustered index the lookup seeks. An index's statistics come
// with the index: they need updating, and creating statistics or checking the database's settings changes nothing.
#[test]
fn missing_statistics_on_an_index_key_updates_the_tables_statistics() {
    let mut seek = node(2, "Index Seek", NodeKind::Seek);
    seek.object = Some("app.support_escalations".into());
    seek.index = Some("ix_support_escalations_active".into());
    seek.seek_predicate = Some("[tenant_id] = CONVERT_IMPLICIT(uniqueidentifier,[@par0],0)".into());
    seek.warnings = vec![PlanWarning::MissingStatistics];
    seek.props.insert("ColumnsWithNoStatistics".into(), "id".into());
    let mut lookup = node(4, "Key Lookup", NodeKind::Lookup);
    lookup.object = Some("app.support_escalations".into());
    lookup.index = Some("pk_support_escalations".into());
    lookup.seek_predicate = Some("[id] = [app-db].[app].[support_escalations].[id]".into());
    let mut join = node(1, "Nested Loops", NodeKind::Join);
    join.children = vec![seek, lookup];
    let stats = |p: &Plan| insights(p).into_iter().find(|i| i.rule == "missing-statistics").expect("the warning");
    join.children[0].table_rows = Some(91.0);
    let keyed = stats(&plan(Engine::Mssql, join.clone()));
    assert_eq!(keyed.fix_sql.as_deref(), Some("UPDATE STATISTICS [app].[support_escalations];"));
    // They exist, so the title doesn't say there are none; the plan's own count of the table's rows is the fact it has.
    assert_eq!(keyed.title, "The statistics on id are out of date, so its row counts are guesses");
    assert_eq!(
        keyed.detail,
        "id is a key of pk_support_escalations, so it has statistics (SQL Server creates an index's statistics with the index), but SQL Server found nothing in them it could use. That happens to statistics built while the table was empty or nearly so, and not updated since; the table held 91 rows when this plan was compiled. Without usable statistics the optimizer uses fixed guesses for how many rows match, which leads to the wrong join type or memory size."
    );
    assert!(keyed.fix.starts_with("Update the statistics on support_escalations"), "{}", keyed.fix);
    // A check that read the catalog says what it holds instead.
    let stale = stale_index_statistics(&plan(Engine::Mssql, join.clone())).expect("index keys");
    assert_eq!(stale.indexes, vec!["pk_support_escalations".to_string()]);
    let told =
        stale_index_statistics_detail(&stale, Some("They were last updated on Aug 28, when the table had 1 row."));
    assert!(
        told.contains("could use. They were last updated on Aug 28, when the table had 1 row. Without usable"),
        "{told}"
    );

    // A column no seek keys on has no statistics at all: the database's settings say why.
    join.children[0].props.insert("ColumnsWithNoStatistics".into(), "kind".into());
    let unkeyed = stats(&plan(Engine::Mssql, join));
    assert_eq!(unkeyed.title, "No statistics on kind, so its row counts are guesses");
    assert!(unkeyed.fix_sql.unwrap().starts_with("SELECT name, is_auto_create_stats_on"));
}

// The rows a sort expects from a scan planned on PostgreSQL's default guess (a parameter compared by range in a generic
// plan) are the guess's: no count of them, and no share of the work built on it. Without the guess, the sort speaks.
#[test]
fn a_sort_fed_by_a_guess_states_no_size() {
    let mut scan = node(2, "Seq Scan", NodeKind::Scan);
    scan.object = Some("public.orders".into());
    scan.est_rows = 277_777.0;
    scan.self_cost = 300.0;
    scan.props.insert(crate::plan::GUESSED_SELECTIVITY.into(), "a default selectivity".into());
    let mut sort = node(1, "Sort", NodeKind::Sort);
    sort.est_rows = 277_777.0;
    sort.self_cost = 500.0;
    sort.children = vec![scan];
    assert!(!rules(&plan(Engine::Postgres, sort.clone())).contains(&"big-sort".to_string()));
    sort.children[0].props.clear();
    assert!(rules(&plan(Engine::Postgres, sort)).contains(&"big-sort".to_string()));
}

// One index, one name: an insight names the index it suggests as the findings name it (`IX_`, the table and the keys,
// folded to lower case where the engine folds names).
#[test]
fn a_suggested_index_is_named_as_the_findings_name_it() {
    let keys = vec!["order_id".to_string()];
    let mssql = index_sql(Engine::Mssql, "[db].[shop].[order_items]", &keys, &[]).unwrap();
    assert!(mssql.starts_with("CREATE NONCLUSTERED INDEX [IX_order_items_order_id]"), "{mssql}");
    let postgres = index_sql(Engine::Postgres, "public.order_items", &keys, &[]).unwrap();
    assert!(postgres.contains(" ix_order_items_order_id"), "{postgres}");
}

// MS-10: SQL Server builds an index through `insert [t] select * from [t]` into an Index Insert, reading and sorting the
// whole table: that is the build's work, so no rule reads an index for its sort or statistics on the column indexed.
#[test]
fn a_plan_that_builds_an_index_gets_no_advice() {
    let mut scan = node(3, "Clustered Index Scan", NodeKind::Scan);
    scan.object = Some("culprit_lab.events".into());
    scan.est_rows = 155_000.0;
    scan.warnings = vec![PlanWarning::MissingStatistics];
    let mut sort = node(2, "Sort", NodeKind::Sort);
    sort.est_rows = 155_000.0;
    sort.self_cost = 2.0;
    sort.children = vec![scan];
    let mut build = node(0, "Index Insert", NodeKind::Modify);
    build.object = Some("culprit_lab.events".into());
    build.children = vec![sort];
    let mut p = plan(Engine::Mssql, build);
    p.statement = Some("insert [culprit_lab].[events] select * from [culprit_lab].[events]".into());
    assert!(changes_schema(&p));
    assert!(insights(&p).is_empty(), "{:?}", rules(&p));
    assert!(coverage_note(&p).unwrap().starts_with("This plan builds an index"), "{:?}", coverage_note(&p));
    // The application's own statement, as it wrote it, is a query like any other.
    p.statement = Some("INSERT [culprit_lab].[events] SELECT * FROM [culprit_lab].[events]".into());
    assert!(!changes_schema(&p));
    p.statement = Some("CREATE INDEX ix_events_tenant ON culprit_lab.events (tenant)".into());
    assert!(changes_schema(&p));
}

// MS-12: with the server's settings stamped from the snapshot, parallelism-blocked says which MAXDOP is 1 and the
// threshold the plan passes, rather than every place it could be.
#[test]
fn a_serial_plan_says_which_setting_keeps_it_on_one_cpu() {
    let blocked = |statement: &str, settings: &[(&'static str, &str)]| {
        let mut root = node(0, "Hash Match", NodeKind::Join);
        root.est_rows = 500_000.0;
        root.self_cost = 542.0;
        let mut p = plan(Engine::Mssql, root);
        p.statement = Some(statement.into());
        p.props.insert("NonParallelPlanReason".into(), "MaxDOPSetToOne".into());
        let stamped: Vec<(&'static str, String)> = settings.iter().map(|(k, v)| (*k, v.to_string())).collect();
        mark_parallel_settings(&mut p, &stamped);
        insights(&p).into_iter().find(|i| i.rule == "parallelism-blocked").expect("parallelism-blocked")
    };
    let known = [(COST_THRESHOLD, "5"), (SERVER_MAXDOP, "0"), (DATABASE_MAXDOP, "0")];
    let hinted = blocked(
        "SELECT COUNT_BIG(*) FROM shop.order_items a CROSS JOIN shop.customers c OPTION (MAXDOP 1, LOOP JOIN)",
        &known,
    );
    assert_eq!(hinted.title, "This plan runs on one CPU because its statement says OPTION (MAXDOP 1)");
    assert_eq!(
        hinted.detail,
        "Its estimated cost is 542, above this server's cost threshold for parallelism (5), so SQL Server would \
         consider a parallel plan for it; the whole run happens on a single core instead."
    );
    assert!(hinted.fix.starts_with("Remove OPTION (MAXDOP 1) from the statement"), "{}", hinted.fix);
    let database = blocked("SELECT 1 FROM shop.orders", &[(COST_THRESHOLD, "50"), (DATABASE_MAXDOP, "1")]);
    assert_eq!(database.title, "This plan runs on one CPU because the database's MAXDOP setting is 1");
    let neither = blocked("SELECT 1 FROM shop.orders", &known);
    assert!(
        neither.title.contains("neither its statement nor the database's or server's setting says so"),
        "{}",
        neither.title
    );
    assert!(neither.fix.contains("Query Store hint"), "{}", neither.fix);
    // Without the settings, the plan alone says what it can.
    let unknown = blocked("SELECT 1 FROM shop.orders", &[]);
    assert!(unknown.title.ends_with("(a query hint, or the database or server setting)"), "{}", unknown.title);
    assert!(unknown.detail.contains("5 by default, often raised to 50"), "{}", unknown.detail);
}

// MS-12 and MS-11: where the snapshot marked the scan's table with its indexes, scan-filter says whether one already
// starts with the column instead of "if one already exists"; two equality columns are said to go in any order.
#[test]
fn scan_filter_advice_says_what_the_catalog_has() {
    let scan = |filter: &str, indexes: Option<&str>| {
        let mut n = node(3, "Seq Scan", NodeKind::Scan);
        n.object = Some("public.order_items".into());
        n.predicate = Some(filter.into());
        n.est_rows = 24.0;
        n.rows_read = Some(1_000_000.0);
        n.self_cost = 50.0;
        if let Some(lines) = indexes {
            n.props.insert(TABLE_INDEXES.into(), lines.into());
        }
        insights(&plan(Engine::Postgres, n)).into_iter().find(|i| i.rule == "scan-filter").expect("scan-filter")
    };
    let two = "((order_items.quantity = $1) AND (order_items.product_id = $2))";
    let none = scan(two, Some("order_items_pkey=id"));
    assert_eq!(
        none.fix,
        "No index on order_items starts with quantity or product_id: one on quantity and product_id lets the \
         database go straight to the matching rows. quantity and product_id can go in any order: put first the one \
         with the most distinct values, or one another index or finding on order_items already leads with, so one \
         index serves both."
    );
    assert!(none.fix_sql.as_deref().unwrap().contains("CREATE INDEX"), "{:?}", none.fix_sql);
    let has = scan("(order_items.product_id = $1)", Some("order_items_pkey=id\nix_items_product=product_id,quantity"));
    assert!(
        has.fix.starts_with("ix_items_product already starts with product_id, yet this plan reads order_items"),
        "{}",
        has.fix
    );
    assert!(!has.fix_sql.as_deref().unwrap().contains("CREATE INDEX"), "{:?}", has.fix_sql);
    let unknown = scan("(order_items.product_id = $1)", None);
    assert!(unknown.fix.contains("if one already exists, the optimizer didn't use it"), "{}", unknown.fix);
    assert!(!unknown.fix.contains("any order"), "{}", unknown.fix);
}

// The snapshot's catalog marks each scanned table with its indexes, by schema where the plan names it; a name two
// schemas share without one in the plan, or a catalog without indexes, leaves the scan unmarked.
#[test]
fn scans_are_marked_with_their_tables_indexes() {
    let tables = vec![
        ("public".to_string(), "orders".to_string(), "orders_pkey=id".to_string()),
        ("public".to_string(), "items".to_string(), String::new()),
        ("archive".to_string(), "items".to_string(), "items_pkey=id".to_string()),
    ];
    let marked = |object: &str| {
        let mut n = node(1, "Seq Scan", NodeKind::Scan);
        n.object = Some(object.into());
        mark_table_indexes(&mut n, &tables, false);
        n.props.get(TABLE_INDEXES).cloned()
    };
    assert_eq!(marked("public.orders").as_deref(), Some("orders_pkey=id"));
    assert_eq!(marked("orders").as_deref(), Some("orders_pkey=id"));
    assert_eq!(marked("public.items").as_deref(), Some(""));
    assert_eq!(marked("items"), None);
    assert_eq!(marked("public.gone"), None);
}
