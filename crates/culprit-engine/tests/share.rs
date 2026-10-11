//! Compare, scenarios, exports, redaction and verification on real plans and reports from the SQL Server, Postgres and
//! MySQL demo databases: every engine gets the same treatment.

use std::collections::HashSet;

use culprit_core::{ChangeKind, Engine, Plan, Report, ScenarioKind, VerifyStatus, compare_reports, plan_diff_named};
use culprit_engine::export::{self, ExportFormat, ExportNote, ExportOptions};
use culprit_engine::{export_diff, export_plan, plans};

fn fixture(name: &str) -> String {
    std::fs::read_to_string(format!("{}/tests/plans/{name}", env!("CARGO_MANIFEST_DIR"))).unwrap()
}

fn plan(name: &str) -> Plan {
    plans::parse_plan(&fixture(name)).unwrap_or_else(|e| panic!("{name}: {}", e.message))
}

fn report(name: &str) -> Report {
    let path = format!("{}/../../fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

/// Seek-vs-scan pairs per engine, captured from the demo databases.
fn pairs() -> Vec<(Engine, Plan, Plan)> {
    vec![
        (Engine::Mssql, plan("mssql-seek.sqlplan"), plan("mssql-scan.sqlplan")),
        (Engine::Postgres, plan("pg-index.json"), plan("pg-seqscan.json")),
        (Engine::Mysql, plan("mysql-index.json"), plan("mysql-scan.json")),
    ]
}

#[test]
fn every_engine_gets_a_why_headline_regions_and_scenarios() {
    for (engine, a, b) in pairs() {
        let diff = plan_diff_named(&a, "Fast plan", &b, "Slow plan");
        assert_eq!(a.engine, engine);
        let lead = &diff.headlines[0];
        assert!(lead.starts_with("Slow plan"), "{engine:?}: {lead}");
        assert!(
            diff.changes.iter().any(|c| matches!(
                c.kind,
                ChangeKind::Access | ChangeKind::Operator | ChangeKind::Added | ChangeKind::Removed
            )),
            "{engine:?}: {:#?}",
            diff.changes
        );
        // Regions only hold identical, matched operators, and each node is in at most one.
        let mut seen = HashSet::new();
        for r in &diff.regions {
            assert_eq!(r.a.len(), r.b.len());
            for id in &r.a {
                assert!(seen.insert(*id), "{engine:?}: node {id} in two regions");
                assert!(diff.matches.iter().any(|m| m.a == *id));
            }
        }
        // Swapping the sides flips the verdict wording, not the facts.
        let back = plan_diff_named(&b, "Slow plan", &a, "Fast plan");
        assert!(back.headlines[0].starts_with("Fast plan"), "{engine:?}: {}", back.headlines[0]);
        if diff.changes.iter().any(|c| c.kind == ChangeKind::Access) {
            assert!(
                diff.scenarios.iter().any(|s| s.kind == ScenarioKind::Index) || lead.contains("because"),
                "{engine:?}"
            );
        }
    }
}

#[test]
fn sql_server_explains_the_seek_turning_into_a_scan() {
    let (a, b) = (plan("mssql-seek.sqlplan"), plan("mssql-scan.sqlplan"));
    let diff = plan_diff_named(&a, "Plan 68", &b, "Plan 69");
    assert!(diff.headlines[0].contains("because it scans"), "{:?}", diff.headlines);
    assert!(diff.headlines[0].contains("of its cost) where Plan 68 seeks"), "{:?}", diff.headlines);
    assert!(
        diff.scenarios.iter().any(|s| s.kind == ScenarioKind::Conversion && s.title.starts_with("Plan 69 converts")),
        "{:#?}",
        diff.scenarios
    );
}

#[test]
fn an_actual_plan_against_an_estimated_one_says_so() {
    let (a, b) = (plan("mssql-seek.sqlplan"), plan("mssql-actual.sqlplan"));
    let diff = plan_diff_named(&a, "estimated", &b, "actual");
    assert!(diff.scenarios.iter().any(|s| s.kind == ScenarioKind::PlanKind), "{:#?}", diff.scenarios);
}

/// Literal values that appear in the captured plans, per engine.
fn literals(engine: Engine) -> &'static [&'static str] {
    match engine {
        Engine::Mssql => &["customer42@example.com", "'shipped'"],
        Engine::Postgres | Engine::Mysql => &["'shipped'", "'returned'"],
    }
}

/// A literal in any of the escapings an export can use.
fn leaks(out: &str, literal: &str) -> bool {
    [literal.to_string(), literal.replace('\'', "&#39;"), literal.replace('\'', "&apos;")]
        .iter()
        .any(|l| out.contains(l))
}

#[test]
fn exports_redact_literals_in_every_format_and_engine() {
    let options = ExportOptions::default();
    for (engine, a, b) in pairs() {
        let raw: String = [&a, &b].iter().filter_map(|p| p.raw.clone()).collect();
        let present: Vec<&str> = literals(engine).iter().copied().filter(|l| leaks(&raw, l)).collect();
        assert!(!present.is_empty(), "{engine:?}: the fixtures should hold a literal to redact");
        let diff = plan_diff_named(&a, "A", &b, "B");
        let outputs = [
            export_plan(&a, "Plan", ExportFormat::Markdown, &options).unwrap().contents,
            export_plan(&a, "Plan", ExportFormat::Html, &options).unwrap().contents,
            export_diff(&a, &b, &diff, "A", "B", ExportFormat::Markdown, &options).unwrap().contents,
            export_diff(&a, &b, &diff, "A", "B", ExportFormat::Html, &options).unwrap().contents,
            export_plan(&b, "Plan", ExportFormat::Native, &options).map(|f| f.contents).unwrap_or_default(),
        ];
        for out in &outputs {
            for literal in &present {
                assert!(!leaks(out, literal), "{engine:?}: {literal} leaked into an export:\n{out}");
            }
        }
        let unredacted = ExportOptions { redact: false, ..ExportOptions::default() };
        let md = export_plan(&a, "Plan", ExportFormat::Markdown, &unredacted).unwrap().contents;
        assert!(!md.contains("Literal values are redacted"), "{engine:?}");
    }
}

#[test]
fn native_exports_keep_their_format_and_open_again() {
    let options = ExportOptions::default();
    for name in [
        "mssql-scan.sqlplan",
        "mssql-actual.sqlplan",
        "pg-seqscan.json",
        "pg-analyze.txt",
        "pg-subplans.txt",
        "mysql-scan.json",
        "mysql-v2.json",
        "mysql-analyze.txt",
    ] {
        let original = plan(name);
        let file = export_plan(&original, "plan", ExportFormat::Native, &options).unwrap();
        let reread = plans::parse_plan(&file.contents)
            .unwrap_or_else(|e| panic!("{name}: redacted export doesn't parse: {}", e.message));
        assert_eq!(reread.nodes().len(), original.nodes().len(), "{name}");
        assert!((reread.total_cost - original.total_cost).abs() < 1e-6, "{name}: costs must survive redaction");
    }
}

#[test]
fn sql_server_exports_drop_the_database_name() {
    let a = plan("mssql-seek.sqlplan");
    let raw = a.raw.clone().unwrap();
    assert!(raw.contains("[culprit_demo]"), "fixture names its database");
    let md = export_plan(&a, "Plan", ExportFormat::Markdown, &ExportOptions::default()).unwrap().contents;
    let html = export_plan(&a, "Plan", ExportFormat::Html, &ExportOptions::default()).unwrap().contents;
    assert!(!md.contains("culprit_demo") && !html.contains("culprit_demo"));
}

#[test]
fn html_exports_are_self_contained_and_escaped() {
    let mut a = plan("pg-index.json");
    a.statement = Some("SELECT '<script>alert(1)</script>' AS x FROM \"<b>\"".into());
    let options = ExportOptions {
        redact: false,
        notes: vec![ExportNote {
            title: "<img src=x onerror=alert(1)>".into(),
            detail: None,
            fix: None,
            node: Some(0),
        }],
    };
    let html = export_plan(&a, "<Plan & co>", ExportFormat::Html, &options).unwrap().contents;
    assert!(!html.contains("<script"), "scripts are never emitted");
    assert!(!html.contains("<img"), "database and note text is escaped");
    assert!(html.contains("&lt;script&gt;") && html.contains("&lt;Plan &amp; co&gt;"));
    for forbidden in ["http://", "https://", "@import", "url(http", "<link"] {
        assert!(!html.contains(forbidden), "no network requests: {forbidden}");
    }
    assert!(html.contains("<svg class=\"diagram\"") && html.contains("prefers-color-scheme:dark"));
}

#[test]
fn notes_from_insights_appear_in_every_format() {
    let a = plan("mysql-scan.json");
    let options = ExportOptions {
        redact: true,
        notes: vec![ExportNote {
            title: "Full scan of orders".into(),
            detail: Some("Reads every row to return a few.".into()),
            fix: Some("Index orders (status).".into()),
            node: None,
        }],
    };
    for format in [ExportFormat::Markdown, ExportFormat::Html] {
        let out = export_plan(&a, "Plan", format, &options).unwrap().contents;
        assert!(out.contains("Full scan of orders") && out.contains("Index orders (status)."), "{format:?}");
    }
}

#[test]
fn finding_exports_never_name_the_connection_and_redact_query_text() {
    for file in ["mssql-demo.report.json", "pg-demo.report.json", "mysql-demo.report.json"] {
        let report = report(file);
        for finding in &report.findings {
            for out in [
                export::finding_markdown(&report, finding, &ExportOptions::default()),
                export::finding_html(&report, finding, &ExportOptions::default()),
            ] {
                assert!(!out.contains(&report.snapshot.connection), "{file}: {} names the connection", finding.id);
                assert!(
                    out.contains(&finding.title.replace('&', "&amp;").replace('<', "&lt;"))
                        || out.contains(&finding.title)
                );
                assert!(
                    out.contains(&format!("culprit verify &lt;connection&gt; {}", finding.id))
                        || out.contains(&format!("`culprit verify <connection> {}`", finding.id))
                        || finding.verify.is_empty()
                );
            }
        }
        let md = export::report_markdown(&report, &ExportOptions::default());
        assert!(md.contains("opportunit"), "{file}");
        let html = export::report_html(&report, &ExportOptions::default());
        assert!(html.starts_with("<!doctype html>"), "{file}");
    }
}

#[test]
fn verification_reads_fixed_still_present_and_new_for_every_engine() {
    for file in ["mssql-demo.report.json", "pg-demo.report.json", "mysql-demo.report.json"] {
        let before = report(file);
        assert!(before.findings.len() >= 2, "{file}");
        let mut after = before.clone();
        let gone = after.findings.remove(0);
        let mut added = after.findings[0].clone();
        added.id = "missing-index:shop.brand_new(col)".into();
        added.title = "A new finding".into();
        after.findings.push(added);
        let v = compare_reports(&before, &after, &[]);
        let status = |id: &str| v.findings.iter().find(|f| f.id == id).map(|f| f.status);
        assert_eq!(status(&gone.id), Some(VerifyStatus::Fixed), "{file}");
        assert_eq!(status(&after.findings[0].id), Some(VerifyStatus::StillPresent), "{file}");
        assert_eq!(v.new_findings.len(), 1, "{file}");
        assert!(v.summary.contains("fixed") && v.summary.contains("1 new finding"), "{file}: {}", v.summary);
        // A workload finding that is still there explains why a shipped fix can still show.
        if let Some(q) = before.findings.iter().find(|f| !f.queries.is_empty())
            && after.findings.iter().any(|f| f.id == q.id)
        {
            let one = compare_reports(&before, &after, std::slice::from_ref(&q.id));
            assert!(
                one.findings[0].note.as_deref().is_some_and(|n| n.contains("before the change")),
                "{file}: {:?}",
                one.findings[0].note
            );
        }
        let unknown = compare_reports(&before, &after, &["no-such:id".into()]);
        assert_eq!(unknown.findings[0].status, VerifyStatus::Unknown);
        let md = export::verification_markdown(&v);
        assert!(md.contains("✅ Fixed") && md.contains("🆕 New"), "{file}");
    }
}
