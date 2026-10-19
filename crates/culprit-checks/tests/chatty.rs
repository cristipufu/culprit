//! Frequent tiny statements: a poll that finds nothing, a write repeated row by row, a read inside a loop. MySQL counts
//! the rows a write changed apart from rows returned, and returns none for a write, so a busy INSERT must not read as a
//! poll.

use culprit_checks::analyze;
use culprit_core::Snapshot;

fn snapshot() -> Snapshot {
    let text = std::fs::read_to_string(format!("{}/../../fixtures/mysql-demo.json", env!("CARGO_MANIFEST_DIR")))
        .expect("fixture");
    let mut s: Snapshot = serde_json::from_str(&text).expect("snapshot");
    // Two hours of statement history: enough for a rate. The statements run over all of it, not the fixture's two
    // minutes, which in two hours would be a burst.
    let workload = s.workload.as_mut().expect("workload");
    workload.window_hours = 2.0;
    for q in &mut workload.queries {
        (q.first_seen, q.last_seen) = (None, None);
    }
    s
}

/// The demo's first statement, made a single-row INSERT run 30K times in the window.
fn with_insert(rows_sent: f64, rows_affected: Option<f64>) -> Snapshot {
    let mut s = snapshot();
    let workload = s.workload.as_mut().expect("workload");
    let q = &mut workload.queries[0];
    q.text = "INSERT INTO `shop`.`events` (`kind`, `at`) VALUES (?, NOW())".into();
    q.executions = 30_000.0;
    q.total_time_ms = 30_000.0 * 0.4;
    q.mean_time_ms = 0.4;
    q.rows = Some(rows_sent);
    q.rows_affected = rows_affected;
    workload.total_executions = workload.queries.iter().map(|q| q.executions).sum();
    s
}

/// The chatty finding about the demo's first statement (the one these tests make an INSERT).
fn chatty_title(s: Snapshot) -> Option<String> {
    let id = s.workload.as_ref().expect("workload").queries[0].id.clone();
    analyze(s).findings.into_iter().find(|f| f.id == format!("chatty-query:{id}")).map(|f| f.title)
}

#[test]
fn a_busy_insert_is_a_row_by_row_write_not_a_poll() {
    let title = chatty_title(with_insert(0.0, Some(30_000.0))).expect("a chatty finding");
    assert!(title.contains("one row each time"), "{title}");
    assert!(!title.contains("find nothing"), "{title}");
}

#[test]
fn without_rows_affected_a_write_is_judged_by_what_the_engine_counted() {
    // Engines that count a write's rows as `rows` (SQL Server, PostgreSQL) leave rows_affected empty.
    let title = chatty_title(with_insert(30_000.0, None)).expect("a chatty finding");
    assert!(title.contains("one row each time"), "{title}");
}

#[test]
fn a_count_over_a_short_window_counts_when_it_already_passes_the_daily_bar() {
    // One minute of history: 30K runs are already more than the 20K a day the check asks for, with nothing stretched;
    // 4K runs in a minute would only reach it by extrapolating to a day.
    let busy = |runs: f64| {
        let mut s = with_insert(0.0, Some(runs));
        let workload = s.workload.as_mut().expect("workload");
        workload.window_hours = 1.0 / 60.0;
        workload.queries[0].executions = runs;
        workload.queries[0].total_time_ms = runs * 0.4;
        workload.total_executions = workload.queries.iter().map(|q| q.executions).sum();
        chatty_title(s).is_some()
    };
    assert!(busy(30_000.0));
    assert!(!busy(4_000.0));
}

#[test]
fn a_read_in_a_loop_costs_round_trips_and_is_verified_over_the_window_it_was_counted_in() {
    // 30K single-row reads in two minutes: a pooled call costs a round trip, not a connection, and two minutes of
    // counters say nothing about a day.
    let mut s = snapshot();
    let workload = s.workload.as_mut().expect("workload");
    workload.window_hours = 2.0 / 60.0;
    let q = &mut workload.queries[0];
    q.text = "SELECT `name`, `price` FROM `shop`.`products` WHERE `id` = ?".into();
    (q.executions, q.total_time_ms, q.mean_time_ms) = (30_000.0, 30_000.0 * 0.4, 0.4);
    (q.rows, q.rows_affected) = (Some(30_000.0), None);
    let id = q.id.clone();
    workload.total_executions = workload.queries.iter().map(|q| q.executions).sum();
    let f = analyze(s).findings.into_iter().find(|f| f.id == format!("chatty-query:{id}")).expect("a chatty read");
    assert!(f.title.ends_with("runs 30K times in 2 minutes, one key at a time"), "{}", f.title);
    assert!(f.summary.contains("Its runs are "), "{}", f.summary);
    assert!(f.summary.contains(" of all statement runs, and each run returns about 1 row."), "{}", f.summary);
    assert!(f.summary.contains("a network round trip.") && !f.summary.contains("connection"), "{}", f.summary);
    // performance_schema keeps running totals: the count a later scan reads still holds every run from before.
    let verify = f.verify.join(" ");
    assert_eq!(
        verify,
        "It ran 30K times in 2 minutes. performance_schema keeps running totals, so a later scan's runs, share and averages for this query still hold every run from before the change. Scan when the change ships and again a day or more later, then compare the two in What changed, with Since at the ship time (`culprit changes <connection> --since <ship time>`): there it should run fewer times and take less time in all. A rewritten statement is a new entry with an id of its own, and this one then shows as stopped running."
    );
    assert!(!verify.contains("per day"), "{verify}");
}

