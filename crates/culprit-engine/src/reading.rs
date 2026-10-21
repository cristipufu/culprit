//! What a person reads in a report, as one plain text in a stable order: every finding as the app shows it (its
//! evidence, fix, how to verify it, caveats and queries), its agent brief, and the insights and notes of every captured
//! plan. The golden tests (`tests/golden.rs`) compare it with `fixtures/golden/`, so a change in what users read shows
//! up as a diff and is accepted deliberately.
//!
//! Nothing in it depends on the clock or on hash order: dates are the scan's own, numbers are written as the product
//! writes them. The raw `Impact::score` is left out (the findings' order already says how they rank, and a float's last
//! digit isn't something a person reads), and so is each plan's tree (its parser's changes show in the insights,
//! the briefs and the findings that name its steps).

use std::fmt::Write;
use std::path::Path;

use culprit_core::{Finding, Plan, Report, Result, format};

use crate::reports;

/// A saved report or snapshot, read the way the app opens one: its plans parsed again from their text
/// ([`crate::plans::reparse_saved`]), read against the scans saved beside it ([`reports::rebase_on_scans`]), its checks run
/// again, and each plan marked against the catalog and given its insights.
pub fn open(path: &Path) -> Result<Report> {
    let mut input = reports::load_analysis_input(path)?;
    if let Some(dir) = path.parent() {
        reports::rebase_on_scans(dir, &mut input.snapshot);
    }
    Ok(crate::analysis::reread(input, &[]))
}

/// Refresh presentation on a legacy or externally assembled report without replacing its findings. Reports returned
/// by Workspace or `open` already have this presentation; clients render those directly.
pub fn for_reading(mut report: Report) -> Report {
    culprit_core::insights::mark_snapshot(&mut report.snapshot);
    culprit_core::insights::mark_snapshot_keys(&mut report.snapshot);
    culprit_core::insights::mark_snapshot_index_kinds(&mut report.snapshot);
    culprit_checks::annotate_plans(&mut report.snapshot);
    report
}

/// The report as plain text: a header, then `=== finding <id>` for each finding in report order (with its agent
/// brief), then `=== plan <query id>` for each captured plan in workload order. Each line ends without trailing spaces.
pub fn text(report: &Report) -> String {
    let mut out = String::new();
    let s = &report.snapshot;
    let _ = writeln!(out, "reading: {} · database {} · scanned {}", s.server.product, s.server.database, s.taken_at);
    line(&mut out, "", "headline", &report.summary.headline);
    if let Some(why) = &report.summary.no_workload {
        line(&mut out, "", "no workload", why);
    }
    if let Some(share) = report.summary.top_share {
        let _ = writeln!(out, "top share: {}", format::percent(share));
    }
    let _ = writeln!(out, "findings: {}", report.findings.len());
    for f in &report.findings {
        finding(&mut out, report, f);
    }
    if let Some(w) = &s.workload {
        for q in &w.queries {
            if let Some(plan) = &q.plan {
                let _ = writeln!(out, "\n=== plan {}", q.id);
                plan_section(&mut out, plan, Some(q));
            }
        }
    }
    let mut clean: String = out.lines().map(str::trim_end).collect::<Vec<_>>().join("\n");
    clean.push('\n');
    clean
}

