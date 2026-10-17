//! The agent brief gives a coding agent what the scan supports and nothing more: a plan step's share only where the
//! plan can state one, a step only for a finding about one, what a SQLite step does, and work in the codebase that
//! matches where the fix lives (the statements, the schema, a setting, the database's upkeep, the connection pool),
//! never a promise that the finding goes away where a later scan can't show it.

use culprit_checks::{agent_brief, analyze};
use culprit_core::{Category, Finding, Fix, Metric, QueryRef, Report, Snapshot, SubjectKind, Unit};

fn report(name: &str) -> Report {
    let text =
        std::fs::read_to_string(format!("{}/../../fixtures/{name}", env!("CARGO_MANIFEST_DIR"))).expect("fixture");
    analyze(serde_json::from_str::<Snapshot>(&text).expect("snapshot"))
}

/// The first finding whose id starts with `prefix`.
fn finding<'a>(report: &'a Report, prefix: &str) -> &'a Finding {
    report
        .findings
        .iter()
        .find(|f| f.id.starts_with(prefix))
        .unwrap_or_else(|| panic!("no {prefix} in {:?}", report.findings.iter().map(|f| &f.id).collect::<Vec<_>>()))
}

/// The section a coding agent acts on.
fn codebase(brief: &str) -> String {
    brief.split("## What to do in the codebase").nth(1).expect("the section").to_string()
}

fn plan_line(brief: &str) -> String {
    brief.lines().find(|l| l.starts_with("Plan:")).expect("a plan line").to_string()
}

/// `finding` with this fix.
fn with_fix(finding: &Finding, summary: &str, sql: Option<&str>) -> Finding {
    let mut f = finding.clone();
    f.fix = Some(Fix { summary: summary.into(), sql: sql.map(Into::into), cost: None });
    f
}

/// A finding about `query` of the demo, naming step `node` of its captured plan (or none): the plan line reads the
/// fixture's plan whichever check speaks about it.
fn about(report: &Report, query: &str, node: Option<u32>) -> Finding {
    let mut f = report.findings[0].clone();
    f.queries = vec![QueryRef { id: query.into(), plan_node: node, note: None }];
    f
}

#[test]
fn no_brief_promises_that_its_finding_disappears() {
    // A count kept since a restart, a forced plan that helps, a statistic no query reads: the right change can leave
    // them on the list. Each brief sends the agent to its own way of verifying instead.
    for name in ["mssql-demo.json", "postgres-demo.json", "mysql-demo.json", "sqlite-demo.json"] {
        let report = report(name);
        for f in &report.findings {
            let section = codebase(&agent_brief(&report, f));
            assert!(!section.contains("no longer appears"), "{name} {}: {section}", f.id);
            assert!(section.contains("culprit scan"), "{name} {}: {section}", f.id);
            if !f.verify.is_empty() {
                assert!(section.contains("How to verify"), "{name} {}: {section}", f.id);
            }
        }
    }
}

#[test]
fn a_setting_is_changed_where_the_server_is_managed_not_found_in_the_code() {
    let report = report("mssql-demo.json");
    let setting = report
        .findings
        .iter()
        .find(|f| f.category == Category::Configuration && f.queries.is_empty())
        .expect("a setting to change");
    // A server option, and one the platform's control sets (no SQL): the server's configuration.
    for sql in [Some("EXEC sp_configure 'cost threshold for parallelism', 50;\nRECONFIGURE;"), None] {
        let f = with_fix(setting, "Raise it.", sql);
        let section = codebase(&agent_brief(&report, &f));
        assert!(section.contains("The fix is a setting of the database server"), "{section}");
        assert!(section.contains("the platform's parameter group"), "{section}");
        assert!(!section.contains("search for their table"), "{section}");
        // MS-12e: where nothing manages the server's configuration, the change is still made: by hand.
        let by_hand = if sql.is_some() {
            "Where nothing manages it, run the fix's SQL against the server as a login allowed to change its \
             settings, the way the fix says."
        } else {
            "Where nothing manages it, make the change by hand the way the fix says."
        };
        assert!(section.contains(by_hand), "{section}");
        assert!(!section.contains("if it does"), "{section}");
    }
}

#[test]
fn a_log_throughput_brief_keeps_batching_before_capacity_changes() {
    let path = format!("{}/../../fixtures/mssql-demo-staging.report.json", env!("CARGO_MANIFEST_DIR"));
    let saved: Report = serde_json::from_str(&std::fs::read_to_string(path).expect("fixture")).expect("report");
    let report = analyze(saved.snapshot);
    let finding = finding(&report, "tier-limit:log-write");
    let section = codebase(&agent_brief(&report, finding));
    let batch = section.find("batch small writes into fewer transactions").expect("application batching");
    let capacity = section.find("change the service tier or capacity").expect("capacity fallback");
    assert!(batch < capacity, "{section}");
    assert!(section.contains("application or job"), "{section}");
    assert!(!section.contains("The fix is a setting of the database server"), "{section}");
}

#[test]
fn a_measurement_only_fix_gathers_evidence_before_prescribing_code_changes() {
    let report = report("mssql-demo.json");
    let heavy = finding(&report, "heavy-query:");
    for diagnostic in [
        "Read a retained actual plan when the server has one; otherwise capture a representative actual plan in a test environment.",
        "Capture an actual execution plan for a representative run in a test environment.",
    ] {
        let f = with_fix(heavy, diagnostic, None);
        let section = codebase(&agent_brief(&report, &f));
        assert!(section.contains("Gather the runtime evidence the fix asks for first"), "{section}");
        assert!(section.contains("before choosing a change"), "{section}");
        assert!(!section.contains("apply the fix in that calling code"), "{section}");
        assert!(section.contains("How to verify"), "{section}");
    }
}

#[test]
fn a_scan_heavy_diagnostic_gathers_measurements_before_choosing_a_change() {
    let path = format!("{}/../../fixtures/mariadb-demo.report.json", env!("CARGO_MANIFEST_DIR"));
    let saved: Report = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    for engine in [culprit_core::Engine::Mysql, culprit_core::Engine::Postgres] {
        let mut snapshot = saved.snapshot.clone();
        snapshot.server.engine = engine;
        let report = analyze(snapshot);
        let diagnostic = finding(&report, "scan-heavy:");
        let fix = diagnostic.fix.as_ref().unwrap();
        assert!(fix.sql.is_none() && fix.summary.contains("capture an actual plan with representative values"));
        if engine == culprit_core::Engine::Postgres {
            assert!(fix.summary.contains("Culprit can show a generic plan, which does not measure actual rows"));
        }
        let section = codebase(&agent_brief(&report, diagnostic));
        assert!(section.contains("Gather the runtime evidence the fix asks for first"), "{section}");
        assert!(section.contains("before choosing a change"), "{section}");
        assert!(!section.contains("apply the fix in that calling code"), "{section}");
        assert!(!section.contains("Ship the fix's index change"), "{section}");
        assert!(section.contains("How to verify"), "{section}");
    }
}

