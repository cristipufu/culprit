//! Plan insights: plain-language problems in one query plan, each tied to the operators it is about, ranked by how much of
//! the plan's work those operators take, with a fix and a way to verify it.
//!
//! A pure function of the plan, so every surface shows the same thing: diagram badges, the Insights list, the Operations
//! table, findings, the CLI and the MCP server. Rules read SQL Server, Postgres and MySQL plan shapes alike; when an engine
//! (or an estimated plan) doesn't record something a rule needs, the rule stays silent and [`coverage_note`] says so in one
//! sentence instead of pretending there's nothing to find.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::finding::Severity;
use crate::format;
use crate::model::Engine;
#[cfg(test)]
use crate::model::{ColumnInfo, IndexStat, Snapshot};
use crate::plan::{INDEX_KEEPS_AT_MOST, MISESTIMATE_FACTOR, NodeKind, Plan, PlanKind, PlanNode, PlanWarning};
use crate::sql::{clean_identifier, split_top_level_and as top_level_and};

mod catalog;
mod growth;
mod index;
mod memory;
mod parallelism;
mod predicates;
mod query;
mod server;
mod statistics;
#[cfg(test)]
mod tests;

pub use catalog::{
    CHANGED_SINCE_CAPTURE, COST_THRESHOLD, DATABASE_MAXDOP, LIMIT_ROWS, SERVER_MAXDOP, TABLE_GONE, TABLE_INDEXES,
    catalog_table_matches, mark_changed_columns, mark_parallel_settings, mark_plan, mark_possible_keys, mark_query,
    mark_snapshot, mark_snapshot_index_kinds, mark_snapshot_keys, statistics_maintenance_note,
};
#[cfg(test)]
use catalog::{CatalogTable, mark_index_kinds, mark_table_indexes};
use catalog::{table_indexes, updates_statistics};
use index::*;
use memory::*;
use parallelism::*;
use predicates::*;
pub use predicates::{condition_count, converts_column, current_conversions, mismatch_fix, mysql_guessed_filtered};
use query::*;
use server::*;
use statistics::*;
pub use statistics::{StaleIndexStatistics, stale_index_statistics, stale_index_statistics_detail};

/// What kind of change fixes it, for grouping and icons.
#[derive(Serialize, Deserialize, TS, Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[serde(rename_all = "lowercase")]
#[ts(export)]
pub enum InsightArea {
    /// An index would fix it.
    Index,
    /// The query text should change.
    Query,
    /// Row estimates are off: statistics.
    Statistics,
    /// Memory: grants, spills, work_mem.
    Memory,
    /// Parallelism.
    Parallelism,
    /// A server or database setting, or maintenance.
    Server,
}

#[derive(Serialize, Deserialize, TS, Clone, Debug, PartialEq)]
#[ts(export)]
pub struct PlanInsight {
    /// Stable rule id, e.g. `key-lookups`.
    pub rule: String,
    pub area: InsightArea,
    pub severity: Severity,
    /// Share of the plan's work at the operators involved, 0–1. The ranking key after severity.
    pub impact: f64,
    /// `impact` is a weight the rule gives for ranking, not a measured share of the plan's work: the optimizer
    /// stopped searching, a setting, a memory grant, a plan without a run time. It isn't shown as a share and doesn't
    /// raise the severity; the rule's floor and cap set it.
    #[serde(default)]
    pub fixed_weight: bool,
    /// Operators this is about; the first one is the one to show.
    pub nodes: Vec<u32>,
    /// One plain sentence.
    pub title: String,
    /// Why it matters, with the plan's own numbers.
    pub detail: String,
    /// What to change.
    pub fix: String,
    /// Copy-ready SQL for the fix, when there is a statement to run. Culprit never runs it.
    pub fix_sql: Option<String>,
    /// How to tell the fix worked.
    pub verify: String,
    // The exact optimizer request: several requests can share a table/operator. Never infer keys from the node
    // or the human title. Kept last so ts-rs exports it with the existing final field on one line.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub index_hint: Option<crate::plan::PlanIndexHint>,
}

/// Insights for one plan plus what this plan can't tell (an estimated plan has no actual rows; MySQL keeps no memory
/// grants), shown next to an empty list so "nothing found" is honest.
#[derive(Serialize, Deserialize, TS, Clone, Debug, PartialEq, Default)]
#[ts(export)]
pub struct PlanInsights {
    pub items: Vec<PlanInsight>,
    pub note: Option<String>,
}

