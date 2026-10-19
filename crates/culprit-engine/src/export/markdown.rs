//! Markdown exports, written for a pull request description or an issue: a short lead, the evidence, the fix, how to
//! verify it. Renders well on GitHub, GitLab and in chat tools.

use std::fmt::Write;

use culprit_core::{ChangeKind, Finding, Metric, Plan, PlanDiff, PlanNode, Report, Verification, VerifyStatus, format};

use super::{
    ExportNote, ExportOptions, exported_at, kind_label, plan_engine_label, prepared, rows_phrase, rows_text,
    server_line, share, shares_note, sql_text, stats_line, top_costs, where_,
};

const MAX_STEPS: usize = 40;
const NBSP: char = '\u{a0}';

/// A table cell: kept on one line, with HTML, Markdown and pipes escaped.
fn cell(raw: &str) -> String {
    text(raw)
}

/// Database-sourced text (names, titles built from them) outside code: HTML and Markdown that would render in a pull
/// request or wiki become plain characters. A table named `<img src=x onerror=…>` stays text.
fn text(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for c in raw.chars() {
        match c {
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '&' => out.push_str("&amp;"),
            // Characters Markdown would turn into links, images, emphasis or headings inside a line.
            '[' | ']' | '*' | '_' | '`' | '|' | '\\' => {
                out.push('\\');
                out.push(c);
            }
            '\n' | '\r' => out.push(' '),
            _ => out.push(c),
        }
    }
    out
}

/// Inline code that survives backticks in the text.
fn code(text: &str) -> String {
    if text.contains('`') { format!("`` {text} ``") } else { format!("`{text}`") }
}

fn fence(out: &mut String, lang: &str, text: &str) {
    let ticks = if text.contains("```") { "````" } else { "```" };
    let _ = writeln!(out, "{ticks}{lang}\n{}\n{ticks}\n", text.trim_end());
}

fn redaction_note(out: &mut String, options: &ExportOptions) {
    if options.redact {
        let _ = writeln!(out, "> Literal values are redacted (`'?'`, `?`); names and parameter markers are kept.\n");
    }
}

fn footer(out: &mut String, extra: &str) {
    let _ = writeln!(out, "<sub>Exported from Culprit on {}{extra}.</sub>", exported_at());
}

/// One plan: where the cost goes, the operators as an indented table, warnings, notes and the statement.
pub fn plan_markdown(plan: &Plan, title: &str, options: &ExportOptions) -> String {
    let plan = prepared(plan, options);
    let mut out = String::new();
    let count = plan.nodes().len();
    let _ = writeln!(out, "## {}\n", text(title.trim()));
    let _ = writeln!(
        out,
        "{} · {} · {count} operator{} · estimated cost {}\n",
        plan_engine_label(&plan),
        kind_label(plan.kind).to_lowercase(),
        if count == 1 { "" } else { "s" },
        cost(plan.total_cost)
    );
    redaction_note(&mut out, options);
    plan_body(&mut out, &plan, &options.notes);
    footer(&mut out, "");
    out
}