#[test]
fn a_role_only_query_fix_leaves_calling_code_alone_but_keeps_local_options() {
    let postgres = report("postgres-demo.json");
    let query = finding(&postgres, "temp-spill:");
    let role =
        with_fix(query, "Turn JIT off for the application's role.", Some("ALTER ROLE <app_role> SET jit = off;"));
    let section = codebase(&agent_brief(&postgres, &role));
    assert!(section.contains("The role setting in the fix's SQL"), "{section}");
    assert!(!section.contains("apply the fix in that calling code"), "{section}");

    let local = with_fix(
        query,
        "Raise memory for the application's role or just for this statement.",
        Some("ALTER ROLE <app_role> SET work_mem = '64MB';\nSET LOCAL work_mem = '64MB';"),
    );
    let section = codebase(&agent_brief(&postgres, &local));
    assert!(section.contains("The role setting in the fix's SQL"), "{section}");
    assert!(section.contains("apply the fix in that calling code"), "{section}");
    assert!(section.contains("`SET LOCAL` is the exception"), "{section}");
    assert!(section.contains("inside the transaction that runs the statement"), "{section}");
}

#[test]
fn temp_spill_options_route_application_and_index_work_conditionally() {
    let mysql = report("mysql-demo.json");
    let path = format!("{}/../../fixtures/mariadb-demo.report.json", env!("CARGO_MANIFEST_DIR"));
    let saved: Report = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    let mariadb = analyze(saved.snapshot);
    for report in [&mysql, &mariadb] {
        let spill = finding(report, "temp-spill:");
        let section = codebase(&agent_brief(report, spill));
        assert!(section.contains("If you choose an index") && section.contains("schema migrations"), "{section}");
        assert!(section.contains("If you choose a query change") && section.contains("calling code"), "{section}");
        assert!(section.contains("If you choose the session-setting option"), "{section}");
        assert!(section.contains("The setting in the fix's SQL belongs to the database server"), "{section}");
        assert!(!section.contains("apply the fix in that calling code"), "{section}");
    }
    // The TempTable engine's shared RAM cap offers no session-variable route.
    let shared = with_fix(
        finding(&mysql, "temp-spill:"),
        "Check whether an index helps, or raise the shared TempTable RAM cap.",
        Some("SET GLOBAL temptable_max_ram = 2147483648;"),
    );
    let section = codebase(&agent_brief(&mysql, &shared));
    assert!(section.contains("If you choose an index"), "{section}");
    assert!(!section.contains("session-setting option"), "{section}");
}

#[test]
fn a_database_or_role_setting_is_sql_run_against_the_database() {
    // ALTER DATABASE … SET and SQL Server's database options live in the database: run once as its owner or shipped as
    // a migration, never put in a parameter group that sets every database on the server.
    let report = report("mssql-demo.json");
    let setting = report
        .findings
        .iter()
        .find(|f| f.category == Category::Configuration && f.queries.is_empty())
        .expect("a setting to change");
    for sql in [
        "ALTER DATABASE aptabase SET random_page_cost = 1.1;",
        "ALTER DATABASE [culprit_demo] SET READ_COMMITTED_SNAPSHOT ON WITH ROLLBACK IMMEDIATE;",
        "ALTER DATABASE [culprit_demo] SET QUERY_STORE (QUERY_CAPTURE_MODE = AUTO);",
        "ALTER DATABASE SCOPED CONFIGURATION SET LAST_QUERY_PLAN_STATS = ON;",
    ] {
        let f = with_fix(setting, "Set it for this database only.", Some(sql));
        let section = codebase(&agent_brief(&report, &f));
        assert!(section.contains("The fix is a setting kept in this database alone"), "{sql}: {section}");
        assert!(section.contains("run its SQL once against the database as its owner"), "{sql}: {section}");
        assert!(section.contains("shipped as a migration"), "{sql}: {section}");
        assert!(section.contains("which is for settings of the whole server"), "{sql}: {section}");
        assert!(!section.contains("a setting of the database server"), "{sql}: {section}");
        assert!(!section.contains("apply it where this project manages"), "{sql}: {section}");
    }

    // A role's setting changes that role's new connections; `SET LOCAL` only in the code, around the statement.
    let postgres = self::report("postgres-demo.json");
    let section = codebase(&agent_brief(&postgres, finding(&postgres, "temp-spill:")));
    assert!(section.contains("The role setting in the fix's SQL (`ALTER ROLE … SET`)"), "{section}");
    assert!(section.contains("in place of `<app_role>`"), "{section}");
    assert!(!section.contains("belongs to the database server"), "{section}");
    assert!(section.contains("`SET LOCAL` is the exception"), "{section}");
    assert!(section.contains("inside the transaction that runs the statement"), "{section}");
}

#[test]
fn a_setting_the_fix_calls_a_fallback_comes_after_the_application_change() {
    // query-family: "Prefer fixing the app. As a database-wide fallback: … PARAMETERIZATION FORCED".
    let mssql = report("mssql-demo.json");
    let mut f = with_fix(
        finding(&mssql, "heavy-query:"),
        "Send values as parameters so one plan serves every value. If you can't change the app, forced \
         parameterization does it for the whole database.",
        Some(
            "-- Prefer fixing the app. As a database-wide fallback:\nALTER DATABASE [culprit_demo] SET PARAMETERIZATION FORCED;",
        ),
    );
    f.queries.clear();
    let section = codebase(&agent_brief(&mssql, &f));
    assert!(section.trim_start().starts_with("The change the fix prefers is in the application's code"), "{section}");
    assert!(section.contains("only the fallback it names"), "{section}");
    assert!(section.contains("The database setting in the fix's SQL (`ALTER DATABASE`)"), "{section}");
    assert!(!section.contains("The fix is a setting"), "{section}");
    assert!(!section.contains("a setting of the database server"), "{section}");
}