/// Every rule, for docs and `culprit checks`-style listings: (id, engines, one line).
pub const RULES: &[(&str, &[Engine], &str)] = &[
    ("deep-pagination", ALL, "A literal OFFSET skips many matches before returning a later page"),
    (
        "partition-filtering",
        &[Engine::Postgres],
        "A measured run visits many partitions and filters most rows afterwards; inspect intended pruning",
    ),
    (
        "cross-table-or",
        ALL,
        "A filter across tables after a scan; evaluate a semantics-preserving rewrite before indexing",
    ),
    ("key-lookups", &[Engine::Mssql], "Thousands of key or RID lookups back into the table"),
    ("scan-filter", ALL, "A scan reads far more rows than its filter keeps"),
    ("missing-index-hint", &[Engine::Mssql], "The optimizer's own index suggestion"),
    ("implicit-conversion", &[Engine::Mssql], "A type mismatch that blocks an index seek (CONVERT_IMPLICIT)"),
    ("non-sargable", ALL, "A function or cast around a filtered column"),
    ("leading-wildcard", ALL, "LIKE '%…' that can't use an index"),
    ("not-in", ALL, "NOT IN that checks the subquery row by row"),
    ("scalar-udf", &[Engine::Mssql], "A scalar function called once per row"),
    ("table-variable", &[Engine::Mssql], "A table variable estimated at one row"),
    ("parallelism-blocked", &[Engine::Mssql], "An expensive plan that can't go parallel"),
    ("parallel-skew", &[Engine::Mssql, Engine::Postgres], "One thread or worker doing most of the work"),
    ("eager-spool", &[Engine::Mssql], "A temporary index built on every run"),
    ("automatic-index", &[Engine::Sqlite], "SQLite builds a throwaway index on every run because none fits"),
    ("spill", ALL, "A sort or hash that ran out of memory and wrote to disk, or an exchange that spilled"),
    ("memory-grant", &[Engine::Mssql], "A memory grant far larger than what the query uses, or a wait for one"),
    ("misestimate", ALL, "Actual rows far from the estimate"),
    ("nested-loops", ALL, "A nested loop rereading its inner side thousands of times"),
    ("big-sort", ALL, "A large sort that dominates the plan"),
    ("no-join-predicate", ALL, "Two inputs joined without a condition"),
    ("missing-statistics", &[Engine::Mssql], "Columns without statistics"),
    ("optimizer-timeout", &[Engine::Mssql], "The optimizer stopped before finding the best plan"),
    ("legacy-ce", &[Engine::Mssql], "Planned with the legacy cardinality estimator"),
    ("slow-compile", &[Engine::Mssql, Engine::Postgres], "Planning takes long, or longer than running"),
    ("many-to-many", &[Engine::Mssql], "A many-to-many merge join working through tempdb"),
    ("remote-query", &[Engine::Mssql], "Rows pulled from a linked server"),
    ("heap-fetches", &[Engine::Postgres], "An index-only scan that still visits the table"),
    ("lossy-bitmap", &[Engine::Postgres], "A bitmap scan that ran out of work_mem"),
    ("workers-short", &[Engine::Postgres], "Fewer parallel workers started than planned"),
    ("jit", &[Engine::Postgres], "JIT compilation taking a large share of the run"),
    ("correlated-subquery", &[Engine::Postgres, Engine::Mysql], "A subquery run once per outer row"),
    ("join-buffer", &[Engine::Mysql], "A join without an index (join buffer / hash join over a scan)"),
    ("temp-table-sort", &[Engine::Mysql], "Grouping in a temporary table, then a filesort"),
    ("index-ignored", &[Engine::Mysql], "An index was possible but the whole table was read"),
];

const ALL: &[Engine] = &[Engine::Mssql, Engine::Postgres, Engine::Mysql];

/// Rows of the lookups/loops a rule needs before it's worth saying anything.
const MANY_RUNS: f64 = 1_000.0;
const MANY_LOOPS: f64 = 10_000.0;

/// The materiality gate (`Draft::finish`): an insight is more than Low only when the operators it is about handle at
/// least this many rows…
const MANY_ROWS: f64 = 10_000.0;
/// …in a plan that does real work: a measured run of at least this long, or, unmeasured, an estimated cost of at least
/// what reading about `MANY_ROWS` rows costs in the engine's own units (`estimated_work_threshold`). A problem measured in time of
/// its own (compiling, JIT, waiting for memory) is material when that time reaches this.
pub const MATERIAL_MS: f64 = 10.0;

const STATISTICS_NOTE: &str = "This plan updates optimizer statistics. Reading and sorting samples is maintenance work, so ordinary query index, ORDER BY and parallelism advice does not apply. Review its frequency and sampling only if measured maintenance cost warrants it.";

/// Compute and attach insights (and the coverage note) to a plan.
pub fn annotate(plan: &mut Plan) {
    plan.insights = Some(PlanInsights { items: insights(plan), note: coverage_note(plan) });
}

/// Whether a plan builds or changes the schema rather than answering a query: a CREATE, ALTER or DROP (an index build
/// reads and sorts the whole table because that is its work), or SQL Server's own statement for an index build, `insert
/// [t] select * from [t]` into an Index Insert. No rule reads advice from it: an index for its sort, statistics on the
/// column it indexes.
pub fn changes_schema(plan: &Plan) -> bool {
    let statement = plan.statement.as_deref().unwrap_or_default().trim_start().to_ascii_lowercase();
    let first = statement.split(|c: char| !c.is_ascii_alphanumeric()).next().unwrap_or_default();
    if matches!(first, "create" | "alter" | "drop" | "truncate" | "reindex" | "cluster" | "vacuum" | "analyze") {
        return true;
    }
    // `insert [s].[t] select * from [s].[t]`, as SQL Server writes it (lower case, bracketed): the same table into
    // itself, through an index insert at the root.
    let written = plan.statement.as_deref().unwrap_or_default().trim();
    let words: Vec<&str> = written.split_whitespace().collect();
    plan.root.op.contains("Index Insert")
        && written == written.to_ascii_lowercase()
        && matches!(words.as_slice(), ["insert", into, "select", "*", "from", from] if into == from && into.starts_with('['))
}