// The MySQL demo's fresh reading: "runs 4K times in 6 seconds, one row at a time" for runs that return about 1.6 rows
// each, and "8% of every statement the database executes" for a share of the statements' runs.
#[test]
fn a_chatty_title_says_one_key_a_call_and_the_rows_each_run_handles() {
    let mut s = snapshot();
    let workload = s.workload.as_mut().expect("workload");
    let q = &mut workload.queries[0];
    q.text = "SELECT `id`, `total` FROM `shop`.`orders` WHERE `customer_id` = ?".into();
    (q.executions, q.total_time_ms, q.mean_time_ms) = (30_000.0, 30_000.0 * 0.4, 0.4);
    (q.rows, q.rows_affected) = (Some(48_000.0), None);
    let id = q.id.clone();
    workload.total_executions = workload.queries.iter().map(|q| q.executions).sum();
    let f = analyze(s).findings.into_iter().find(|f| f.id == format!("chatty-query:{id}")).expect("a chatty read");
    assert!(f.title.ends_with(", one key at a time"), "{}", f.title);
    // In the tense the statement's history says (it may have stopped in the fixture's window).
    assert!(f.summary.contains(" of all statement runs, and each run ret"), "{}", f.summary);
    assert!(f.summary.contains(" about 1.6 rows."), "{}", f.summary);
    assert!(!f.summary.contains("every statement the database executes"), "{}", f.summary);
    // A write of 1.6 rows a run isn't "one row each time".
    let f = chatty_write(
        snapshot(),
        "c7b5ac9bb4bcecfe",
        "UPDATE `shop` . `sessions` SET `seen_at` = NOW ( ) WHERE `user_id` = ?",
        Some(48_000.0),
    );
    assert!(f.title.ends_with(", about 1.6 rows each time"), "{}", f.title);
    let f = chatty_write(
        snapshot(),
        "c7b5ac9bb4bcecfe",
        "UPDATE `shop` . `sessions` SET `seen_at` = NOW ( ) WHERE `id` = ?",
        Some(30_000.0),
    );
    assert!(f.title.ends_with(", one row each time"), "{}", f.title);
}

/// The chatty finding about `text`, made the statement of the fixture's query `id`: a single-row write run 30K times
/// in the window.
fn chatty_write(mut s: Snapshot, id: &str, text: &str, rows_affected: Option<f64>) -> culprit_core::Finding {
    let workload = s.workload.as_mut().expect("workload");
    let q = workload.queries.iter_mut().find(|q| q.id == id).expect("query");
    q.text = text.into();
    (q.executions, q.total_time_ms, q.mean_time_ms) = (30_000.0, 30_000.0 * 0.13, 0.13);
    (q.rows, q.rows_affected, q.plan) = (Some(30_000.0), rows_affected, None);
    workload.total_executions = workload.queries.iter().map(|q| q.executions).sum();
    let id = format!("chatty-query:{id}");
    analyze(s).findings.into_iter().find(|f| f.id == id).expect("a chatty write")
}

fn mssql() -> Snapshot {
    let text = std::fs::read_to_string(format!("{}/../../fixtures/mssql-demo.json", env!("CARGO_MANIFEST_DIR")))
        .expect("fixture");
    serde_json::from_str(&text).expect("snapshot")
}