// CX-4 and CX-2, a staging SQL Server reading: "The fix is upkeep of the database … run it" beside a fix that says no
// update is needed, and beside one whose way on, if the update doesn't change the plan, is a table hint in the
// statement. Optional upkeep is said as optional; a change in the code the fix's SQL offers is said as one.
#[test]
fn upkeep_the_fix_calls_optional_or_follows_with_a_code_change_is_briefed_as_such() {
    let mssql = report("mssql-demo.json");
    let base = finding(&mssql, "heavy-query:");
    let mut optional = with_fix(
        base,
        "No update is needed before a query uses these statistics: SQL Server refreshes them on that query's compile. Updating them now (below) spares that compile the wait.",
        Some("UPDATE STATISTICS [shop].[products];"),
    );
    optional.queries.clear();
    let section = codebase(&agent_brief(&mssql, &optional));
    assert!(section.trim_start().starts_with("The fix's upkeep is optional, as it says"), "{section}");
    assert!(!section.contains("The fix is upkeep of the database"), "{section}");
    let hinted = with_fix(
        base,
        "Update the statistics (the SQL below). If a re-scan still shows the same reads, change the statement where the application sends it to read shop.products with a table hint.",
        Some(
            "UPDATE STATISTICS [shop].[products];\n\n-- If a re-scan still shows about 750 pages a run, change the statement in the application:\n--    read [shop].[products] WITH (FORCESEEK ([ix_products_sku] ([sku])))",
        ),
    );
    let section = codebase(&agent_brief(&mssql, &hinted));
    assert!(
        !section.contains("The fix is upkeep of the database, not a change to the application's code"),
        "{section}"
    );
    assert!(section.contains("The upkeep in the fix's SQL isn't a change to the application's code"), "{section}");
    assert!(section.contains("The fix's other option is a change in the application's code"), "{section}");
}

#[test]
fn statements_a_pool_sends_are_not_looked_for_in_the_code() {
    // DISCARD ALL comes from the pool resetting connections: there is no query of the application's to find.
    let mut report = report("postgres-demo.json");
    let workload = report.snapshot.workload.as_mut().expect("workload");
    let mut discard = workload.queries[0].clone();
    (discard.id, discard.text, discard.plan) = ("discard".into(), "DISCARD ALL".into(), None);
    workload.queries.push(discard);
    let mut f = finding(&report, "heavy-query:").clone();
    f.queries = vec![QueryRef { id: "discard".into(), plan_node: None, note: None }];
    let section = codebase(&agent_brief(&report, &f));
    assert!(section.contains("the driver or the connection pool"), "{section}");
    assert!(!section.contains("search for their table"), "{section}");
}

#[test]
fn statements_that_stopped_running_may_need_nothing() {
    let report = report("postgres-demo.json");
    let mut f = finding(&report, "chatty-query:").clone();
    let running = codebase(&agent_brief(&report, &f));
    assert!(running.contains("Find where the application sends the statements above"), "{running}");
    assert!(!agent_brief(&report, &f).contains("it no longer runs"));
    f.inactive = true;
    // Its rates are what it ran before it stopped, not what it runs now.
    let brief = agent_brief(&report, &f);
    assert!(brief.contains(" average (it no longer runs):"), "{brief}");
    let stopped = codebase(&brief);
    assert!(stopped.contains("stopped running"), "{stopped}");
    assert!(stopped.contains("a deploy may have changed or fixed them"), "{stopped}");
    assert!(!stopped.contains("most likely"), "{stopped}");
    assert!(stopped.contains("there is nothing to do"), "{stopped}");
    assert!(!stopped.contains("Find where the application sends the statements above"), "{stopped}");
    // Nothing to check until it runs again. Marked by hand, it keeps its own step, which already says the totals keep
    // the earlier runs: the brief says that once.
    assert!(
        stopped.contains("There is nothing to check unless it runs again: if it does, run `culprit scan"),
        "{stopped}"
    );
    assert!(!stopped.contains("After any change is live"), "{stopped}");
    assert_eq!(brief.matches("keeps running totals").count(), 1, "{brief}");
    // Once refine has seen it stop, its step is that there is nothing to check unless it runs again: said there alone.
    f.verify = vec![
        "Nothing to check unless it runs again: a later scan that sees it run lists this finding as current again."
            .into(),
    ];
    let stopped = codebase(&agent_brief(&report, &f));
    assert!(!stopped.to_lowercase().contains("nothing to check"), "{stopped}");
    assert!(stopped.trim_end().ends_with("there is nothing to do."), "{stopped}");
}

#[test]
fn statements_unchanged_since_the_first_scan_may_have_stopped_long_before_it() {
    // Last seen at Culprit's first scan of a source that keeps no last run: that is only the latest it can have run,
    // so the deploy that changed it may be an earlier one.
    let mut report = report("postgres-demo.json");
    let first = "2026-10-11T14:31:51Z";
    let workload = report.snapshot.workload.as_mut().expect("workload");
    workload.timeline = Some(
        serde_json::from_value(serde_json::json!({
            "source": "scans",
            "starts": [first, "2026-10-12T14:31:51Z"],
            "end": "2026-10-13T14:31:51Z",
            "time_ms": [0.0, 0.0],
            "executions": [0.0, 0.0],
        }))
        .expect("timeline"),
    );
    let mut f = finding(&report, "chatty-query:").clone();
    (f.inactive, f.last_seen) = (true, Some(first.into()));
    let section = codebase(&agent_brief(&report, &f));
    assert!(
        section.contains("haven't run since Culprit's first scan, and may have stopped long before it"),
        "{section}"
    );
    assert!(section.contains("a deploy before that scan may have changed or fixed them"), "{section}");
    assert!(!section.contains("before the scan's data ends"), "{section}");

    // Seen later than the first scan, it ran until about then.
    f.last_seen = Some("2026-10-12T14:31:51Z".into());
    let section = codebase(&agent_brief(&report, &f));
    assert!(section.contains("stopped running before the scan's data ends"), "{section}");

    // The dashboard's findings tile says it the same way.
    let detail = |report: &Report| {
        let d = culprit_checks::dashboard::dashboard(report);
        d.tile(culprit_core::TileKey::Findings).and_then(|t| t.detail.clone()).expect("a detail")
    };
    let stop = |report: &mut Report, last: &str| {
        for (i, f) in report.findings.iter_mut().enumerate() {
            (f.inactive, f.last_seen) = (i == 0, Some(last.into()));
        }
    };
    stop(&mut report, first);
    let text = detail(&report);
    assert!(text.contains("hasn't run since Culprit's first scan and may have stopped before it"), "{text}");
    assert!(!text.contains("most likely"), "{text}");
    stop(&mut report, "2026-10-12T14:31:51Z");
    let text = detail(&report);
    assert!(text.contains("about a query that stopped running: a deploy may have changed it"), "{text}");
}

#[test]
fn an_index_ships_as_a_migration_and_upkeep_runs_on_the_database() {
    let mssql = report("mssql-demo.json");
    let section = codebase(&agent_brief(&mssql, finding(&mssql, "duplicate-index:")));
    assert!(section.contains("the way this project ships schema changes (a migration)"), "{section}");
    assert!(section.contains("keeping the options the fix's SQL already has"), "{section}");
    assert!(!section.contains("CONCURRENTLY"), "{section}");

    let sqlite = report("sqlite-demo.json");
    let upkeep = sqlite
        .findings
        .iter()
        .find(|f| {
            f.category == Category::Maintenance
                && f.queries.is_empty()
                && f.fix.as_ref().is_some_and(|x| x.sql.is_some())
        })
        .expect("upkeep to run");
    let section = codebase(&agent_brief(&sqlite, upkeep));
    assert!(section.contains("upkeep of the database"), "{section}");
    assert!(!section.contains("migration"), "{section}");
}

