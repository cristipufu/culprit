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
use crate::model::{ColumnInfo, Engine, IndexStat, Snapshot};
use crate::plan::{INDEX_KEEPS_AT_MOST, MISESTIMATE_FACTOR, NodeKind, Plan, PlanKind, PlanNode, PlanWarning};

mod index;
mod memory;
mod parallelism;
mod query;
mod server;
mod statistics;
#[cfg(test)]
mod tests;

use index::*;
use memory::*;
use parallelism::*;
use query::*;
use server::*;
use statistics::*;

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
/// what reading about `MANY_ROWS` rows costs in the engine's own units (`cheap_below`). A problem measured in time of
/// its own (compiling, JIT, waiting for memory) is material when that time reaches this.
const MATERIAL_MS: f64 = 10.0;

/// Compute and attach insights (and the coverage note) to a plan.
pub fn annotate(plan: &mut Plan) {
    plan.insights = Some(PlanInsights { items: insights(plan), note: coverage_note(plan) });
}

/// What this plan can't show, in one sentence, so an empty list isn't mistaken for a clean bill of health.
pub fn coverage_note(plan: &Plan) -> Option<String> {
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
        (Engine::Postgres, false) => Some(
            "Without EXPLAIN ANALYZE there are no actual rows, spills or timings to check. PostgreSQL plans never carry index suggestions; Culprit reads filters instead.".into(),
        ),
        (Engine::Postgres, true) => Some(
            "PostgreSQL plans carry no index suggestions or memory grants; index advice comes from the filters.".into(),
        ),
        // EXPLAIN ANALYZE arrived in MySQL 8.0.18: older servers can't add actual rows at all.
        (Engine::Mysql, false)
            if plan.props.get("server_version").is_some_and(|v| !crate::version_at_least(v, [8, 0, 18])) =>
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

/// Every insight for this plan, most important first.
pub fn insights(plan: &Plan) -> Vec<PlanInsight> {
    let cx = Cx::new(plan);
    let rules: &[fn(&Cx) -> Vec<Draft>] = &[
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
            if out.iter().any(|o| o.rule == d.rule && o.nodes.first() == d.nodes.first()) {
                continue;
            }
            out.push(d.finish(&cx));
        }
    }
    out.sort_by(|a, b| b.severity.cmp(&a.severity).then(b.impact.total_cmp(&a.impact)));
    out
}

// ── context ──────────────────────────────────────────────────────────────────────────────────────

struct Cx<'a> {
    plan: &'a Plan,
    nodes: Vec<&'a PlanNode>,
    parent: HashMap<u32, &'a PlanNode>,
    total: f64,
    /// Whether the plan does real work (see `MATERIAL_MS`).
    material: bool,
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
        // A measured run says what the plan costs; without one, the optimizer's estimate does.
        let material = match elapsed_ms(plan) {
            Some(ms) => ms >= MATERIAL_MS,
            None => plan.total_cost.max(plan.root.subtree_cost) >= cheap_below(plan),
        };
        Cx { plan, nodes, parent, total: if total > 0.0 { total } else { 1.0 }, material }
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

    fn prop(&self, key: &str) -> Option<&'a str> {
        self.plan.props.get(key).map(String::as_str)
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

/// The statement's measured run time, when the plan has one.
fn elapsed_ms(plan: &Plan) -> Option<f64> {
    let prop = |key: &str| plan.props.get(key).and_then(|v| v.trim().parse::<f64>().ok());
    prop("ElapsedMs")
        .or_else(|| prop("Execution Time"))
        .or_else(|| plan.root.act_time_ms.filter(|_| plan.kind == PlanKind::Actual))
}

/// The estimated cost of reading about `MANY_ROWS` rows, in each engine's own units: a plan estimated below it is cheap
/// whatever its shape.
fn cheap_below(plan: &Plan) -> f64 {
    match plan.engine {
        // SQLite's plans carry no cost: never "real work" by an estimate.
        Engine::Sqlite => f64::INFINITY,
        // A 10K-row clustered index scan costs about 0.1.
        Engine::Mssql => 0.1,
        // seq_page_cost is 1: about 100 pages plus the per-row CPU of 10K rows.
        Engine::Postgres => 250.0,
        // MariaDB before 11 reports no costs; Culprit counts rows examined instead.
        Engine::Mysql if is_mariadb(plan) && plan.props.contains_key("cost_basis") => MANY_ROWS,
        // MariaDB 11's costs are close to milliseconds: about 0.1 per thousand rows scanned.
        Engine::Mysql if is_mariadb(plan) => 1.0,
        // MySQL costs about 0.1 per row evaluated.
        Engine::Mysql => 1_000.0,
    }
}

