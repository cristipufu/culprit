use super::predicates::is_value;
use super::*;

/// A name a database allows (a quoted identifier with a line break in it) can't end a comment of fix SQL and run the
/// rest of itself as a statement.
#[test]
fn a_name_in_a_comment_stays_in_the_comment() {
    for engine in [Engine::Mssql, Engine::Postgres, Engine::Mysql, Engine::Sqlite] {
        let hostile = "orders\nDROP TABLE users;\r\n--";
        let line = format!("-- read {}", comment_text(&qualified(engine, "app", hostile)));
        assert_eq!(line.lines().count(), 1, "{engine:?}: {line:?}");
        assert!(line.starts_with("-- read "), "{line}");
    }
    assert_eq!(comment_text("a\u{2028}b\u{85}c\u{2029}d\te"), "a b c d e");
    assert_eq!(comment_text("plain [app].[orders]"), "plain [app].[orders]");
}

/// Names and statements come from the database in any script: every text helper slices only at character
/// boundaries, so none of them can panic on a scan.
#[test]
fn text_helpers_never_panic_on_non_ascii() {
    let texts = [
        "SELECT * FROM [dbo].[Bestellungen_Übersicht] AS b JOIN İstanbul_şube s ON s.id = b.id WHERE b.ñ = N'ç' AND b.x::text LIKE 'é%'",
        "UPDATE 顧客 SET 名前 = @p1 WHERE ид = @p2",
        "INSERT INTO `tábla` (`ő`) VALUES (?)",
        "CALL `prøcédure`(?)",
        "ǅ İ ß ﬁ 𝔘 :: AS İ AND ǅ",
        "",
    ];
    for text in texts {
        let _ = mentions_word(text, "i̇stanbul_şube");
        let _ = mentions_word(text, "ß");
        let _ = predicate_columns(text);
        let _ = parse_epoch_seconds(text);
        let _ = parse_epoch_seconds("2026-10-14T08:54:29+İ");
        // Out-of-range parts from a hostile saved report: no overflow, just not a time.
        assert_eq!(parse_epoch_seconds("9223372036854775807-12-31T00:00:00Z"), None);
        assert_eq!(parse_epoch_seconds("2026-9223372036854775807-01T00:00:00Z"), None);
        assert_eq!(parse_epoch_seconds("2026-10-14Tinf:00:00Z"), None);
        assert!(parse_epoch_seconds("2026-10-14T08:54:29Z").is_some());
        let mut q = crate::waits::snapshot_with(Engine::Mssql).queries()[0].clone();
        q.text = text.into();
        let _ = query_handle(&q);
    }
    let server = crate::waits::snapshot_with(Engine::Postgres).server;
    let _ = setting_change(&server, "work_mem", "64MB ünï");
}

// The aptabase readings: random_page_cost can be set for one database, which RDS allows at once and without a
// custom parameter group; the parameter group re-plans every database on the instance. Settings only a superuser
// sets per database (track_io_timing), and those that are the server's alone (pg_stat_statements.max), stay the
// platform's.
#[test]
fn a_per_database_setting_is_set_for_this_database_first_on_a_managed_server() {
    let mut server = crate::waits::snapshot_with(Engine::Postgres).server;
    server.cloud = Some("Amazon RDS".into());
    let change = setting_change(&server, "random_page_cost", "1.1");
    assert_eq!(change.sql.as_deref(), Some("ALTER DATABASE demo SET random_page_cost = 1.1;"));
    assert!(change.how.starts_with("Set random_page_cost to 1.1 for this database only"), "{}", change.how);
    assert!(change.how.contains("sessions that connect after the change use it"), "{}", change.how);
    assert!(change.how.ends_with("set it in the instance's DB parameter group."), "{}", change.how);
    for name in ["track_io_timing", "pg_stat_statements.max", "autovacuum"] {
        let change = setting_change(&server, name, "on");
        assert!(change.sql.is_none() && change.how.contains("DB parameter group"), "{name}: {}", change.how);
    }
    // A name that needs quoting is quoted.
    server.database = "Shop-EU".into();
    let change = setting_change(&server, "work_mem", "64MB");
    assert_eq!(change.sql.as_deref(), Some("ALTER DATABASE \"Shop-EU\" SET work_mem = '64MB';"));
    // A server you run takes ALTER SYSTEM, as before; MySQL keeps no settings per database.
    server.cloud = None;
    let change = setting_change(&server, "random_page_cost", "1.1");
    assert!(change.sql.unwrap().starts_with("ALTER SYSTEM SET random_page_cost = 1.1;"));
    let mut mysql = crate::waits::snapshot_with(Engine::Mysql).server;
    mysql.cloud = Some("Amazon RDS".into());
    let change = setting_change(&mysql, "sort_buffer_size", "4194304");
    assert!(change.sql.is_none() && change.how.contains("DB parameter group"), "{}", change.how);
}