/// What this plan can't show, in one sentence, so an empty list isn't mistaken for a clean bill of health.
pub fn coverage_note(plan: &Plan) -> Option<String> {
    if plan.kind != PlanKind::Actual && (plan.kind == PlanKind::Generic || plan.facts().generic_plan_reason.is_some()) {
        if plan.engine == Engine::Sqlite {
            return Some("Planned with Culprit's SQLite library and unknown parameter values. The application's version, modules and build options may differ. This plan shows access paths, without row counts, costs or timings. Compare application-log durations and a plan from the application's connection with representative values.".into());
        }
        return Some("This generic plan was obtained without the application's parameter values. Its estimates and connection settings may differ from an application's run, and partitions listed here may be skipped at runtime. It does not identify the plan used by historical executions. Actual rows and timing require a measured plan with representative values.".into());
    }
    if updates_statistics(plan) {
        return Some(STATISTICS_NOTE.into());
    }
    if changes_schema(plan) {
        return Some(
            "This plan builds an index or changes the schema: reading and sorting the table is the work it's for, so \
             no advice is read from it."
                .into(),
        );
    }
    let actual = has_actuals(plan);
    let mariadb = is_mariadb(plan);
    match (plan.engine, actual) {
        (Engine::Sqlite, _) => Some(
            "SQLite's EXPLAIN QUERY PLAN shows the access path only: no estimated or actual rows, costs or timings.".into(),
        ),
        (Engine::Mysql, false) if mariadb => Some(
            "MariaDB's EXPLAIN has no actual rows or timings; ANALYZE adds them, with pages read and sort spills.".into(),
        ),
        (Engine::Mysql, true) if mariadb => {
            Some("MariaDB plans don't record memory grants or parallel threads, so those checks don't apply.".into())
        }
        (Engine::Mssql, false) => Some(
            "Estimated plan: misestimates, spills, unused memory grants and uneven threads need an actual plan.".into(),
        ),
        (Engine::Mssql, true) => None,
        (Engine::Yugabyte, false) => Some("Without EXPLAIN ANALYZE DIST, actual DocDB RPCs, storage rows scanned and execution timings are unknown.".into()),
        (Engine::Yugabyte, true) => Some("DocDB storage RPCs and rows scanned are distinct from PostgreSQL buffer pages. RPC timings may overlap and are not exclusive shares of execution time.".into()),
        (Engine::Postgres, false) => Some(
            "Without EXPLAIN ANALYZE there are no actual rows, spills or timings to check. PostgreSQL plans never carry index suggestions; Culprit reads filters instead.".into(),
        ),
        (Engine::Postgres, true) => Some(
            "PostgreSQL plans carry no index suggestions or memory grants; index advice comes from the filters.".into(),
        ),
        // EXPLAIN ANALYZE arrived in MySQL 8.0.18: older servers can't add actual rows at all.
        (Engine::Mysql, false)
            if plan.facts().server_version.as_ref().is_some_and(|v| !crate::version_at_least(v, [8, 0, 18])) =>
        {
            Some("MySQL's EXPLAIN has no actual rows, spills, memory or threads, and this version can't measure a run (EXPLAIN ANALYZE arrived in MySQL 8.0.18).".into())
        }
        (Engine::Mysql, false) => Some(
            "MySQL's EXPLAIN has no actual rows, spills, memory or threads; EXPLAIN ANALYZE adds actual rows.".into(),
        ),
        (Engine::Mysql, true) => {
            Some("MySQL plans don't record spills, memory grants or parallel threads, so those checks don't apply.".into())
        }
    }
}

fn has_actuals(plan: &Plan) -> bool {
    plan.kind == PlanKind::Actual || plan.nodes().iter().any(|n| n.act_rows.is_some())
}

/// Every insight for this plan, most important first. None for a plan that changes the schema ([`changes_schema`]).
pub fn insights(plan: &Plan) -> Vec<PlanInsight> {
    if changes_schema(plan) || updates_statistics(plan) {
        return Vec::new();
    }
    let cx = Cx::new(plan);
    let rules: &[fn(&Cx) -> Vec<Draft>] = &[
        growth::deep_pagination,
        growth::partition_filtering,
        key_lookups,
        missing_index_hint,
        scan_filter,
        implicit_conversion,
        non_sargable,
        leading_wildcard,
        not_in,
        scalar_udf,
        table_variable,
        parallelism_blocked,
        parallel_skew,
        eager_spool,
        automatic_index,
        spill,
        memory_grant,
        misestimate,
        nested_loops,
        big_sort,
        no_join_predicate,
        missing_statistics,
        optimizer_limits,
        slow_compile,
        many_to_many,
        remote_query,
        heap_fetches,
        lossy_bitmap,
        workers_short,
        jit,
        correlated_subquery,
        join_buffer,
        temp_table_sort,
        index_ignored,
    ];
    let mut out: Vec<PlanInsight> = Vec::new();
    for rule in rules {
        for d in rule(&cx) {
            if out.iter().any(|o| o.rule == d.rule && o.nodes.first() == d.nodes.first()) || on_system_objects(&cx, &d)
            {
                continue;
            }
            let mut insight = d.finish(&cx);
            // An index for a table that is gone: nothing to create, whatever the plan read.
            if insight.area == InsightArea::Index
                && let Some(table) = insight.nodes.iter().find_map(|id| cx.node(*id)?.facts().table_missing.clone())
            {
                insight.fix = format!(
                    "Nothing to create: {} isn't in the database's catalog any more, so it was dropped or renamed \
                     since this plan was captured.",
                    table_leaf(&table)
                );
                insight.fix_sql = None;
                insight.verify = "Nothing to check while the table is gone.".into();
            }
            out.push(insight);
        }
    }
    out.sort_by(|a, b| b.severity.cmp(&a.severity).then(b.impact.total_cmp(&a.impact)));
    out
}