fn plan_body(out: &mut String, plan: &Plan, notes: &[ExportNote]) {
    let costs: Vec<String> =
        top_costs(plan, 4).iter().map(|(n, s)| format!("**{}** {}", format::percent(*s), cell(&label(n)))).collect();
    if !costs.is_empty() {
        let _ = writeln!(out, "**Where the cost goes:** {}\n", costs.join(" · "));
    }
    let hot = plan.hottest().map(|n| n.id);
    let _ = writeln!(
        out,
        "| Step | Table · index | Cost | Rows{} | Notes |",
        if plan.kind == culprit_core::PlanKind::Actual { " (est. → actual)" } else { " (est.)" }
    );
    let _ = writeln!(out, "|---|---|---:|---:|---|");
    let mut rows = Vec::new();
    walk(&plan.root, 0, &mut rows);
    for (node, depth) in rows.iter().take(MAX_STEPS) {
        let indent: String = std::iter::repeat_n(NBSP, depth * 3).collect();
        let arrow = if *depth > 0 { "↳ " } else { "" };
        let mut notes_cell: Vec<String> = node.warnings.iter().map(|w| w.label().to_string()).collect();
        if Some(node.id) == hot && share(plan, node).is_some_and(|s| s >= 0.05) {
            notes_cell.insert(0, "most expensive".into());
        }
        // An exchange's cost is the charge for passing rows between threads: never the step where the work is.
        if node.kind == culprit_core::NodeKind::Parallelism && share(plan, node).is_some_and(|s| s >= 0.05) {
            notes_cell.insert(0, "passing rows between workers".into());
        }
        let _ = writeln!(
            out,
            "| {indent}{arrow}{} | {} | {} | {} | {} |",
            cell(&node.op),
            cell(&where_(node)),
            // No share where the plan can't state one (no costs, a step it doesn't cost, a default guess).
            share(plan, node).map(format::percent).unwrap_or_else(|| "—".into()),
            rows_text(node),
            cell(&notes_cell.join(", "))
        );
    }
    if rows.len() > MAX_STEPS {
        let _ = writeln!(out, "\n_…and {} more operators._", rows.len() - MAX_STEPS);
    }
    let _ = writeln!(out);
    if let Some(note) = shares_note(plan, None) {
        let _ = writeln!(out, "_{}_\n", text(&note.replace('`', "")));
    }
    let predicates: Vec<String> = rows
        .iter()
        .filter(|(n, _)| share(plan, n).is_some_and(|s| s >= 0.05))
        .flat_map(|(n, _)| {
            let mut lines = Vec::new();
            if let Some(p) = &n.seek_predicate {
                lines.push(format!("- {} seeks on {}", cell(&label(n)), code(&format::predicate(p))));
            }
            if let Some(p) = &n.predicate {
                lines.push(format!("- {} filters {}", cell(&label(n)), code(&format::predicate(p))));
            }
            lines
        })
        .collect();
    if !predicates.is_empty() {
        let _ = writeln!(out, "**Predicates on the expensive steps**\n\n{}\n", predicates.join("\n"));
    }
    if !plan.warnings.is_empty() {
        let names: Vec<&str> = plan.warnings.iter().map(|w| w.label()).collect();
        let _ = writeln!(out, "**Plan warnings:** {}\n", names.join(", "));
    }
    if !plan.index_hints.is_empty() {
        let _ = writeln!(out, "**The optimizer asked for these indexes**\n");
        for h in &plan.index_hints {
            let keys: Vec<String> = h.equality.iter().chain(h.inequality.iter()).cloned().collect();
            let include =
                if h.include.is_empty() { String::new() } else { format!(" include ({})", h.include.join(", ")) };
            let _ = writeln!(
                out,
                "- {} ({}){include}: estimated {} cheaper",
                code(&h.table),
                keys.join(", "),
                format::percent(h.impact_pct / 100.0)
            );
        }
        let _ = writeln!(out);
    }
    notes_section(out, notes, plan);
    if let Some(statement) = &plan.statement {
        let _ = writeln!(out, "<details><summary>Statement</summary>\n");
        fence(out, "sql", statement);
        let _ = writeln!(out, "</details>\n");
    }
}

fn notes_section(out: &mut String, notes: &[ExportNote], plan: &Plan) {
    if notes.is_empty() {
        return;
    }
    let _ = writeln!(out, "**Insights**\n");
    for n in notes {
        let on = n
            .node
            .and_then(|id| plan.nodes().into_iter().find(|x| x.id == id))
            .map(|x| format!(" ({})", label(x)))
            .unwrap_or_default();
        let _ = writeln!(
            out,
            "- **{}**{on}{}",
            cell(&n.title),
            n.detail.as_deref().map(|d| format!(" — {d}")).unwrap_or_default()
        );
        if let Some(fix) = &n.fix {
            let _ = writeln!(out, "  Fix: {fix}");
        }
    }
    let _ = writeln!(out);
}

fn walk<'a>(node: &'a PlanNode, depth: usize, out: &mut Vec<(&'a PlanNode, usize)>) {
    out.push((node, depth));
    for c in &node.children {
        walk(c, depth + 1, out);
    }
}

fn label(node: &PlanNode) -> String {
    super::node_label(node)
}

fn cost(value: f64) -> String {
    if value < 10.0 { format!("{value:.3}") } else { format::count(value) }
}