// Theme A's note on missing-index: a request's grade took the statements' share over Culprit's scans with the time
// their totals hold. A share is graded on the basis it is on: over the scans, the amount a day it is of what the
// database ran in them.
#[test]
fn a_share_over_the_scans_is_graded_on_their_amount_not_the_totals() {
    let path = format!("{}/../../fixtures/postgres-demo.json", env!("CARGO_MANIFEST_DIR"));
    let mut s: Snapshot = serde_json::from_str(&std::fs::read_to_string(path).expect("fixture")).expect("snapshot");
    let w = s.workload.as_mut().expect("workload");
    // 40 days of totals; 36 hours of scans, in which the database ran 10 s a 12-hour step and the statement 3 s.
    (w.window_start, w.window_hours) = (Some("2026-09-01T11:54:21Z".into()), 960.0);
    w.timeline = Some(
        serde_json::from_value(serde_json::json!({
            "source": "scans",
            "starts": ["2026-10-10T00:00:00Z", "2026-10-10T12:00:00Z", "2026-10-11T00:00:00Z"],
            "end": "2026-10-11T12:00:00Z",
            "time_ms": [10000.0, 10000.0, 10000.0],
            "executions": [5000.0, 5000.0, 5000.0]
        }))
        .expect("timeline"),
    );
    let q = &mut w.queries[0];
    q.trend = Some(culprit_core::QueryTrend { time_ms: vec![Some(3_000.0); 3], executions: vec![Some(10.0); 3] });
    // Its totals: 1,000 runs of a minute since the reset, 25 minutes a day and 30% of the database's.
    (q.executions, q.mean_time_ms, q.total_time_ms) = (1_000.0, 60_000.0, 60_000_000.0);
    w.total_time_ms = 200_000_000.0;
    let q = s.workload.as_ref().unwrap().queries[0].clone();
    assert!(s.workload.as_ref().unwrap().scans_of(&q).is_some_and(|s| !s.is_recent(&q)));
    // 30% of the scans' time is 6 s a day of a database that ran 20 s a day in them: Low, however much time the
    // totals hold; the totals' time beside the scans' share made it High.
    assert_eq!(severity_for_statement_share(&s, &[&q], 0.3), Severity::Low);
    assert_eq!(amount_severity_of(&s, &[&q], 0.3), Severity::Low);
    assert_eq!(severity_for_time_of(&s, &[&q], q.total_time_ms, 0.3), Severity::High, "the old mix");
    // Without the scans, the totals are the basis of both.
    let mut totals = s.clone();
    totals.workload.as_mut().unwrap().timeline = None;
    assert_eq!(severity_for_statement_share(&totals, &[&q], 0.3), Severity::High);
}