/// Whether an insight advises an index, a rewrite or statistics for nothing but the engine's own objects: a monitoring
/// query's plan reads the catalog, whose tables and views no one can index, rewrite or analyze. Its objects are those
/// of the operators it is about, or of everything below one that reads none (a sort, a join, the whole plan).
fn on_system_objects(cx: &Cx, d: &Draft) -> bool {
    if !matches!(d.area, InsightArea::Index | InsightArea::Query | InsightArea::Statistics) {
        return false;
    }
    let mut objects: Vec<String> = Vec::new();
    // A warning on the plan itself may highlight no operator (for example a compilation-time conversion warning).
    // Its scope is the whole plan, just like a warning attached to the root, not an absence of catalog evidence.
    if d.nodes.is_empty() {
        objects.extend(cx.nodes.iter().filter_map(|n| actionable_object(n)));
    }
    for n in d.nodes.iter().filter_map(|id| cx.node(*id)) {
        let mut involved = Vec::new();
        if d.subtree || n.object.is_none() {
            n.walk(&mut involved);
        } else {
            involved.push(n);
        }
        objects.extend(involved.into_iter().filter_map(actionable_object));
    }
    let catalog_plan = reads_only_the_catalog(cx.plan);
    !objects.is_empty() && objects.into_iter().all(|o| is_system_object(&o) || (catalog_plan && !o.contains('.')))
}

/// Whether a SQL Server plan names a schema only for the engine's own objects. Its internal table-valued functions
/// (Query Store's `QUERY_STORE_RUNTIME_STATS_INTERVAL`, the in-memory halves of `sys.query_store_*`) and table
/// variables carry no schema; in a plan whose every schema-qualified object is the catalog's, they are the catalog's
/// too. Other engines name tables without a schema when the session has none, so only SQL Server, which always names
/// the schema of an application's table, reads a missing one this way.
fn reads_only_the_catalog(plan: &Plan) -> bool {
    if plan.engine != Engine::Mssql {
        return false;
    }
    let nodes = plan.nodes();
    let mut qualified = nodes.iter().filter_map(|n| n.object.as_deref()).filter(|o| o.contains('.')).peekable();
    qualified.peek().is_some() && qualified.all(is_system_object)
}

/// Whether step `n` of `plan` reads a whole table in a way that can weigh on a run: it reads its table whole on every
/// run ([`PlanNode::reads_table_whole`]), the table has [`SCAN_ROWS`](crate::plan::SCAN_ROWS) rows or more or the step
/// reads that many over its runs, and the table isn't the engine's own (its catalog, [`is_system_object`], or an
/// unqualified object in a plan that reads only SQL Server's catalog). Reading all 2 rows of `pg_database` on every
/// run, or a table of a few hundred rows, is no reason a plan's runs cost what they do, and no surface says it as one.
pub fn whole_read_matters(plan: &Plan, n: &PlanNode) -> bool {
    if !n.reads_table_whole() {
        return false;
    }
    let object = n.object.as_deref().unwrap_or_default();
    if is_system_object(object) || (!object.contains('.') && reads_only_the_catalog(plan)) {
        return false;
    }
    let bar = crate::plan::SCAN_ROWS;
    n.table_rows.is_some_and(|rows| rows >= bar) || n.rows_read_total().is_some_and(|rows| rows >= bar)
}

/// Whether a plan's object is the engine's own, its catalog and statistics: SQL Server's `sys` schema (Query Store's
/// `plan_persist_*` tables among them) and resource database, the standard `INFORMATION_SCHEMA`, PostgreSQL's
/// `pg_catalog` and `pg_toast`, MySQL's `mysql` and `performance_schema` databases, and SQLite's `sqlite_*` tables.
pub fn is_system_object(object: &str) -> bool {
    let parts: Vec<String> = object.split('.').map(|p| clean_identifier(p).to_ascii_lowercase()).collect();
    let schema = parts.len().checked_sub(2).and_then(|at| parts.get(at)).map(String::as_str);
    matches!(schema, Some("sys" | "information_schema" | "pg_catalog" | "pg_toast" | "performance_schema" | "mysql"))
        || schema.is_some_and(|s| s.starts_with("_timescaledb_") || s == "timescaledb_information")
        || parts.first().is_some_and(|db| db == "mssqlsystemresource")
        || parts.last().is_some_and(|table| table.starts_with("sqlite_"))
}

// ── context ──────────────────────────────────────────────────────────────────────────────────────

struct Cx<'a> {
    plan: &'a Plan,
    nodes: Vec<&'a PlanNode>,
    parent: HashMap<u32, &'a PlanNode>,
    total: f64,
    /// Whether the plan does real work (see `MATERIAL_MS`).
    material: bool,
    /// Steps whose cost rests on a default guess for a parameter ([`Plan::guesses`]).
    guessed: Vec<u32>,
}

