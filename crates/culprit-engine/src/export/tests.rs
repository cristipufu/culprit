use super::*;

#[test]
fn file_names_are_short_safe_and_lowercase() {
    assert_eq!(
        file_stem("A type mismatch on customers.email stops a read"),
        "a-type-mismatch-on-customers-email-stops-a-read"
    );
    assert_eq!(file_stem("../../etc/passwd"), "etc-passwd");
    assert_eq!(file_stem("   "), "culprit");
    assert!(file_stem(&"x".repeat(500)).len() <= 61);
}

#[test]
fn native_exports_keep_the_engine_format() {
    let mut plan: Plan = serde_json::from_value(serde_json::json!({
        "engine": "postgres", "kind": "estimated", "statement": null, "total_cost": 1.0, "warnings": [],
        "index_hints": [], "props": {}, "raw": "[{\"Plan\": {}}]",
        "root": {"id": 0, "op": "Seq Scan", "kind": "scan", "detail": null, "object": "orders", "index": null,
            "seek_predicate": null, "predicate": "(status = 'open'::text)", "est_rows": 1.0, "est_executions": 1.0,
            "act_rows": null, "act_executions": null, "rows_read": null, "table_rows": null, "subtree_cost": 1.0,
            "self_cost": 1.0, "act_time_ms": null, "warnings": [], "props": {}, "children": []}
    }))
    .unwrap();
    assert_eq!(native_extension(&plan), "json");
    plan.raw = Some("Seq Scan on orders  (cost=0.00..1.00 rows=1 width=4)".into());
    assert_eq!(native_extension(&plan), "txt");
    plan.raw = Some("<ShowPlanXML/>".into());
    assert_eq!(native_extension(&plan), "sqlplan");
    plan.raw = None;
    assert!(plan_native(&plan, &ExportOptions::default()).is_none());
}

#[test]
fn redaction_is_on_unless_asked() {
    assert!(ExportOptions::default().redact);
    let parsed: ExportOptions = serde_json::from_str("{}").unwrap();
    assert!(parsed.redact && parsed.notes.is_empty());
}

#[cfg(feature = "yugabyte")]
#[test]
fn retained_native_exports_preserve_missing_costs_and_row_estimates() {
    let mut plan = culprit_yugabyte::plan::parse(
        include_str!("../../../culprit-yugabyte/tests/data/qpm.json"),
        culprit_core::PlanKind::Estimated,
        None,
    )
    .unwrap();
    culprit_checks::present_retained_plan(&mut plan, "Independent native plan-cache period");
    let options = ExportOptions::default();
    let markdown = plan_markdown(&plan, "Retained plan", &options);
    let html = plan_html(&plan, "Retained plan", &options);
    assert!(markdown.contains("estimated cost unavailable") && markdown.contains("| unavailable |"));
    assert!(
        html.contains("Row estimate unavailable") && html.contains("unavailable</div><div class=\"l\">estimated cost")
    );
    assert!(!html.contains("0 rows") && !html.contains("0.000"));
    let diff = culprit_core::plan_diff(&plan, &plan);
    assert!(
        diff_markdown(&plan, &plan, &diff, "A", "B", &options)
            .contains("| Estimated cost | unavailable | unavailable |")
    );
    assert!(!diff_html(&plan, &plan, &diff, "A", "B", &options).contains("0.000"));
}

#[test]
fn a_plan_file_never_puts_markup_into_an_html_page() {
    let text = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/plans/mariadb-index.json")).unwrap();
    let mut plan = crate::plans::parse_plan(&text).unwrap();
    assert_eq!(plan_engine_label(&plan), "MariaDB");
    // A plan opened from a saved JSON file can carry any properties.
    for key in ["flavor", "server_version", "query_cost", "dop"] {
        plan.props.insert(key.into(), "<img src=x onerror=alert(1)>".into());
    }
    plan.facts_mut().flavor = Some("<img src=x onerror=alert(1)>".into());
    plan.facts_mut().server_version = Some("<img src=x onerror=alert(1)>".into());
    assert_eq!(plan_engine_label(&plan), "MySQL");
    let options = ExportOptions::default();
    let page = plan_html(&plan, "Plan", &options);
    assert!(!page.contains("onerror"), "plan page");
    let diff = culprit_core::plan_diff_named(&plan, "A", &plan, "B");
    assert!(!diff_html(&plan, &plan, &diff, "A", "B", &options).contains("onerror"), "diff page");
}

