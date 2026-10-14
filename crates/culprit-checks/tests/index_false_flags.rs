//! Index advice that has to hold: each test plants, on a demo snapshot, a case where an index check or the index
//! advisor used to say something the database contradicts, and checks what is said now.

use culprit_checks::{analyze, index_advice};
use culprit_core::{
    CandidateSource, CleanupKind, Confidence, ConstraintIssue, ConstraintKind, Finding, IndexStat, MissingIndex,
    NodeKind, PlanNode, QueryStat, Report, Severity, Snapshot, TableStorage,
};

fn snapshot(name: &str) -> Snapshot {
    let text =
        std::fs::read_to_string(format!("{}/../../fixtures/{name}", env!("CARGO_MANIFEST_DIR"))).expect("fixture");
    serde_json::from_str(&text).expect("snapshot")
}

/// The demo's counters are minutes old; claims of absence and rates a day need days of them.
fn counted_for_ten_days(mut s: Snapshot) -> Snapshot {
    s.server.started_at = Some("2026-10-01T12:00:00Z".into());
    s.taken_at = "2026-10-11T12:00:00Z".into();
    if let Some(p) = s.pressure.as_mut() {
        p.uptime_hours = Some(240.0);
    }
    s
}

fn finding<'a>(report: &'a Report, id: &str) -> Option<&'a Finding> {
    report.findings.iter().find(|f| f.id == id)
}

fn ids(report: &Report) -> Vec<&str> {
    report.findings.iter().map(|f| f.id.as_str()).collect()
}

fn sql(f: &Finding) -> &str {
    f.fix.as_ref().and_then(|x| x.sql.as_deref()).unwrap_or_default()
}

fn names(v: &[&str]) -> Vec<String> {
    v.iter().map(|c| c.to_string()).collect()
}

/// SQL Server's optimizer asks for an index on support_tickets, for one query in the window.
fn request(s: &mut Snapshot, equality: &[&str], inequality: &[&str], include: &[&str]) {
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
    let mut q = w.queries[0].clone();
    q.id = "4242".into();
    q.query_hash = Some(hash.into());
    q.plan = None;
    q.text = "(@par0 int)select [id], [status] from [shop].[support_tickets] where [customer_id] = @par0".into();
    w.queries.push(q);
}

fn index_like(s: &Snapshot, template: &str, name: &str, columns: &[&str]) -> IndexStat {
    let mut i = s.indexes.iter().find(|i| i.name == template).expect("template index").clone();
    i.name = name.into();
    i.columns = names(columns);
    i.include.clear();
    i.filter = None;
    i.is_unique = false;
    i.is_primary_key = false;
    i.is_clustered = false;
    i.is_disabled = false;
    i
}

// I2: rebuilding an index to include columns keeps what it is: unique stays unique, keys keep their order and direction,
// the columns it already includes stay included.
#[test]
fn widening_a_unique_index_keeps_it_unique_with_its_keys_and_includes() {
    let mut s = counted_for_ten_days(snapshot("mssql-demo.json"));
    let mut ux = index_like(
        &s,
        "ix_support_tickets_customer_id",
        "ux_support_tickets_customer",
        &["customer_id", "created_at DESC"],
    );
    ux.is_unique = true;
    ux.include = names(&["priority"]);
    s.indexes.push(ux);
    request(&mut s, &["customer_id"], &["created_at"], &["status"]);
    let report = analyze(s);
    let f = finding(&report, "missing-index:shop.support_tickets(customer_id,created_at)").expect("the request");
    assert_eq!(
        sql(f),
        "CREATE UNIQUE NONCLUSTERED INDEX [ux_support_tickets_customer]\n    ON [shop].[support_tickets] ([customer_id], [created_at] DESC)\n    INCLUDE ([priority], [status])\n    WITH (DROP_EXISTING = ON, ONLINE = ON);"
    );
    let summary = &f.fix.as_ref().unwrap().summary;
    assert!(summary.contains("It stays unique."), "{summary}");
    // The advisor gives the same statement and names the index it widens.
    let advice = index_advice(&report);
    let c = advice.candidates.iter().find(|c| c.finding_id.as_deref() == Some(f.id.as_str())).expect("advised");
    assert_eq!(c.sql.as_deref(), Some(sql(f)));
    assert_eq!(c.widens.as_deref(), Some("ux_support_tickets_customer"));
}