impl<'a> Cx<'a> {
    fn new(plan: &'a Plan) -> Cx<'a> {
        let nodes = plan.nodes();
        let mut parent = HashMap::new();
        for n in &nodes {
            for c in &n.children {
                parent.insert(c.id, *n);
            }
        }
        // Under a Postgres Limit, children's costs assume they run to completion and add up to more than the root's.
        let own: f64 = nodes.iter().map(|n| n.self_cost).sum();
        let total = plan.total_cost.max(plan.root.subtree_cost).max(own);
        let guessed = plan.guesses().into_iter().map(|(id, _)| id).collect();
        Cx {
            plan,
            nodes,
            parent,
            total: if total > 0.0 { total } else { 1.0 },
            material: does_real_work(plan),
            guessed,
        }
    }

    /// " 45% of this plan's work." where the plan states the share of these steps ([`stated_share`]), else nothing: a
    /// cheap plan's or a guessed step's share of the cost isn't where the statement's work is.
    fn work_sentence(&self, impact: f64, nodes: &[u32]) -> String {
        if self.states_share(nodes) {
            format!(" {} of this plan's work.", capitalize(&format::percent(impact)))
        } else {
            String::new()
        }
    }

    /// Whether the plan states a share of its work for these steps: the one rule of [`stated_share`].
    fn states_share(&self, nodes: &[u32]) -> bool {
        self.plan.states_shares() && !nodes.iter().any(|id| self.guessed.contains(id))
    }

    fn engine(&self) -> Engine {
        self.plan.engine
    }

    fn node(&self, id: u32) -> Option<&'a PlanNode> {
        self.nodes.iter().find(|n| n.id == id).copied()
    }

    fn mariadb(&self) -> bool {
        is_mariadb(self.plan)
    }

    fn share(&self, n: &PlanNode) -> f64 {
        (n.self_cost / self.total).clamp(0.0, 1.0)
    }

    fn subtree(&self, n: &PlanNode) -> f64 {
        (n.subtree_cost / self.total).clamp(0.0, 1.0)
    }

    fn parent(&self, n: &PlanNode) -> Option<&'a PlanNode> {
        self.parent.get(&n.id).copied()
    }

    fn statement(&self) -> String {
        self.plan.statement.clone().unwrap_or_default().to_ascii_lowercase()
    }
}

struct Draft {
    rule: &'static str,
    area: InsightArea,
    floor: Severity,
    cap: Severity,
    impact: f64,
    nodes: Vec<u32>,
    index_hint: Option<crate::plan::PlanIndexHint>,
    /// The insight is about everything below its first operator (the whole plan, a parallel branch): the materiality
    /// gate counts the rows of the busiest operator there.
    subtree: bool,
    /// Time the problem itself measurably costs (compiling, JIT, waiting for memory): the gate weighs that instead of
    /// rows.
    cost_ms: Option<f64>,
    /// `impact` is the rule's weight, not a share (see [`PlanInsight::fixed_weight`]).
    fixed_weight: bool,
    title: String,
    detail: String,
    fix: String,
    fix_sql: Option<String>,
    verify: String,
}

impl Draft {
    fn new(rule: &'static str, area: InsightArea, impact: f64, nodes: Vec<u32>) -> Draft {
        Draft {
            rule,
            area,
            floor: Severity::Low,
            cap: Severity::High,
            impact,
            nodes,
            index_hint: None,
            subtree: false,
            cost_ms: None,
            fixed_weight: false,
            title: String::new(),
            detail: String::new(),
            fix: String::new(),
            fix_sql: None,
            verify: String::new(),
        }
    }

    /// Severity from the share of the plan's work, within the rule's floor and cap, then the one materiality gate: at
    /// most Low unless the problem costs real work (`MANY_ROWS` rows in a material plan, or `MATERIAL_MS` of its own),
    /// so no floor lifts a cheap plan's quirk.
    fn finish(self, cx: &Cx) -> PlanInsight {
        let by_impact = if self.fixed_weight {
            Severity::Low
        } else if self.impact >= 0.3 {
            Severity::High
        } else if self.impact >= 0.08 {
            Severity::Medium
        } else {
            Severity::Low
        };
        let mut severity = by_impact.max(self.floor).min(self.cap);
        if !self.material(cx) {
            severity = severity.min(Severity::Low);
        }
        PlanInsight {
            rule: self.rule.into(),
            area: self.area,
            severity,
            impact: (self.impact * 1000.0).round() / 1000.0,
            fixed_weight: self.fixed_weight,
            nodes: self.nodes,
            index_hint: self.index_hint,
            title: self.title,
            detail: self.detail,
            fix: self.fix,
            fix_sql: self.fix_sql,
            verify: self.verify,
        }
    }

    fn material(&self, cx: &Cx) -> bool {
        if let Some(ms) = self.cost_ms {
            return ms >= MATERIAL_MS;
        }
        let mut involved: Vec<&PlanNode> = Vec::new();
        for n in self.nodes.iter().filter_map(|id| cx.node(*id)) {
            if self.subtree {
                n.walk(&mut involved);
            } else {
                involved.push(n);
            }
        }
        cx.material && involved.into_iter().map(handled).fold(0.0, f64::max) >= MANY_ROWS
    }
}