/// The demo's report with its first query finding saying which values a plan was compiled for, as the
/// parameter-sensitivity check does.
fn report_quoting_values() -> (Report, String) {
    let text = std::fs::read_to_string(format!("{}/../../fixtures/mssql-demo.json", env!("CARGO_MANIFEST_DIR")))
        .expect("fixture");
    let mut report = culprit_checks::analyze(serde_json::from_str(&text).expect("snapshot"));
    let f = report.findings.iter_mut().find(|f| !f.queries.is_empty()).expect("a query finding");
    f.summary = format!(
        "{} It was compiled for `@p0 = N'9b9143e3-90c5-f111-bf5b-000d3aacaa10', @p1 = N'secret-ish'`.",
        f.summary
    );
    let id = f.id.clone();
    (report, id)
}

#[test]
fn redacted_exports_and_digest_details_drop_values_quoted_in_prose() {
    let (report, id) = report_quoting_values();
    let finding = report.findings.iter().find(|f| f.id == id).expect("finding");
    let markdown = finding_markdown(&report, finding, &ExportOptions { redact: true, ..Default::default() });
    let html = finding_html(&report, finding, &ExportOptions { redact: true, ..Default::default() });
    let detail = serde_json::to_string(&finding_detail(&report, finding)).expect("json");
    for out in [&markdown, &html, &detail] {
        assert!(!out.contains("9b9143e3") && !out.contains("secret-ish"), "{out}");
    }
    // The export renders the finding as checked from the redacted snapshot: text added to the original isn't in it.
    assert!(!markdown.contains("It was compiled for"), "{markdown}");
    // Asked for the raw text, an export keeps it.
    let raw = finding_markdown(&report, finding, &ExportOptions { redact: false, ..Default::default() });
    assert!(raw.contains("9b9143e3"));
    // The detail carries the query with its statement, and where its plan spends.
    let detail = finding_detail(&report, finding);
    assert!(!detail.queries.is_empty() && !detail.queries[0].sql.is_empty());
    assert!(detail.queries[0].stats.contains("of database time"));
}

#[test]
fn shared_fixes_keep_generated_sql_constants_and_mask_unrechecked_plugin_values() {
    let report: Report = serde_json::from_str(include_str!("../../../../fixtures/mssql-demo.report.json")).unwrap();
    let finding = report.findings.iter().find(|f| f.id == "failing-query:246").unwrap();
    let shared = shared_report(&report);
    let checked = shared.findings.iter().find(|f| f.id == finding.id).unwrap();
    let expected = checked.fix.as_ref().unwrap().sql.as_ref().unwrap();
    assert!(expected.contains("severity >= 11") && expected.contains("SELECT 1"));
    let exported = shareable(&report, &shared, finding);
    assert_eq!(exported.fix.as_ref().unwrap().sql.as_ref(), Some(expected));
    assert_eq!(finding_detail(&report, finding).fix.unwrap().sql.as_ref(), Some(expected));
    assert_eq!(finding_detail_shared(&shared, checked).fix.unwrap().sql.as_ref(), Some(expected));
    assert!(finding_markdown(&report, finding, &ExportOptions::default()).contains("severity >= 11"));

    let mut plugin = finding.clone();
    plugin.id = "external-check:orders".into();
    plugin.check = "external-check".into();
    plugin.fix.as_mut().unwrap().sql = Some("SELECT 'private-plugin-value', 424242".into());
    for output in [
        serde_json::to_string(&shareable(&report, &shared, &plugin)).unwrap(),
        serde_json::to_string(&finding_detail(&report, &plugin)).unwrap(),
        finding_markdown(&report, &plugin, &ExportOptions::default()),
        finding_html(&report, &plugin, &ExportOptions::default()),
    ] {
        assert!(!output.contains("private-plugin-value") && !output.contains("424242"));
    }
}