// D's open gap (MY-5): performance_schema's table counters start over with its statements, so "no writes" waits a
// week of the digests' own counting, however long the server has been up; then it is said as counted.
#[test]
fn no_writes_on_counters_that_may_be_reset_waits_a_week_of_their_own() {
    let path = format!("{}/../../fixtures/mysql-demo.json", env!("CARGO_MANIFEST_DIR"));
    let mut s: Snapshot = serde_json::from_str(&std::fs::read_to_string(path).expect("fixture")).expect("snapshot");
    let taken = parse_epoch_seconds(&s.taken_at).expect("taken_at");
    s.server.started_at = Some(culprit_core::format::iso_from_epoch(taken - 10.0 * 86_400.0));
    s.pressure.get_or_insert_with(Default::default).uptime_hours = Some(240.0);
    let digests = |s: &mut Snapshot, days: f64| {
        let w = s.workload.as_mut().expect("workload");
        w.window_start = Some(culprit_core::format::iso_from_epoch(taken - days * 86_400.0));
        w.window_hours = days * 24.0;
    };
    digests(&mut s, 2.0);
    assert!(counters_may_be_reset(&s) && watched_days(&s) == Some(2.0));
    let cost = crate::indexes::write_cost(&s, "shop", "orders").expect("a cost");
    assert!(cost.contains("in what its counters cover, which may be short"), "{cost}");
    digests(&mut s, 8.0);
    let cost = crate::indexes::write_cost(&s, "shop", "orders").expect("a cost");
    assert_eq!(
        cost,
        "shop.orders received no writes since the server started or its performance_schema counters were last \
         reset, so this index would cost next to nothing to maintain."
    );
}

#[test]
fn usage_windows_say_how_long_the_server_has_been_up() {
    let mut s = crate::waits::snapshot_with(Engine::Mssql);
    s.taken_at = "2026-10-11T17:05:00Z".into();
    s.pressure = None;
    let mut at = |started: &str| {
        s.server.started_at = Some(started.into());
        since_phrase(&s)
    };
    assert_eq!(at("2026-10-11T16:49:00Z"), "in the 16 minutes since the server last restarted");
    assert_eq!(at("2026-10-11T16:05:00Z"), "in the hour since the server last restarted");
    assert_eq!(at("2026-10-11T12:05:00Z"), "in the 5 hours since the server last restarted");
    assert_eq!(at("2026-10-01T17:05:00Z"), "in the 10 days since the server last restarted");
    // A staging SQL Server reading: 9.51 days of counters were "the 10 days", more than they counted.
    assert_eq!(at("2026-10-02T04:50:36Z"), "in the 9.5 days since the server last restarted");
    assert_eq!(at("2026-10-02T05:05:00Z"), "in the 9.5 days since the server last restarted");
    assert_eq!(at("2026-10-09T17:05:00Z"), "in the 2 days since the server last restarted");
    assert_eq!(at("2026-09-28T23:53:00Z"), "in the 12 days since the server last restarted");
}

#[test]
fn postgres_counters_that_outlived_a_restart_say_so() {
    // The server restarted 5 hours ago, but its statistics were last reset 10.9 hours ago and survived.
    let mut s = crate::waits::snapshot_with(Engine::Postgres);
    s.taken_at = "2026-10-11T17:00:00Z".into();
    s.server.started_at = Some("2026-10-11T12:00:00Z".into());
    s.pressure = Some(culprit_core::Pressure { uptime_hours: Some(10.9), ..Default::default() });
    assert_eq!(since_phrase(&s), "in the 11 hours since its statistics were last reset");
    assert!((counter_days(&s).unwrap() * 24.0 - 10.9).abs() < 1e-9);
    // Counters as old as the server: a restart, as before.
    s.pressure = Some(culprit_core::Pressure { uptime_hours: Some(5.0), ..Default::default() });
    assert_eq!(since_phrase(&s), "in the 5 hours since the server last restarted");
}

#[test]
fn counts_are_per_day_only_over_a_day_of_counters() {
    let mut s = crate::waits::snapshot_with(Engine::Mssql);
    s.taken_at = "2026-10-11T17:05:00Z".into();
    s.pressure = None;
    s.server.started_at = Some("2026-10-11T12:05:00Z".into());
    let days = counter_days(&s).unwrap();
    // 5 hours: what was counted, not "~1.6K times a day".
    assert_eq!(how_many(&s, 340.0, days, "time", "times"), "340 times in the 5 hours since the server last restarted");
    s.server.started_at = Some("2026-10-09T17:05:00Z".into());
    let days = counter_days(&s).unwrap();
    assert_eq!(how_many(&s, 340.0, days, "time", "times"), "about 170 times a day");
}