/// The share of the plan's work an insight states, on every surface that shows one (the plan viewer, the CLI, MCP, a
/// finding's metric, the readings): its `impact` where that is a share at all (not a rule's own weight,
/// [`PlanInsight::fixed_weight`]), the plan states shares ([`Plan::states_shares`]: it weighs its steps, does real work
/// and costs every step doing work), and none of the insight's steps has a cost that rests on a guess
/// ([`Plan::guess`]). The same rule as [`Plan::step_share`], and the one the insight's own text follows.
pub fn stated_share(plan: &Plan, insight: &PlanInsight) -> Option<f64> {
    let guessed = plan.guesses();
    let rests_on_guess = insight.nodes.iter().any(|id| guessed.iter().any(|(g, _)| g == id));
    (!insight.fixed_weight && plan.states_shares() && !rests_on_guess).then_some(insight.impact)
}

/// What a statement's runs measured against a plan that does no real work ([`does_real_work`]): whether they run as
/// cheaply as it is costed, or why not. The one reading behind [`runs_cheaply`] and every sentence that says why no
/// step of such a plan is given a share of its cost: a misestimate isn't "cheap", and nor is a scan of a whole table
/// big enough to matter ([`whole_read_matters`]; all 2 rows of `pg_database` are no reason).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum CheapPlanRuns {
    /// The plan does real work, or weighs no steps: nothing to say here.
    NotCheap,
    /// Its runs are as cheap as its plan: the time is the call itself.
    Cheap,
    /// Without the statement's reads and rows, nothing says how its runs compare.
    Unmeasured,
    /// Its runs return or change far more rows than the plan expected (`expected` a run, as `rows` a run).
    MoreRows { expected: f64, rows: f64 },
    /// Its runs read many pages (rows examined, MySQL). This alone proves no row misestimate: large values and index
    /// maintenance can read many pages for a correctly estimated row.
    MoreReads { reads: f64 },
    /// A step reads a whole table that can weigh on a run on every run (the costliest such scan's id,
    /// [`whole_read_matters`]): cheap only while the table is small.
    ScansTable(u32),
    /// Its runs take `MATERIAL_MS` or longer on average (`ms`), which the plan's cost doesn't account for.
    Slow(f64),
}

/// How a statement's runs compare with a plan that does no real work ([`CheapPlanRuns`]). Without the statement, only
/// what the plan itself shows: a whole table read.
pub fn cheap_plan_runs(plan: &Plan, q: Option<&crate::model::QueryStat>) -> CheapPlanRuns {
    if !plan.weighed() || does_real_work(plan) {
        return CheapPlanRuns::NotCheap;
    }
    if estimated_work_threshold(plan).is_none() {
        return CheapPlanRuns::Unmeasured;
    }
    // A whole read of a small table or of the catalog is no reason the runs cost what they do: the runs are then read as
    // if it weren't there (cheap, or slow).
    let scan = plan
        .nodes()
        .into_iter()
        .filter(|n| whole_read_matters(plan, n))
        .max_by(|a, b| a.self_cost.total_cmp(&b.self_cost))
        .map(|n| n.id);
    // What the plan expects a run to return or change: PostgreSQL estimates that a modification returns nothing, so
    // what it feeds the modification.
    let root = &plan.root;
    let fed = root.children.first().filter(|_| root.kind == NodeKind::Modify).map_or(0.0, |c| c.est_rows);
    let expected = root.est_rows.max(fed).max(1.0);
    let variant = q.and_then(|q| q.captured_plan_variant());
    let ambiguous = variant.is_none() && q.is_some_and(|q| q.workload_plans().len() > 1);
    let reads = match variant {
        Some(p) => p.logical_reads.filter(|_| p.executions > 0.0).map(|r| r / p.executions),
        None if ambiguous => None,
        None => q.and_then(|q| q.reads_per_execution()),
    };
    // Query Store keeps no rows per variant in this model. Never compare a plan with rows measured over other plans.
    let rows = q.filter(|q| q.workload_plans().len() <= 1).and_then(|q| q.rows_handled_per_execution());
    let mean = variant.map(|p| p.mean_time_ms).or_else(|| q.filter(|_| !ambiguous).map(|q| q.mean_time_ms));
    if let Some(rows) =
        rows.filter(|rows| *rows >= expected * MISESTIMATE_FACTOR && *rows >= crate::plan::MISESTIMATE_MIN_ROWS)
    {
        return CheapPlanRuns::MoreRows { expected, rows };
    }
    if let Some(reads) = reads.filter(|reads| *reads > LOOKUP_PAGES_PER_ROW * rows.unwrap_or(expected).max(1.0)) {
        return CheapPlanRuns::MoreReads { reads };
    }
    if let Some(id) = scan {
        return CheapPlanRuns::ScansTable(id);
    }
    if let Some(ms) = mean.filter(|ms| *ms >= MATERIAL_MS) {
        return CheapPlanRuns::Slow(ms);
    }
    if q.is_some() && (reads.is_none() || rows.is_none()) { CheapPlanRuns::Unmeasured } else { CheapPlanRuns::Cheap }
}

/// Whether the plan does real work: a measured run of at least `MATERIAL_MS`, or, unmeasured, an estimated cost of at
/// least what reading about `MANY_ROWS` rows costs (`estimated_work_threshold`). A point lookup's plan doesn't: what a run of it
/// costs is the call itself, not any of its steps. Every insight's severity and every surface that points at a plan's
/// step as where its work is ask this.
pub fn does_real_work(plan: &Plan) -> bool {
    // A measured run says what the plan costs; without one, the optimizer's estimate does.
    match elapsed_ms(plan) {
        Some(ms) => ms >= MATERIAL_MS,
        None => estimated_work_threshold(plan)
            .is_some_and(|threshold| plan.total_cost.max(plan.root.subtree_cost) >= threshold),
    }
}