#[test]
fn shared_findings_explain_deadlock_collection_limits_without_exposing_private_notes() {
    for (fixture, expected) in [
        (include_str!("../../../../fixtures/mariadb-demo.report.json"), "innodb_print_all_deadlocks"),
        (include_str!("../../../../fixtures/pg-demo.report.json"), "server log"),
    ] {
        let mut report: Report = serde_json::from_str(fixture).unwrap();
        report.snapshot.deadlocks.as_mut().unwrap().note = Some("private-collector-endpoint".into());
        let shared = shared_report(&report);
        let finding = shared.findings.iter().find(|f| f.check == "deadlocks").unwrap();
        let markdown = finding_markdown(&shared, finding, &ExportOptions::default());
        assert!(markdown.contains(expected), "{markdown}");
        assert!(!markdown.contains("private-collector-endpoint"));
    }
}

/// The demo's snapshot with values planted everywhere data can reach a scan: in statements, in plan predicates, and
/// in the values plans were compiled for, one of them holding a backtick and a quote to break naive parsing.
fn report_with_planted_values() -> Report {
    let text = std::fs::read_to_string(format!("{}/../../fixtures/mssql-demo.json", env!("CARGO_MANIFEST_DIR")))
        .expect("fixture");
    let mut snapshot: culprit_core::Snapshot = serde_json::from_str(&text).expect("snapshot");
    let w = snapshot.workload.as_mut().expect("workload");
    for q in &mut w.queries {
        q.text = format!("{} /* x */ AND note = N'PLANTED-1'", q.text);
        if let Some(plan) = &mut q.plan {
            plan.root.predicate = Some("[shop].[t].[c]=N'PLANTED-2' AND [d]=N'a`b'".into());
        }
        for p in &mut q.plans {
            p.compiled = Some("@p0 = N'x`y''z', @p1 = N'PLANTED-3', @p2 = (424242)".into());
        }
    }
    culprit_checks::analyze(snapshot)
}

#[test]
fn shared_text_is_checked_from_a_redacted_snapshot() {
    let report = report_with_planted_values();
    let options = ExportOptions { redact: true, ..Default::default() };
    let shared = shared_report(&report);
    let mut outputs = vec![report_markdown(&report, &options), report_html(&report, &options)];
    for f in &report.findings {
        outputs.push(finding_markdown(&report, f, &options));
        outputs.push(serde_json::to_string(&shareable(&report, &shared, f)).expect("json"));
    }
    for f in &shared.findings {
        outputs.push(serde_json::to_string(&finding_detail(&shared, f)).expect("json"));
    }
    for out in &outputs {
        for planted in ["PLANTED-1", "PLANTED-2", "PLANTED-3", "424242", "a`b", "x`y"] {
            assert!(!out.contains(planted), "{planted} left in:\n{out}");
        }
    }
    // The re-check finds the same problems.
    let ids = |r: &Report| r.findings.iter().map(|f| f.id.clone()).collect::<Vec<_>>();
    assert_eq!(ids(&shared), ids(&report));
}

#[test]
fn a_step_the_plan_puts_no_cost_on_gets_no_share_in_an_export() {
    // MySQL costs the scan and nothing above it: no "100%" for the scan beside an uncosted filesort, and no "0%" for
    // the steps it doesn't cost. The export says why there is no share instead.
    let text = std::fs::read_to_string(format!("{}/../../fixtures/mysql-demo.json", env!("CARGO_MANIFEST_DIR")))
        .expect("fixture");
    let report = culprit_checks::analyze(serde_json::from_str(&text).expect("snapshot"));
    let finding = report.findings.iter().find(|f| f.id == "heavy-query:19bd4ff468839ea3").expect("the scan's finding");
    let options = ExportOptions { redact: false, ..Default::default() };
    let markdown = finding_markdown(&report, finding, &options);
    let line = markdown.lines().find(|l| l.starts_with("Plan:")).expect("its step");
    assert!(!line.contains("of the plan's cost"), "{line}");
    assert!(line.contains("has no cost of its own in the plan, so no share is given"), "{line}");
    let html = finding_html(&report, finding, &options);
    assert!(!html.contains("100% of the plan's cost") && html.contains("no share is given"), "{html}");
    let plan =
        report.snapshot.workload.as_ref().and_then(|w| w.query("19bd4ff468839ea3")?.plan.as_ref()).expect("plan");
    let table = plan_markdown(plan, "plan", &options);
    assert!(!table.contains("| 100% |") && !table.contains("| 0% |"), "{table}");
    assert!(table.contains("no share is given"), "{table}");
}