#[test]
fn a_step_the_plan_puts_no_cost_on_leaves_the_share_out() {
    // MySQL costs the scan and nothing above it: the temporary table and the filesort carry no cost, so "100% of
    // estimated cost" on the scan says nothing about where the statement's time goes.
    let mysql = report("mysql-demo.json");
    let grouped = plan_line(&agent_brief(&mysql, &about(&mysql, "04cd7be9c366fadc", Some(2))));
    assert!(!grouped.contains("of estimated cost"), "{grouped}");
    assert!(!grouped.contains("most expensive"), "{grouped}");
    assert!(grouped.contains("The database leaves some of this plan's work without a cost estimate"), "{grouped}");
    assert!(grouped.contains("the step this finding is about is `Group`"), "{grouped}");
    // Named by no finding, the scan isn't "the most expensive step" of a plan that doesn't cost its other steps.
    let mut unnamed = about(&mysql, "04cd7be9c366fadc", None);
    unnamed.check = "heavy-query".into();
    let brief = agent_brief(&mysql, &unnamed);
    assert!(!brief.lines().any(|l| l.starts_with("Plan:")), "{brief}");
    let scan = plan_line(&agent_brief(&mysql, &about(&mysql, "19bd4ff468839ea3", Some(2))));
    assert!(scan.contains("the step this finding is about is `Table scan` on `shop.orders`"), "{scan}");
    assert!(scan.contains("The database leaves some of this plan's work without a cost estimate"), "{scan}");
    assert!(!scan.contains("100%"), "{scan}");

    // A plan that costs every step keeps its share.
    let postgres = report("postgres-demo.json");
    let costed = plan_line(&agent_brief(&postgres, &about(&postgres, "5308963031944094559", Some(3))));
    assert!(costed.contains("the most expensive step ("), "{costed}");
    assert!(costed.contains("of estimated cost) is `Seq Scan` on `public.orders`"), "{costed}");
    assert!(!costed.contains("no cost of"), "{costed}");
}

#[test]
fn a_sqlite_step_says_what_it_does() {
    // SQLite's plan has no costs; the step's text is its point: it builds an index for itself on every run.
    let sqlite = report("sqlite-demo.json");
    let line = plan_line(&agent_brief(&sqlite, &about(&sqlite, "500217f7b40055be", Some(22))));
    assert!(line.contains("the step to look at is `Search` on `orders`"), "{line}");
    assert!(line.contains("USING AUTOMATIC COVERING INDEX"), "{line}");
    assert!(!line.contains("of estimated cost"), "{line}");
}

#[test]
fn a_share_resting_on_a_default_guess_is_left_out() {
    // The scan under the sort keeps a share the planner guessed for `created_at > now() - $1`: the sort's rows, and
    // its share of the cost, rest on that guess.
    let mut postgres = report("postgres-demo.json");
    let spill = finding(&postgres, "temp-spill:9065391607317686058").clone();
    let measured = plan_line(&agent_brief(&postgres, &spill));
    assert!(measured.contains("the step this finding is about (23% of estimated cost) is `Sort`"), "{measured}");
    let workload = postgres.snapshot.workload.as_mut().expect("workload");
    let q = workload.queries.iter_mut().find(|q| q.id == "9065391607317686058").expect("the statement");
    let scan = &mut q.plan.as_mut().expect("its plan").root.children[0].children[0];
    scan.props.insert(culprit_core::GUESSED_SELECTIVITY.into(), "0.333".into());
    let guessed = plan_line(&agent_brief(&postgres, &spill));
    assert!(guessed.contains("the step this finding is about is `Sort`"), "{guessed}");
    assert!(!guessed.contains("of estimated cost"), "{guessed}");
    assert!(guessed.contains("rest on the planner's default guess for a parameter"), "{guessed}");
}

#[test]
fn a_finding_about_no_step_names_none() {
    // A result's size isn't about where the plan does its work: no costliest step is named for it.
    let postgres = report("postgres-demo.json");
    let mut result = about(&postgres, "9065391607317686058", None);
    result.check = "big-result".into();
    let big = agent_brief(&postgres, &result);
    assert!(!big.lines().any(|l| l.starts_with("Plan:")), "{big}");

    // Nor for a plan insight about the whole plan (it runs on one CPU), which names no step of it.
    let mssql: Report = serde_json::from_str(
        &std::fs::read_to_string(format!("{}/../../fixtures/mssql-demo.report.json", env!("CARGO_MANIFEST_DIR")))
            .expect("fixture"),
    )
    .expect("report");
    let mut whole = about(&mssql, "263", None);
    whole.check = "plan-insights".into();
    let brief = agent_brief(&mssql, &whole);
    assert!(!brief.lines().any(|l| l.starts_with("Plan:")), "{brief}");
}

#[test]
fn a_cheap_plan_puts_no_share_on_its_step() {
    // An index scan costed 12 of the plan's 12: "99% of estimated cost" of a plan too cheap for any step to matter isn't
    // where the statement's time goes, as heavy-query says it.
    let postgres = report("postgres-demo.json");
    let mut cheap = about(&postgres, "2885972271926804069", Some(2));
    cheap.check = "heavy-query".into();
    let line = plan_line(&agent_brief(&postgres, &cheap));
    assert!(line.contains("the step this finding is about is `Index Scan` on `public.events`"), "{line}");
    assert!(!line.contains("of estimated cost"), "{line}");
    assert!(line.contains("The plan as a whole is cheap, so no share of its cost is put on one step."), "{line}");
}

#[test]
fn a_findings_whole_table_read_is_said_whatever_the_tables_size() {
    // A heavy query about a scan of a table of a few thousand rows: under the size bar a plan's whole read is no reason
    // worth naming, but this finding is about that read, so its Plan line says it (the plan viewer's note stays plain).
    let text = std::fs::read_to_string(format!("{}/../../fixtures/postgres-demo.json", env!("CARGO_MANIFEST_DIR")))
        .expect("fixture");
    let mut snapshot: Snapshot = serde_json::from_str(&text).expect("snapshot");
    fn node(n: &mut culprit_core::PlanNode, id: u32) -> Option<&mut culprit_core::PlanNode> {
        if n.id == id {
            return Some(n);
        }
        n.children.iter_mut().find_map(|c| node(c, id))
    }
    let q = snapshot.workload.as_mut().unwrap().queries.iter_mut().find(|q| q.id == "2885972271926804069").unwrap();
    let plan = q.plan.as_mut().expect("its plan");
    let scan = node(&mut plan.root, 2).expect("step 2");
    (scan.kind, scan.op, scan.table_rows) = (culprit_core::NodeKind::Scan, "Seq Scan".into(), Some(3_900.0));
    let postgres = analyze(snapshot);
    let mut f = about(&postgres, "2885972271926804069", Some(2));
    f.check = "heavy-query".into();
    let line = plan_line(&agent_brief(&postgres, &f));
    assert!(line.contains("but `Seq Scan` reads all 3.9K rows of public.events on every run."), "{line}");
    let q =
        postgres.snapshot.workload.as_ref().unwrap().queries.iter().find(|q| q.id == "2885972271926804069").unwrap();
    let note = culprit_checks::plan_shares_note(q.plan.as_ref().unwrap(), Some(q)).unwrap();
    assert_eq!(note, "The plan as a whole is cheap, so no share of its cost is put on one step.");
}

