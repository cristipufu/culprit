//! Share what Culprit found: a finding, a whole report, a plan or a comparison of two plans, as Markdown for a pull
//! request or as one self-contained HTML file (inline CSS and SVG; no scripts, fonts or network). Literal values are
//! redacted by default, and exports never include connection names, hosts or credentials: only the engine, the
//! environment and what the database reported about its own objects.

mod html;
mod markdown;
mod svg;

use std::borrow::Cow;

use culprit_core::redact::{redact_native, redact_plan, redact_sql};
use culprit_core::{Engine, Environment, Finding, Plan, PlanKind, PlanNode, Report};
use serde::{Deserialize, Serialize};
use ts_rs::TS;

pub use html::{diff_html, finding_html, plan_html, report_html};
pub use markdown::{
    diff_markdown, finding_detail, finding_markdown, plan_markdown, report_markdown, verification_markdown,
};

#[derive(Serialize, Deserialize, TS, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
#[ts(export)]
pub enum ExportFormat {
    /// Markdown for a pull request, an issue or a chat.
    #[default]
    Markdown,
    /// One self-contained HTML page with the plan diagram.
    Html,
    /// The plan's own text (.sqlplan, EXPLAIN JSON or text), so it opens in the engine's tools.
    Native,
}

/// Something to say about a plan beyond Culprit's own summary, e.g. a plan insight: what it is, why it matters, how to
/// fix it and which operator it is about. Rendered under the plan in every export format.
#[derive(Serialize, Deserialize, TS, Clone, Debug, PartialEq)]
#[ts(export)]
pub struct ExportNote {
    pub title: String,
    #[serde(default)]
    pub detail: Option<String>,
    #[serde(default)]
    pub fix: Option<String>,
    /// Plan node id, to mark the operator in the diagram.
    #[serde(default)]
    pub node: Option<u32>,
}

#[derive(Serialize, Deserialize, TS, Clone, Debug, PartialEq)]
#[ts(export)]
pub struct ExportOptions {
    /// Replace literal values in SQL text, predicates and parameter values with placeholders. On unless asked.
    #[serde(default = "on")]
    pub redact: bool,
    /// Notes to include with the plan (e.g. insights). The insights engine fills these; exports render them.
    #[serde(default)]
    pub notes: Vec<ExportNote>,
}

fn on() -> bool {
    true
}

impl Default for ExportOptions {
    fn default() -> Self {
        ExportOptions { redact: true, notes: Vec::new() }
    }
}

/// A file to save or copy: a suggested name and the contents.
#[derive(Serialize, Deserialize, TS, Clone, Debug)]
#[ts(export)]
pub struct ExportFile {
    pub name: String,
    pub contents: String,
    pub format: ExportFormat,
}

/// The plan's own text with literal values redacted (or as is), when the plan kept it.
pub fn plan_native(plan: &Plan, options: &ExportOptions) -> Option<String> {
    let raw = plan.raw.as_deref()?;
    Some(if options.redact { redact_native(raw, plan.engine) } else { raw.to_string() })
}

/// File extension for the plan's own text.
pub fn native_extension(plan: &Plan) -> &'static str {
    let raw = plan.raw.as_deref().unwrap_or("").trim_start();
    if raw.starts_with('<') {
        "sqlplan"
    } else if raw.starts_with('{') || raw.starts_with('[') {
        "json"
    } else {
        "txt"
    }
}

/// A short, safe file name from a title: "plan-orders-by-customer".
pub fn file_stem(title: &str) -> String {
    let mut out = String::new();
    for c in title.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.ends_with('-') && !out.is_empty() {
            out.push('-');
        }
        if out.len() >= 60 {
            break;
        }
    }
    let out = out.trim_end_matches('-').to_string();
    if out.is_empty() { "culprit".into() } else { out }
}

pub(crate) fn prepared<'a>(plan: &'a Plan, options: &ExportOptions) -> Cow<'a, Plan> {
    if options.redact { Cow::Owned(redact_plan(plan)) } else { Cow::Borrowed(plan) }
}

/// The report as it may leave the machine: checked again from its redacted snapshot, so every finding's text quotes
/// only SQL the lexer has already redacted.
pub(crate) fn shared_report(report: &Report) -> Report {
    culprit_checks::analyze(culprit_core::redact::redact_snapshot(&report.snapshot))
}

/// `finding` as it may leave the machine: the same finding from `shared` (see [`shared_report`]) with its prose
/// redacted again as a second layer. A finding the re-check doesn't produce (a plug-in check) gets the second layer
/// only.
pub(crate) fn shareable(report: &Report, shared: &Report, finding: &Finding) -> Finding {
    let engine = report.snapshot.server.engine;
    let source = shared.findings.iter().find(|f| f.id == finding.id).unwrap_or(finding);
    culprit_core::redact::redact_finding(source, engine)
}

pub(crate) fn sql_text(text: &str, engine: Engine, options: &ExportOptions) -> String {
    if options.redact { redact_sql(text.trim(), engine) } else { text.trim().to_string() }
}

