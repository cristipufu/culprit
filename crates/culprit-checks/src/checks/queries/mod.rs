//! Checks of statements: the heaviest ones and why, chatty ones, plan warnings and insights, plans that changed for
//! the worse, statements that got slower, runs that swing with their values, and spills to temporary files. What several of them share (how a slow log's time is named, the
//! statement's shape) is here; each check is a file of its own.

use culprit_core::insights::{converts_column, current_conversions, mismatch_fix};
use culprit_core::{
    Capability, Category, CheckInfo, Confidence, Engine, Finding, Fix, Metric, NodeKind, Plan, PlanNode, PlanVariant,
    PlanWarning, QueryRef, QueryStat, Severity, Snapshot, SubjectKind, Unit, format,
};

use culprit_core::Check;

use crate::indexes::scan_groups;
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

/// What Culprit's own scans measured of a statement whose entry is recent, on a source of running totals: at least half
/// of its runs came in a day or more of those scans ([`culprit_core::Workload::over_scans`]), where it took a larger
/// share of database time than over everything since the source began counting. Its share of those totals is diluted
/// by all the time before its entry existed (a statement the application started sending days ago, or an entry evicted
/// and created again), so it is judged by the scans instead: what it costs now.
pub(crate) fn recent_entry(snapshot: &Snapshot, q: &QueryStat) -> Option<culprit_core::ScanSpan> {
    let w = snapshot.workload.as_ref()?;
    w.over_scans(q).filter(|s| s.runs >= q.executions * 0.5 && s.share() > w.share_of(q.total_time_ms, q))
}

/// The share of database time a statement is weighed by for being heavy: its share of the workload, or of Culprit's
/// scans for a recent entry ([`recent_entry`]).
pub(crate) fn heavy_share(snapshot: &Snapshot, q: &QueryStat) -> f64 {
    let Some(w) = snapshot.workload.as_ref() else { return 0.0 };
    recent_entry(snapshot, q).map_or_else(|| w.share_of(q.total_time_ms, q), |s| s.share())
}

/// How one statement changes many rows by their keys, in the engine's terms.
pub(super) fn many_keys(engine: Engine, verb: &str) -> String {
    match engine {
        Engine::Mssql => format!("an {verb} over a list of keys, or joined to a table-valued parameter"),
        Engine::Postgres => format!("an {verb} over an array of keys (`= ANY($1)`), or joined to a VALUES list"),
        _ => format!("an {verb} over a list of keys (`IN (…)`), or joined to a temporary table of the changes"),
    }
}

/// How one statement writes many rows, in the engine's terms.
pub(super) fn bulk_insert(engine: Engine) -> &'static str {
    match engine {
        Engine::Mssql => "a multi-row VALUES, a table-valued parameter, or a bulk insert",
        Engine::Postgres => "a multi-row VALUES, arrays unnested into rows, or COPY",
        Engine::Mysql => "a multi-row VALUES, or LOAD DATA",
        Engine::Sqlite => "a multi-row VALUES",
    }
}

/// The columns an INSERT's column list names, unquoted: `INSERT INTO t (a, "b") VALUES …` is `a, b`. `None` without
/// one (`INSERT INTO t VALUES …`): it sets every column.
pub(super) fn insert_columns(text: &str) -> Option<Vec<String>> {
    let (at, verb) = first_verb(text)?;
    let rest = text[at + verb.len()..].trim_start();
    let open = rest.find('(')?;
    // Only the target before the list: a VALUES, SELECT or OUTPUT first means there is none.
    if rest[..open]
        .split_whitespace()
        .any(|w| ["values", "select", "default", "output"].iter().any(|k| w.eq_ignore_ascii_case(k)))
    {
        return None;
    }
    let close = open + rest[open..].find(')')?;
    let columns: Vec<String> = rest[open + 1..close]
        .split(',')
        .map(|c| c.trim().trim_matches(|ch| matches!(ch, '"' | '`' | '[' | ']')).to_string())
        .filter(|c| !c.is_empty())
        .collect();
    (verb == "insert" && !columns.is_empty()).then_some(columns)
}

/// Whether an INSERT sets every column of a unique key of `table` (a key the database generates, an identity or a
/// sequence, never repeats): another transaction can insert the same key, and waits for this one to commit. Unknown,
/// with no index listed for the table, is true.
pub(super) fn inserts_repeatable_key(snapshot: &Snapshot, q: &QueryStat, table: &culprit_core::TableStat) -> bool {
    snapshot.indexes_on(&table.schema, &table.name).next().is_none()
        || inserted_unique_key(snapshot, q, table).is_some()
}