#[test]
fn system_schema_reads_are_monitoring_on_every_engine() {
    assert!(is_monitoring_text(
        "SELECT `EVENT_NAME` FROM `performance_schema` . `events_waits_summary_global_by_event_name`"
    ));
    assert!(is_monitoring_text("SELECT * FROM mysql.slow_log WHERE db = ?"));
    assert!(is_monitoring_text("SELECT * FROM sys.innodb_lock_waits"));
    assert!(is_monitoring_text("SELECT * FROM `information_schema` . `TABLES`"));
    assert!(!is_monitoring_text("SELECT `id` FROM `orders` WHERE `customer_id` = ?"));
    assert!(!is_monitoring_text("SELECT analysis.id FROM analysis WHERE mysqlish = 1"));
}

#[test]
fn epoch_parsing_handles_offsets() {
    let a = parse_epoch_seconds("2026-10-01T00:00:00Z").unwrap();
    let b = parse_epoch_seconds("2026-10-01T02:00:00+02:00").unwrap();
    assert_eq!(a, b);
    assert_eq!(days_between("2026-10-01T00:00:00Z", "2026-10-11 12:00:00").unwrap(), 10.5);
}

#[test]
fn word_matching() {
    assert!(mentions_word("SELECT * FROM [dbo].[Orders] m", "orders"));
    assert!(!mentions_word("SELECT * FROM OrdersArchive", "orders"));
}

#[test]
fn query_handles() {
    let mut q = sample();
    q.text = "SELECT TOP 10 * FROM [dbo].[Orders] WHERE x = @p1".into();
    assert_eq!(query_handle(&q), "a read from Orders");
    q.text = "UPDATE dbo.Users SET a = 1".into();
    assert_eq!(query_handle(&q), "an update of Users");
    q.text = "UPDATE STATISTICS dbo.[Orders] WITH SAMPLE 100 PERCENT".into();
    assert_eq!(query_handle(&q), "a statistics refresh on Orders");
    q.text = "UPDATE\nSTATISTICS dbo.[Orders]".into();
    assert_eq!(query_handle(&q), "a statistics refresh on Orders");
    q.text = "(@P1 int) UPDATE STATISTICS dbo.[Orders]".into();
    assert_eq!(query_handle(&q), "a statistics refresh on Orders");
    assert!(statistics_maintenance_table("UPDATE dbo.Statistics SET value = 1").is_none());
    assert!(statistics_maintenance_table("UPDATE statistics_log SET value = 1").is_none());
    assert!(statistics_maintenance_table("UPDATE statistics SET value = 1").is_none());
    assert!(statistics_maintenance_table("UPDATE statistics s SET value = 1").is_none());
    assert!(statistics_maintenance_table("UPDATE statistics AS s SET value = 1").is_none());
    q.text = "UPDATE TOP (1000) dbo.[Orders] SET state = 1".into();
    assert_eq!(query_handle(&q), "an update of Orders");
    q.text = "UPDATE TOP (@limit) dbo.[Orders] SET state = 1".into();
    assert_eq!(query_handle(&q), "an update of Orders");
    q.text = "UPDATE TOP 10 PERCENT dbo.[Orders] SET state = 1".into();
    assert_eq!(query_handle(&q), "an update of Orders");
    q.text = "UPDATE top100 SET state = 1".into();
    assert_eq!(query_handle(&q), "an update of top100");
}

pub(crate) fn sample() -> QueryStat {
    QueryStat {
        resolved_write_target: None,
        requests: None,
        id: "1".into(),
        query_hash: None,
        text: String::new(),
        parameters: None,
        object_name: None,
        executions: 1.0,
        total_time_ms: 1.0,
        mean_time_ms: 1.0,
        max_time_ms: None,
        stddev_time_ms: None,
        cpu_time_ms: None,
        rows: None,
        logical_reads: None,
        physical_reads: None,
        temp_writes: None,
        first_seen: None,
        last_seen: None,
        plans: Vec::new(),
        plan: None,
        waits: Vec::new(),
        trend: None,
        spread: None,
        costs: None,
        rows_affected: None,
        plan_id: None,
    }
}