// I2: SQL Server won't change a primary key's index to include columns: no rebuild, a separate index and why.
#[test]
fn a_primary_key_is_never_rebuilt_to_include_columns() {
    let mut s = counted_for_ten_days(snapshot("mssql-demo.json"));
    let mut pk =
        index_like(&s, "ix_support_tickets_customer_id", "pk_support_tickets_customer", &["customer_id", "created_at"]);
    pk.is_primary_key = true;
    pk.is_unique = true;
    s.indexes.push(pk);
    request(&mut s, &["customer_id"], &["created_at"], &["status"]);
    let report = analyze(s);
    let f = finding(&report, "missing-index:shop.support_tickets(customer_id,created_at)").expect("the request");
    assert!(!sql(f).contains("DROP_EXISTING"), "{}", sql(f));
    assert!(sql(f).starts_with("CREATE NONCLUSTERED INDEX [IX_support_tickets_customer_id_created_at]"), "{}", sql(f));
    assert!(
        f.fix.as_ref().unwrap().summary.starts_with("pk_support_tickets_customer enforces the primary key"),
        "{}",
        f.fix.as_ref().unwrap().summary
    );
}

// I2: an index that already has the keys and includes the columns needs nothing; never a twin of it.
#[test]
fn an_index_that_already_covers_the_request_is_not_built_twice() {
    let mut s = counted_for_ten_days(snapshot("mssql-demo.json"));
    s.indexes.iter_mut().find(|i| i.name == "ix_support_tickets_customer_id").unwrap().include = names(&["status"]);
    request(&mut s, &["customer_id"], &[], &["status"]);
    let report = analyze(s);
    let f = finding(&report, "missing-index:shop.support_tickets(customer_id)").expect("the request");
    assert!(f.fix.as_ref().unwrap().sql.is_none(), "{}", sql(f));
    assert!(
        f.fix.as_ref().unwrap().summary.starts_with("ix_support_tickets_customer_id already starts with these columns")
    );
}

// I2: an index with the request's keys that is disabled serves nothing; the fix brings it back, not "check the
// predicates for type mismatches".
#[test]
fn a_disabled_index_with_the_keys_is_brought_back() {
    let disabled = |include: &[&str]| {
        let mut s = counted_for_ten_days(snapshot("mssql-demo.json"));
        s.indexes.iter_mut().find(|i| i.name == "ix_support_tickets_customer_id").unwrap().is_disabled = true;
        request(&mut s, &["customer_id"], &[], include);
        analyze(s)
    };
    let report = disabled(&[]);
    let f = finding(&report, "missing-index:shop.support_tickets(customer_id)").expect("the request");
    assert_eq!(
        sql(f),
        "ALTER INDEX [ix_support_tickets_customer_id] ON [shop].[support_tickets] REBUILD WITH (ONLINE = ON);"
    );
    let summary = &f.fix.as_ref().unwrap().summary;
    assert!(summary.contains("is disabled"), "{summary}");
    assert!(!summary.contains("type mismatches"), "{summary}");
    let advice = index_advice(&report);
    let c = advice.candidates.iter().find(|c| c.finding_id.as_deref() == Some(f.id.as_str())).expect("advised");
    assert_eq!((c.sql.as_deref(), c.widens.as_deref()), (Some(sql(f)), Some("ix_support_tickets_customer_id")));

    // With columns to include, the rebuild that brings it back includes them.
    let report = disabled(&["status"]);
    let f = finding(&report, "missing-index:shop.support_tickets(customer_id)").expect("the request");
    assert!(
        sql(f).starts_with("CREATE NONCLUSTERED INDEX [ix_support_tickets_customer_id]\n    ON [shop].[support_tickets] ([customer_id])\n    INCLUDE ([status])\n    WITH (DROP_EXISTING = ON"),
        "{}",
        sql(f)
    );
}