fn finding(out: &mut String, report: &Report, f: &Finding) {
    let _ = writeln!(out, "\n=== finding {}", f.id);
    let _ = writeln!(
        out,
        "{} · {} severity · {} confidence · {}{}",
        f.actionability.label(),
        format!("{:?}", f.severity).to_lowercase(),
        format!("{:?}", f.confidence).to_lowercase(),
        f.category.label().to_lowercase(),
        match (f.inactive, &f.last_seen) {
            (true, Some(at)) => format!(" · inactive, last seen {at}"),
            (true, None) => " · inactive".to_string(),
            (false, _) => String::new(),
        }
    );
    line(out, "", "title", &f.title);
    line(out, "", "summary", &f.summary);
    let share = f.impact.db_time_share.map(|s| format!(" · {:.1}% of database time", s * 100.0)).unwrap_or_default();
    line(out, "", "impact", &format!("{}{share}", f.impact.label));
    if !f.metrics.is_empty() {
        let _ = writeln!(out, "metrics:");
        for m in &f.metrics {
            line(out, "  ", &format!("- {}", m.label), &culprit_checks::metric_text(m));
        }
    }
    if let Some(fix) = &f.fix {
        line(out, "", "fix", &fix.summary);
        if let Some(sql) = &fix.sql {
            block(out, "  ", "sql", sql);
        }
        if let Some(cost) = &fix.cost {
            line(out, "  ", "cost", cost);
        }
    }
    if !f.verify.is_empty() {
        let _ = writeln!(out, "verify:");
        for (i, step) in f.verify.iter().enumerate() {
            item(out, "  ", &format!("{}.", i + 1), step);
        }
    }
    if !f.caveats.is_empty() {
        let _ = writeln!(out, "caveats:");
        for c in &f.caveats {
            item(out, "  ", "-", c);
        }
    }
    if !f.queries.is_empty() {
        let _ = writeln!(out, "queries:");
        let workload = report.snapshot.workload.as_ref();
        for r in &f.queries {
            let share = match workload.and_then(|w| Some((w, w.query(&r.id)?))) {
                Some((w, q)) => format!("{} of database time", format::percent(culprit_checks::query_share(w, f, q))),
                None => "not in the workload".to_string(),
            };
            let step = r.plan_node.map(|n| format!(" · plan step {n}")).unwrap_or_default();
            line(out, "  ", &format!("- {}", r.id), &format!("{share}{step}"));
            if let Some(note) = &r.note {
                line(out, "    ", "note", note);
            }
        }
    }
    block(out, "", "brief", &culprit_checks::agent_brief(report, f));
}

fn plan_section(out: &mut String, plan: &Plan, q: Option<&culprit_core::QueryStat>) {
    let insights = plan.insights.as_ref();
    let items = insights.map(|i| i.items.as_slice()).unwrap_or_default();
    if items.is_empty() {
        let _ = writeln!(out, "no insights");
    }
    for i in items {
        // The share of the work the plan viewer states, where it states one (`insights::stated_share`).
        let impact = match culprit_core::insights::stated_share(plan, i) {
            Some(share) => format::percent(share),
            None if i.fixed_weight => "a fixed weight".to_string(),
            None => "no share stated".to_string(),
        };
        let _ = writeln!(
            out,
            "insight {} · {} severity · impact {impact}",
            i.rule,
            format!("{:?}", i.severity).to_lowercase()
        );
        line(out, "  ", "title", &i.title);
        line(out, "  ", "detail", &i.detail);
        line(out, "  ", "fix", &i.fix);
        if let Some(sql) = &i.fix_sql {
            block(out, "  ", "sql", sql);
        }
        line(out, "  ", "verify", &i.verify);
    }
    if let Some(note) = insights.and_then(|i| i.note.as_deref()) {
        line(out, "", "note", note);
    }
    if let Some(note) = crate::export::shares_note(plan, q) {
        line(out, "", "shares", &note);
    }
}

/// `label: text` on one line, or as a [`block`] when the text has several.
fn line(out: &mut String, indent: &str, label: &str, text: &str) {
    if text.contains('\n') {
        block(out, indent, label, text);
    } else {
        let _ = writeln!(out, "{indent}{label}: {text}");
    }
}

/// One entry of a list (`1. text`, `- text`), its further lines indented under it.
fn item(out: &mut String, indent: &str, marker: &str, text: &str) {
    let mut lines = text.trim_end().lines();
    let _ = writeln!(out, "{indent}{marker} {}", lines.next().unwrap_or_default());
    for l in lines {
        let _ = writeln!(out, "{indent}    {l}");
    }
}

/// `label:` and then every line of the text, indented under it.
fn block(out: &mut String, indent: &str, label: &str, text: &str) {
    let _ = writeln!(out, "{indent}{label}:");
    for l in text.trim_end().lines() {
        let _ = writeln!(out, "{indent}    {l}");
    }
}