#[test]
fn statements_without_a_table_are_named() {
    assert_eq!(statement_handle("DO SLEEP(?)"), "a call to SLEEP");
    assert_eq!(statement_handle("DISCARD ALL"), "DISCARD ALL");
    // A write behind a WITH list is the write, wherever its CTEs read.
    assert_eq!(statement_handle("WITH b AS (SELECT id FROM jobs) DELETE FROM jobs USING b"), "a delete from jobs");
    assert_eq!(statement_handle("WITH b AS (SELECT 'delete' FROM x) UPDATE t SET a = 1"), "an update of t");
    assert_eq!(statement_handle("SELECT sleep(?)"), "a call to sleep");
    assert_eq!(statement_handle("SELECT pg_sleep($1)"), "a call to pg_sleep");
    assert_eq!(statement_handle("DO `SLEEP` (?)"), "a call to SLEEP");
    assert_eq!(statement_handle("CALL `refresh_totals` (?)"), "a call to refresh_totals");
    assert_eq!(statement_handle("EXEC [dbo].[usp_orders] @id = 1"), "a call to dbo.usp_orders");
    assert_eq!(statement_handle("CALL shop.refresh(?)"), "a call to shop.refresh");
    assert_eq!(statement_handle("CALL refresh_totals(?)"), "a call to refresh_totals");
    assert_eq!(statement_handle("SELECT id FROM orders WHERE id = ?"), "a read from orders");
    // A pool's ping is named as it is.
    assert_eq!(statement_handle("SELECT 1"), "SELECT 1");
    assert_eq!(
        statement_handle("SELECT count(*) FROM (VALUES ($1),($2)) v(x) JOIN orders o ON o.id = v.x"),
        "a read from orders"
    );
    assert_eq!(statement_handle("SELECT count(*) FROM (SELECT 1) s"), "a read");
}

#[test]
fn mysql_online_ddl_options_are_space_separated() {
    // `ALGORITHM=INPLACE, LOCK=NONE` is ERROR 1064 in CREATE/DROP INDEX on MySQL and MariaDB: index options don't
    // take commas (only ALTER TABLE's clauses do).
    // MariaDB's DROP INDEX has no such options at all, so the drop goes through ALTER TABLE (whose clauses do).
    let drop = drop_index_sql(Engine::Mysql, "shop", "orders", "ix_orders_status");
    assert_eq!(drop, "ALTER TABLE `shop`.`orders` DROP INDEX `ix_orders_status`, ALGORITHM=INPLACE, LOCK=NONE;");
}

#[test]
fn postgres_predicates() {
    let p = predicate_columns("(customer_id = $1)");
    assert_eq!(p.equality, vec!["customer_id"]);
    let p = predicate_columns("(((status)::text = 'open'::text) AND (created_at > (now() - '7 days'::interval)))");
    assert_eq!(p.equality, vec!["status"]);
    assert_eq!(p.range, vec!["created_at"]);
    let p = predicate_columns("(o.customer_id = c.id)");
    assert!(p.equality.is_empty());
}

#[test]
fn sql_server_predicates() {
    let p = predicate_columns(
        "[shop].[dbo].[Orders].[CustomerId] as [m].[CustomerId]=[@P1] AND [shop].[dbo].[Orders].[CreatedAt] as [m].[CreatedAt]>=[@P2]",
    );
    assert_eq!(p.equality, vec!["CustomerId"]);
    assert_eq!(p.range, vec!["CreatedAt"]);
    let p = predicate_columns("[shop].[dbo].[Users].[TenantId]=CONVERT_IMPLICIT(uniqueidentifier,[@P1],0)");
    assert_eq!(p.equality, vec!["TenantId"]);
}

// A function of parameters and constants is a value (the planted shipped_at range on SQL Server), and a function of
// a column isn't.
#[test]
fn a_function_of_values_is_a_value() {
    let p = predicate_columns(
        "[culprit_demo].[shop].[orders].[shipped_at]>=dateadd(day,[@P1],'2026-01-01 00:00:00.000') AND \
         [culprit_demo].[shop].[orders].[shipped_at]<dateadd(day,[@P2],'2026-01-01 00:00:00.000')",
    );
    assert_eq!(p.range, vec!["shipped_at"]);
    assert_eq!(predicate_columns("(total > COALESCE($1, 0))").range, vec!["total"]);
    assert_eq!(predicate_columns("(`o`.`created_at` > DATE_SUB(?, INTERVAL ? DAY))").range, vec!["created_at"]);
    assert_eq!(predicate_columns("([o].[due]>dateadd(day,(1),[o].[created_at]))"), PredicateColumns::default());
    assert_eq!(predicate_columns("(name = upper(other_name))"), PredicateColumns::default());
}

