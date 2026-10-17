//! MariaDB rides on the MySQL engine: the same checks, in MariaDB's words and syntax, on the MariaDB demo scan.

use culprit_checks::{analyze, index_advice};
use culprit_core::{Capability, Finding, Report, Setting, Severity, Snapshot, WaitStat};

fn snapshot() -> Snapshot {
    let path = format!("{}/../../fixtures/mariadb-demo.report.json", env!("CARGO_MANIFEST_DIR"));
    let report: Report = serde_json::from_str(&std::fs::read_to_string(path).expect("fixture")).expect("report");
    report.snapshot
}

fn find<'a>(report: &'a Report, id: &str) -> Option<&'a Finding> {
    report.findings.iter().find(|f| f.id == id)
}

fn set(s: &mut Snapshot, name: &str, value: &str) {
    s.settings.retain(|x| x.name != name);
    s.settings.push(Setting { name: name.into(), value: value.into(), default_value: None, scope: "server".into() });
}

#[test]
fn findings_use_mariadb_words_and_syntax() {
    let mut s = snapshot();
    assert_eq!(s.server.name(), "MariaDB");
    // The demo's counters are an hour old; an index is only called unused after a week of them.
    s.server.started_at = Some("2026-10-01T12:00:00Z".into());
    if let Some(p) = s.pressure.as_mut() {
        p.uptime_hours = Some(240.0);
    }
    crate::counter_age::digests_counted_since_the_start(&mut s);
    // MariaDB estimates that a filter on an unindexed column keeps every row. The demo's statement ends in `LIMIT ?`,
    // so the rows it returns say nothing about how many the filter kept: no evidence, no index.
    let scan = "filtered-scan:shop.orders(promo_code)";
    assert!(!analyze(s.clone()).findings.iter().any(|f| f.id == scan));
    // The same statement returning every row it keeps: rows examined against rows returned say the filter keeps few,
    // so the scan is found.
    let q = s.workload.as_mut().unwrap().queries.iter_mut().find(|q| q.text.contains("promo_code")).unwrap();
    q.text = q.text.split(" ORDER BY").next().unwrap().to_string();
    let report = analyze(s);
    assert!(report.findings.iter().any(|f| f.id == scan));
    let unused = report.findings.iter().find(|f| f.id.starts_with("unused-index:")).expect("unused index");
    let sql = unused.fix.as_ref().and_then(|f| f.sql.as_deref()).unwrap_or_default();
    assert!(sql.contains("IGNORED") && !sql.contains("INVISIBLE"), "{sql}");
    let advice = index_advice(&report);
    assert!(advice.notes.iter().any(|n| n.starts_with("MariaDB doesn't record missing-index requests")));
}

// The saved MariaDB demo: "80 times in the hour since the server last restarted" and "no writes in the 1 h 9 min its
// counters cover", where the setup truncated performance_schema's table counters with its statements 68 minutes after
// the start. Counters that may have started over are named for what they are, with no length and no rate a day.
#[test]
fn performance_schema_counters_reset_with_the_statements_are_named_so() {
    let report = analyze(snapshot());
    let f = find(&report, "scan-heavy:shop.orders").expect("scan-heavy");
    let since = "80 times since the server started or its performance_schema counters were last reset";
    assert!(f.title.contains(since) && f.summary.contains(since), "{}\n{}", f.title, f.summary);
    assert!(!f.summary.contains("restarted") && !f.summary.contains("a day"), "{}", f.summary);
    let cost = f.fix.as_ref().and_then(|x| x.cost.as_deref()).unwrap_or_default();
    assert!(cost.contains("in what its counters cover, which may be short") && !cost.contains(" min "), "{cost}");
    // A server whose statements' counters began with it is dated by its start.
    let mut s = snapshot();
    s.server.started_at = s.workload.as_ref().and_then(|w| w.window_start.clone());
    let report = analyze(s);
    let f = find(&report, "scan-heavy:shop.orders").expect("scan-heavy");
    assert!(f.title.contains("since the server last restarted"), "{}", f.title);
}