// I2: a scan whose only leading index is disabled (on PostgreSQL, invalid after a failed build) gets that index back,
// not a twin of it.
#[test]
fn a_scan_past_an_invalid_index_rebuilds_it() {
    let mut s = counted_for_ten_days(snapshot("postgres-demo.json"));
    let mut invalid = index_like(&s, "orders_status_idx", "orders_customer_idx", &["customer_id"]);
    invalid.is_disabled = true;
    s.indexes.push(invalid);
    let report = analyze(s);
    let f = finding(&report, "filtered-scan:public.orders(customer_id)").expect("the scan");
    assert_eq!(sql(f), "REINDEX INDEX CONCURRENTLY public.orders_customer_idx;");
    assert!(
        f.summary.contains("Only orders_customer_idx, which is disabled, starts with customer_id"),
        "{}",
        f.summary
    );
    let advice = index_advice(&report);
    let c = advice.candidates.iter().find(|c| c.finding_id.as_deref() == Some(f.id.as_str())).expect("advised");
    assert_eq!((c.sql.as_deref(), c.widens.as_deref()), (Some(sql(f)), Some("orders_customer_idx")));
}

// I3: a request on (status) isn't served by the index made for (customer_id, status): its queries couldn't seek it.
// A request on (customer_id) is, and its lookups add up there.
#[test]
fn requests_merge_only_into_an_index_that_leads_with_their_columns() {
    let mut s = counted_for_ten_days(snapshot("mssql-demo.json"));
    let ask = |equality: &[&str], seeks: f64| MissingIndex {
        schema: "shop".into(),
        table: "orders".into(),
        equality: names(equality),
        inequality: Vec::new(),
        include: Vec::new(),
        seeks,
        scans: 0.0,
        avg_query_cost: 1.0,
        avg_impact_pct: 60.0,
        last_used: None,
        query_hashes: Vec::new(),
    };
    s.missing_indexes.extend([
        ask(&["customer_id", "status"], 500.0),
        ask(&["status"], 300.0),
        ask(&["customer_id"], 200.0),
    ]);
    let report = analyze(s);
    let lookups = |id: &str| {
        let f = finding(&report, id).unwrap_or_else(|| panic!("{id}: {:?}", ids(&report)));
        f.metrics.iter().find(|m| m.label == "Lookups that wanted it").map(|m| m.value).unwrap()
    };
    assert_eq!(lookups("missing-index:shop.orders(customer_id,status)"), 700.0);
    assert_eq!(lookups("missing-index:shop.orders(status)"), 300.0);
    assert!(finding(&report, "missing-index:shop.orders(customer_id)").is_none());
}

// I4a: the advisor lists what the refined findings say. A scan the optimizer's request serves lends it its query; it
// isn't a second index that would duplicate the first.
#[test]
fn the_advisor_adds_one_index_for_a_scan_and_the_request_that_serves_it() {
    let mut s = counted_for_ten_days(snapshot("mssql-demo.json"));
    let m = s.missing_indexes.iter_mut().find(|m| m.table == "order_items").unwrap();
    m.equality = names(&["order_id", "product_id"]);
    let report = analyze(s);
    assert!(!ids(&report).iter().any(|id| id.starts_with("filtered-scan:shop.order_items")), "{:?}", ids(&report));
    let advice = index_advice(&report);
    let on_items: Vec<_> = advice.candidates.iter().filter(|c| c.table == "order_items").collect();
    assert_eq!(on_items.len(), 1, "{:?}", on_items.iter().map(|c| &c.keys).collect::<Vec<_>>());
    assert_eq!(on_items[0].keys, ["order_id", "product_id"]);
    assert!(on_items[0].queries.iter().any(|q| q.id == "9"));

    // An index made for (product_id, order_id) can't seek a scan on order_id alone: the scan keeps its own finding and
    // its own index, in the findings and in the advisor alike.
    let mut s = counted_for_ten_days(snapshot("mssql-demo.json"));
    let m = s.missing_indexes.iter_mut().find(|m| m.table == "order_items").unwrap();
    m.equality = names(&["product_id", "order_id"]);
    let report = analyze(s);
    assert!(finding(&report, "filtered-scan:shop.order_items(order_id)").is_some(), "{:?}", ids(&report));
    let advice = index_advice(&report);
    let scan = advice.candidates.iter().find(|c| c.source == CandidateSource::PlanScan).expect("the scan's index");
    assert_eq!(scan.keys, ["order_id"]);
    assert_eq!(scan.finding_id.as_deref(), Some("filtered-scan:shop.order_items(order_id)"));
}