#[test]
fn a_compound_counter_preserves_its_other_work_and_results() {
    // A conversation's next message number: one row locked and updated per message, not a loop writing row by row.
    let f = chatty_write(
        mssql(),
        "7",
        "(@par0 nvarchar(4000))update [app].[conversations] set [last_seq] = [app].[conversations].[last_seq] + 1, [updated_at] = SYSUTCDATETIME() output INSERTED.[last_seq], INSERTED.[status] where ([app].[conversations].[id] = @par0)",
        None,
    );
    assert!(
        f.summary.contains("adds to last_seq in one row of conversations and returns the new value"),
        "{}",
        f.summary
    );
    assert!(!f.summary.contains("row by row"), "{}", f.summary);
    let fix = f.fix.as_ref().unwrap().summary.clone();
    assert!(fix.contains("preserve every assignment, returned value, row filter and transaction guarantee"), "{fix}");
    assert!(!fix.contains("in the INSERT") && !fix.contains("NEXT VALUE FOR"));
    assert!(!fix.contains("multi-row VALUES") && !fix.contains("bulk insert"), "{fix}");
    assert!(!f.caveats.iter().any(|c| c.contains("lookups")), "{:?}", f.caveats);
    assert!(f.verify[0].contains("numbering, metadata updates and returned values remain correct"));
    // MySQL's counter hands its number back through LAST_INSERT_ID: no sequences there.
    let f = chatty_write(
        snapshot(),
        "c7b5ac9bb4bcecfe",
        "UPDATE `shop` . `counters` SET `n` = LAST_INSERT_ID ( `n` + ? ) WHERE `name` = ?",
        Some(30_000.0),
    );
    let fix = f.fix.unwrap().summary;
    assert!(fix.contains("an AUTO_INCREMENT column") && !fix.contains("SEQUENCE"), "{fix}");
}

#[test]
fn a_single_row_update_is_batched_by_its_keys_not_by_a_bulk_insert() {
    let f = chatty_write(
        snapshot(),
        "c7b5ac9bb4bcecfe",
        "UPDATE `shop` . `sessions` SET `seen_at` = NOW ( ) WHERE `id` = ?",
        Some(30_000.0),
    );
    assert!(
        f.summary.contains("independent requests, row-by-row loops, heartbeats or status changes"),
        "{}",
        f.summary
    );
    assert!(f.summary.contains("counters do not show whether calls already share a transaction"));
    assert!(!f.summary.contains("each one is a round trip and a log flush"));
    let fix = f.fix.as_ref().unwrap().summary.clone();
    assert!(fix.contains("an UPDATE over a list of keys") && !fix.contains("multi-row VALUES"), "{fix}");
    assert!(f.caveats.iter().any(|c| c.contains("single-row updates")), "{:?}", f.caveats);
    // An INSERT keeps the multi-row advice, in MySQL's terms.
    let f = chatty_write(
        snapshot(),
        "c7b5ac9bb4bcecfe",
        "INSERT INTO `shop` . `events` ( `kind` ) VALUES ( ? )",
        Some(30_000.0),
    );
    let fix = f.fix.unwrap().summary;
    assert!(fix.contains("a multi-row VALUES, or LOAD DATA") && !fix.contains("table-valued"), "{fix}");
}

// A staging SQL Server reading: "the row stays locked until its transaction commits, so callers that need the next number of the
// same row wait for each other (4% of database time in all)" read as 4% of database time spent waiting on the
// counter's row, where Query Store measured 1 ms of lock waits: the 4% is the statement's own time.
#[test]
fn a_counter_says_its_share_apart_from_the_lock_waits_measured() {
    let counter = "(@par0 nvarchar(4000))update [app].[conversations] set [last_seq] = [app].[conversations].[last_seq] + 1 output INSERTED.[last_seq] where ([app].[conversations].[id] = @par0)";
    let mut s = mssql();
    let q = s.workload.as_mut().unwrap().queries.iter_mut().find(|q| q.id == "7").expect("query");
    q.waits = vec![
        culprit_core::WaitStat { category: "Lock".into(), wait_type: Some("Lock".into()), wait_ms: 1.0, tasks: None },
        culprit_core::WaitStat {
            category: "IO".into(),
            wait_type: Some("Buffer IO".into()),
            wait_ms: 179.0,
            tasks: None,
        },
    ];
    let f = chatty_write(s, "7", counter, None);
    assert!(
        f.summary
            .contains("so callers that need the next number of the same row can wait for each other. In all it takes "),
        "{}",
        f.summary
    );
    assert!(
        f.summary.ends_with(" of database time, and Query Store recorded 1 ms of lock waits out of its 3.9 s."),
        "{}",
        f.summary
    );
    assert!(!f.summary.contains("in all)"), "{}", f.summary);
    // Waits recorded, none on locks: none measured.
    let mut s = mssql();
    let q = s.workload.as_mut().unwrap().queries.iter_mut().find(|q| q.id == "7").expect("query");
    q.waits = vec![culprit_core::WaitStat { category: "IO".into(), wait_type: None, wait_ms: 179.0, tasks: None }];
    let f = chatty_write(s, "7", counter, None);
    assert!(f.summary.ends_with("Query Store recorded 0 ms of lock waits out of its 3.9 s."), "{}", f.summary);
    // No waits kept for the statement: only its share.
    let mut s = mssql();
    s.workload.as_mut().unwrap().queries.iter_mut().for_each(|q| q.waits.clear());
    let f = chatty_write(s, "7", counter, None);
    assert!(f.summary.ends_with(" of database time.") && !f.summary.contains("lock waits"), "{}", f.summary);
}