/// Pages a run may read for each row it returns or changes and still be finding its rows through indexes: a descent of
/// a B-tree is three or four pages, and a run makes one or two per row it reads (a seek, then a lookup) or, for a
/// write, one per index it maintains and per foreign key it checks. MySQL counts rows examined instead, about one per
/// row for a lookup, so the bar is generous there.
const LOOKUP_PAGES_PER_ROW: f64 = 32.0;

/// Whether a statement's runs are cheap, as measured: its plan does no real work ([`does_real_work`]), its runs take
/// under `MATERIAL_MS`, read no more than a lookup's pages for each row they return or change
/// (`LOOKUP_PAGES_PER_ROW`), return or change not far more rows than the plan expected (`MISESTIMATE_FACTOR`), and no
/// step reads a whole table. A cheap estimate alone isn't enough: the runs may handle more rows, read many pages of
/// large values, or wait, and a scan is cheap only while its table is small. Without the statement's reads and rows,
/// nothing says its runs are cheap.
///
/// Any whole-table read keeps the runs from being called cheap here, a small table's too: what it reads grows with the
/// table, so heavy-query weighs the scan as the step to fix. Only the note on a plan's shares
/// ([`cheap_plan_runs`]) leaves out one that can't matter yet ([`whole_read_matters`]).
pub fn runs_cheaply(plan: &Plan, q: &crate::model::QueryStat) -> bool {
    cheap_plan_runs(plan, Some(q)) == CheapPlanRuns::Cheap && !plan.nodes().iter().any(|n| n.reads_table_whole())
}

/// Whether the plan's own rule `rule` (`key-lookups`, `big-sort`…) speaks about `node`: what a check asks before
/// advising the same fix for that step, so the advice and the plan's insight never disagree.
pub fn has_insight(plan: &Plan, rule: &str, node: &PlanNode) -> bool {
    insights(plan).iter().any(|i| i.rule == rule && i.nodes.contains(&node.id))
}

/// Whether every spill the plan reports is too small to matter: each wrote less than `SPILL_MATERIAL_BYTES`, where the
/// `spill` insight stops at Low. False when a spill's size isn't known.
pub fn spills_are_minor(plan: &Plan) -> bool {
    let spills: Vec<&PlanNode> =
        plan.nodes().into_iter().filter(|n| n.warnings.contains(&PlanWarning::Spill)).collect();
    !spills.is_empty()
        && spills.iter().all(|n| parallelism::spilled_bytes(n).is_some_and(|bytes| bytes < SPILL_MATERIAL_BYTES))
}

/// The step where the plan's work is: its costliest, in a plan that does real work ([`does_real_work`]). None for a
/// cheap plan: half of a point lookup's cost isn't where a statement's time goes, and naming it as such sends the
/// reader after a step there's nothing to gain from.
pub fn working_step(plan: &Plan) -> Option<&PlanNode> {
    plan.hottest().filter(|_| does_real_work(plan))
}

/// The statement's measured run time, when the plan has one.
fn elapsed_ms(plan: &Plan) -> Option<f64> {
    plan.facts()
        .elapsed_ms
        .or_else(|| plan.root.act_time_ms.filter(|_| plan.kind == PlanKind::Actual))
        .filter(|ms| ms.is_finite() && *ms >= 0.0 && (*ms > 0.0 || crate::actual::self_times(plan).is_some()))
}

/// The estimated cost of reading about `MANY_ROWS` rows, in each engine's own units. None when that scale is unknown:
/// an uncalibrated estimate establishes neither expensive work nor a cheap plan.
pub fn estimated_work_threshold(plan: &Plan) -> Option<f64> {
    Some(match plan.engine {
        // SQLite's plans carry no cost, and DocDB costs have no calibrated materiality threshold here.
        Engine::Sqlite | Engine::Yugabyte => return None,
        // A 10K-row clustered index scan costs about 0.1.
        Engine::Mssql => 0.1,
        // seq_page_cost is 1: about 100 pages plus the per-row CPU of 10K rows.
        Engine::Postgres => 250.0,
        // MariaDB before 11 reports no costs; Culprit counts rows examined instead.
        Engine::Mysql if is_mariadb(plan) && plan.facts().cost_basis.is_some() => MANY_ROWS,
        // MariaDB 11's costs are close to milliseconds: about 0.1 per thousand rows scanned.
        Engine::Mysql if is_mariadb(plan) => 1.0,
        // MySQL costs about 0.1 per row evaluated.
        Engine::Mysql => 1_000.0,
    })
}

/// MariaDB's own plan reader sets the flavor; plans of logged statements carry the server's version instead.
fn is_mariadb(plan: &Plan) -> bool {
    plan.facts().flavor.as_ref().is_some_and(|f| f.eq_ignore_ascii_case("MariaDB"))
        || plan.facts().server_version.as_ref().is_some_and(|v| v.contains("MariaDB"))
}

/// Rows an operator handles: what it reads, returns, or takes from its inputs.
fn handled(n: &PlanNode) -> f64 {
    n.children.iter().map(rows_out).fold(rows_read(n).max(rows_out(n)), f64::max)
}

// ── helpers ──────────────────────────────────────────────────────────────────────────────────────