/// MariaDB's own plan reader sets the flavor; plans of logged statements carry the server's version instead.
fn is_mariadb(plan: &Plan) -> bool {
    plan.props.get("flavor").is_some_and(|f| f == "MariaDB")
        || plan.props.get("server_version").is_some_and(|v| v.contains("MariaDB"))
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

fn prop_f64(n: &PlanNode, key: &str) -> Option<f64> {
    n.props.get(key).and_then(|v| v.trim().parse::<f64>().ok())
}

fn is_true(v: Option<&String>) -> bool {
    v.is_some_and(|v| matches!(v.to_ascii_lowercase().as_str(), "1" | "true"))
}

/// `shop.orders` → "shop.orders"; falls back to the operator name.
fn what(n: &PlanNode) -> String {
    n.object.clone().unwrap_or_else(|| n.op.clone())
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

/// One name part as written in a plan, without its delimiters: `[a]]b]` → `a]b`, `` `a``b` `` → ``a`b``, `"a""b"` → `a"b`.
fn clean_identifier(raw: &str) -> String {
    let raw = raw.trim();
    let inner = |open: char, close: char| {
        raw.strip_prefix(open)
            .and_then(|r| r.strip_suffix(close))
            .map(|r| r.replace(&format!("{close}{close}"), &close.to_string()))
    };
    inner('[', ']')
        .or_else(|| inner('`', '`'))
        .or_else(|| inner('"', '"'))
        .unwrap_or_else(|| raw.trim_matches(|c| c == '[' || c == ']' || c == '`' || c == '"').to_string())
}

/// The last name in `[db].[shop].[orders].[created_at]`, `` `shop`.`orders`.`created_at` `` or `o.created_at`.
fn column_leaf(raw: &str) -> String {
    let leaf = raw.rsplit('.').next().unwrap_or(raw);
    clean_identifier(leaf)
}

const NOT_COLUMNS: &[&str] = &[
    "and",
    "or",
    "not",
    "is",
    "null",
    "true",
    "false",
    "like",
    "in",
    "any",
    "all",
    "between",
    "as",
    "text",
    "date",
    "integer",
    "bigint",
    "numeric",
    "timestamp",
    "timestamptz",
    "varchar",
    "nvarchar",
    "char",
    "bpchar",
    "interval",
    "subplan",
    "hashed",
    "exists",
    "case",
    "when",
    "then",
    "else",
    "end",
    "col1",
    "double",
    "decimal",
    "int",
    "cast",
    "convert_implicit",
    "convert",
    "scalar_operator",
    "getdate",
    "now",
    "current_date",
    "current_timestamp",
    "day",
    "month",
    "year",
    "hour",
    "minute",
    "second",
    "week",
    "quarter",
    "escape",
    "collate",
    // Type names a CONVERT or CAST writes beside its operand.
    "datetime",
    "datetime2",
    "datetimeoffset",
    "smalldatetime",
    "time",
    "money",
    "smallmoney",
    "float",
    "real",
    "bit",
    "smallint",
    "tinyint",
    "uniqueidentifier",
    "binary",
    "varbinary",
    "nchar",
    "ntext",
    "xml",
    "signed",
    "unsigned",
    "json",
    "jsonb",
    "boolean",
    "bool",
    "uuid",
    "bytea",
    "character",
    "varying",
    "precision",
];

/// Identifiers in a condition, in order: bracketed (SQL Server), backticked (MySQL) or bare/qualified (Postgres, MySQL
/// trees). Quoted literals, parameters, numbers, function names and type names are skipped.
fn identifiers(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut out: Vec<String> = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '\'' {
            i += 1;
            while i < chars.len() {
                if chars[i] == '\'' {
                    if chars.get(i + 1) == Some(&'\'') {
                        i += 2;
                        continue;
                    }
                    break;
                }
                i += 1;
            }
            i += 1;
            continue;
        }
        if c == '[' || c == '`' || c == '"' || c.is_ascii_alphabetic() || c == '_' {
            // A dotted name: [a].[b].[c], `a`.`b`, a.b.c, "a"."b"
            let start = i;
            loop {
                match chars.get(i) {
                    Some('[') => {
                        while i < chars.len() && chars[i] != ']' {
                            i += 1;
                        }
                        i += 1;
                    }
                    Some('`') | Some('"') => {
                        let q = chars[i];
                        i += 1;
                        while i < chars.len() && chars[i] != q {
                            i += 1;
                        }
                        i += 1;
                    }
                    Some(ch) if ch.is_ascii_alphanumeric() || *ch == '_' || *ch == '$' || *ch == '#' => {
                        while i < chars.len()
                            && (chars[i].is_ascii_alphanumeric() || matches!(chars[i], '_' | '$' | '#'))
                        {
                            i += 1;
                        }
                    }
                    _ => break,
                }
                if chars.get(i) == Some(&'.') && chars.get(i + 1).is_some_and(|n| *n != ' ') {
                    i += 1;
                    continue;
                }
                break;
            }
            let token: String = chars[start..i.min(chars.len())].iter().collect();
            let leaf = column_leaf(&token);
            let previous = chars[..start].iter().rev().find(|c| !c.is_whitespace()).copied();
            let next = chars[i.min(chars.len())..].iter().find(|c| !c.is_whitespace()).copied();
            let is_type = previous == Some(':');
            let is_function = next == Some('(') && !token.starts_with('[') && !token.starts_with('`');
            let lower = leaf.to_ascii_lowercase();
            if !leaf.is_empty()
                && !leaf.starts_with('@')
                && !leaf.starts_with('$')
                && !is_type
                && !is_function
                && !NOT_COLUMNS.contains(&lower.as_str())
                && !leaf.chars().all(|c| c.is_ascii_digit())
                && !out.iter().any(|o| o.eq_ignore_ascii_case(&leaf))
            {
                out.push(leaf);
            }
            continue;
        }
        i += 1;
    }
    out
}

/// The columns of a scan's filter an index could seek on, equality columns first, then ranges: only conditions that
/// compare one plain column with a value. A condition no B-tree index can serve gives no key, which `filter_columns`
/// would take its first name from: an OR across columns (`status = 'x' OR region = 'eu'`), an inequality (`<>`,
/// `NOT LIKE`, `NOT IN`), two columns of the row compared (`shipped_at > created_at`), arithmetic or a cast on the
/// column (`(price * 1.1) > 100`, `(created_at)::date = $1`), and SQL Server's bitmap probe of a hash join
/// (`PROBE([Opt_Bitmap1005], …)`), which names no column at all.
fn seekable_columns(text: &str) -> Vec<String> {
    let mut equality: Vec<String> = Vec::new();
    let mut range: Vec<String> = Vec::new();
    for part in top_level_and(text) {
        let lower = part.to_ascii_lowercase();
        if lower.contains("probe(") {
            continue;
        }
        let ids = identifiers(part);
        let [column] = ids.as_slice() else { continue };
        if lower.contains(" or ") {
            // `status = 'a' OR status = 'b'` is an IN list on one column.
            if !lower.contains("<>") && !lower.contains("!=") {
                push_unique(&mut equality, column);
            }
            continue;
        }
        if ["<>", "!=", " not like ", " not in ", " not between ", "is not null", "<> all"]
            .iter()
            .any(|n| lower.contains(n))
        {
            continue;
        }
        // The column's side of the comparison holds nothing but the column (a harmless cast to text aside).
        let Some((at, op)) = ["<=", ">=", "=", "<", ">", " like ", " in (", " between ", " is null", " = any"]
            .iter()
            .filter_map(|op| lower.find(op).map(|at| (at, *op)))
            .min_by_key(|(at, _)| *at)
        else {
            continue;
        };
        let (left, right) = (&lower[..at], &lower[at + op.len()..]);
        let side = if identifiers(left).is_empty() { right } else { left };
        let arithmetic = side.split([' ', '(', ')']).any(|t| matches!(t, "%" | "+" | "-" | "*" | "/" | "||"));
        let cast = side.split("::").skip(1).any(|ty| {
            let ty: String = ty.chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == ' ').collect();
            !matches!(ty.trim(), "text" | "varchar" | "character varying" | "name" | "unknown")
        });
        if arithmetic || cast {
            continue;
        }
        let is_eq = (op == "=" || op == " in (" || op == " is null" || op == " = any")
            && !lower.contains(">=")
            && !lower.contains("<=");
        push_unique(if is_eq { &mut equality } else { &mut range }, column);
    }
    for c in range {
        push_unique(&mut equality, &c);
    }
    equality.truncate(4);
    equality
}