// I4a: a function around a filtered column is why the scan happens; the findings say rewrite it, and so does the advisor
// (no index of its own).
#[test]
fn the_advisor_adds_no_index_for_a_scan_a_rewrite_fixes() {
    let mut s = counted_for_ten_days(snapshot("postgres-demo.json"));
    let q = s.workload.as_mut().unwrap().queries.iter_mut().find(|q| q.id == "5308963031944094559").unwrap();
    let scan = scan_node(q).expect("the scan");
    scan.predicate = Some("((orders.customer_id = $1) AND (date_trunc('day'::text, orders.created_at) = $2))".into());
    scan.table_rows = Some(1_999_995.0);
    let report = analyze(s);
    assert!(finding(&report, "filtered-scan:public.orders(customer_id)").is_none(), "{:?}", ids(&report));
    assert!(ids(&report).iter().any(|id| id.starts_with("plan-insights:non-sargable:")), "{:?}", ids(&report));
    let advice = index_advice(&report);
    assert!(
        !advice.candidates.iter().any(|c| c.table == "orders" && c.source == CandidateSource::PlanScan),
        "{:?}",
        advice.candidates.iter().map(|c| (&c.table, &c.keys)).collect::<Vec<_>>()
    );
}

fn scan_node(q: &mut QueryStat) -> Option<&mut PlanNode> {
    fn find(node: &mut PlanNode) -> Option<&mut PlanNode> {
        if node.kind == NodeKind::Scan && node.object.is_some() {
            return Some(node);
        }
        node.children.iter_mut().find_map(find)
    }
    find(&mut q.plan.as_mut()?.root)
}

/// The PostgreSQL demo's events table, read in full 100 times a day for ten days.
fn events_read_in_full(s: &mut Snapshot, seq_scans: f64) {
    let t = s.tables.iter_mut().find(|t| t.name == "events").unwrap();
    t.seq_scans = Some(seq_scans);
    t.seq_rows_read = Some(1000.0 * t.rows.unwrap());
}

// I5: the statements behind a table's full reads are the ones that scan it. A statement that seeks it through an
// index, or only inserts into it, isn't, so its share of database time isn't the scans'.
#[test]
fn full_reads_are_charged_to_the_statements_that_scan() {
    let mut s = counted_for_ten_days(snapshot("postgres-demo.json"));
    events_read_in_full(&mut s, 1000.0);
    let w = s.workload.as_mut().unwrap();
    let mut seek = w.queries.iter().find(|q| q.id == "2004611025554862962").unwrap().clone();
    let node = scan_node(&mut seek).unwrap();
    node.kind = NodeKind::Seek;
    node.op = "Index Scan".into();
    node.object = Some("public.events".into());
    seek.id = "seeks-events".into();
    seek.text = "SELECT kind FROM events WHERE id = $1".into();
    seek.total_time_ms = 400_000.0;
    let mut insert = seek.clone();
    insert.id = "inserts-events".into();
    insert.text = "INSERT INTO events (customer_id, kind) VALUES ($1, $2)".into();
    insert.plan = None;
    w.queries.extend([seek, insert]);
    w.total_time_ms += 800_000.0;
    let report = analyze(s);
    let f = finding(&report, "scan-heavy:public.events").expect("scan-heavy");
    assert_eq!(f.severity, Severity::Low);
    assert!(f.queries.is_empty(), "{:?}", f.queries);
    assert!(f.impact.db_time_share.is_none());
    assert!(f.summary.contains("reach it through an index"), "{}", f.summary);
}

// I17: PostgreSQL counts a parallel scan once per worker and leader; the rows it read say how many full reads there were.
#[test]
fn full_reads_come_from_rows_read_when_the_scan_count_is_higher() {
    let mut s = counted_for_ten_days(snapshot("postgres-demo.json"));
    // 1,000 full reads in ten days, counted 3,000 times by a leader and two workers each.
    events_read_in_full(&mut s, 3000.0);
    let report = analyze(s);
    let f = finding(&report, "scan-heavy:public.events").expect("scan-heavy");
    assert!(f.title.contains("read in full about 100 times a day"), "{}", f.title);
}