/// Rows an operator hands to its parent across all its executions.
fn rows_out(n: &PlanNode) -> f64 {
    n.act_rows.unwrap_or_else(|| n.est_rows_total())
}

/// Rows an operator read (see `PlanNode::rows_read_total`), else what it returned.
fn rows_read(n: &PlanNode) -> f64 {
    n.rows_read_total().unwrap_or_else(|| rows_out(n))
}

/// `shop.orders` → "shop.orders"; falls back to the operator name.
fn what(n: &PlanNode) -> String {
    actionable_object(n).unwrap_or_else(|| n.op.clone())
}

fn actionable_object(n: &PlanNode) -> Option<String> {
    if n.facts().table_kind.as_deref() == Some("timescale-chunk")
        && let Some(parent) = &n.facts().partition_parent
    {
        return Some(parent.clone());
    }
    n.object.clone()
}

/// The table (or the operator) a subtree reads first, for naming the outer and inner side of a join.
fn first_object(n: &PlanNode) -> Option<String> {
    if let Some(o) = &n.object {
        return Some(o.clone());
    }
    n.children.iter().find_map(first_object)
}

fn plural(n: f64, one: &str, many: &str) -> String {
    format!("{} {}", rows_text(n), if (n - 1.0).abs() < 0.05 { one } else { many })
}

/// Row counts read as whole rows once they're past a handful (14, not 14.0); averages below 10 keep a decimal.
fn rows_text(n: f64) -> String {
    if n >= 10.0 { format::count(n.round()) } else { format::count((n * 10.0).round() / 10.0) }
}

/// Everything an operator compares: seek/index condition and filter.
fn conditions(n: &PlanNode) -> String {
    [n.seek_predicate.as_deref(), n.predicate.as_deref()].into_iter().flatten().collect::<Vec<_>>().join(" AND ")
}

/// The last name in `[db].[shop].[orders].[created_at]`, `` `shop`.`orders`.`created_at` `` or `o.created_at`.
fn column_leaf(raw: &str) -> String {
    let leaf = raw.rsplit('.').next().unwrap_or(raw);
    clean_identifier(leaf)
}

/// A name quoted for a script (`Engine::dialect`).
fn quote(engine: Engine, name: &str) -> String {
    engine.dialect().quote(name)
}

/// A dotted table name as the plan writes it, each part cleaned of the plan's quoting and quoted for a script.
fn qualified(engine: Engine, table: &str) -> String {
    let parts: Vec<String> = table.split('.').map(clean_identifier).collect();
    engine.dialect().qualified(&parts.iter().map(String::as_str).collect::<Vec<_>>())
}

fn table_leaf(table: &str) -> String {
    clean_identifier(table.rsplit('.').next().unwrap_or(table))
}

/// The table variable an operator reads (`@t`): SQL Server keeps no statistics on those.
fn table_variable_of(n: &PlanNode) -> Option<String> {
    n.object.as_deref().map(table_leaf).filter(|t| t.starts_with('@'))
}

/// Node prop (SQL Server): a Parallelism exchange wrote its buffers to tempdb. That resolves a deadlock between parallel
/// threads (an order-preserving exchange fed unevenly); it isn't a sort or hash short of memory.
pub const EXCHANGE_SPILL: &str = "ExchangeSpill";

/// A spill smaller than this is at most Low: it costs little next to the work around it.
const SPILL_MATERIAL_BYTES: f64 = 1024.0 * 1024.0;

/// An index the engine builds for itself on every run of a statement because none fits (SQLite's `AUTOMATIC COVERING
/// INDEX ON t(x)`): its own verdict on what's missing, read once here for the plan insight and the index advisor.
#[derive(Clone, Debug, PartialEq)]
pub struct EngineBuiltIndex {
    /// The plan step that builds it.
    pub node: u32,
    /// The table, with any alias the plan used resolved to it.
    pub table: String,
    pub keys: Vec<String>,
}

/// Every index `plan` builds for itself on a table; none on engines that don't. One SQLite builds on a common table
/// expression or a subquery it materialized (`MATERIALIZE recent`, `CO-ROUTINE x`, `(subquery-1)`) is on rows that
/// exist only while the statement runs: no index can be created on them, so it isn't listed.
pub fn engine_built_indexes(plan: &Plan) -> Vec<EngineBuiltIndex> {
    if plan.engine != Engine::Sqlite {
        return Vec::new();
    }
    let nodes = plan.nodes();
    let transient: Vec<String> = nodes
        .iter()
        .filter_map(|n| {
            let detail = n.detail.as_deref()?.trim();
            let upper = detail.to_ascii_uppercase();
            let rest = ["MATERIALIZE ", "CO-ROUTINE "]
                .iter()
                .find_map(|w| upper.starts_with(w).then(|| &detail[w.len()..]))?;
            Some(rest.split_whitespace().next()?.trim_matches(|c| c == '"' || c == '`').to_ascii_lowercase())
        })
        .collect();
    nodes
        .into_iter()
        .filter_map(|n| {
            let facts = n.facts();
            let built = facts.automatic_index.as_ref()?;
            let table = n.object.clone().unwrap_or_else(|| built.table.clone());
            let leaf = table_leaf(&table).to_ascii_lowercase();
            if table.trim_start().starts_with('(') || transient.contains(&leaf) {
                return None;
            }
            let keys = built.columns.clone();
            Some(EngineBuiltIndex { node: n.id, table, keys })
        })
        .collect()
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}