#[test]
fn recurring_upkeep_runs_where_it_fits() {
    // A histogram reads the whole table: a scheduled job outside busy hours, never as connections open and close.
    let mysql = report("mysql-demo.json");
    let histogram = with_fix(
        finding(&mysql, "temp-spill:"),
        "Build a histogram on promo_code, and refresh it after the data changes a lot.",
        Some("ANALYZE TABLE `shop`.`orders` UPDATE HISTOGRAM ON `promo_code` WITH 64 BUCKETS;"),
    );
    let section = codebase(&agent_brief(&mysql, &histogram));
    assert!(section.contains("outside busy hours"), "{section}");
    assert!(section.contains("its recurring database work (a scheduled job)."), "{section}");
    assert!(!section.contains("opens and closes"), "{section}");

    // SQLite's PRAGMA optimize is made to run where the application closes its connections.
    let sqlite = report("sqlite-demo.json");
    let f = sqlite
        .findings
        .iter()
        .find(|f| f.fix.as_ref().is_some_and(|x| x.summary.contains("PRAGMA optimize")))
        .expect("the statistics finding");
    let section = codebase(&agent_brief(&sqlite, f));
    assert!(section.contains("where the application opens and closes its connections"), "{section}");
}

#[test]
fn upkeep_is_said_only_for_a_fix_that_runs_it() {
    let postgres = report("postgres-demo.json");
    // A table's rows made smaller by the application: no SQL to run, so no upkeep.
    let mut rows = with_fix(finding(&postgres, "chatty-query:"), "Read only the columns a statement needs.", None);
    (rows.category, rows.queries, rows.verify) = (Category::Maintenance, Vec::new(), Vec::new());
    (rows.subject.kind, rows.subject.name) = (SubjectKind::Table, "public.orders".into());
    let section = codebase(&agent_brief(&postgres, &rows));
    assert!(!section.contains("upkeep"), "{section}");
    assert!(section.contains("search the code for the statements that touch it"), "{section}");
    // An index's subject is `table.index`: the application reads and writes its table, not the index (CX-10c).
    (rows.subject.kind, rows.subject.name) = (SubjectKind::Index, "app.job_runs.pk_job_runs".into());
    let section = codebase(&agent_brief(&postgres, &rows));
    assert!(section.contains("reads or writes `app.job_runs`, search the code"), "{section}");
    assert!(!section.contains("pk_job_runs"), "{section}");

    // Statistics refreshed for a statement's plan: upkeep, and nothing about changing the statement.
    let stats = with_fix(
        finding(&postgres, "chatty-query:"),
        "Refresh the statistics on orders.",
        Some("ANALYZE public.orders;"),
    );
    let section = codebase(&agent_brief(&postgres, &stats));
    assert!(section.contains("The fix is upkeep of the database"), "{section}");
    assert!(!section.contains("Find where the application sends the statements above"), "{section}");

    // The same for a plan warning whose fix is SQL Server's UPDATE STATISTICS.
    let mssql = report("mssql-demo.json");
    let stats = with_fix(
        finding(&mssql, "plan-warnings:"),
        "Update the statistics on customers, so its index statistics describe the rows it holds.",
        Some("UPDATE STATISTICS [shop].[customers];"),
    );
    let section = codebase(&agent_brief(&mssql, &stats));
    assert!(section.contains("The fix is upkeep of the database"), "{section}");
    assert!(!section.contains("Find where the application sends the statements above"), "{section}");
}

#[test]
fn a_fix_that_advises_against_itself_ships_nothing() {
    let mssql = report("mssql-demo.json");
    let index = finding(&mssql, "missing-index:");
    let sql = index.fix.as_ref().and_then(|f| f.sql.clone());
    let low = with_fix(
        index,
        "Azure scores it 3 out of 100: it expects the index to save little next to what it costs every write, so it's \
         usually not worth having. If the queries that read orders are slow all the same, create it online.",
        sql.as_deref(),
    );
    let section = codebase(&agent_brief(&mssql, &low));
    assert!(section.contains("The fix doesn't recommend this change as things stand"), "{section}");
    assert!(!section.contains("Ship the fix's index change"), "{section}");
    assert!(!section.contains("Find where the application sends the statements above"), "{section}");
    // No change to wait for: the next scan shows where the finding stands.
    assert!(!section.contains("After any change is live"), "{section}");
    assert!(section.contains("A later `culprit scan mssql-demo` shows where"), "{section}");

    let mut nothing = with_fix(index, "Nothing to create: an index already finds these rows.", None);
    let section = codebase(&agent_brief(&mssql, &nothing));
    assert!(section.contains("there is nothing to do in the codebase"), "{section}");
    assert!(!section.contains("After any change is live"), "{section}");
    nothing.verify.clear();
    let section = codebase(&agent_brief(&mssql, &nothing));
    assert!(section.contains(&format!("shows whether `{}` still applies", nothing.id)), "{section}");
}

#[test]
fn a_setting_in_a_statement_fix_is_applied_where_the_server_is_managed() {
    // SET PERSIST is the server's: where the server's configuration is managed.
    let mysql = report("mysql-demo.json");
    let section = codebase(&agent_brief(&mysql, finding(&mysql, "temp-spill:")));
    assert!(section.contains("The setting in the fix's SQL belongs to the database server"), "{section}");
    assert!(
        section.contains(
            "Where nothing manages it, run the setting's statements from the fix's SQL against the server as a login \
             allowed to change its settings."
        ),
        "{section}"
    );
    assert!(!section.contains("SET LOCAL"), "{section}");
}

#[test]
fn an_index_change_keeps_what_its_engine_needs() {
    // PostgreSQL's CONCURRENTLY can't run in the transaction a migration tool wraps each migration in.
    let postgres = report("postgres-demo.json");
    let section = codebase(&agent_brief(&postgres, finding(&postgres, "duplicate-index:")));
    assert!(section.contains("keeping the options the fix's SQL already has"), "{section}");
    assert!(section.contains("`CONCURRENTLY` can't run inside a transaction"), "{section}");
    assert!(section.contains("disable_ddl_transaction!"), "{section}");
    assert!(!section.contains("ONLINE/CONCURRENTLY"), "{section}");

    // SQLite builds an index under the write lock: when writes are quiet.
    let sqlite = report("sqlite-demo.json");
    let section = codebase(&agent_brief(&sqlite, finding(&sqlite, "unindexed-fk:")));
    assert!(section.contains("run it when writes are quiet"), "{section}");
    assert!(!section.contains("CONCURRENTLY"), "{section}");
}

