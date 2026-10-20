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
    } else if s.server.engine == culprit_core::Engine::Postgres {
        // PostgreSQL's counter age is the one its scan measured, never the time since the server started.
        s.pressure.get_or_insert_with(Default::default).uptime_hours = Some(240.0);
    }
    // performance_schema's statements counted as long, or its counters may have been reset since.
    crate::counter_age::digests_counted_since_the_start(&mut s);
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

// I4a: a function around the filtered column is why the scan happens; the findings say rewrite it, and so does the
// advisor (no index of its own). A filter that also compares a plain column (`customer_id = $1 AND date_trunc(…) = $2`)
// is another case: an index on that column serves it whatever the function does, and the plan's one insight says so.
#[test]
fn the_advisor_adds_no_index_for_a_scan_a_rewrite_fixes() {
    let mut s = counted_for_ten_days(snapshot("postgres-demo.json"));
    let q = s.workload.as_mut().unwrap().queries.iter_mut().find(|q| q.id == "5308963031944094559").unwrap();
    let scan = scan_node(q).expect("the scan");
    scan.predicate = Some("(date_trunc('day'::text, orders.created_at) = $1)".into());
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

// A staging SQL Server reading "job_runs was written 5.2K times": SQL Server counts the writes that had to update each index
// (user_updates), so the number is the index's own, and said as such.
#[test]
fn sql_server_index_writes_are_said_as_the_indexs() {
    let mut s = counted_for_ten_days(snapshot("mssql-demo.json"));
    let report = analyze(s.clone());
    let f = finding(&report, "unused-index:shop.order_items").expect("unused");
    assert!(f.summary.contains("and no write has had to update it either"), "{}", f.summary);
    assert!(!f.summary.contains("hasn't been written to"), "{}", f.summary);
    s.indexes.iter_mut().find(|i| i.name == "ix_order_items_product_id").unwrap().writes = Some(5_198.0);
    let report = analyze(s);
    let f = finding(&report, "unused-index:shop.order_items").expect("unused");
    assert!(f.summary.contains("while writes had to update it 5.2K times"), "{}", f.summary);
    assert!(!f.summary.contains("was written"), "{}", f.summary);
    assert!(
        f.metrics.iter().any(|m| m.label == "Writes that updated the index" && m.value == 5_198.0),
        "{:?}",
        f.metrics
    );
}

// The postgres-demo reading: Medium for order_items' key on its million rows alone, with no statement using the key
// and no parent row deleted. Medium needs an amount: the parent's deletes, each a read of the whole child table.
#[test]
fn a_big_child_table_alone_is_not_medium() {
    let id = "unindexed-fk:public.order_items(order_id)";
    let key = |s: Snapshot| analyze(s).findings.into_iter().find(|f| f.id == id).expect("the key");
    assert_eq!(key(snapshot("postgres-demo.json")).severity, Severity::Low);
    // 50 deletes a day from orders are no amount yet.
    let mut s = counted_for_ten_days(snapshot("postgres-demo.json"));
    s.tables.iter_mut().find(|t| t.name == "orders").unwrap().deletes = Some(500.0);
    assert_eq!(key(s.clone()).severity, Severity::Low);
    // 4K a day, each reading a million rows, are, and the finding says so.
    s.tables.iter_mut().find(|t| t.name == "orders").unwrap().deletes = Some(40_000.0);
    let f = key(s);
    assert_eq!(f.severity, Severity::Medium);
    assert!(
        f.summary.contains(
            "Deletes from public.orders came to about 4.0K rows a day, each a read of the whole of public.order_items."
        ),
        "{}",
        f.summary
    );
}

// I12: a request SQL Server's optimizer last made before the window is history: no query in the window wanted it.
#[test]
fn a_request_last_made_before_the_window_is_history() {
    let mut s = snapshot("mssql-demo.json");
    s.missing_indexes.iter_mut().find(|m| m.table == "order_items").unwrap().last_used =
        Some("2026-08-01T09:00:00Z".into());
    let report = analyze(s.clone());
    let f = finding(&report, "missing-index:shop.order_items(order_id)").expect("the request");
    assert_eq!(f.actionability, culprit_core::Actionability::History);
    assert!(!f.inactive, "an old request does not prove that its associated statements stopped");
    assert_eq!(f.severity, Severity::Low);
    assert!(f.caveats[0].starts_with("SQL Server's optimizer last asked for this index on Aug 1"), "{}", f.caveats[0]);
    assert!(!index_advice(&report).candidates.iter().any(|c| c.finding_id.as_deref() == Some(f.id.as_str())));
    // Historical requests get no DDL without new evidence in the selected period.
    let fix = f.fix.as_ref().unwrap();
    assert!(fix.sql.is_none(), "{:?}", fix.sql);
    assert!(fix.summary.starts_with("Keep this candidate as context"), "{}", fix.summary);
    assert!(f.verify[0].starts_with("A later scan must show a new request"), "{:?}", f.verify);
    assert!(f.title.contains("previously requested an index"), "{}", f.title);
    assert!(f.summary.contains("does not establish that this historical request is still needed"), "{}", f.summary);

    // A small newer request on the same key is still evidence of activity, even below the candidate count threshold.
    let mut same_key = s.clone();
    let mut newer = same_key.missing_indexes.iter().find(|m| m.table == "order_items").unwrap().clone();
    newer.include.push("quantity".into());
    newer.seeks = 4.0;
    newer.scans = 0.0;
    newer.last_used = Some("2026-10-11T06:00:00Z".into());
    same_key.missing_indexes.push(newer);
    let report = analyze(same_key);
    let f = finding(&report, "missing-index:shop.order_items(order_id)").unwrap();
    assert!(!f.inactive, "a newer request on the same keys keeps it active: {:?}", f.caveats);

    // A staging SQL Server reading: requests last made just before a 2-hour window were called stopped, and the next
    // scan showed them still coming. Under a day of silence (here 6 hours before the demo's data ends, before its
    // 4.6-hour window), the finding says what the window shows and stays informational, without DDL.
    let mut recent = s.clone();
    recent.missing_indexes.iter_mut().find(|m| m.table == "order_items").unwrap().last_used =
        Some("2026-10-11T06:00:00Z".into());
    let report = analyze(recent);
    let f = finding(&report, "missing-index:shop.order_items(order_id)").expect("the request");
    assert!(!f.inactive, "{:?}", f.caveats);
    assert!(f.title.contains("previously requested an index"), "{}", f.title);
    assert_eq!(f.actionability, culprit_core::Actionability::Information);
    assert!(f.fix.as_ref().is_some_and(|x| x.sql.is_none()), "{:?}", f.fix);
    let said = f.summary.clone() + " " + &f.caveats.join(" ");
    assert!(
        said.contains("None of the last 5 hours' statements asked for it: SQL Server's optimizer last did on Oct 11 at 06:00 UTC, 6 h 14 min before this scan's data ends"),
        "{said}"
    );
    assert!(said.contains("too short a time to say the statements that wanted it stopped"), "{said}");

    // Without query history, a request nobody made in the last week is history too.
    s.workload = None;
    let report = analyze(s);
    let f = finding(&report, "missing-index:shop.order_items(order_id)").expect("the request");
    assert_eq!(f.actionability, culprit_core::Actionability::History);
    assert!(!f.inactive, "an old request does not prove that its associated statements stopped");
    assert!(f.caveats[0].contains("days before this scan's data ends"), "{}", f.caveats[0]);
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

// MS-9: no writes over less than a week of counters is no claim that the table gets none.
#[test]
fn no_writes_is_said_over_the_time_counted() {
    let cost = |hours: f64| {
        let mut s = snapshot("postgres-demo.json");
        s.pressure.get_or_insert_with(Default::default).uptime_hours = Some(hours);
        let t = s.tables.iter_mut().find(|t| t.name == "order_items").unwrap();
        (t.inserts, t.updates, t.deletes) = (Some(0.0), Some(0.0), Some(0.0));
        let report = analyze(s);
        let f = finding(&report, "unindexed-fk:public.order_items(order_id)").expect("the key");
        f.fix.as_ref().unwrap().cost.clone().unwrap_or_default()
    };
    assert_eq!(
        cost(136.0),
        "public.order_items had no writes in the 5.7 days its counters cover, too short to say it gets none (a weekly \
         job may not have run yet); each write it gets will also update this index."
    );
    let week = cost(240.0);
    assert!(week.starts_with("public.order_items received no writes in the 10"), "{week}");
    assert!(week.ends_with("so this index would cost next to nothing to maintain."), "{week}");
}

// MS-8: a scan of a table the catalog no longer lists, by a statement that stopped running: the table was dropped or
// renamed, so there is nothing to index and no SQL that would fail. A statement still running on a table the catalog
// doesn't list (an indexed view) keeps its advice.
#[test]
fn a_table_that_is_gone_gets_no_index() {
    let scan = |stopped: bool| {
        let mut s = snapshot("postgres-demo.json");
        let w = s.workload.as_mut().unwrap();
        let end = w.window_end.clone().unwrap_or_else(|| s.taken_at.clone());
        let q = w.queries.iter_mut().find(|q| q.id == "5308963031944094559").unwrap();
        if stopped {
            q.last_seen = Some("2026-01-01T00:00:00Z".into());
        } else {
            q.last_seen = Some(end);
        }
        // The plan says how many rows the table had when it ran; the catalog no longer lists it.
        scan_node(q).expect("the scan").table_rows = Some(1_999_995.0);
        s.tables.retain(|t| t.name != "orders");
        let report = analyze(s);
        report.findings.into_iter().find(|f| f.id == "filtered-scan:public.orders(customer_id)")
    };
    let gone = scan(true).expect("the scan's finding");
    let fix = gone.fix.as_ref().unwrap();
    assert!(fix.sql.is_none(), "{:?}", fix.sql);
    assert!(
        fix.summary.starts_with("Nothing to change: public.orders isn't in the database's catalog any more"),
        "{}",
        fix.summary
    );
    // MS-9: the catalog that would say which indexes it had is gone with it: its plan speaks for it, in the past.
    assert!(
        gone.summary.contains("The plan it last ran with had no index starting with customer_id to seek."),
        "{}",
        gone.summary
    );
    assert!(gone.summary.starts_with("One query read public.orders"), "{}", gone.summary);
    assert!(gone.verify[0].starts_with("Nothing to check: the tables are gone"), "{:?}", gone.verify);
    let running = scan(false).expect("the scan's finding");
    assert!(running.fix.as_ref().unwrap().sql.is_some(), "{:?}", running.fix);
}

// Rows returned under a LIMIT never establish the filter's selectivity from grouped statement totals: a sampled
// LIMIT can differ across runs. Actual operator counts can establish it, as can an uncapped single-table read.
#[test]
fn sampled_limits_do_not_turn_returned_rows_into_filter_evidence() {
    let plant = |text: &str, rows_per_run: f64, sample_limit: Option<&str>| {
        let mut s = snapshot("mysql-demo.json");
        let q = s.workload.as_mut().unwrap().queries.iter_mut().find(|q| q.id == "c7b5ac9bb4bcecfe").unwrap();
        q.text = text.into();
        q.rows = Some(rows_per_run * q.executions);
        q.logical_reads = Some(498_573.0 * q.executions);
        let plan = q.plan.as_mut().unwrap();
        if let Some(limit) = sample_limit {
            plan.props.insert(culprit_core::insights::LIMIT_ROWS.into(), limit.into());
        }
        let node = &mut plan.root;
        let node = if node.object.is_some() { node } else { &mut node.children[0] };
        node.kind = NodeKind::Scan;
        node.op = "Table scan".into();
        node.index = None;
        node.predicate = Some("(shop.orders.promo_code = 'SPRING0')".into());
        node.rows_read = Some(498_573.0);
        // MySQL's fixed 10% for an equality it has no statistics for: a guess, not an estimate.
        node.props.insert("filtered".into(), "10.00".into());
        node.est_rows = 49_857.0;
        analyze(s)
    };
    let id = "filtered-scan:shop.orders(promo_code)";
    let text = |limit: &str| {
        format!("SELECT `id` , `total` FROM `orders` WHERE `promo_code` = ? ORDER BY `total` DESC LIMIT {limit}")
    };
    let report = plant(&text("50"), 5.0, None);
    assert!(finding(&report, id).is_none(), "{:?}", ids(&report));
    let report = plant(&text("5"), 5.0, None);
    assert!(finding(&report, id).is_none(), "{:?}", ids(&report));
    let report = plant(&text("?"), 5.0, None);
    assert!(finding(&report, id).is_none(), "{:?}", ids(&report));
    // The digest's `LIMIT ?`, with one sample's count: other runs can have used LIMIT 5 and matched the whole table.
    let report = plant(&text("?"), 5.0, Some("50"));
    assert!(finding(&report, id).is_none(), "{:?}", ids(&report));

    let no_limit = "SELECT id, total FROM orders WHERE promo_code = ? ORDER BY total DESC";
    let uncapped = plant(no_limit, 5.0, None);
    assert!(finding(&uncapped, id).is_some(), "{:?}", ids(&uncapped));
    // Reopening a prepared snapshot withdraws the old inferred mark once the evidence no longer supports it.
    let mut saved = uncapped.snapshot;
    saved.workload.as_mut().unwrap().queries.iter_mut().find(|q| q.id == "c7b5ac9bb4bcecfe").unwrap().text = text("?");
    let report = analyze(saved);
    assert!(finding(&report, id).is_none(), "{:?}", ids(&report));

    let mut actual = plant(&text("?"), 5.0, Some("50")).snapshot;
    let q = actual.workload.as_mut().unwrap().queries.iter_mut().find(|q| q.id == "c7b5ac9bb4bcecfe").unwrap();
    let plan = q.plan.as_mut().unwrap();
    plan.kind = culprit_core::PlanKind::Actual;
    let node = &mut plan.root;
    let scan = if node.object.is_some() { node } else { &mut node.children[0] };
    scan.act_rows = Some(5.0);
    scan.act_executions = Some(1.0);
    let report = analyze(actual);
    assert!(finding(&report, id).is_some(), "{:?}", ids(&report));
}

// MS-9: filtered-scan on a table that was dropped said "No index started with sensor, so the database had no faster
// way in", where Query Store held the CREATE INDEX on sensor and the fast plan read 3 pages a run through it; and it
// mixed tenses. A gone table has no catalog to say which indexes it had: its plan speaks for it, in the past, and an
// index the workload shows created and the catalog doesn't hold was dropped.
#[test]
fn a_scan_of_a_table_that_is_gone_is_told_from_its_plan_in_the_past() {
    let plant = |gone: bool| {
        let mut s = snapshot("postgres-demo.json");
        let w = s.workload.as_mut().unwrap();
        let id = "-3722457391793582113";
        let mut create = w.queries.iter().find(|q| q.id == id).cloned().expect("a scan of order_items");
        (create.id, create.plan, create.executions) = ("create".into(), None, 1.0);
        create.text = "CREATE INDEX ix_items_product ON public.order_items (product_id)".into();
        create.first_seen = Some("2026-10-11T08:00:00Z".into());
        w.queries.push(create);
        if gone {
            let q = w.queries.iter_mut().find(|q| q.id == id).unwrap();
            q.executions = 1.0;
            scan_node(q).unwrap().table_rows = Some(1_000_000.0);
            s.tables.retain(|t| t.name != "order_items");
            s.indexes.retain(|i| i.table != "order_items");
        }
        let report = analyze(s);
        finding(&report, "filtered-scan:public.order_items(product_id)").expect("the scan").summary.clone()
    };
    let created = "An index on product_id did exist: pg_stat_statements recorded `CREATE INDEX ix_items_product ON \
                   public.order_items (product_id)` on Oct 11 at 08:00 UTC";
    let here = plant(false);
    assert!(
        here.contains(&format!(
            "{created}, and the catalog doesn't list ix_items_product now, so it was dropped or renamed since."
        )),
        "{here}"
    );
    let gone = plant(true);
    assert!(gone.contains("One query read public.order_items and kept only"), "{gone}");
    assert!(
        gone.contains("The plan it last ran with had no index starting with product_id to seek. It accounted for"),
        "{gone}"
    );
    assert!(gone.contains(&format!("isn't in the database's catalog any more. {created}.")), "{gone}");
    assert!(!gone.contains("No index start") && !gone.contains("plans to read"), "{gone}");
}

// MS-5: unindexed-fk:public.order_items(order_id) listed a join whose plan hashed order_items whole and never read it
// by the key (the build side was empty: the 91 ms were the orders scan). A statement is listed with an index finding
// only where its plan, when captured, shows the index serving it; without a captured plan it stays as evidence.
#[test]
fn a_join_whose_plan_never_reads_the_child_by_the_key_is_not_the_foreign_keys() {
    let plant = |with_plan: bool| {
        let mut s = snapshot("postgres-demo.json");
        let w = s.workload.as_mut().unwrap();
        let mut q = w.queries.iter().find(|q| q.id == "-3722457391793582113").cloned().expect("a scan of order_items");
        let scan = scan_node(&mut q).cloned().expect("its scan");
        let node = |op: &str, kind: NodeKind, object: Option<&str>, predicate: Option<&str>, children| PlanNode {
            op: op.into(),
            kind,
            object: object.map(String::from),
            predicate: predicate.map(String::from),
            children,
            ..scan.clone()
        };
        let orders = node("Seq Scan", NodeKind::Scan, Some("public.orders"), Some("(o.note ~~ $1)"), vec![]);
        let items = node("Seq Scan", NodeKind::Scan, Some("public.order_items"), None, vec![]);
        let hash = node("Hash", NodeKind::Other, None, None, vec![orders]);
        let join = node("Hash Join", NodeKind::Join, None, Some("(i.order_id = o.id)"), vec![items, hash]);
        q.id = "planted-join".into();
        q.text = "SELECT count(*) FROM orders o JOIN order_items i ON i.order_id = o.id WHERE o.note LIKE $1".into();
        if with_plan {
            q.plan.as_mut().unwrap().root = join;
        } else {
            q.plan = None;
        }
        w.queries.push(q);
        let report = analyze(s);
        let listed = finding(&report, "unindexed-fk:public.order_items(order_id)")
            .expect("the unindexed foreign key")
            .queries
            .iter()
            .any(|r| r.id == "planted-join");
        let advised = index_advice(&report)
            .candidates
            .iter()
            .filter(|c| c.source == CandidateSource::ForeignKey)
            .any(|c| c.queries.iter().any(|a| a.id == "planted-join"));
        (listed, advised)
    };
    assert_eq!(plant(true), (false, false), "its plan reads order_items whole as a hash join's side, not by the key");
    assert_eq!(plant(false), (true, true), "without a plan, a join on the key is evidence the key is used");
}

// MS-1: a restart empties SQL Server's list of missing-index requests, and the plan's own request is then all that is
// left of the planted shipped_at index. It is a finding of its own unless a missing-index or filtered-scan finding is
// about the same table and key, which then takes its statement.
#[test]
fn a_plans_own_index_request_is_never_hidden() {
    let hint_finding = "plan-insights:missing-index-hint:shop.orders";
    // With the server's list, the request is the missing-index finding's.
    let report = analyze(snapshot("mssql-demo.json"));
    assert!(finding(&report, hint_finding).is_none());
    let request = finding(&report, "missing-index:shop.orders(shipped_at)").expect("the server's request");
    assert!(request.queries.iter().any(|r| r.id == "8"));
    // Without it, the scan behind it (a range on a function of parameters and constants) is the finding.
    let mut s = snapshot("mssql-demo.json");
    s.missing_indexes.clear();
    let report = analyze(s.clone());
    assert!(finding(&report, hint_finding).is_none());
    let scan = finding(&report, "filtered-scan:shop.orders(shipped_at)").expect("the scan the plan's request is about");
    assert!(scan.queries.iter().any(|r| r.id == "8"));
    assert!(sql(scan).contains("INCLUDE ([total])"), "the plan's covering columns survive: {}", sql(scan));
    // And with neither, the plan's own request is.
    let plan = s
        .workload
        .as_mut()
        .and_then(|w| w.queries.iter_mut().find(|q| q.id == "8"))
        .and_then(|q| q.plan.as_mut())
        .expect("query 8's plan");
    fn unfilter(n: &mut PlanNode) {
        n.predicate = None;
        n.children.iter_mut().for_each(unfilter);
    }
    unfilter(&mut plan.root);
    let report = analyze(s);
    let own = finding(&report, hint_finding).expect("the plan's own request, shown");
    assert!(own.title.contains("shipped_at"), "{}", own.title);
    assert!(own.fix.as_ref().and_then(|f| f.sql.as_deref()).is_some_and(|sql| sql.contains("[shipped_at]")));
}