/// The server a plan came from, MariaDB included (its plans carry `flavor`; the engine is MySQL's for both). Always one
/// of a fixed set of names: a plan opened from a file can carry any `flavor`, and this text goes into HTML pages.
pub(crate) fn plan_engine_label(plan: &Plan) -> &'static str {
    let mariadb =
        plan.engine == Engine::Mysql && plan.props.get("flavor").is_some_and(|f| f.eq_ignore_ascii_case("mariadb"));
    if mariadb { "MariaDB" } else { plan.engine.label() }
}

/// The server a report came from, MariaDB included.
pub(crate) fn report_engine_label(report: &Report) -> &'static str {
    if report.snapshot.server.product.to_ascii_lowercase().contains("mariadb") {
        "MariaDB"
    } else {
        report.snapshot.server.engine.label()
    }
}

pub(crate) fn environment_label(environment: Environment) -> String {
    format!("{environment:?}").to_lowercase()
}

pub(crate) fn kind_label(kind: PlanKind) -> &'static str {
    match kind {
        PlanKind::Estimated => "Estimated plan",
        PlanKind::Generic => "Generic plan",
        PlanKind::Actual => "Actual plan",
    }
}

/// A step's share of the plan's cost where the plan can state one (`Plan::step_share`): none on a plan without costs,
/// beside a step the plan puts no cost on (MySQL's filesort), or for a step whose rows rest on a default guess.
pub(crate) fn share(plan: &Plan, node: &PlanNode) -> Option<f64> {
    plan.step_share(node)
}

/// Why steps of a plan that weighs them show no share of its cost, for under its steps (every surface that lists a
/// plan's steps says it the same); `None` when each costed step shows one.
pub fn shares_note(plan: &Plan) -> Option<String> {
    if !plan.weighed() {
        return None;
    }
    if !plan.uncosted_steps().is_empty() {
        return culprit_checks::no_share_reason(plan, &plan.root);
    }
    plan.nodes().iter().any(|n| n.props.contains_key(culprit_core::GUESSED_SELECTIVITY)).then(|| {
        "No share is given for a step whose rows rest on the planner's default guess for a parameter, nor for the \
         steps above it."
            .to_string()
    })
}

/// Rows flowing out of an operator: actual when the plan ran, else the estimate across executions.
pub(crate) fn rows_out(node: &PlanNode) -> f64 {
    node.act_rows.unwrap_or_else(|| node.est_rows_total())
}

pub(crate) fn table(node: &PlanNode) -> Option<String> {
    let object = node.object.as_deref()?;
    Some(object.rsplit('.').next().unwrap_or(object).chars().filter(|c| !matches!(c, '[' | ']' | '"' | '`')).collect())
}

/// "customers · ix_customers_email", the object column of operator lists.
pub(crate) fn where_(node: &PlanNode) -> String {
    match (table(node), node.index.as_deref()) {
        (Some(t), Some(i)) => format!("{t} · {i}"),
        (Some(t), None) => t,
        (None, _) => node.detail.clone().unwrap_or_default(),
    }
}

pub(crate) fn rows_text(node: &PlanNode) -> String {
    let est = node.est_rows_total();
    match node.act_rows {
        Some(act) => format!("{} → {}", culprit_core::format::count(est), culprit_core::format::count(act)),
        None => culprit_core::format::count(est),
    }
}

/// `rows_text` with its noun: "1 row", "12 rows", "12 → 1 row" (the noun follows the last number).
pub(crate) fn rows_phrase(node: &PlanNode) -> String {
    let last = node.act_rows.unwrap_or_else(|| node.est_rows_total());
    format!("{} {}", rows_text(node), if last.round() == 1.0 { "row" } else { "rows" })
}

/// The plan's statement and the highest-cost operators, for the one-line "where the cost goes" summary.
pub(crate) fn top_costs(plan: &Plan, limit: usize) -> Vec<(&'_ PlanNode, f64)> {
    let mut list: Vec<(&PlanNode, f64)> = plan.nodes().into_iter().filter_map(|n| Some((n, share(plan, n)?))).collect();
    list.sort_by(|a, b| b.1.total_cmp(&a.1));
    list.into_iter().filter(|(_, s)| *s >= 0.01).take(limit).collect()
}

pub(crate) fn node_label(node: &PlanNode) -> String {
    match table(node) {
        Some(t) => format!("{} on {t}", node.op),
        None => node.op.clone(),
    }
}

/// When the export was made, for the footer.
pub(crate) fn exported_at() -> String {
    chrono::Utc::now().format("%Y-%m-%d %H:%M UTC").to_string()
}

/// The report's server, without names that identify the deployment: "SQL Server 2022, production".
pub(crate) fn server_line(report: &Report) -> String {
    let s = &report.snapshot;
    format!("{}, {} environment", s.server.product, environment_label(s.environment))
}

#[cfg(test)]
mod tests;