#[test]
fn without_performance_schema_the_slow_log_is_offered_first() {
    let mut s = snapshot();
    set(&mut s, "performance_schema", "OFF");
    set(&mut s, "slow_query_log", "OFF");
    set(&mut s, "log_output", "FILE");
    set(&mut s, "userstat", "OFF");
    set(&mut s, "long_query_time", "10.000000");
    s.server.capabilities.retain(|c| *c != Capability::PerformanceSchema);
    s.workload = None;
    let report = analyze(s);
    let f = report.findings.iter().find(|f| f.id == "config:performance_schema").expect("finding");
    assert_eq!(f.severity, Severity::High);
    assert!(f.summary.starts_with("Without it MariaDB keeps"), "{}", f.summary);
    let sql = f.fix.as_ref().and_then(|x| x.sql.as_deref()).unwrap_or_default();
    // One statement per setting, worded by setting_change (a managed MariaDB gets its parameter group instead).
    assert!(
        sql.contains(
            "SET GLOBAL userstat = 'ON';\nSET GLOBAL slow_query_log = 'ON';\nSET GLOBAL log_output = 'FILE,TABLE';\nSET GLOBAL long_query_time = 0.1;"
        ),
        "{sql}"
    );
    assert!(
        !report.findings.iter().any(|f| f.id == "config:slow_query_log"),
        "the performance_schema finding covers it"
    );

    // Once the slow log feeds the workload, performance_schema is a gap, not a blind spot.
    let mut logged = snapshot();
    set(&mut logged, "performance_schema", "OFF");
    logged.server.capabilities.retain(|c| *c != Capability::PerformanceSchema);
    if let Some(w) = logged.workload.as_mut() {
        w.source = "slow query log".into();
        w.windowed = true;
    }
    let report = analyze(logged);
    let f = report.findings.iter().find(|f| f.id == "config:performance_schema").expect("finding");
    assert_eq!(f.severity, Severity::Medium);
    assert!(f.summary.contains("Culprit reads the slow query log"), "{}", f.summary);
}

// The saved demo: lock-time:375c "spends nearly all its time waiting on locks, 2.5 s per run" for a GROUP BY that reads
// a million rows a run (49,963.3 ms of lock in 49,963.9). Its slow log writes to a table (log_output FILE,TABLE,
// long_query_time 20 ms), and MariaDB then counts a logged statement's whole run as its lock time: a scan with the slow
// log off showed 9.6 ms. row-lock-waits named the same plain SELECT, which takes no InnoDB row locks, as the statement
// that waited longest, and said "3 min" for 152 s.
/// The demo scan as it was read, with the lock time MariaDB reported for its two logged statements. The saved report
/// holds the snapshot its analysis prepared, where that lock time is unknown (`pipeline.rs`
/// `lock_time_that_counts_the_run`): what the scan recorded is put back to test the rule on it.
fn as_scanned() -> Snapshot {
    let mut s = snapshot();
    for q in s.workload.as_mut().unwrap().queries.iter_mut() {
        let lock = match q.id.as_str() {
            "375c64398248650e" => 49_963.346,
            "258e3c9b580f1ecc" => 2_251.482,
            _ => continue,
        };
        q.spread.as_mut().expect("the slow log's spread").lock_ms = Some(lock);
        q.waits.push(WaitStat {
            category: "Lock".into(),
            wait_type: Some("lock time".into()),
            wait_ms: lock,
            tasks: None,
        });
    }
    s
}