/// Two plans compared: the lead sentence, the numbers side by side, what changed, why the optimizer may have chosen
/// differently, and each plan's steps.
pub fn diff_markdown(
    a: &Plan,
    b: &Plan,
    diff: &PlanDiff,
    name_a: &str,
    name_b: &str,
    options: &ExportOptions,
) -> String {
    let (a, b) = (prepared(a, options), prepared(b, options));
    let mut out = String::new();
    let _ = writeln!(out, "## Plan comparison: {name_a} → {name_b}\n");
    redaction_note(&mut out, options);
    if let Some((lead, rest)) = diff.headlines.split_first() {
        let _ = writeln!(out, "**{}**\n", lead);
        for h in rest {
            let _ = writeln!(out, "- {h}");
        }
        if !rest.is_empty() {
            let _ = writeln!(out);
        }
    }
    let _ = writeln!(out, "| | {} | {} |\n|---|---:|---:|", cell(name_a), cell(name_b));
    let _ = writeln!(out, "| Engine | {} | {} |", plan_engine_label(&a), plan_engine_label(&b));
    let _ = writeln!(out, "| Plan | {} | {} |", kind_label(a.kind).to_lowercase(), kind_label(b.kind).to_lowercase());
    let _ = writeln!(out, "| Estimated cost | {} | {} |", cost(diff.cost_a), cost(diff.cost_b));
    if let (Some(ta), Some(tb)) = (diff.time_a_ms, diff.time_b_ms) {
        let _ = writeln!(out, "| Actual time | {} | {} |", format::duration(ta), format::duration(tb));
    }
    let _ = writeln!(out, "| Operators | {} | {} |\n", a.nodes().len(), b.nodes().len());
    if !diff.changes.is_empty() {
        let _ = writeln!(out, "### What changed\n");
        for c in diff.changes.iter().take(20) {
            let detail = c.detail.as_deref().map(|d| format!(" — {d}")).unwrap_or_default();
            let _ = writeln!(out, "- {} {}{}", change_word(c.kind), text(&c.title), text(&detail));
        }
        let _ = writeln!(out);
    }
    if !diff.scenarios.is_empty() {
        let _ = writeln!(out, "### Why the optimizer may have chosen differently\n");
        for s in &diff.scenarios {
            let detail = s.detail.as_deref().map(|d| format!(" {d}")).unwrap_or_default();
            let _ = writeln!(out, "- **{}**{}", text(&s.title), text(&detail));
        }
        let _ = writeln!(out);
    }
    for (name, plan) in [(name_a, &a), (name_b, &b)] {
        let _ = writeln!(out, "<details><summary>{}: {} operators</summary>\n", text(name), plan.nodes().len());
        plan_body(&mut out, plan, &[]);
        let _ = writeln!(out, "</details>\n");
    }
    notes_section(&mut out, &options.notes, &b);
    footer(&mut out, "");
    out
}

fn change_word(kind: ChangeKind) -> &'static str {
    match kind {
        ChangeKind::Access => "**Access**",
        ChangeKind::Operator => "**Operator**",
        ChangeKind::Added => "**Added**",
        ChangeKind::Removed => "**Removed**",
        ChangeKind::Cost => "**Cost**",
        ChangeKind::Rows => "**Rows**",
        ChangeKind::Warning => "**Warning**",
    }
}

/// A finding's metric as the brief says it (`culprit_checks::metric_text`): a total wait to the second, as its title.
pub(crate) fn metric_text(m: &Metric) -> String {
    culprit_checks::metric_text(m)
}

/// One finding in full for a digest (a chat thread, a ticket): the same pieces as [`finding_markdown`], always
/// redacted, as data for the consumer to lay out.
pub fn finding_detail(report: &Report, finding: &Finding) -> culprit_core::DigestDetail {
    let s = &report.snapshot;
    let engine = s.server.engine;
    let f = culprit_core::redact::redact_finding(finding, engine);
    let queries = s
        .workload
        .as_ref()
        .map(|w| {
            f.queries
                .iter()
                .filter_map(|r| w.query(&r.id).map(|q| (r, q)))
                .take(2)
                .map(|(r, q)| {
                    let plan = q.plan.as_ref().and_then(|plan| {
                        let plan = culprit_core::redact::redact_plan(plan);
                        // The step the finding names, else its plan's working step where the finding is about one.
                        let node = culprit_checks::finding_step(&f, r, &plan)?;
                        // A plan without costs (SQLite) names the step, and no share or estimate it doesn't have; a
                        // share only where the plan can state one.
                        let mut line = match (plan.weighed(), culprit_checks::finding_step_share(&plan, node)) {
                            (true, Some(s)) => format!(
                                "{}: {} of the plan's cost, {}",
                                label(node),
                                format::percent(s),
                                rows_phrase(node)
                            ),
                            (true, None) => format!("{}: {}", label(node), rows_phrase(node)),
                            (false, _) => label(node),
                        };
                        if let Some(p) = node.predicate.as_deref().or(node.seek_predicate.as_deref()) {
                            let _ = write!(line, ", on `{}`", format::predicate(p));
                        }
                        Some(line)
                    });
                    culprit_core::DigestQuery {
                        stats: stats_line(w, q),
                        sql: culprit_core::redact::redact_sql(q.text.trim(), engine),
                        plan,
                        note: r.note.clone(),
                    }
                })
                .collect()
        })
        .unwrap_or_default();
    culprit_core::DigestDetail {
        confidence: format!("{:?}", f.confidence).to_lowercase(),
        summary: f.summary,
        metrics: f
            .metrics
            .iter()
            .take(6)
            .map(|m| culprit_core::DigestMetric { label: m.label.clone(), value: metric_text(m) })
            .collect(),
        queries,
        fix: f.fix,
        verify: f.verify,
        caveats: f.caveats,
    }
}