#[test]
fn a_statement_that_ran_once_is_scanned_again_only_if_it_runs_again() {
    let mut postgres = report("postgres-demo.json");
    let f = finding(&postgres, "chatty-query:").clone();
    let workload = postgres.snapshot.workload.as_mut().expect("workload");
    for r in &f.queries {
        workload.queries.iter_mut().filter(|q| q.id == r.id).for_each(|q| q.executions = 1.0);
    }
    let section = codebase(&agent_brief(&postgres, &f));
    assert!(section.contains("Change nothing unless it keeps running."), "{section}");
    assert!(section.contains("If it runs again, run `culprit scan"), "{section}");
    assert!(!section.contains("After any change is live"), "{section}");
}

#[test]
fn without_a_verify_step_the_brief_says_what_a_later_scan_shows() {
    // An index dropped is gone from the next scan's catalog.
    let mssql = report("mssql-demo.json");
    let mut dropped = finding(&mssql, "duplicate-index:").clone();
    dropped.verify.clear();
    let section = codebase(&agent_brief(&mssql, &dropped));
    assert!(section.contains(&format!("`{}` should be gone", dropped.id)), "{section}");
    assert!(!section.contains("with this scan"), "{section}");

    // pg_stat_statements keeps totals since its reset: the runs before the change stay in them. The brief says what to
    // compare: the finding's numbers under *Evidence*.
    let postgres = report("postgres-demo.json");
    let mut spill = finding(&postgres, "temp-spill:").clone();
    spill.verify.clear();
    let section = codebase(&agent_brief(&postgres, &spill));
    assert!(
        section.contains(&format!("compare `{}`'s numbers under *Evidence* with this scan's", spill.id)),
        "{section}"
    );
    assert!(
        section.contains("pg_stat_statements keeps running totals, so the runs from before the change stay in them"),
        "{section}"
    );
    assert!(!section.contains("should be gone"), "{section}");
}

#[test]
fn a_verify_step_on_totals_since_a_reset_says_they_keep_the_old_runs() {
    // performance_schema keeps totals since the server's restart: a run count only grows, whatever *How to verify*
    // says to look for. Its own step says so already: the brief points at it and doesn't say it again.
    let mysql = report("mysql-demo.json");
    let chatty = finding(&mysql, "chatty-query:");
    assert!(chatty.verify.iter().any(|v| v.contains("performance_schema keeps running totals")), "{:?}", chatty.verify);
    let brief = agent_brief(&mysql, chatty);
    let section = codebase(&brief);
    assert!(section.trim_end().ends_with("the way *How to verify* says."), "{section}");
    assert_eq!(brief.matches("keeps running totals").count(), 1, "{brief}");
    // A step that doesn't say it: the brief does.
    let mut quiet = chatty.clone();
    quiet.verify = vec!["Re-scan: it should run fewer times.".into()];
    let section = codebase(&agent_brief(&mysql, &quiet));
    assert!(
        section.contains("the way *How to verify* says: performance_schema keeps running totals, so the runs from before the change stay in them"),
        "{section}"
    );

    // Query Store's window can leave the old runs out: no such clause.
    let mssql = report("mssql-demo.json");
    let heavy = finding(&mssql, "heavy-query:");
    let section = codebase(&agent_brief(&mssql, heavy));
    assert!(!section.contains("since its last reset"), "{section}");
}

#[test]
fn slow_runs_that_waited_change_no_statement() {
    // An insert whose slow runs may have waited (another transaction inserting the same key, or holding the row its
    // foreign key checks): the work is in the transactions around it, and its slowest run and spread are kept since the
    // statistics' reset, so a scan can't show the change.
    let mut postgres = report("postgres-demo.json");
    let workload = postgres.snapshot.workload.as_mut().expect("workload");
    let mut insert = workload.queries[0].clone();
    (insert.id, insert.text, insert.plan) =
        ("insert".into(), "INSERT INTO order_items (order_id, product_id, quantity) VALUES ($1, $2, $3)".into(), None);
    workload.queries.push(insert);
    let mut waited = with_fix(
        finding(&postgres, "chatty-query:"),
        "Find what the slow runs wait for while they happen. Keep those transactions short.",
        None,
    );
    (waited.check, waited.verify) = ("unstable-query".into(), Vec::new());
    waited.queries = vec![QueryRef { id: "insert".into(), plan_node: None, note: None }];
    let section = codebase(&agent_brief(&postgres, &waited));
    assert!(section.contains("The fix changes no statement"), "{section}");
    // The summary offers the wait as a possibility; the brief doesn't settle it.
    assert!(section.contains("the slow runs may have waited rather than worked"), "{section}");
    assert!(!section.contains("the slow runs waited rather than worked"), "{section}");
    assert!(section.contains("writes to `order_items`"), "{section}");
    // Who holds the referenced rows is the fix's to say, in its engine's words (a non-key update doesn't block
    // PostgreSQL's FOR KEY SHARE).
    assert!(
        section
            .contains("or holds the rows it references in `public.products` and `public.orders` the way the fix says"),
        "{section}"
    );
    assert!(!section.contains("updates or deletes"), "{section}");
    assert!(section.contains("keep those transactions short"), "{section}");
    assert!(!section.contains("Find where the application sends the statements above"), "{section}");
    assert!(!section.contains("with this scan"), "{section}");
    assert!(section.contains("can't show the change"), "{section}");
    assert!(section.contains("Catch the next slow runs instead, the way the fix says"), "{section}");
    // A statement that stopped running has nothing to catch.
    waited.inactive = true;
    let section = codebase(&agent_brief(&postgres, &waited));
    assert!(section.contains("There is nothing to check unless it runs again"), "{section}");
    assert!(section.contains("even then a later `culprit scan pg-demo` can't show a change"), "{section}");
    assert!(!section.contains("Catch the next slow runs") && !section.contains("culprit live"), "{section}");
    // Once refine has seen it stop, *How to verify* says there is nothing to check: the brief doesn't say it again.
    waited.verify = vec![
        "Nothing to check unless it runs again: a later scan that sees it run lists this finding as current again."
            .into(),
    ];
    let section = codebase(&agent_brief(&postgres, &waited));
    assert!(!section.to_lowercase().contains("nothing to check"), "{section}");
    assert!(section.contains("If it runs again, even a later `culprit scan pg-demo` can't show a change"), "{section}");
}