#[test]
fn lock_time_mariadb_makes_of_a_logged_run_is_not_waiting() {
    let report = analyze(as_scanned());
    assert!(
        report
            .snapshot
            .notes
            .iter()
            .any(|n| n.message.contains("the lock time of 2 statements is left out as unknown")),
        "{:?}",
        report.snapshot.notes
    );
    let locks: Vec<&str> = report.findings.iter().filter(|f| f.check == "lock-time").map(|f| f.id.as_str()).collect();
    assert!(locks.is_empty(), "{locks:?}");
    let waits = find(&report, "row-lock-waits:server").expect("finding");
    assert!(waits.title.starts_with("Transactions on this server waited 2 min 32 s for row locks"), "{}", waits.title);
    assert!(waits.summary.contains("20 times, 2 min 32 s in all"), "{}", waits.summary);
    assert!(!waits.summary.contains("waited longest") && waits.queries.is_empty(), "{}", waits.summary);

    // Logged to a file only, the lock time is the statement's own; a plain SELECT still took no row locks, and a
    // write that waited is the one named.
    let mut file = as_scanned();
    set(&mut file, "log_output", "FILE");
    let report = analyze(file.clone());
    assert!(find(&report, "lock-time:375c64398248650e").is_some());
    assert!(!find(&report, "row-lock-waits:server").unwrap().summary.contains("waited longest"));
    let q = file.workload.as_mut().unwrap().queries.iter_mut().find(|q| q.id == "375c64398248650e").unwrap();
    q.text = "UPDATE `orders` SET `total` = `total` + ? WHERE `customer_id` = ?".into();
    q.plan = None;
    let waits = find(&analyze(file), "row-lock-waits:server").cloned().unwrap();
    assert!(
        waits.summary.contains("In this database, an update of orders waited longest on locks"),
        "{}",
        waits.summary
    );
}

// The saved demo: scan-heavy:shop.orders counted the GROUP BY over every row (375c, no WHERE: temp-spill's) with the
// statements whose filter has no index, at "93% of DB time", told it to "index the columns in their WHERE clause", and
// said "explain the statements below" where the statements are listed above the fix.
#[test]
fn a_statement_with_no_where_is_not_told_to_index_its_filter() {
    let report = analyze(snapshot());
    let f = find(&report, "scan-heavy:shop.orders").expect("finding");
    assert_eq!(f.queries.iter().map(|q| q.id.as_str()).collect::<Vec<_>>(), vec!["258e3c9b580f1ecc"]);
    assert!(
        f.summary.ends_with(
            "1 of the captured statements reads it in full (its plan scans it); explain it to check the access path, \
             then measure rows kept in representative runs. A read from orders (query 375c64398248650e) reads it in full too, on purpose: it has no WHERE \
             clause, so no index on a filter changes that."
        ),
        "{}",
        f.summary
    );
    assert!(f.impact.db_time_share.unwrap_or(0.0) < 0.1, "{:?}", f.impact.db_time_share);
    let fix = f.fix.as_ref().unwrap();
    assert!(fix.summary.starts_with("Explain the statements listed with this finding to check"), "{}", fix.summary);
    // Read in full only by it, as often as the counters count: nothing for an index on a filter to remove.
    let mut s = snapshot();
    let w = s.workload.as_mut().unwrap();
    w.queries.retain(|q| q.id != "258e3c9b580f1ecc");
    w.queries.iter_mut().find(|q| q.id == "375c64398248650e").unwrap().executions = 80.0;
    assert!(find(&analyze(s), "scan-heavy:shop.orders").is_none());
}