#[test]
fn a_total_wait_reads_to_the_second_as_its_title_says_it() {
    // 152 s waiting: "2 min 32 s", as the finding's title says it, not a rounded "3 min" under it.
    let text = std::fs::read_to_string(format!("{}/../../fixtures/mssql-demo.json", env!("CARGO_MANIFEST_DIR")))
        .expect("fixture");
    let report = culprit_checks::analyze(serde_json::from_str(&text).expect("snapshot"));
    let mut finding = report.findings[0].clone();
    finding.id = "row-lock-waits:test".into();
    finding.metrics = vec![
        culprit_core::Metric::new("Time waiting since start", 152_000.0, culprit_core::Unit::Ms),
        culprit_core::Metric::new("Average wait", 2_500.0, culprit_core::Unit::Ms),
    ];
    let options = ExportOptions { redact: false, ..Default::default() };
    let markdown = finding_markdown(&report, &finding, &options);
    assert!(markdown.contains("2 min 32 s") && !markdown.contains("3 min"), "{markdown}");
    assert!(markdown.contains("2.5 s"), "{markdown}");
    let html = finding_html(&report, &finding, &options);
    assert!(html.contains("2 min 32 s"), "{html}");
}

#[test]
fn text_metrics_keep_their_label_and_full_escaped_evidence() {
    let (mut report, id) = report_quoting_values();
    let finding = report.findings.iter_mut().find(|f| f.id == id).expect("finding");
    let evidence = format!("{}\n<img src=x onerror=alert(1)> & details", "long_identifier_".repeat(30));
    finding.metrics = vec![
        culprit_core::Metric::new("Rows", 486_000.0, culprit_core::Unit::Rows),
        culprit_core::Metric::text("Evidence <label>", &evidence),
    ];
    let finding = finding.clone();
    let options = ExportOptions { redact: false, ..Default::default() };
    for html in [finding_html(&report, &finding, &options), report_html(&report, &options)] {
        assert!(html.contains(r#"<div class="fig"><div class="v">486K</div><div class="l">Rows</div>"#));
        assert!(html.contains(&format!(
            r#"<div class="fig metric-text"><div class="l">Evidence &lt;label&gt;</div><div class="v">{}</div>"#,
            super::svg::escape(&evidence)
        )));
        assert!(!html.contains("<img src=x") && !html.contains("Evidence <label>"));
    }
}

#[test]
fn sharing_uses_retained_input_and_never_copies_private_archived_evidence() {
    let mut report = report_with_planted_values();
    let tables = report.analysis_input().tables.len();
    let mut collected = report.analysis_input().clone();
    collected.workload.as_mut().unwrap().queries[0].text = "SELECT 'ARCHIVED_PRIVATE_VALUE'".into();
    let evidence = report.evidence.as_mut().unwrap();
    evidence.collected = Some(Box::new(collected));
    evidence.recorded_changes.push(culprit_core::ChangeEvent::new(
        culprit_core::EventKind::Table,
        culprit_core::EventSource::Database,
        "private",
        "2026-10-10T00:00:00Z",
        "ARCHIVED_PRIVATE_CHANGE",
    ));
    report.snapshot.tables.clear();
    let shared = shared_report(&report);
    assert_eq!(shared.snapshot.tables.len(), tables);
    let evidence = shared.evidence.as_ref().unwrap();
    assert!(evidence.collected.is_none());
    assert!(evidence.recorded_changes.is_empty());
    let json = serde_json::to_string(&shared).unwrap();
    for private in ["ARCHIVED_PRIVATE_VALUE", "ARCHIVED_PRIVATE_CHANGE", "PLANTED-1", "PLANTED-2", "PLANTED-3"] {
        assert!(!json.contains(private), "{private} escaped through shared replay evidence");
    }
}