#[test]
fn a_statement_that_waited_is_not_rewritten() {
    // Heavy queries whose time is a pause, an application lock or locks other transactions hold: no rewrite of the
    // statement shortens it, so the brief sends the agent to the code around it.
    let mut postgres = report("postgres-demo.json");
    let workload = postgres.snapshot.workload.as_mut().expect("workload");
    for (id, text) in [("lock", "SELECT pg_advisory_lock($1)"), ("sleep", "SELECT pg_sleep($1)")] {
        let mut q = workload.queries[0].clone();
        (q.id, q.text, q.plan) = (id.into(), text.into(), None);
        workload.queries.push(q);
    }
    let about_query = |report: &Report, id: &str| {
        let mut f = with_fix(finding(report, "heavy-query:"), "Find what holds it.", None);
        f.queries = vec![QueryRef { id: id.into(), plan_node: None, note: None }];
        f
    };
    let section = codebase(&agent_brief(&postgres, &about_query(&postgres, "sleep")));
    assert!(section.contains("Its time is a pause the statement asks for"), "{section}");
    assert!(!section.contains("Find where the application sends the statements above"), "{section}");
    let section = codebase(&agent_brief(&postgres, &about_query(&postgres, "lock")));
    assert!(section.contains("waiting for an application lock another session holds"), "{section}");
    assert!(!section.contains("Find where the application sends the statements above"), "{section}");

    // MySQL counts the lock time: an update that spent most of its time waiting for row locks.
    let mut mysql = report("mysql-demo.json");
    let workload = mysql.snapshot.workload.as_mut().expect("workload");
    let mut update = workload.queries[0].clone();
    (update.id, update.text, update.plan, update.spread) =
        ("update".into(), "UPDATE orders SET status = ? WHERE id = ?".into(), None, None);
    (update.executions, update.total_time_ms, update.mean_time_ms) = (10.0, 5000.0, 500.0);
    update.waits =
        vec![culprit_core::WaitStat { category: "Lock".into(), wait_type: None, wait_ms: 4500.0, tasks: None }];
    workload.queries.push(update);
    let section = codebase(&agent_brief(&mysql, &about_query(&mysql, "update")));
    assert!(section.contains("waiting for locks other transactions hold"), "{section}");
    assert!(section.contains("so no rewrite or index of it shortens it"), "{section}");
    assert!(!section.contains("Find where the application sends the statements above"), "{section}");

    // The same statement doing work is changed in the application.
    let section = codebase(&agent_brief(&mysql, &with_fix(finding(&mysql, "heavy-query:"), "Read less.", None)));
    assert!(section.contains("Find where the application sends the statements above"), "{section}");

    // lock-time is a statement's time spent waiting, whichever statement: its fix says making it faster won't help, so
    // its brief sends the agent to the transactions that hold the locks, not to a rewrite.
    let saved =
        std::fs::read_to_string(format!("{}/../../fixtures/mysql-demo.report.json", env!("CARGO_MANIFEST_DIR")))
            .expect("fixture");
    let demo = analyze(serde_json::from_str::<Report>(&saved).expect("report").snapshot);
    for f in demo.findings.iter().filter(|f| f.check == "lock-time") {
        let section = codebase(&agent_brief(&demo, f));
        assert!(!section.contains("an ORM change or a query rewrite"), "{}: {section}", f.id);
        assert!(section.contains("keep those transactions short"), "{}: {section}", f.id);
    }
    assert!(demo.findings.iter().any(|f| f.check == "lock-time"), "the demo's lock-time finding");
    // Settings-only SQL catches the next wait; it doesn't turn the fix into a rewrite of the waiting statement.
    let mut f = demo.findings.iter().find(|f| f.check == "lock-time").unwrap().clone();
    for sql in ["ALTER SYSTEM SET log_lock_waits = 'on';\nSELECT pg_reload_conf();", "SET GLOBAL slow_query_log = ON;"]
    {
        f.fix.as_mut().unwrap().sql = Some(sql.into());
        let section = codebase(&agent_brief(&demo, &f));
        assert!(section.contains("keep those transactions short"), "{section}");
        assert!(!section.contains("an ORM change or a query rewrite"), "{section}");
        assert!(section.contains("setting in the fix's SQL belongs to the database server"), "{section}");
    }
}

#[test]
fn a_plan_note_does_not_invent_a_row_misestimate_from_large_reads_or_hide_a_few_runs() {
    let report = report("mssql-demo.json");
    let mut q = report.snapshot.workload.unwrap().queries[0].clone();
    let mut plan = q.plan.clone().unwrap();
    plan.root.children.clear();
    plan.root.object = None;
    plan.root.props.clear();
    plan.root.kind = culprit_core::NodeKind::Modify;
    plan.root.est_rows = 1.0;
    plan.total_cost = 0.001;
    plan.root.subtree_cost = 0.001;
    plan.root.self_cost = 0.001;
    q.plans.clear();
    q.plan_id = None;
    q.executions = 1.0;
    q.rows = Some(1.0);
    q.logical_reads = Some(72.0);
    let note = culprit_checks::plan_shares_note(&plan, Some(&q)).unwrap();
    assert!(note.contains("72 pages a run"), "{note}");
    assert!(!note.contains("costs rest on an estimate of 1 row"), "{note}");
    q.executions = 4.0;
    q.rows = Some(4.0);
    q.logical_reads = Some(4.0);
    q.total_time_ms = 2404.0;
    q.mean_time_ms = 601.0;
    q.max_time_ms = Some(2400.0);
    let note = culprit_checks::plan_shares_note(&plan, Some(&q)).unwrap();
    assert!(note.contains("4 runs took 2.4 s in all, with the slowest taking 2.4 s"), "{note}");
    assert!(!note.contains("601 ms"), "{note}");
}