/// One finding as a PR section: what's happening, the evidence, the query and its plan, the fix and how to verify it.
pub fn finding_markdown(report: &Report, finding: &Finding, options: &ExportOptions) -> String {
    let mut out = String::new();
    let shared = options.redact.then(|| super::shared_report(report));
    finding_section(&mut out, report, shared.as_ref(), finding, options, "##");
    footer(&mut out, &format!(" · finding {}", code(&finding.id)));
    out
}

/// `shared` is the report checked from its redacted snapshot when the export redacts (see `shared_report`).
fn finding_section(
    out: &mut String,
    report: &Report,
    shared: Option<&Report>,
    finding: &Finding,
    options: &ExportOptions,
    level: &str,
) {
    let s = &report.snapshot;
    let redacted;
    let finding = match shared {
        Some(shared) => {
            redacted = super::shareable(report, shared, finding);
            &redacted
        }
        None => finding,
    };
    let _ = writeln!(out, "{level} {}\n", text(&finding.title));
    let _ = writeln!(
        out,
        "**{}** · {} · {} impact, {} confidence · {}\n",
        finding.impact.label,
        finding.actionability.label(),
        format!("{:?}", finding.severity).to_lowercase(),
        format!("{:?}", finding.confidence).to_lowercase(),
        server_line(report)
    );
    let _ = writeln!(out, "{}\n", text(&finding.summary));
    if !finding.metrics.is_empty() {
        for m in &finding.metrics {
            let _ = writeln!(out, "- {}: **{}**", text(&m.label), text(&metric_text(m)));
        }
        let _ = writeln!(out);
    }
    let engine = s.server.engine;
    let queries: Vec<_> =
        finding.queries.iter().filter_map(|r| s.workload.as_ref()?.query(&r.id).map(|q| (r, q))).take(3).collect();
    for (r, q) in &queries {
        let w = s.workload.as_ref().expect("filtered on workload");
        let _ = writeln!(out, "**Query {}** · {}\n", code(&q.id), stats_line(w, q));
        fence(out, "sql", &sql_text(&q.text, engine, options));
        if let Some(plan) = &q.plan {
            let plan = prepared(plan, options);
            // The step the finding names, else its plan's working step where the finding is about one.
            if let Some(node) = culprit_checks::finding_step(finding, r, &plan) {
                let mut details = Vec::new();
                if plan.weighed() {
                    if let Some(s) = culprit_checks::finding_step_share(&plan, node) {
                        details.push(format!("{} of the plan's cost", format::percent(s)));
                    }
                    details.push(rows_phrase(node));
                }
                if let Some(p) = node.predicate.as_deref().or(node.seek_predicate.as_deref()) {
                    details.push(format!("on {}", code(&format::predicate(p))));
                }
                let mut line = format!("Plan: the step to look at is **{}**", label(node));
                if !details.is_empty() {
                    let _ = write!(line, " ({})", details.join(", "));
                }
                line.push('.');
                // Where the plan can't state the step's share, why.
                if let Some(why) = culprit_checks::finding_no_share_reason(&plan, node, Some(q)) {
                    let _ = write!(line, " {}", text(&why.replace('`', "")));
                }
                let _ = writeln!(out, "{line}\n");
            }
        }
    }
    if let Some(fix) = &finding.fix {
        let _ = writeln!(out, "**Fix.** {}\n", text(&fix.summary));
        if let Some(sql) = &fix.sql {
            fence(out, "sql", sql);
        }
        if let Some(cost) = &fix.cost {
            let _ = writeln!(out, "_Cost:_ {}\n", text(cost));
        }
    }
    if !finding.verify.is_empty() {
        let _ = writeln!(out, "**How to verify**\n");
        for (i, step) in finding.verify.iter().enumerate() {
            let _ = writeln!(out, "{}. {step}", i + 1);
        }
        // Only where a re-scan can show it gone: not for statements that stopped, nor on running totals.
        if culprit_checks::gone_after_rescan(report, finding) {
            let _ = writeln!(
                out,
                "{}. After it ships: `culprit verify <connection> {}` re-scans and reports whether it's gone.",
                finding.verify.len() + 1,
                finding.id
            );
        }
        out.push('\n');
    }
    if !finding.caveats.is_empty() {
        let _ = writeln!(out, "**Before you act**\n");
        for c in &finding.caveats {
            let _ = writeln!(out, "- {c}");
        }
        let _ = writeln!(out);
    }
    if let Some(plan) = queries.first().and_then(|(_, q)| q.plan.as_ref()) {
        notes_section(out, &options.notes, plan);
    }
}