// I6: rows returned measure what a filter kept only when the statement returns every row it keeps. A count returns
// one row whatever the filter keeps, so the plan's "keeps 90%" stands and no index is suggested.
#[test]
fn rows_returned_by_an_aggregate_say_nothing_about_the_filter() {
    let plant = |text: &str, rows_per_run: f64| {
        let mut s = snapshot("mysql-demo.json");
        let q = s.workload.as_mut().unwrap().queries.iter_mut().find(|q| q.id == "c7b5ac9bb4bcecfe").unwrap();
        q.text = text.into();
        q.rows = Some(rows_per_run * q.executions);
        q.logical_reads = Some(498_573.0 * q.executions);
        let node = &mut q.plan.as_mut().unwrap().root;
        let node = if node.object.is_some() { node } else { &mut node.children[0] };
        node.kind = NodeKind::Scan;
        node.op = "Table scan".into();
        node.index = None;
        node.predicate = Some("(shop.orders.created_at >= TIMESTAMP'2026-01-01 00:00:00')".into());
        node.rows_read = Some(498_573.0);
        node.est_rows = 448_716.0;
        analyze(s)
    };
    let id = "filtered-scan:shop.orders(created_at)";
    let report = plant("SELECT COUNT ( * ) FROM `orders` WHERE `created_at` >= ?", 1.0);
    assert!(finding(&report, id).is_none(), "{:?}", ids(&report));
    let report = plant("SELECT `id` FROM `orders` WHERE `created_at` >= ? LIMIT ?", 5.0);
    assert!(finding(&report, id).is_none(), "{:?}", ids(&report));
    // A plain read returns every row the filter keeps: five a run out of half a million is a filter that keeps few.
    let report = plant("SELECT `id` , `total` FROM `orders` WHERE `created_at` >= ?", 5.0);
    assert!(finding(&report, id).is_some(), "{:?}", ids(&report));
}

// I7: an index on one branch of an OR can't find the other branch's rows.
#[test]
fn a_filter_with_or_gets_no_index_on_one_branch() {
    let mut s = counted_for_ten_days(snapshot("postgres-demo.json"));
    let q = s.workload.as_mut().unwrap().queries.iter_mut().find(|q| q.id == "5308963031944094559").unwrap();
    scan_node(q).unwrap().predicate =
        Some("((orders.customer_id = $1) OR ((orders.status)::text = 'open'::text))".into());
    let report = analyze(s);
    assert!(!ids(&report).iter().any(|id| id.starts_with("filtered-scan:public.orders")), "{:?}", ids(&report));
    assert!(
        !index_advice(&report).candidates.iter().any(|c| c.table == "orders" && c.source == CandidateSource::PlanScan)
    );
}

/// Seven more indexes on the PostgreSQL demo's orders table.
fn many_indexes(s: &mut Snapshot, table: &str, template: &str) {
    for n in 0..7 {
        let i = index_like(s, template, &format!("ix_extra_{n}"), &[&format!("zz_col_{n}")]);
        let mut i = i;
        i.table = table.into();
        i.reads = Some(5000.0);
        s.indexes.push(i);
    }
}

// I10: PostgreSQL's HOT updates write no index entry, so they aren't upkeep for the table's indexes.
#[test]
fn hot_updates_are_not_index_upkeep() {
    let write = |hot: Option<f64>| {
        let mut s = counted_for_ten_days(snapshot("postgres-demo.json"));
        many_indexes(&mut s, "orders", "orders_status_idx");
        let t = s.tables.iter_mut().find(|t| t.name == "orders").unwrap();
        t.inserts = Some(100_000.0);
        t.updates = Some(9_900_000.0);
        t.storage = Some(TableStorage { hot_updates: hot, ..Default::default() });
        analyze(s)
    };
    let report = write(None);
    let f = finding(&report, "over-indexed:public.orders").expect("over-indexed");
    assert!(f.title.contains("about 1.0M rows written a day"), "{}", f.title);
    assert!(f.summary.contains("isn't HOT"), "{}", f.summary);
    // Nearly every update was HOT: 10K writes a day reach the indexes.
    let report = write(Some(9_800_000.0));
    assert!(finding(&report, "over-indexed:public.orders").is_none(), "{:?}", ids(&report));
    assert!(
        !index_advice(&report).tables.iter().any(|t| t.table == "orders" && t.reason.contains("secondary indexes"))
    );
}

// I10: SQL Server counts write statements, not rows, and says so.
#[test]
fn sql_server_writes_are_counted_as_statements() {
    let mut s = counted_for_ten_days(snapshot("mssql-demo.json"));
    many_indexes(&mut s, "orders", "ix_orders_channel");
    s.indexes.iter_mut().find(|i| i.name == "pk_orders").unwrap().writes = Some(1_000_000.0);
    let report = analyze(s);
    let f = finding(&report, "over-indexed:shop.orders").expect("over-indexed");
    assert!(f.title.contains("write statements a day"), "{}", f.title);
    assert!(f.metrics.iter().any(|m| m.label == "Write statements per day"), "{:?}", f.metrics);
}