#[test]
fn statements_listed_for_reference_or_caught_by_a_diagnostic_are_not_rewritten() {
    // lock-escalations: the writes are listed for reference, and the fix's SQL starts an event session that catches the
    // statement to change.
    let mssql = report("mssql-demo.json");
    let session = "-- this session keeps the next ones with their statement\n\
                   CREATE EVENT SESSION culprit_lock_escalation ON DATABASE\n\
                   ADD EVENT sqlserver.lock_escalation (SET collect_statement = 1)\n\
                   ADD TARGET package0.ring_buffer;\n\
                   ALTER EVENT SESSION culprit_lock_escalation ON DATABASE STATE = START;\n\
                   -- Then read what it caught\n\
                   SELECT CAST(t.target_data AS xml) AS d FROM sys.dm_xe_database_session_targets t;";
    let f = with_fix(
        finding(&mssql, "heavy-query:"),
        "To learn which statement it is, start the event session.",
        Some(session),
    );
    let section = codebase(&agent_brief(&mssql, &f));
    assert!(!section.contains("Find where the application sends the statements above"), "{section}");
    assert!(section.contains("starts an event session on the database"), "{section}");
    assert!(section.contains("The statement to change is the one it catches"), "{section}");
    // CX-12: created only where it isn't defined yet, it is still an event session; the brief says how to stop it.
    let guarded = "IF NOT EXISTS (SELECT 1 FROM sys.database_event_sessions WHERE name = N'culprit_x')\n    \
                   CREATE EVENT SESSION culprit_x ON DATABASE\n    ADD TARGET package0.ring_buffer;\n\
                   IF NOT EXISTS (SELECT 1 FROM sys.dm_xe_database_sessions WHERE name = N'culprit_x')\n    \
                   ALTER EVENT SESSION culprit_x ON DATABASE STATE = START;\n\
                   -- Once it has caught what you need, stop and remove it: DROP EVENT SESSION culprit_x ON DATABASE;";
    let f = with_fix(finding(&mssql, "heavy-query:"), "Start the event session.", Some(guarded));
    let section = codebase(&agent_brief(&mssql, &f));
    assert!(section.contains("The fix's SQL starts an event session on the database: run it there"), "{section}");
    assert!(section.contains("remove it with the DROP in the SQL's last line"), "{section}");
    assert!(!section.contains("reads what it catches"), "{section}");

    // An index advised for statements that stay as they are: the index is the whole change.
    let mut index = with_fix(
        finding(&mssql, "heavy-query:"),
        "Create it online, outside the busiest hours.",
        Some("CREATE NONCLUSTERED INDEX [ix] ON [shop].[orders] ([status]) WITH (ONLINE = ON)"),
    );
    index.category = Category::Indexes;
    let section = codebase(&agent_brief(&mssql, &index));
    assert!(!section.contains("Find where the application sends the statements above"), "{section}");
    assert!(section.contains("Ship the fix's index change"), "{section}");
}

#[test]
fn an_option_in_the_application_is_kept_beside_the_query_store_one() {
    // unstable-query on SQL Server: a Query Store hint, or OPTIMIZE FOR UNKNOWN appended in the application. The second
    // lives only in a comment of the fix's SQL.
    let mssql = report("mssql-demo.json");
    let f = with_fix(
        finding(&mssql, "heavy-query:"),
        "Plan each call for its own values, or for the typical value.",
        Some(
            "-- 1. Plan each call for its own values (costs compile CPU per call):\n\
             EXEC sys.sp_query_store_set_hints @query_id = 10, @query_hints = N'OPTION (RECOMPILE)';\n\n\
             -- 2. Or in the application, plan for the typical value instead of the first one seen:\n\
             --    append OPTION (OPTIMIZE FOR UNKNOWN) to the statement",
        ),
    );
    let section = codebase(&agent_brief(&mssql, &f));
    assert!(section.contains("The fix's Query Store procedure runs against the database"), "{section}");
    assert!(section.contains("The fix's other option is a change in the application's code"), "{section}");
}

#[test]
fn the_brief_never_tells_its_reader_to_copy_the_brief() {
    let postgres = report("postgres-demo.json");
    let f = with_fix(
        finding(&postgres, "chatty-query:"),
        "Find this statement in the application (copy the brief for your coding agent) and check whether it reads \
         more rows or columns than it needs.",
        None,
    );
    let brief = agent_brief(&postgres, &f);
    assert!(!brief.contains("copy the brief"), "{brief}");
    assert!(brief.contains("Find this statement in the application and check whether it reads"), "{brief}");
}

#[test]
fn a_total_wait_in_the_evidence_reads_as_its_title_says_it() {
    // 152 s of row-lock waits: "2 min 32 s", as the title says, where a rounded "3 min" would disagree with it.
    let postgres = report("postgres-demo.json");
    let mut f = finding(&postgres, "chatty-query:").clone();
    f.metrics = vec![
        Metric::new("Time waiting since start", 152_000.0, Unit::Ms),
        Metric::new("Time waiting", 88_960.0, Unit::Ms),
        Metric::new("Average duration", 152_000.0, Unit::Ms),
    ];
    let brief = agent_brief(&postgres, &f);
    assert!(brief.contains("- Time waiting since start: 2 min 32 s"), "{brief}");
    assert!(brief.contains("- Time waiting: 1 min 29 s"), "{brief}");
    // Other times keep their usual rounding.
    assert!(brief.contains("- Average duration: 3 min"), "{brief}");
}

#[test]
fn a_statement_quoted_in_a_caveat_is_the_one_to_look_for() {
    // query-family lists no statement of the history, but quotes one copy as recorded: the brief points at it rather
    // than saying no statement is named.
    let postgres = report("postgres-demo.json");
    let mut family = with_fix(
        finding(&postgres, "chatty-query:"),
        "In the code that sends this statement (one copy is quoted below, as recorded), pass value lists as one array \
         parameter.",
        None,
    );
    (family.check, family.queries, family.verify) = ("query-family".into(), Vec::new(), Vec::new());
    family.caveats = vec!["One of them, as recorded:\nSELECT count(*) FROM (VALUES ($1),($2)) v(x)".into()];
    let section = codebase(&agent_brief(&postgres, &family));
    assert!(section.contains("The statement to look for is quoted under *Caveats*, as recorded"), "{section}");
    assert!(!section.contains("No statement"), "{section}");
    // With the fix's SQL as a fallback, the application change points at the quoted statement too.
    let mut fallback = with_fix(
        &family,
        "Send values as parameters.",
        Some(
            "-- Prefer fixing the app. As a database-wide fallback:\nALTER DATABASE [demo] SET PARAMETERIZATION FORCED;",
        ),
    );
    fallback.caveats = family.caveats.clone();
    let section = codebase(&agent_brief(&postgres, &fallback));
    assert!(
        section.contains(
            "find where it sends the statement quoted under *Caveats* (search for its table and column names) and \
             change it"
        ),
        "{section}"
    );
}

#[test]
fn a_heavy_query_line_states_the_share_its_finding_states() {
    // A statement's query line says the share every surface states it at (`Workload::statement_share`: over Culprit's
    // scans where they measured it), which a heavy query's own finding is weighed by; another check's share is its own
    // (the slow runs' extra time), and the line keeps the statement's.
    let postgres = report("postgres-demo.json");
    let mut heavy = finding(&postgres, "heavy-query:").clone();
    let w = postgres.snapshot.workload.as_ref().expect("a workload");
    let q = w.query(&heavy.queries[0].id).expect("its statement");
    assert_eq!(heavy.impact.db_time_share, Some(w.statement_share(q)));
    let said = format!(" — {} of DB time, ", culprit_core::format::percent(w.statement_share(q)));
    heavy.impact.db_time_share = Some(0.42);
    for check in ["heavy-query", "unstable-query"] {
        heavy.check = check.into();
        let brief = agent_brief(&postgres, &heavy);
        let line = brief.lines().find(|l| l.starts_with("Query `")).expect("a query line");
        assert!(line.contains(&said) && !line.contains(" — 42% of DB time, "), "{line}");
    }
}