/// The unique index of `table` (the primary key first) whose every column an INSERT's column list sets, or every one
/// when it has no list: each run looks its key up there before it writes, and another transaction can insert the same
/// key. `None` when no enabled unique index has all its columns set by it.
pub(super) fn inserted_unique_key<'a>(
    snapshot: &'a Snapshot,
    q: &QueryStat,
    table: &'a culprit_core::TableStat,
) -> Option<&'a culprit_core::IndexStat> {
    let columns = insert_columns(main_statement(&q.text));
    snapshot
        .indexes_on(&table.schema, &table.name)
        .filter(|i| (i.is_unique || i.is_primary_key) && !i.is_disabled)
        .filter(|i| match &columns {
            // An expression (lower(email)) is of columns the list may name: it can repeat.
            Some(named) => i.columns.iter().all(|c| {
                !c.chars().all(|ch| ch.is_alphanumeric() || ch == '_')
                    || named.iter().any(|n| n.eq_ignore_ascii_case(c))
            }),
            None => true,
        })
        .min_by_key(|i| !i.is_primary_key)
}

/// How a later scan shows a change to `q` on a source of running totals (pg_stat_statements, performance_schema, the
/// plan cache), with `between` what the difference between two scans should show. Those totals still hold every run
/// from before the change, so a week moves a years-long count or average by a hair: the difference between two of
/// Culprit's scans (What changed) is what shows it, and a rewritten statement is a new entry there, the old one then
/// stopped running. `None` on a windowed source (Query Store, a log), whose next scan covers the runs since.
pub(super) fn verify_between_scans(snapshot: &Snapshot, q: &QueryStat, between: &str) -> Option<String> {
    let w = snapshot.workload.as_ref().filter(|w| !w.windowed)?;
    let rewritten = if culprit_core::sql::is_session_statement(&q.text) {
        ""
    } else {
        " A rewritten statement is a new entry with an id of its own, and this one then shows as stopped running."
    };
    Some(format!(
        "{} keeps running totals, so a later scan's count of runs, share and averages for this query still hold every run from before the change. After it ships, scan again a day or more later and open What changed from the change on: there {between}.{rewritten}",
        w.source
    ))
}

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

/// A plan step in words: "an index range scan of shop.orders that examines 11K rows a run". What it reads is said as
/// measured where it was (an actual plan's rows, or MySQL's rows examined per run, `examined_per_run`, when the plan reads
/// no other table), and otherwise as the estimate it is; the table's size only for a scan that reads it in full, never
/// for a seek.
fn describe_node(plan: &Plan, node: &PlanNode, examined_per_run: Option<f64>) -> String {
    let what = node.op.to_ascii_lowercase();
    // "an index scan", "a seq scan".
    let a = if what.starts_with(['a', 'e', 'i', 'o', 'u']) { "an" } else { "a" };
    let Some(object) = node.object.as_deref() else {
        return format!("{a} {what}");
    };
    // The rows a statement examined are all this step's when its table is the only one the plan reads.
    let only_table = plan.nodes().into_iter().filter(|n| reads_table(n)).all(|n| n.object.as_deref() == Some(object));
    if let Some(rows) = examined_per_run.filter(|_| only_table) {
        return format!("{a} {what} of {object} that examines {} rows a run", format::count(rows));
    }
    // A scan that reads its table in full, once, reads the table's size: a fact of the table, not an estimate.
    let whole_table = node.kind == NodeKind::Scan
        && node.runs() <= 1.0
        && node.rows_read.is_none_or(|read| node.table_rows == Some(read));
    match node.rows_read_total() {
        Some(rows) if node.act_rows.is_some() || whole_table => {
            format!("{a} {what} of {object} that reads {} rows", format::count(rows))
        }
        Some(rows) => format!("{a} {what} of {object}, estimated to read {} rows", format::count(rows)),
        None => format!("{a} {what} of {object}"),
    }
}

/// Whether a plan step reads rows of a table or an index (a scan, a seek, a lookup), as opposed to combining, sorting
/// or writing what other steps read.
fn reads_table(node: &PlanNode) -> bool {
    matches!(node.kind, NodeKind::Scan | NodeKind::Seek | NodeKind::Lookup)
        && (node.object.is_some() || node.index.is_some())
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(first) => first.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}