// I11: what an index saves is its queries' time times the optimizer's gain: 11% of a query nobody waits on is Low.
#[test]
fn a_small_gain_on_a_small_query_is_low() {
    let mut s = counted_for_ten_days(snapshot("mssql-demo.json"));
    request(&mut s, &["customer_id"], &["opened_at"], &[]);
    let m = s.missing_indexes.last_mut().unwrap();
    m.avg_impact_pct = 11.0;
    let w = s.workload.as_mut().unwrap();
    w.queries.last_mut().unwrap().total_time_ms = 2.0;
    let report = analyze(s);
    let f = finding(&report, "missing-index:shop.support_tickets(customer_id,opened_at)").expect("the request");
    assert_eq!(f.severity, Severity::Low);
}

// I11: severity needs an amount. On the PostgreSQL demo, order_items' scan is 4% of database time, but about half a
// minute of it a day: Low. The orders scan takes minutes a day: Medium.
#[test]
fn a_scan_needs_time_a_day_as_well_as_a_share() {
    let report = analyze(snapshot("postgres-demo.json"));
    let small = finding(&report, "filtered-scan:public.order_items(product_id)").expect("order_items");
    assert!(small.impact.db_time_share.unwrap() >= 0.03);
    assert_eq!(small.severity, Severity::Low);
    let big = finding(&report, "filtered-scan:public.orders(customer_id)").expect("orders");
    assert_eq!(big.severity, Severity::Medium);
}

// I11: an unread index's upkeep is judged as a rate: two million writes since a restart a year ago is a quiet table.
#[test]
fn unused_index_upkeep_is_a_rate() {
    let unused = |started: &str| {
        let mut s = counted_for_ten_days(snapshot("mssql-demo.json"));
        s.server.started_at = Some(started.into());
        for i in s.indexes.iter_mut().filter(|i| i.table == "order_items") {
            i.writes = Some(2_000_000.0);
        }
        analyze(s).findings.into_iter().find(|f| f.id == "unused-index:shop.order_items").expect("unused")
    };
    assert_eq!(unused("2025-10-11T12:00:00Z").severity, Severity::Low);
    assert_eq!(unused("2026-10-01T12:00:00Z").severity, Severity::Medium);
}

// I12: a request SQL Server's optimizer last made before the window is history: no query in the window wanted it.
#[test]
fn a_request_last_made_before_the_window_is_history() {
    let mut s = snapshot("mssql-demo.json");
    s.missing_indexes.iter_mut().find(|m| m.table == "order_items").unwrap().last_used =
        Some("2026-08-01T09:00:00Z".into());
    let report = analyze(s.clone());
    let f = finding(&report, "missing-index:shop.order_items(order_id)").expect("the request");
    assert!(f.inactive);
    assert_eq!(f.severity, Severity::Low);
    assert!(f.caveats[0].starts_with("SQL Server's optimizer last asked for this index on Aug 1"), "{}", f.caveats[0]);
    assert!(!index_advice(&report).candidates.iter().any(|c| c.finding_id.as_deref() == Some(f.id.as_str())));

    // Without query history, a request nobody made in the last week is history too.
    s.workload = None;
    let report = analyze(s);
    let f = finding(&report, "missing-index:shop.order_items(order_id)").expect("the request");
    assert!(f.inactive);
    assert!(f.caveats[0].contains("before the last week"), "{}", f.caveats[0]);
}