fn push_unique(list: &mut Vec<String>, column: &str) {
    if !list.iter().any(|c| c.eq_ignore_ascii_case(column)) {
        list.push(column.to_string());
    }
}

/// The parts of a condition ANDed at its top level, outer parentheses removed: `((a = 1) AND (b > 2 OR c < 3))` →
/// `a = 1`, `b > 2 OR c < 3`.
fn top_level_and(text: &str) -> Vec<&str> {
    fn strip(mut t: &str) -> &str {
        loop {
            let s = t.trim();
            if s.starts_with('(') && s.ends_with(')') && balanced(&s[1..]).len() == s.len() - 2 {
                t = &s[1..s.len() - 1];
            } else {
                return s;
            }
        }
    }
    let text = strip(text);
    let lower = text.to_ascii_lowercase();
    let (mut parts, mut depth, mut start, mut quote) = (Vec::new(), 0i32, 0usize, false);
    let bytes = lower.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\'' => quote = !quote,
            b'(' if !quote => depth += 1,
            b')' if !quote => depth -= 1,
            // BETWEEN's own AND belongs to it.
            b' ' if !quote
                && depth == 0
                && lower[i..].starts_with(" and ")
                && (!lower[start..i].contains(" between ") || lower[start..i].matches(" and ").count() > 0) =>
            {
                parts.push(strip(&text[start..i]));
                start = i + 5;
                i += 5;
                continue;
            }
            _ => {}
        }
        i += 1;
    }
    parts.push(strip(&text[start..]));
    parts.into_iter().filter(|p| !p.is_empty()).collect()
}

