//! Checks of statements: the heaviest ones and why, chatty ones, plan warnings and insights, plans that changed for
//! the worse, statements that got slower, runs that swing with their values, and spills to temporary files. What several of them share (how a slow log's time is named, the
//! statement's shape) is here; each check is a file of its own.

use culprit_core::insights::{converts_column, current_conversions, mismatch_fix};
use culprit_core::{
    Capability, Category, CheckInfo, Confidence, Engine, Finding, Fix, Metric, NodeKind, PlanNode, PlanVariant,
    PlanWarning, QueryRef, QueryStat, Severity, Snapshot, SubjectKind, Unit, format,
};

use culprit_core::Check;

use crate::support::*;

mod chatty_query;
mod heavy_query;
mod parameter_sensitive;
mod plan_insights;
mod plan_regression;
mod plan_warnings;
mod regression;
mod temp_spill;
mod unstable_query;

pub use chatty_query::*;
pub use heavy_query::*;
pub use parameter_sensitive::*;
pub use plan_insights::*;
pub use plan_regression::*;
pub use plan_warnings::*;
pub use regression::*;
pub use temp_spill::*;
pub use unstable_query::*;

/// The checks this area registers (`crate::all()` runs every area's), in the order their findings tie.
pub(crate) const CHECKS: &[&dyn Check] = &[
    &HeavyQueryCheck,
    &ChattyQueryCheck,
    &PlanWarningsCheck,
    &PlanInsightsCheck,
    &PlanRegressionCheck,
    &RegressionCheck,
    &UnstableQueryCheck,
    &ParameterSensitiveCheck,
    &TempSpillCheck,
];

const HEAVY_SHARE: f64 = 0.05;

/// Text lowercased with names unquoted, whitespace collapsed and no space before an opening parenthesis, so `COUNT ( * )`
/// (MySQL's digest text) and `[dbo].[f] (x)` read like `count(*)` and `dbo.f(x)`.
fn squeeze(text: &str) -> String {
    let lower = text.to_ascii_lowercase().replace(['`', '[', ']', '"'], "");
    let collapsed = lower.split_whitespace().collect::<Vec<_>>().join(" ");
    collapsed.replace(" (", "(").replace("( ", "(").replace(" )", ")").replace(" . ", ".")
}

/// What a workload's times add up to, in sentences: "database time", or "logged slow-run time" when the slow query log
/// keeps only runs over long_query_time.
pub(crate) fn time_noun(snapshot: &Snapshot) -> &'static str {
    if slow_log_only(snapshot) { "logged slow-run time" } else { "database time" }
}

pub(crate) fn share_metric_label(snapshot: &Snapshot) -> &'static str {
    if slow_log_only(snapshot) { "Share of logged slow-run time" } else { "Share of DB time" }
}

/// [`apply_query_impact`], with the label naming the time the share is of.
pub(crate) fn apply_impact(snapshot: &Snapshot, f: &mut Finding, impact: &QueryImpact) {
    apply_query_impact(f, impact);
    if slow_log_only(snapshot) {
        f.impact.label = format!("{} of logged slow-run time", format::percent(impact.share));
    }
}

fn describe_node(node: &PlanNode) -> String {
    let what = node.op.to_ascii_lowercase();
    // "an index scan", "a seq scan".
    let a = if what.starts_with(['a', 'e', 'i', 'o', 'u']) { "an" } else { "a" };
    match (&node.object, node.rows_read.or(node.table_rows)) {
        (Some(object), Some(rows)) => format!("{a} {what} of {object} that reads {} rows", format::count(rows)),
        (Some(object), None) => format!("{a} {what} of {object}"),
        _ => format!("{a} {what}"),
    }
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(first) => first.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}