// MY-3: a scan with no captured plans (MariaDB's fresh demo) counted every SELECT that names the table as maybe reading
// it in full, and told the reader to index columns indexes already lead with. MySQL counts the rows each statement
// examined: about the whole table a run reads it in full (and counts in the share); a sliver of it is an index's work.
#[test]
fn rows_examined_say_which_statements_read_the_table_in_full() {
    let mut s = snapshot();
    for q in s.workload.as_mut().unwrap().queries.iter_mut() {
        q.plan = None;
    }
    let report = analyze(s);
    let f = find(&report, "scan-heavy:shop.orders").expect("finding");
    // 258e examines 500K of 499K rows a run; decf 11K (2%) and 46db 5: through an index.
    assert_eq!(f.queries.iter().map(|q| q.id.as_str()).collect::<Vec<_>>(), vec!["258e3c9b580f1ecc"]);
    assert!(
        f.summary.contains(
            "1 of the captured statements reads it in full (it examines 500K rows a run, about the whole table); \
             explain it to check the access path, then measure rows kept in representative runs."
        ),
        "{}",
        f.summary
    );
    assert!(!f.summary.contains("without a captured plan"), "{}", f.summary);
    // Its time is the scans' cost.
    assert!(f.impact.db_time_share.is_some_and(|s| s > 0.0), "{:?}", f.impact.db_time_share);
    // No SQL creates an index: the cost line names the one the reader would add (MY-6).
    let cost = f.fix.as_ref().and_then(|x| x.cost.as_deref()).unwrap_or_default();
    assert!(!cost.contains("this index") && cost.contains("any index you add"), "{cost}");
}

#[test]
fn a_full_read_does_not_prove_that_an_index_would_help() {
    for planned in [true, false] {
        let mut s = snapshot();
        if !planned {
            for q in s.workload.as_mut().unwrap().queries.iter_mut() {
                q.plan = None;
            }
        }
        // The captured plan's filter keeps every row by its estimate, and the parameterized LIMIT prevents the
        // statement's rows returned from proving otherwise. Without the plan, all we know is a full read too.
        let report = analyze(s);
        assert!(find(&report, "filtered-scan:shop.orders(promo_code)").is_none());
        let f = find(&report, "scan-heavy:shop.orders").expect("full reads still need a look");
        let fix = f.fix.as_ref().unwrap();
        assert!(fix.sql.is_none());
        assert!(
            fix.summary.contains("capture an actual plan with representative values in a controlled environment")
                && fix.summary.contains("this runs the statement")
                && fix.summary.contains("If a filter keeps at most 10%")
                && fix.summary.contains("if it keeps more, read fewer rows or columns"),
            "{}",
            fix.summary
        );
        assert!(f.verify[0].starts_with("After adding an index for a selective filter"), "{:?}", f.verify);
        assert!(f.verify[1].contains("Whether the fix adds an index or reads fewer rows or columns"), "{:?}", f.verify);
        assert!(
            f.verify[1].contains("What changed") && f.verify[1].contains("time per run should drop"),
            "{:?}",
            f.verify
        );
    }
}

// InnoDB keeps only its most recent deadlock: "deadlocked once in the last day" counted what it doesn't count.
#[test]
fn innodbs_latest_deadlock_is_at_least_one() {
    let report = analyze(snapshot());
    let f = report.findings.iter().find(|f| f.check == "deadlocks").expect("finding");
    assert_eq!(
        f.title,
        "Two queries deadlocked at least once, most recently on Oct 11 at 16:05 UTC (opposite access order on shop.customers and shop.orders)"
    );
}

// MY-8: the deadlock's brief pointed at one of its two tables, and "Rolled back: root" named a login as if it were the
// victim itself.
#[test]
fn a_deadlocks_brief_names_both_tables_and_its_victim_is_a_session() {
    let report = analyze(snapshot());
    let f = report.findings.iter().find(|f| f.check == "deadlocks").expect("finding");
    let victim = f.metrics.iter().find(|m| m.label == "Rolled back").and_then(|m| m.text.as_deref());
    assert_eq!(victim, Some("session 202 (login root)"));
    let brief = culprit_checks::agent_brief(&report, f);
    let code = brief.split("## What to do in the codebase").nth(1).expect("codebase section");
    assert!(
        code.contains(
            "The statements to look for are quoted under *What's happening*: find the code that sends them (search \
             for `shop.customers` and `shop.orders`) and make the change the fix describes there."
        ),
        "{code}"
    );
}