/// Columns compared in a condition, suitable as index keys: equality columns first, then ranges.
fn filter_columns(text: &str) -> Vec<String> {
    let lower = text.to_ascii_lowercase();
    let mut equality = Vec::new();
    let mut other = Vec::new();
    for part in lower.split(" and ") {
        let ids = identifiers(part);
        let Some(first) = ids.first() else { continue };
        let original = identifiers(text).into_iter().find(|i| i.eq_ignore_ascii_case(first)).unwrap_or(first.clone());
        let is_eq = part.contains('=') && !part.contains(">=") && !part.contains("<=") && !part.contains("<>")
            || part.contains(" in (");
        let list = if is_eq { &mut equality } else { &mut other };
        if !list.iter().any(|c: &String| c.eq_ignore_ascii_case(&original)) {
            list.push(original);
        }
    }
    let mut cols = equality;
    for c in other {
        if !cols.iter().any(|x| x.eq_ignore_ascii_case(&c)) {
            cols.push(c);
        }
    }
    cols.truncate(4);
    cols
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

// ── rules: indexes and access ────────────────────────────────────────────────────────────────────

/// MySQL's fixed guesses for the share of rows a condition keeps when it has neither index statistics nor a histogram
/// for the column: 10% for an equality, 11.11% for a range or LIKE, 33.33% for an inequality, multiplied together for
/// several conditions. A `filtered` that is exactly such a product (to the two decimals MySQL prints) says nothing about
/// the data, so it is never read as the share a filter keeps; a histogram that happens to give one of these values is
/// taken as a guess too. `conditions`: how many conditions are ANDed in the filter.
pub fn mysql_guessed_filtered(filtered: f64, conditions: usize) -> bool {
    const GUESSES: [f64; 3] = [0.1, 1.0 / 9.0, 1.0 / 3.0];
    let mut products = vec![1.0];
    let mut all = Vec::new();
    for _ in 0..conditions.clamp(1, 3) {
        products = products.iter().flat_map(|p| GUESSES.iter().map(move |g| p * g)).collect();
        all.extend(products.iter().copied());
    }
    all.into_iter().any(|g| (g * 100.0 - filtered).abs() < 0.006)
}

/// How many conditions a filter ANDs together, for [`mysql_guessed_filtered`].
pub fn condition_count(filter: &str) -> usize {
    filter.to_ascii_lowercase().matches(" and ").count() + 1
}

/// The share of the rows a scan reads that its filter keeps, from the plan's own numbers: measured counts when the
/// plan has them (MySQL's EXPLAIN ANALYZE keeps the filter in a Filter node above the scan), else the optimizer's
/// estimate. None when the plan can't say: no rows-read figure (a PostgreSQL estimate without the table's size), or
/// one of MySQL's fixed guesses ([`mysql_guessed_filtered`]).
fn filter_keeps(cx: &Cx, n: &PlanNode) -> Option<f64> {
    if let (Some(read), Some(filter)) = (n.act_rows, cx.parent(n).filter(|p| p.op == "Filter"))
        && n.rows_read.is_none()
    {
        let kept = filter.act_rows?;
        return (read > 0.0).then(|| (kept / read).min(1.0));
    }
    if cx.engine() == Engine::Mysql
        && n.act_rows.is_none()
        && let Some(filtered) = prop_f64(n, "filtered")
    {
        // Per scan: rows examined, and the share the condition is expected to keep.
        let conditions = n.predicate.as_deref().map_or(1, condition_count);
        return (!mysql_guessed_filtered(filtered, conditions)).then_some((filtered / 100.0).clamp(0.0, 1.0));
    }
    n.kept_share()
}

/// Whether an index could pay off for a scan, judged from the plan's own numbers. Every rule that advises an index, or
/// a rewrite that lets one be used, from a scan passes this: a scan (a seek already uses an index) of rowstore data (a
/// columnstore index is read by segments and batches, not seeks), reading at least `MANY_ROWS` rows, whose filter keeps
/// at most `INDEX_KEEPS_AT_MOST` of them. When the plan can't tell how much the filter keeps, none of them speaks.
///
/// A SQL Server scan whose filter probes a hash join's bitmap (`PROBE([Opt_Bitmap1005], …)`) keeps what the join lets
/// through: the share it keeps is the join's doing, not a filter an index could serve.
fn index_could_pay(cx: &Cx, n: &PlanNode) -> bool {
    n.kind == NodeKind::Scan
        && !n.props.get("Storage").is_some_and(|s| s.eq_ignore_ascii_case("ColumnStore"))
        && rows_read(n) >= MANY_ROWS
        && !conditions(n).to_ascii_lowercase().contains("probe(")
        && filter_keeps(cx, n).is_some_and(|k| k <= INDEX_KEEPS_AT_MOST)
}

/// What keeps a scan's filter from an index, when the filter itself does: one insight per scan, from the first that
/// applies (a converted column, a leading wildcard, then a function or cast around a column).
#[derive(PartialEq)]
enum Blocker {
    Conversion,
    Wildcard(usize),
    Function(String, String),
}

fn blocker(cx: &Cx, n: &PlanNode, filter: &str) -> Option<Blocker> {
    if !current_conversions(cx.plan, &conditions(n)).is_empty() {
        return Some(Blocker::Conversion);
    }
    if let Some(at) = wildcard_at(filter) {
        return Some(Blocker::Wildcard(at));
    }
    wrapped_column(filter, cx.engine()).map(|(f, c)| Blocker::Function(f, c))
}

/// MySQL's possible keys for a scan that could serve its filter. MySQL lists keys for the WHERE clause and for joins
/// alike, and names them without their columns: with one table in the query every key comes from the filter. In a
/// join, a key counts when it leads with a filtered column, by the catalog ([`mark_possible_keys`]), so an index on the
/// join column isn't taken for one on the filter; on a plan the catalog didn't mark, when its name carries a filtered
/// column (`ix_orders_promo_code` for `promo_code`).
fn possible_keys_for_filter(cx: &Cx, n: &PlanNode) -> Vec<String> {
    let Some(keys) = n.props.get("possible_keys") else { return Vec::new() };
    let keys: Vec<String> = keys.split(", ").map(str::trim).filter(|k| !k.is_empty()).map(str::to_string).collect();
    let mut up = cx.parent(n);
    let mut joined = false;
    while let Some(p) = up {
        joined |= p.kind == NodeKind::Join;
        up = cx.parent(p);
    }
    if !joined {
        return keys;
    }
    let columns: Vec<String> =
        n.predicate.as_deref().map(filter_columns).unwrap_or_default().iter().map(|c| c.to_ascii_lowercase()).collect();
    if let Some(described) = n.props.get(POSSIBLE_KEY_COLUMNS) {
        let leading: HashMap<&str, String> = described
            .lines()
            .filter_map(|line| line.split_once('='))
            .map(|(key, cols)| (key, cols.split(',').next().unwrap_or("").to_ascii_lowercase()))
            .collect();
        return keys
            .into_iter()
            .filter(|k| leading.get(k.as_str()).is_some_and(|lead| columns.iter().any(|c| c == lead)))
            .collect();
    }
    keys.into_iter()
        .filter(|k| {
            let name = k.to_ascii_lowercase();
            columns.iter().any(|c| {
                name.match_indices(c.as_str()).any(|(at, _)| {
                    let before = name[..at].chars().last();
                    let after = name[at + c.len()..].chars().next();
                    before.is_none_or(|b| !b.is_ascii_alphanumeric())
                        && after.is_none_or(|a| !a.is_ascii_alphanumeric())
                })
            })
        })
        .collect()
}

/// Each CONVERT_IMPLICIT operand in a predicate as written: `[db].[s].[t].[c]`, `[@1]`, `N'x'`, `[Expr1004]`.
fn convert_operands(text: &str) -> Vec<&str> {
    convert_calls(text).into_iter().map(|(_, operand)| operand).collect()
}

/// Each CONVERT_IMPLICIT as (the type it converts to, its operand): `nvarchar(64)`, `[db].[s].[t].[c]`.
fn convert_calls(text: &str) -> Vec<(&str, &str)> {
    const CALL: &str = "CONVERT_IMPLICIT(";
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(i) = rest.find(CALL) {
        let after = &rest[i + CALL.len()..];
        // The target type comes first and may have its own parentheses: nvarchar(200).
        let mut depth = 0i32;
        let start = after.char_indices().find_map(|(j, ch)| {
            match ch {
                '(' => depth += 1,
                ')' => depth -= 1,
                ',' if depth == 0 => return Some(j + 1),
                _ => {}
            }
            None
        });
        if let Some(start) = start {
            let operand = &after[start..];
            let (mut depth, mut bracket, mut quote) = (0i32, false, false);
            let mut end = operand.len();
            for (j, ch) in operand.char_indices() {
                match ch {
                    '[' if !quote => bracket = true,
                    ']' if !quote => bracket = false,
                    '\'' if !bracket => quote = !quote,
                    '(' if !bracket && !quote => depth += 1,
                    ')' if !bracket && !quote => {
                        if depth == 0 {
                            end = j;
                            break;
                        }
                        depth -= 1;
                    }
                    ',' if depth == 0 && !bracket && !quote => {
                        end = j;
                        break;
                    }
                    _ => {}
                }
            }
            out.push((after[..start - 1].trim(), operand[..end].trim()));
        }
        rest = after;
    }
    out
}

/// Plan prop: columns this plan converts that the catalog says already have the type it converts them to, one
/// `table.column type` a line. The plan was captured before they changed: Query Store keeps a plan's first XML when a
/// recompile after the schema change keeps the plan's shape, so its text can describe types the table no longer has.
pub const CHANGED_SINCE_CAPTURE: &str = "Columns changed since capture";

/// The columns `text` implicitly converts, as ("table.column", the type SQL Server converts it to), leaving out those
/// `plan` was captured before they changed (see `CHANGED_SINCE_CAPTURE`): the mismatches the database still has.
/// `CONVERT_IMPLICIT(nvarchar(64),[db].[s].[jobs].[name],0)=[@p1]` → [("jobs.name", "nvarchar(64)")].
pub fn current_conversions(plan: &Plan, text: &str) -> Vec<(String, String)> {
    let changed: Vec<String> = plan
        .props
        .get(CHANGED_SINCE_CAPTURE)
        .map(|v| v.lines().filter_map(|l| l.split_whitespace().next()).map(str::to_ascii_lowercase).collect())
        .unwrap_or_default();
    convert_calls(text)
        .into_iter()
        .filter(|(_, operand)| is_column_operand(operand))
        .filter_map(|(target, operand)| Some((column_label(operand)?, target.to_string())))
        .filter(|(label, _)| !changed.contains(&label.to_ascii_lowercase()))
        .collect()
}

/// Mark the columns `plan` converts that `columns` (the catalog now) says already have the converted-to type (see
/// `CHANGED_SINCE_CAPTURE`). SQL Server never converts a column to its own type, and compares text of any length without
/// converting it, so such a conversion is from before the column changed.
pub fn mark_changed_columns(plan: &mut Plan, columns: &[ColumnInfo]) {
    if plan.engine != Engine::Mssql || columns.is_empty() {
        return;
    }
    let mut texts: Vec<String> = plan.props.get("PlanAffectingConvert").map(|v| vec![v.clone()]).unwrap_or_default();
    texts.extend(plan.nodes().into_iter().map(conditions));
    let mut changed: Vec<String> = Vec::new();
    for text in &texts {
        for (target, operand) in convert_calls(text).into_iter().filter(|(_, operand)| is_column_operand(operand)) {
            let parts: Vec<String> = operand.split('.').map(clean_identifier).collect();
            let [.., schema, table, column] = parts.as_slice() else {
                if let [table, column] = parts.as_slice() {
                    mark_if_same(&mut changed, columns, None, table, column, target);
                }
                continue;
            };
            mark_if_same(&mut changed, columns, Some(schema), table, column, target);
        }
    }
    if !changed.is_empty() {
        plan.props.insert(CHANGED_SINCE_CAPTURE.into(), changed.join("\n"));
    }
}

fn mark_if_same(
    changed: &mut Vec<String>,
    columns: &[ColumnInfo],
    schema: Option<&String>,
    table: &str,
    column: &str,
    converted_to: &str,
) {
    let now = columns.iter().find(|c| {
        c.table.eq_ignore_ascii_case(table)
            && c.name.eq_ignore_ascii_case(column)
            && schema.is_none_or(|s| c.schema.eq_ignore_ascii_case(s))
    });
    let Some(now) = now.filter(|c| same_type(&c.data_type, converted_to)) else { return };
    let line = format!("{table}.{column} {}", now.data_type);
    if !changed.contains(&line) {
        changed.push(line);
    }
}

/// Whether a column of type `now` needs no conversion to `converted_to`: the same type, or text of the same kind.
fn same_type(now: &str, converted_to: &str) -> bool {
    let norm = |t: &str| t.to_ascii_lowercase().replace(' ', "");
    let (now, to) = (norm(now), norm(converted_to));
    let base = |t: &str| t.split('(').next().unwrap_or("").to_string();
    now == to || (matches!(base(&now).as_str(), "varchar" | "nvarchar" | "char" | "nchar") && base(&now) == base(&to))
}

/// `mark_changed_columns` on every plan of the snapshot's workload, against the snapshot's own catalog.
pub fn mark_snapshot(snapshot: &mut Snapshot) {
    let columns = &snapshot.columns;
    if let Some(workload) = snapshot.workload.as_mut() {
        for plan in workload.queries.iter_mut().filter_map(|q| q.plan.as_mut()) {
            mark_changed_columns(plan, columns);
        }
    }
}

/// Where [`mark_possible_keys`] keeps each possible key's columns: `name=col,col` per key, one per line.
const POSSIBLE_KEY_COLUMNS: &str = "possible_keys_columns";

/// MySQL names a scan's possible keys without their columns. Name each with its key columns from the catalog, so an
/// index on the filtered column can be told from one on the join column ([`possible_keys_for_filter`]). A key the
/// catalog doesn't show, or shows on two tables of that name, leaves the scan unmarked.
pub fn mark_possible_keys(plan: &mut Plan, indexes: &[IndexStat]) {
    if plan.engine != Engine::Mysql || indexes.is_empty() {
        return;
    }
    fn walk(node: &mut PlanNode, indexes: &[IndexStat]) {
        if let (Some(keys), Some(object)) = (node.props.get("possible_keys"), node.object.as_deref()) {
            let table = table_leaf(object);
            let described: Option<Vec<String>> = keys
                .split(", ")
                .map(str::trim)
                .filter(|k| !k.is_empty())
                .map(|key| {
                    let mut found = indexes.iter().filter(|i| i.table.eq_ignore_ascii_case(&table) && i.name == key);
                    let index = found.next()?;
                    let same = found.all(|other| other.columns == index.columns);
                    same.then(|| format!("{key}={}", index.columns.join(",")))
                })
                .collect();
            if let Some(described) = described.filter(|d| !d.is_empty()) {
                node.props.insert(POSSIBLE_KEY_COLUMNS.into(), described.join("\n"));
            }
        }
        for child in &mut node.children {
            walk(child, indexes);
        }
    }
    walk(&mut plan.root, indexes);
}

/// [`mark_possible_keys`] on every plan in a snapshot, against its catalog.
/// Mark every operator that reads an index with the index's kind from the catalog ([`crate::plan::INDEX_KIND`]):
/// the rules that read a bitmap scan's rows or a lossy bitmap need to know a BRIN index from a B-tree.
pub fn mark_snapshot_index_kinds(snapshot: &mut Snapshot) {
    let kinds: Vec<(String, String, String)> = snapshot
        .indexes
        .iter()
        .map(|i| (i.table.to_ascii_lowercase(), i.name.to_ascii_lowercase(), i.kind.to_ascii_lowercase()))
        .collect();
    let Some(workload) = snapshot.workload.as_mut() else { return };
    for plan in workload.queries.iter_mut().filter_map(|q| q.plan.as_mut()) {
        mark_index_kinds(&mut plan.root, &kinds);
    }
}

fn mark_index_kinds(n: &mut PlanNode, kinds: &[(String, String, String)]) {
    if let Some(index) = n.index.as_deref().map(|i| table_leaf(i).to_ascii_lowercase()) {
        let table = n.object.as_deref().map(|o| table_leaf(o).to_ascii_lowercase());
        // By name, on the operator's table where the plan says it (an index name is unique per schema).
        let found = kinds
            .iter()
            .filter(|(_, name, _)| *name == index)
            .find(|(t, _, _)| table.as_deref().is_none_or(|table| table == t))
            .or_else(|| kinds.iter().find(|(_, name, _)| *name == index));
        if let Some((_, _, kind)) = found.filter(|(_, _, k)| !k.is_empty()) {
            n.props.insert(crate::plan::INDEX_KIND.into(), kind.clone());
        }
    }
    for c in &mut n.children {
        mark_index_kinds(c, kinds);
    }
}

pub fn mark_snapshot_keys(snapshot: &mut Snapshot) {
    let indexes = &snapshot.indexes;
    if let Some(workload) = snapshot.workload.as_mut() {
        for plan in workload.queries.iter_mut().filter_map(|q| q.plan.as_mut()) {
            mark_possible_keys(plan, indexes);
        }
    }
}

/// How to fix a column SQL Server converts to `target` to compare it with a value of that type: make the types match,
/// from either side. Which side is right depends on the schema (a team that keeps text as Unicode changes the column),
/// so both are offered. The one wording for every type mismatch, in plan insights and in findings.
pub fn mismatch_fix(column: Option<&str>, target: Option<&str>) -> String {
    let col = column.unwrap_or("the column");
    let change = match target {
        Some(t) => format!("change {col} to {t}"),
        None => format!("change {col} to the type the application sends"),
    };
    let unicode = target.is_some_and(|t| {
        let t = t.to_ascii_lowercase();
        t.starts_with("nvarchar") || t.starts_with("nchar")
    });
    if unicode {
        format!(
            "Make the types match: send the value as non-Unicode text (SqlDbType.VarChar in .NET, sql.VarChar in Node's mssql, a non-Unicode mapping in your ORM), or, if your schema keeps text as Unicode, {change}. Changing a column rewrites it and the indexes on it."
        )
    } else {
        format!(
            "Make the types match: send the value with {col}'s own type, or {change} if that's what it should hold. Changing a column rewrites it and the indexes on it."
        )
    }
}

/// A CONVERT_IMPLICIT operand that is a column, not a parameter (`@1`), variable, literal or a value the optimizer made
/// up (`[Expr1002]`, `[Union1006]`, `[Bmk1000]`: letters then a number, never qualified by a table).
fn is_column_operand(op: &str) -> bool {
    let last = op.rsplit('.').next().unwrap_or(op).trim_matches(|c| c == '[' || c == ']' || c == '"');
    let digits = last.trim_start_matches(|c: char| c.is_ascii_alphabetic());
    let computed = !op.contains('.')
        && digits.len() >= 3
        && digits.len() < last.len()
        && digits.chars().all(|c| c.is_ascii_digit());
    !last.is_empty()
        && !op.contains('(')
        && !op.starts_with('\'')
        && !op.starts_with("N'")
        && !last.starts_with('@')
        && !last.starts_with(|c: char| c.is_ascii_digit() || c == '-')
        && !computed
}

/// Whether SQL Server text (a predicate, or a conversion warning's expression) implicitly converts a table's column to
/// compare it with a value of another type: the mismatch that costs an index seek or a good estimate. A conversion the
/// query writes itself (`CONVERT(varchar(36), id)` in a select list) is asked for, and converting a parameter, literal
/// or the optimizer's own value costs nothing: neither is a mismatch. The one rule the plan parser, insights and checks
/// share.
pub fn converts_column(text: &str) -> bool {
    convert_operands(text).into_iter().any(is_column_operand)
}

/// `[db].[dbo].[Users].[Email]` → "Users.Email".
fn column_label(op: &str) -> Option<String> {
    let parts: Vec<&str> = op.split('.').map(|p| p.trim_matches(|c| c == '[' || c == ']' || c == '"')).collect();
    match parts.as_slice() {
        [.., table, column] if !column.is_empty() => Some(format!("{table}.{column}")),
        [column] if !column.is_empty() => Some((*column).to_string()),
        _ => None,
    }
}

/// Functions that hide a column from its index when wrapped around it.
const WRAPPERS: &[&str] = &[
    "year",
    "month",
    "day",
    "datepart",
    "datediff",
    "dateadd",
    "date",
    "date_trunc",
    "date_format",
    "date_part",
    "extract",
    "to_char",
    "convert",
    "cast",
    "isnull",
    "ifnull",
    "coalesce",
    "nvl",
    "upper",
    "lower",
    "ltrim",
    "rtrim",
    "trim",
    "substring",
    "substr",
    "left",
    "right",
    "len",
    "length",
    "char_length",
    "abs",
    "replace",
    "format",
    "round",
];

/// `(function, column)` for the first function wrapped around a column in a filter, or a cast on a column. SQL Server
/// seeks through a cast of a date and time column to `date` (a dynamic seek), so that one doesn't count there.
fn wrapped_column(text: &str, engine: Engine) -> Option<(String, String)> {
    let lower = text.to_ascii_lowercase();
    for f in WRAPPERS {
        let mut from = 0;
        while let Some(at) = lower[from..].find(&format!("{f}(")) {
            let start = from + at;
            from = start + f.len() + 1;
            let before = lower[..start].chars().last();
            if before.is_some_and(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.') {
                continue;
            }
            let args_start = start + f.len() + 1;
            let args = balanced(&text[args_start..]);
            let args_lower = args.to_ascii_lowercase();
            if args_lower.contains("convert_implicit") {
                continue;
            }
            let to_date = args_lower.trim_start().starts_with("date,") || args_lower.trim_end().ends_with(" as date");
            if engine == Engine::Mssql && matches!(*f, "convert" | "cast") && to_date {
                continue;
            }
            // The value converted, not the type it is converted to: CONVERT(datetime, @from, 0) converts a parameter,
            // CAST(? AS datetime) a value; neither hides a column.
            let operand = match *f {
                "convert" => args.split_once(',').map_or(args, |(_, rest)| rest),
                "cast" => args_lower.rfind(" as ").map_or(args, |at| &args[..at]),
                _ => args,
            };
            if let Some(col) = identifiers(operand).into_iter().next() {
                return Some((f.to_ascii_uppercase(), col));
            }
        }
    }
    // Postgres casts on a column: `(created_at)::date`, `(o.total)::integer`. Casts to text are harmless (varchar).
    let mut from = 0;
    while let Some(at) = lower[from..].find(")::") {
        let end = from + at;
        from = end + 3;
        let ty: String = lower[end + 3..].chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '_').collect();
        if matches!(ty.as_str(), "text" | "varchar" | "bpchar" | "name" | "character" | "unknown" | "regclass") {
            continue;
        }
        let open = lower[..end].rfind('(')?;
        let inner = &text[open + 1..end];
        if let Some(col) = identifiers(inner).into_iter().next() {
            return Some((format!("::{ty}"), col));
        }
    }
    None
}