// I13: an index can take over another's queries only if it holds every row the other does, has the other's columns as
// keys, and isn't the clustered index.
#[test]
fn an_overlap_needs_an_unfiltered_index_with_the_columns_as_keys() {
    let overlap = |name: &str, change: &dyn Fn(&mut IndexStat)| {
        let mut s = counted_for_ten_days(snapshot(name));
        let template = s.indexes.iter().find(|i| !i.is_primary_key && !i.is_unique).unwrap().clone();
        let add = |index: &str, columns: &[&str], reads: f64| {
            let mut i = template.clone();
            i.name = index.into();
            i.columns = names(columns);
            i.include.clear();
            i.reads = Some(reads);
            i.size_bytes = Some(64.0 * 1024.0 * 1024.0);
            i
        };
        let rare = add("ix_rare", &["zz_tenant", "zz_status"], 12.0);
        let mut busy = add("ix_busy", &["zz_tenant", "zz_created", "zz_status"], 900_000.0);
        change(&mut busy);
        s.indexes.extend([rare, busy]);
        analyze(s).findings.iter().any(|f| f.id.starts_with("overlapping-index:") && f.id.ends_with("ix_rare"))
    };
    for name in ["mssql-demo.json", "postgres-demo.json", "mysql-demo.json"] {
        assert!(overlap(name, &|_| {}), "{name}: the plain case");
        assert!(!overlap(name, &|b| b.filter = Some("([is_deleted]=(0))".into())), "{name}: filtered");
        assert!(
            !overlap(name, &|b| {
                b.columns = names(&["zz_tenant", "zz_created"]);
                b.include = names(&["zz_status"]);
            }),
            "{name}: included, not a key"
        );
    }
    // InnoDB's PRIMARY is the clustered index, reported with the same kind as a secondary one.
    assert!(!overlap("mysql-demo.json", &|b| {
        b.is_clustered = true;
        b.is_primary_key = true;
        b.is_unique = true;
    }));
}

// I14: a foreign key SQL Server doesn't check (NOCHECK CONSTRAINT) needs no index for the check.
#[test]
fn a_disabled_foreign_key_needs_no_index() {
    let mut s = counted_for_ten_days(snapshot("mssql-demo.json"));
    let id = "unindexed-fk:shop.ticket_events(ticket_id)";
    assert!(finding(&analyze(s.clone()), id).is_some());
    s.constraints.push(ConstraintIssue {
        schema: "shop".into(),
        table: "ticket_events".into(),
        name: "fk_ticket_events_ticket".into(),
        kind: ConstraintKind::ForeignKey,
        disabled: true,
        inheritance_child: false,
    });
    let report = analyze(s);
    assert!(finding(&report, id).is_none());
    assert!(!index_advice(&report).candidates.iter().any(|c| c.table == "ticket_events"));
}

// I14: the check that needs the index runs when a parent row is deleted; a parent that deleted nothing in ten days
// makes the finding Low and says so. A parent of a handful of rows makes the index unselective for reads.
#[test]
fn a_foreign_key_whose_parent_never_deletes_is_low() {
    let mut s = counted_for_ten_days(snapshot("postgres-demo.json"));
    let id = "unindexed-fk:public.order_items(order_id)";
    let report = analyze(s.clone());
    let f = finding(&report, id).expect("the key");
    assert_eq!((f.severity, f.confidence), (Severity::Low, Confidence::Low));
    assert!(f.summary.contains("public.orders has had no deletes in the 10 days"), "{}", f.summary);
    let advice = index_advice(&report);
    let c = advice.candidates.iter().find(|c| c.finding_id.as_deref() == Some(id)).expect("advised");
    assert!(c.reason.contains("has had no deletes"), "{}", c.reason);

    s.tables.iter_mut().find(|t| t.name == "orders").unwrap().deletes = Some(40_000.0);
    let f = analyze(s).findings.into_iter().find(|f| f.id == id).expect("the key");
    assert_eq!((f.severity, f.confidence), (Severity::Medium, Confidence::High));

    let mut s = counted_for_ten_days(snapshot("mssql-demo.json"));
    s.tables.iter_mut().find(|t| t.name == "support_tickets").unwrap().rows = Some(4.0);
    let f = analyze(s).findings.into_iter().find(|f| f.id == "unindexed-fk:shop.ticket_events(ticket_id)").unwrap();
    assert!(f.caveats.iter().any(|c| c.contains("has only 4 rows")), "{:?}", f.caveats);
}

// I15: InnoDB's PRIMARY is the clustered index: a secondary index on its leading column is narrower, not redundant.
#[test]
fn a_secondary_index_is_not_redundant_to_the_clustered_primary_key() {
    let mut s = counted_for_ten_days(snapshot("mysql-demo.json"));
    s.indexes.iter_mut().find(|i| i.table == "orders" && i.name == "PRIMARY").unwrap().columns =
        names(&["customer_id", "id"]);
    let report = analyze(s);
    assert!(!ids(&report).iter().any(|id| id.starts_with("duplicate-index:") && id.contains("fk_orders_customer")));
    assert!(
        !index_advice(&report)
            .cleanups
            .iter()
            .any(|c| c.index == "fk_orders_customer" && matches!(c.kind, CleanupKind::Prefix | CleanupKind::Duplicate))
    );
}