/// Every finding of a scan: a summary table and the top findings in full.
pub fn report_markdown(report: &Report, options: &ExportOptions) -> String {
    let mut out = String::new();
    let s = &report.snapshot;
    let _ = writeln!(
        out,
        "## Culprit: {} opportunit{}\n",
        report.findings.len(),
        if report.findings.len() == 1 { "y" } else { "ies" }
    );
    let _ = writeln!(out, "{}. Scanned {}.\n", server_line(report), s.taken_at);
    redaction_note(&mut out, options);
    let shared = options.redact.then(|| super::shared_report(report));
    if report.findings.is_empty() {
        let _ = writeln!(out, "Nothing stands out.\n");
    } else {
        let _ = writeln!(out, "| | Finding | Impact |\n|---|---|---|");
        for f in &report.findings {
            let title = match &shared {
                Some(shared) => super::shareable(report, shared, f).title,
                None => f.title.clone(),
            };
            let _ = writeln!(out, "| {} | {} | {} |", severity_dot(f), cell(&title), cell(&f.impact.label));
        }
        let _ = writeln!(out);
        for f in report.findings.iter().take(5) {
            finding_section(&mut out, report, shared.as_ref(), f, options, "###");
        }
        if report.findings.len() > 5 {
            let _ = writeln!(
                out,
                "_The other {} findings: `culprit show <connection> <id>`._\n",
                report.findings.len() - 5
            );
        }
    }
    footer(&mut out, "");
    out
}

fn severity_dot(f: &Finding) -> &'static str {
    if !f.is_actionable() {
        return "○";
    }
    match f.severity {
        culprit_core::Severity::High => "🔴",
        culprit_core::Severity::Medium => "🟠",
        culprit_core::Severity::Low => "🔵",
    }
}

/// The verification as Markdown, for a PR comment after the fix shipped.
pub fn verification_markdown(v: &Verification) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "**{}**\n", text(&v.summary));
    for f in v.findings.iter().chain(v.new_findings.iter()) {
        let mark = match f.status {
            VerifyStatus::Fixed => "✅ Fixed",
            VerifyStatus::StillPresent => "⏳ Still present",
            VerifyStatus::Unknown => "❔ Can't tell",
            VerifyStatus::New => "🆕 New",
        };
        let impact = match (&f.before, &f.after) {
            (Some(b), Some(a)) if b != a => format!(" ({b} → {a})"),
            (Some(b), None) => format!(" (was {b})"),
            (_, Some(a)) => format!(" ({a})"),
            _ => String::new(),
        };
        let _ = writeln!(out, "- {mark}: {}{}", text(&f.title), text(&impact));
        if let Some(note) = &f.note {
            let _ = writeln!(out, "  {}", text(note));
        }
    }
    let (before, after) = culprit_core::changes::range_times(&v.before_taken_at, &v.after_taken_at);
    let _ = writeln!(out, "\n<sub>Scans {before} → {after}.</sub>");
    out
}

#[cfg(test)]
mod tests {
    use super::{cell, text};

    #[test]
    fn database_text_stays_text() {
        assert_eq!(text("T <img src=x onerror=alert(1)>"), "T &lt;img src=x onerror=alert(1)&gt;");
        assert_eq!(text("[click](https://example.com)"), "\\[click\\](https://example.com)");
        assert_eq!(text("order_items is *hot*"), "order\\_items is \\*hot\\*");
        assert_eq!(cell("a | b\nc"), "a \\| b c");
    }
}