/// The text up to the parenthesis that closes the one just opened.
fn balanced(text: &str) -> &str {
    let mut depth = 1;
    for (i, c) in text.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return &text[..i];
                }
            }
            _ => {}
        }
    }
    text
}

/// Where a LIKE with a leading wildcard starts in a condition.
fn wildcard_at(filter: &str) -> Option<usize> {
    let lower = filter.to_ascii_lowercase();
    ["like n'%", "like '%", "~~ '%", "~~* '%", "ilike '%", "like _utf8mb4'%"].iter().find_map(|p| lower.find(p))
}

// ── rules: query shape ───────────────────────────────────────────────────────────────────────────

/// The table variable an operator reads (`@t`): SQL Server keeps no statistics on those.
fn table_variable_of(n: &PlanNode) -> Option<String> {
    n.object.as_deref().map(table_leaf).filter(|t| t.starts_with('@'))
}

// ── rules: estimates and statistics ──────────────────────────────────────────────────────────────

// ── rules: memory ────────────────────────────────────────────────────────────────────────────────

/// Node prop (SQL Server): a Parallelism exchange wrote its buffers to tempdb. That resolves a deadlock between parallel
/// threads (an order-preserving exchange fed unevenly); it isn't a sort or hash short of memory.
pub const EXCHANGE_SPILL: &str = "ExchangeSpill";

/// A spill smaller than this is at most Low: it costs little next to the work around it.
const SPILL_MATERIAL_BYTES: f64 = 1024.0 * 1024.0;

// ── rules: parallelism ───────────────────────────────────────────────────────────────────────────

// ── rules: SQL Server specific shapes ────────────────────────────────────────────────────────────

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
            let on = n.props.get("Automatic Index")?;
            let (named, columns) = on.split_once('(')?;
            let table = n.object.clone().unwrap_or_else(|| named.trim().to_string());
            let leaf = table_leaf(&table).to_ascii_lowercase();
            if table.trim_start().starts_with('(') || transient.contains(&leaf) {
                return None;
            }
            let keys: Vec<String> = columns
                .trim_end_matches(')')
                .split(',')
                .filter_map(|c| {
                    let name: String = c.trim().chars().take_while(|ch| ch.is_alphanumeric() || *ch == '_').collect();
                    (!name.is_empty()).then_some(name)
                })
                .collect();
            Some(EngineBuiltIndex { node: n.id, table, keys })
        })
        .collect()
}

// ── rules: Postgres specific shapes ──────────────────────────────────────────────────────────────

// ── rules: MySQL specific shapes ─────────────────────────────────────────────────────────────────

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}