// An index on one branch of an OR can't find the other branch's rows, and a default b-tree can't seek not-equal,
// NOT, a pattern starting with a wildcard, or PostgreSQL's LIKE.
#[test]
fn only_conditions_an_index_can_seek_name_columns() {
    let p = predicate_columns("((status)::text = 'open'::text) OR (priority > 3)");
    assert_eq!(p, PredicateColumns::default());
    let p = predicate_columns("[db].[dbo].[t].[status]='open' OR [db].[dbo].[t].[priority]>(3)");
    assert_eq!(p, PredicateColumns::default());
    // A condition every row meets still counts next to an OR.
    let p = predicate_columns("((customer_id = $1) AND ((status = 'open') OR (priority > 3)))");
    assert_eq!(p.equality, vec!["customer_id"]);
    assert!(p.range.is_empty());
    // Parentheses decide: `a OR (b AND c)` requires nothing of every row.
    let p = predicate_columns("((a = 1) OR ((b = 2) AND (c = 3)))");
    assert_eq!(p, PredicateColumns::default());
    // The false flag: a cast of the column to date hides it from an index on it, like date(created_at) would.
    let p = predicate_columns("((created_at)::date = $1)");
    assert!(p.equality.is_empty() && p.range.is_empty(), "{p:?}");
    let p = predicate_columns("((amount)::integer > 10) AND (created_at > '2026-01-01'::date) AND (id = $1::bigint)");
    assert_eq!((p.equality, p.range), (vec!["id".to_string()], vec!["created_at".to_string()]));
    let p = predicate_columns("((status)::text <> 'closed'::text)");
    assert_eq!(p, PredicateColumns::default());
    let p = predicate_columns("((email)::text ~~ 'abc%'::text)");
    assert_eq!(p, PredicateColumns::default());
    let p = predicate_columns("(NOT (customer_id = $1))");
    assert_eq!(p, PredicateColumns::default());
    let p = predicate_columns("(shop.orders.status not in ('a','b'))");
    assert_eq!(p, PredicateColumns::default());
    // SQL Server and MySQL seek a LIKE with a fixed start, never one that starts with a wildcard.
    let p = predicate_columns("[db].[dbo].[t].[email] like N'abc%'");
    assert_eq!(p.range, vec!["email"]);
    let p = predicate_columns("[db].[dbo].[t].[email] like N'%abc'");
    assert_eq!(p, PredicateColumns::default());
    // MySQL: typed literals, BETWEEN and the null-safe equal.
    let p = predicate_columns(
        "((shop.orders.status = 'shipped') and (shop.orders.created_at >= TIMESTAMP'2026-09-05 00:00:00'))",
    );
    assert_eq!((p.equality, p.range), (vec!["status".to_string()], vec!["created_at".to_string()]));
    let p = predicate_columns("(shop.o.total between 10 and 20) and (shop.o.customer_id <=> 11272)");
    assert_eq!((p.equality, p.range), (vec!["customer_id".to_string()], vec!["total".to_string()]));
    // Quoted text is never taken apart, even when a literal on the left contains an operator.
    let p = predicate_columns("(note = 'a OR b') AND (kind = 'x <> y') AND ('a > b' = tag)");
    assert_eq!(p.equality, vec!["note", "kind", "tag"]);
}

// A staging SQL Server reading: a Key Lookup's residual on a column qualified by a database whose name has hyphens
// read as a call of values (`app` minus …), so the lookups that found nothing lost their reading and their fix.
#[test]
fn a_column_qualified_by_a_hyphenated_database_is_a_column() {
    let p = predicate_columns(
        "[app-db-01].[app].[call_sessions].[conversation_id]=CONVERT_IMPLICIT(uniqueidentifier,[@par2],0)",
    );
    assert_eq!(p.equality, vec!["conversation_id"]);
    // A call of values stays a value, with or without its parentheses.
    assert!(is_value("dateadd(day,@P1,'2026-01-01')") && is_value("dateadd day,@P1,'2026-01-01'"));
}
