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
        let by_impact = if self.impact >= 0.3 {
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

/// A `CREATE INDEX` to copy, in the engine's dialect. A plan doesn't say the server's edition, so SQL Server's online
/// build is suggested, not assumed. Culprit never runs it.
fn index_sql(engine: Engine, table: &str, keys: &[String], include: &[String]) -> Option<String> {
    if keys.is_empty() {
        return None;
    }
    let name = format!("ix_{}_{}", table_leaf(table), keys.join("_")).to_ascii_lowercase();
    let name: String =
        name.chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' { c } else { '_' }).take(60).collect();
    let target = qualified(engine, table);
    Some(engine.dialect().create_index(&crate::dialect::NewIndex {
        target: &target,
        unique: false,
        name: &name,
        keys,
        include,
        filter: None,
        online: crate::dialect::Online::Unknown,
    }))
}

/// Output columns of an operator (`OutputList` on SQL Server), as plain names.
fn output_columns(n: &PlanNode) -> Vec<String> {
    n.props
        .get("OutputList")
        .map(|list| list.split(", ").map(column_leaf).filter(|c| !c.is_empty() && !c.starts_with("Expr")).collect())
        .unwrap_or_default()
}

// ── rules: indexes and access ────────────────────────────────────────────────────────────────────

fn key_lookups(cx: &Cx) -> Vec<Draft> {
    let mut out = Vec::new();
    for n in cx.nodes.iter().filter(|n| n.kind == NodeKind::Lookup && n.op.contains("Lookup")) {
        let runs = n.act_executions.unwrap_or(n.est_executions).max(rows_out(n));
        if runs < MANY_RUNS {
            continue;
        }
        let table = what(n);
        let cols = output_columns(n);
        let seek = cx.parent(n).and_then(|p| p.children.first()).filter(|s| s.id != n.id && s.index.is_some());
        let index = seek.and_then(|s| s.index.clone());
        let impact = cx.share(n) + cx.parent(n).filter(|p| p.kind == NodeKind::Join).map_or(0.0, |p| cx.share(p));
        let mut nodes = vec![n.id];
        nodes.extend(seek.map(|s| s.id));
        let mut d = Draft::new("key-lookups", InsightArea::Index, impact, nodes);
        d.title = format!("Looks up {} in {table} one at a time", plural(runs, "row", "rows"));
        let heap = n.op.starts_with("RID");
        d.detail = match (&index, cols.is_empty()) {
            (Some(ix), false) => format!(
                "{ix} finds the rows but doesn't hold {}, so SQL Server goes back to {table} for each one ({}). Together that's {} of this plan's work.",
                format::list(&cols),
                n.op,
                format::percent(impact)
            ),
            _ => format!(
                "Each row found by the index needs a second read from {table} ({}). Together that's {} of this plan's work.",
                n.op,
                format::percent(impact)
            ),
        };
        // Said in words only: rebuilding the index needs its whole definition (key order, existing included columns,
        // uniqueness, filter, compression), which the plan doesn't carry.
        let include = if cols.is_empty() { "the looked-up columns".to_string() } else { format::list(&cols) };
        d.fix = match (&index, heap) {
            (_, true) => format!(
                "{table} has no clustered index, so every lookup follows a row pointer. Give it a clustered index, or add {include} to {} as included columns.",
                index.as_deref().unwrap_or("the index")
            ),
            (Some(ix), false) => format!(
                "Add {include} to {ix} as included columns (keep its keys and its existing included columns) so the seek answers the query by itself, or select fewer columns."
            ),
            (None, false) => format!("Add {include} to the index as included columns, or select fewer columns."),
        };
        d.verify = format!(
            "Explain again: the {} is gone and {} returns the rows by itself.",
            n.op,
            index.as_deref().unwrap_or("the seek")
        );
        out.push(d);
    }
    out
}

fn missing_index_hint(cx: &Cx) -> Vec<Draft> {
    // A key lookup on the same table with the same keys already proposes this index, with the columns it needs.
    let lookups: Vec<(String, Vec<String>)> = cx
        .nodes
        .iter()
        .filter(|n| n.kind == NodeKind::Lookup)
        .filter_map(|n| {
            let seek = cx.parent(n)?.children.first().filter(|s| s.id != n.id)?;
            let keys = filter_columns(&conditions(seek)).into_iter().map(|k| k.to_ascii_lowercase()).collect();
            Some((table_leaf(&what(n)).to_ascii_lowercase(), keys))
        })
        .collect();
    let mut hints: Vec<_> = cx
        .plan
        .index_hints
        .iter()
        .filter(|h| {
            let keys: Vec<String> =
                h.equality.iter().chain(&h.inequality).map(|c| clean_identifier(c).to_ascii_lowercase()).collect();
            !lookups.iter().any(|(t, k)| *t == table_leaf(&h.table).to_ascii_lowercase() && *k == keys)
        })
        .collect();
    hints.sort_by(|a, b| b.impact_pct.total_cmp(&a.impact_pct));
    hints
        .into_iter()
        .take(3)
        .map(|h| {
            let leaf = table_leaf(&h.table).to_ascii_lowercase();
            let node = cx
                .nodes
                .iter()
                .filter(|n| n.object.as_deref().is_some_and(|o| table_leaf(o).to_ascii_lowercase() == leaf))
                .max_by(|a, b| a.self_cost.total_cmp(&b.self_cost))
                .copied();
            let keys: Vec<String> = h.equality.iter().chain(&h.inequality).map(|c| clean_identifier(c)).collect();
            let include: Vec<String> = h.include.iter().map(|c| clean_identifier(c)).collect();
            let impact = node.map_or(0.0, |n| cx.share(n)).max(h.impact_pct / 100.0 * 0.5);
            let mut d = Draft::new("missing-index-hint", InsightArea::Index, impact, node.map(|n| vec![n.id]).unwrap_or_default());
            d.title = format!(
                "SQL Server estimates an index on {} ({}) would make this {} cheaper",
                h.table,
                format::list(&keys),
                format::percent(h.impact_pct / 100.0)
            );
            d.detail = format!(
                "The optimizer noticed it had to read {} without a fitting index{}. Its estimate is a guide, not a promise: check the columns against the other queries on {}.",
                h.table,
                node.map(|n| format!(" ({} rows read by {})", format::count(rows_read(n)), n.op)).unwrap_or_default(),
                table_leaf(&h.table)
            );
            d.fix = if include.is_empty() {
                format!("Create the index on {}.", format::list(&keys))
            } else {
                format!("Create the index on {} and include {}.", format::list(&keys), format::list(&include))
            };
            d.fix_sql = index_sql(Engine::Mssql, &h.table, &keys, &include);
            d.verify = format!("Explain again: the suggestion is gone and the read of {} becomes a seek.", table_leaf(&h.table));
            d
        })
        .collect()
}

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

/// The filter a scan applies: its own, or MySQL EXPLAIN ANALYZE's Filter node above it.
fn scan_filter_text(cx: &Cx, n: &PlanNode) -> Option<String> {
    n.predicate.clone().or_else(|| cx.parent(n).filter(|p| p.op == "Filter").and_then(|f| f.predicate.clone()))
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
fn index_could_pay(cx: &Cx, n: &PlanNode) -> bool {
    n.kind == NodeKind::Scan
        && !n.props.get("Storage").is_some_and(|s| s.eq_ignore_ascii_case("ColumnStore"))
        && rows_read(n) >= MANY_ROWS
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

fn scan_filter(cx: &Cx) -> Vec<Draft> {
    let hinted: Vec<String> = cx.plan.index_hints.iter().map(|h| table_leaf(&h.table).to_ascii_lowercase()).collect();
    let mut out = Vec::new();
    for n in cx.nodes.iter().filter(|n| index_could_pay(cx, n)) {
        let Some(table) = n.object.clone() else { continue };
        let Some(filter) = scan_filter_text(cx, n) else { continue };
        // SQL Server's own suggestion, a conversion, a function or a leading wildcard explain the scan better, and
        // MySQL's ignored index says why there's no seek: their rules say so.
        if hinted.contains(&table_leaf(&table).to_ascii_lowercase())
            || blocker(cx, n, &filter).is_some()
            || (cx.engine() == Engine::Mysql && !possible_keys_for_filter(cx, n).is_empty())
        {
            continue;
        }
        let read = rows_read(n);
        let kept = read * filter_keeps(cx, n).unwrap_or(1.0);
        let cols = filter_columns(&filter);
        let impact = cx.share(n).max(cx.subtree(n) * 0.5);
        let mut d = Draft::new("scan-filter", InsightArea::Index, impact, vec![n.id]);
        d.title = format!("Reads {} of {table} to keep {}", plural(read, "row", "rows"), rows_text(kept));
        let estimated = n.act_rows.is_none();
        d.detail = format!(
            "{} {} reads the rows and checks `{}` on each one, so most of the reading is thrown away. {} of this plan's work.",
            if estimated { "As planned, the" } else { "The" },
            n.op,
            format::predicate(&filter),
            capitalize(&format::percent(impact))
        );
        d.fix = if cols.is_empty() {
            "An index on the filtered columns lets the database go straight to the matching rows.".into()
        } else {
            format!(
                "An index on {} lets the database go straight to the matching rows; if one already exists, the optimizer didn't use it, so refresh the table's statistics first.",
                format::list(&cols)
            )
        };
        d.fix_sql = index_sql(cx.engine(), &table, &cols, &[]);
        d.verify = format!("Explain again: {} reads about as many rows as it keeps.", table_leaf(&table));
        out.push(d);
    }
    out
}

fn implicit_conversion(cx: &Cx) -> Vec<Draft> {
    if cx.engine() != Engine::Mssql {
        return Vec::new();
    }
    // Only the warnings about a converted column the table still has (see `current_conversions`); older scans flagged
    // every conversion, and a plan captured before a column changed type still shows its old conversion.
    let plan_lines: Vec<String> = cx
        .prop("PlanAffectingConvert")
        .map(|v| v.lines().filter(|l| !current_conversions(cx.plan, l).is_empty()).map(str::to_string).collect())
        .unwrap_or_default();
    let node = cx
        .nodes
        .iter()
        // A converted column is the problem; a converted parameter, variable or literal costs nothing.
        .filter(|n| !current_conversions(cx.plan, &conditions(n)).is_empty())
        .max_by(|a, b| a.self_cost.total_cmp(&b.self_cost))
        .copied();
    if plan_lines.is_empty() && node.is_none() {
        return Vec::new();
    }
    let seek_line = plan_lines.iter().find(|l| l.starts_with("Seek Plan"));
    let text =
        seek_line.cloned().or_else(|| plan_lines.first().cloned()).or_else(|| node.map(conditions)).unwrap_or_default();
    // What the conversion costs is SQL Server's own verdict, never inferred from the plan's shape: a scan may have no
    // index on the column to seek, or be the right plan for a small table. "Seek Plan" means it kept SQL Server from
    // seeking an index; "Cardinality Estimate" that it swayed the row estimates; no warning, that it changed nothing.
    let blocks_seek = seek_line.is_some();
    let converted = current_conversions(cx.plan, &text).into_iter().next();
    let column = converted.as_ref().map(|(c, _)| c.clone()).or_else(|| convert_column(&text));
    let target = converted.map(|(_, t)| t);
    let impact = node.map_or(0.05, |n| cx.share(n));
    let mut d =
        Draft::new("implicit-conversion", InsightArea::Query, impact, node.map(|n| vec![n.id]).unwrap_or_default());
    let col = column.clone().unwrap_or_else(|| "a column".into());
    let to = target.as_deref().map(|t| format!(" to {t}")).unwrap_or_default();
    let expression = format::predicate(&convert_expression(&text));
    if blocks_seek {
        d.floor = Severity::Medium;
        d.title = format!("A type mismatch on {col} stops SQL Server from seeking an index");
        d.detail = format!(
            "The value it's compared with has a different type, so SQL Server converts {col}{to} on every row (`{expression}`) and, by its own warning, can't seek the index."
        );
    } else if !plan_lines.is_empty() {
        d.cap = Severity::Low;
        d.title = format!("A type mismatch on {col} turns row estimates into guesses");
        d.detail = format!(
            "SQL Server converts {col}{to} before comparing it (`{expression}`), which by its own warning only affects how many rows it expects here; an index can still be used."
        );
    } else {
        d.cap = Severity::Low;
        d.title = format!("A type mismatch on {col} adds a conversion to every row");
        d.detail = format!(
            "SQL Server converts {col}{to} on every row it reads to compare it with a value of that type (`{expression}`). It didn't warn that this changes the plan, so the cost is the conversion itself."
        );
    }
    d.fix = mismatch_fix(column.as_deref(), target.as_deref());
    d.verify = if blocks_seek {
        "Explain again: no CONVERT_IMPLICIT in the plan, and the scan became a seek.".into()
    } else {
        format!("Explain again: no CONVERT_IMPLICIT on {col} in the plan.")
    };
    vec![d]
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

/// `CONVERT_IMPLICIT(nvarchar(256),[db].[dbo].[Users].[Email],0)` → "Users.Email".
fn convert_column(text: &str) -> Option<String> {
    let start = text.find("CONVERT_IMPLICIT(")? + "CONVERT_IMPLICIT(".len();
    let rest = &text[start..];
    let after_type = rest.find("),").map(|i| &rest[i + 2..]).or_else(|| rest.split_once(',').map(|(_, r)| r))?;
    let expr = after_type.split(',').next()?;
    let parts: Vec<String> = expr.split('.').map(clean_identifier).collect();
    match parts.as_slice() {
        [.., table, column] if !column.is_empty() && !column.starts_with('@') => Some(format!("{table}.{column}")),
        [column] if !column.is_empty() && !column.starts_with('@') => Some(column.clone()),
        _ => None,
    }
}

fn convert_expression(text: &str) -> String {
    text.split_once(": ").map(|(_, e)| e.to_string()).unwrap_or_else(|| text.to_string())
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
            if let Some(col) = identifiers(args).into_iter().next() {
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

fn non_sargable(cx: &Cx) -> Vec<Draft> {
    let mut out = Vec::new();
    for n in cx.nodes.iter().filter(|n| index_could_pay(cx, n)) {
        let Some(filter) = n.predicate.as_deref() else { continue };
        let Some(Blocker::Function(func, col)) = blocker(cx, n, filter) else { continue };
        let table = what(n);
        let impact = cx.share(n).max(cx.subtree(n) * 0.5);
        let mut d = Draft::new("non-sargable", InsightArea::Query, impact, vec![n.id]);
        let cast = func.starts_with("::") || func == "CAST" || func == "CONVERT";
        d.title = if cast {
            format!("A cast on {col} keeps the database from using an index on {}", table_leaf(&table))
        } else {
            format!("{func}() around {col} keeps the database from using an index on {}", table_leaf(&table))
        };
        d.detail = format!(
            "Each row's {col} is computed before it can be compared (`{}`), so an index on {col} can't narrow the search: {} reads {} rows to keep {}. {} of this plan's work.",
            format::predicate(filter),
            n.op,
            format::count(rows_read(n)),
            format::percent(filter_keeps(cx, n).unwrap_or(1.0)),
            capitalize(&format::percent(impact))
        );
        let lower = func.to_ascii_lowercase();
        d.fix = if matches!(
            lower.as_str(),
            "year"
                | "month"
                | "day"
                | "date"
                | "datepart"
                | "date_trunc"
                | "date_part"
                | "extract"
                | "date_format"
                | "to_char"
        ) || lower == "::date"
        {
            format!(
                "Compare {col} itself with a range instead, e.g. `{col} >= '2026-03-01' AND {col} < '2026-04-01'`; then an index on {col} is used."
            )
        } else if matches!(lower.as_str(), "isnull" | "ifnull" | "coalesce" | "nvl") {
            format!("Write the condition on {col} directly, e.g. `({col} = @value OR {col} IS NULL)`.")
        } else if cast {
            format!("Compare {col} with a value of its own type, so nothing has to be converted per row.")
        } else {
            match cx.engine() {
                Engine::Postgres => format!(
                    "Store {col} in the form you search for, or index the expression itself, e.g. `CREATE INDEX ON {} (({}({col})))`.",
                    table_leaf(&table),
                    lower
                ),
                Engine::Mysql => format!(
                    "Store {col} in the form you search for, or add a functional index, e.g. `CREATE INDEX ... ON {} (({}({col})))` (MySQL 8.0.13+).",
                    table_leaf(&table),
                    lower
                ),
                Engine::Mssql => format!(
                    "Store {col} in the form you search for, or add a computed column with {func}({col}) and index it."
                ),
                Engine::Sqlite => format!(
                    "Store {col} in the form you search for, or index the expression itself, e.g. `CREATE INDEX ... ON {} ({}({col}))`.",
                    table_leaf(&table),
                    lower
                ),
            }
        };
        d.verify = format!("Explain again: {} is read with a seek or index condition on {col}.", table_leaf(&table));
        out.push(d);
    }
    out
}

fn leading_wildcard(cx: &Cx) -> Vec<Draft> {
    let mut out = Vec::new();
    for n in cx.nodes.iter().filter(|n| index_could_pay(cx, n)) {
        let Some(filter) = n.predicate.as_deref() else { continue };
        let Some(Blocker::Wildcard(at)) = blocker(cx, n, filter) else { continue };
        let column = identifiers(&filter[..at]).into_iter().last().unwrap_or_else(|| "the column".into());
        let table = what(n);
        let impact = cx.share(n).max(cx.subtree(n) * 0.5);
        let mut d = Draft::new("leading-wildcard", InsightArea::Query, impact, vec![n.id]);
        d.title = format!("LIKE '%…' on {column} reads all of {}", table_leaf(&table));
        d.detail = format!(
            "A pattern that starts with a wildcard can't use an index, so {} checks every row's {column} ({} rows) to keep {} of them.",
            n.op,
            format::count(rows_read(n)),
            format::percent(filter_keeps(cx, n).unwrap_or(1.0))
        );
        d.fix = match cx.engine() {
            Engine::Postgres => format!(
                "A trigram index answers '%…%' searches: enable pg_trgm and index {column} with gin_trgm_ops. If you only need prefixes, drop the leading %."
            ),
            Engine::Mssql => format!(
                "Use full-text search for word searches, or drop the leading % if a prefix search is enough. For suffix searches, store the reversed value of {column} and index it."
            ),
            Engine::Mysql => {
                "Use a FULLTEXT index with MATCH … AGAINST for word searches, or drop the leading % if a prefix search is enough."
                    .to_string()
            }
            Engine::Sqlite => {
                "Use an FTS5 virtual table with MATCH for word searches, or drop the leading % if a prefix search is enough."
                    .to_string()
            }
        };
        d.fix_sql = (cx.engine() == Engine::Postgres).then(|| {
            format!(
                "CREATE EXTENSION IF NOT EXISTS pg_trgm;\nCREATE INDEX CONCURRENTLY ix_{}_{}_trgm\n    ON {} USING gin ({} gin_trgm_ops);",
                table_leaf(&table).to_ascii_lowercase(),
                column.to_ascii_lowercase(),
                qualified(Engine::Postgres, &table),
                quote(Engine::Postgres, &column)
            )
        });
        d.verify = format!(
            "Explain again: {} is read through an index (a Bitmap Index Scan on PostgreSQL).",
            table_leaf(&table)
        );
        out.push(d);
    }
    out
}

/// Where a LIKE with a leading wildcard starts in a condition.
fn wildcard_at(filter: &str) -> Option<usize> {
    let lower = filter.to_ascii_lowercase();
    ["like n'%", "like '%", "~~ '%", "~~* '%", "ilike '%", "like _utf8mb4'%"].iter().find_map(|p| lower.find(p))
}

/// Joins that combine every row with every row: a NoJoinPredicate warning, or a Postgres/MySQL join with no condition.
fn cross_joins(cx: &Cx) -> Vec<u32> {
    let mut ids: Vec<u32> =
        cx.nodes.iter().filter(|n| n.warnings.contains(&PlanWarning::NoJoinPredicate)).map(|n| n.id).collect();
    if ids.is_empty() && cx.plan.warnings.contains(&PlanWarning::NoJoinPredicate) {
        ids.extend(cx.nodes.iter().filter(|n| n.kind == NodeKind::Join).take(1).map(|n| n.id));
    }
    if cx.engine() == Engine::Postgres {
        for n in cx.nodes.iter().filter(|n| n.op == "Nested Loop" && n.children.len() == 2) {
            let inner = &n.children[1];
            let no_condition = conditions(inner).is_empty()
                && inner.children.iter().all(|c| conditions(c).is_empty())
                && !n.props.contains_key("Join Filter");
            let product = n.children[0].est_rows * inner.est_rows.max(1.0);
            if no_condition && n.est_rows >= 0.5 * product && product >= 100.0 {
                ids.push(n.id);
            }
        }
    }
    if cx.engine() == Engine::Mysql {
        ids.extend(
            cx.nodes
                .iter()
                .filter(|n| {
                    (n.props.contains_key("using_join_buffer") && n.predicate.is_none() && n.seek_predicate.is_none())
                        || n.op.to_ascii_lowercase().contains("(no condition)")
                })
                .map(|n| n.id),
        );
    }
    ids
}

/// A NOT IN that checks the subquery row by row, and the operators that work exists for (what NOT EXISTS removes).
/// NOT IN that is one build and a probe per row isn't one: PostgreSQL's hashed SubPlan, MySQL's materialized subquery
/// and its antijoin (`Not exists`, MySQL 8.0.17+).
fn not_in_work<'a>(cx: &Cx<'a>, n: &'a PlanNode) -> Option<Vec<&'a PlanNode>> {
    let statement = cx.statement();
    // The same plan shapes come from NOT EXISTS under an OR; when the statement is known, it must say NOT IN.
    let says_not_in = statement.is_empty() || statement.contains("not in");
    match cx.engine() {
        // EXPLAIN QUERY PLAN shows no per-row work to weigh.
        Engine::Sqlite => None,
        Engine::Mssql => {
            let spools: Vec<&PlanNode> = n
                .children
                .iter()
                .flat_map(|c| std::iter::once(c).chain(c.children.iter()))
                .filter(|c| c.op == "Row Count Spool")
                .collect();
            (n.detail.as_deref().is_some_and(|d| d.contains("Anti Semi Join")) && !spools.is_empty()).then_some(spools)
        }
        Engine::Postgres => {
            let l = n.predicate.as_deref()?.to_ascii_lowercase();
            let per_row = l.contains("not (") && l.contains("subplan") && !l.contains("hashed subplan");
            let subplans: Vec<&PlanNode> = n
                .children
                .iter()
                .filter(|c| c.props.get("Parent Relationship").is_some_and(|r| r == "SubPlan"))
                .collect();
            (per_row && says_not_in).then_some(subplans)
        }
        Engine::Mysql => {
            let l = n.predicate.as_deref()?.to_ascii_lowercase();
            let negated = l.contains("<not>(<in_optimizer>") || l.contains("not(<in_optimizer>");
            // `<exists>` is the subquery run for each row; `<materialize>` is built once and looked up.
            let per_row = negated && l.contains("<exists>") && !l.contains("<materialize>");
            (per_row && says_not_in && !is_true(n.props.get("not_exists"))).then(|| n.children.iter().collect())
        }
    }
}

fn not_in(cx: &Cx) -> Vec<Draft> {
    let mut out = Vec::new();
    for n in &cx.nodes {
        let Some(work) = not_in_work(cx, n) else { continue };
        // NOT IN's own work: the operator testing each row and the NULL handling or subquery it repeats, not the
        // input it filters, which NOT EXISTS reads all the same.
        let impact = (cx.share(n) + work.iter().map(|w| cx.subtree(w)).sum::<f64>()).min(1.0);
        let mut nodes = vec![n.id];
        nodes.extend(work.iter().map(|w| w.id));
        let mut d = Draft::new("not-in", InsightArea::Query, impact, nodes);
        d.title = match cx.engine() {
            Engine::Mssql => {
                "NOT IN against a column that can be NULL makes SQL Server check for NULLs row by row".into()
            }
            Engine::Postgres => {
                "NOT IN (subquery) can't run as an anti-join, so PostgreSQL tests every row against the subquery".into()
            }
            Engine::Sqlite => {
                "NOT IN against a subquery whose column can be NULL returns no rows once it meets a NULL".into()
            }
            Engine::Mysql => "NOT IN (subquery) makes MySQL run the subquery once per row".into(),
        };
        d.detail = match cx.engine() {
            Engine::Mssql => format!("Because the subquery's column allows NULL, NOT IN must prove no NULL exists before it can return anything, which adds the Row Count Spool under {}.", n.op),
            Engine::Postgres => "If the subquery ever returns NULL, NOT IN returns nothing at all; that rule stops PostgreSQL from using a Hash Anti Join, and it expected the subquery's rows too many to hash in work_mem, so it searches them again for each row.".into(),
            Engine::Sqlite => "If the subquery ever returns NULL, NOT IN returns nothing at all.".into(),
            Engine::Mysql => "NOT IN has to treat a NULL from the subquery specially, and here MySQL could neither turn it into an antijoin nor materialize the subquery once, so it runs the subquery for each row.".into(),
        };
        d.fix = "Rewrite as NOT EXISTS, which means what you almost certainly intend and runs as an anti-join: `WHERE NOT EXISTS (SELECT 1 FROM orders o WHERE o.customer_id = c.id)`. Declaring the column NOT NULL also helps.".into();
        d.verify = match cx.engine() {
            Engine::Mssql => "Explain again: an Anti Semi Join without a Row Count Spool.".into(),
            Engine::Postgres => {
                "Explain again: a Hash Anti Join (or Nested Loop Anti Join) instead of a SubPlan filter.".into()
            }
            Engine::Sqlite => "Explain again: the subquery runs as a correlated NOT EXISTS.".into(),
            Engine::Mysql => "Explain again: an antijoin (Not exists) instead of a dependent subquery.".into(),
        };
        out.push(d);
    }
    out
}

// ── rules: query shape ───────────────────────────────────────────────────────────────────────────

fn scalar_udf(cx: &Cx) -> Vec<Draft> {
    if cx.engine() != Engine::Mssql {
        return Vec::new();
    }
    let blocks_parallel = cx.prop("NonParallelPlanReason") == Some("TSQLUserDefinedFunctionsNotParallelizable");
    let mut out = Vec::new();
    // Time spent in scalar functions, measured apart from the plan's costs (SQL Server 2016 SP2+ actual plans).
    let measured = match (cx.prop("UdfElapsedMs"), elapsed_ms(cx.plan)) {
        (Some(udf), Some(total)) if total > 0.0 => udf.parse::<f64>().ok().map(|u| (u, (u / total).min(1.0))),
        _ => None,
    };
    for n in cx.nodes.iter().filter(|n| n.props.contains_key("UserDefinedFunctions")) {
        let names = n.props["UserDefinedFunctions"].clone();
        let calls = rows_out(n).max(n.children.first().map_or(0.0, rows_out));
        // The plan's costs leave the function's body out, so its own share says little: the measured time does, and
        // without it the number of calls is the evidence.
        let impact = measured.map_or_else(|| cx.share(n), |(_, share)| share);
        let mut d = Draft::new("scalar-udf", InsightArea::Query, impact, vec![n.id]);
        if measured.is_none() && calls >= MANY_ROWS {
            d.floor = Severity::Medium;
        }
        d.title = format!("{names} runs once for every row (about {} calls per run)", format::count(calls));
        d.detail = format!(
            "A scalar function runs like a small separate query for each row{}. {}",
            if blocks_parallel { ", and it keeps the whole plan on one CPU" } else { "" },
            match measured {
                Some((ms, share)) => format!(
                    "This run spent {} in it, {} of the statement's time.",
                    format::duration(ms),
                    format::percent(share)
                ),
                None => "Its cost doesn't show in the plan's numbers, which makes it easy to miss.".into(),
            }
        );
        d.fix = "Inline it: rewrite the function as an inline table-valued function or a plain expression in the query. SQL Server 2019+ inlines scalar functions automatically unless they use things like GETDATE(), table variables or loops.".into();
        d.verify = "Explain again: no UserDefinedFunction in the plan, and no NonParallelPlanReason.".into();
        out.push(d);
    }
    if out.is_empty() && blocks_parallel {
        let mut d = Draft::new("scalar-udf", InsightArea::Query, 0.1, vec![cx.plan.root.id]);
        d.subtree = true;
        d.floor = Severity::Medium;
        d.title = "A scalar function keeps this plan on one CPU and runs once per row".into();
        d.detail = "SQL Server reports TSQLUserDefinedFunctionsNotParallelizable: a T-SQL scalar function in the query can't run in parallel, and it's called for each row.".into();
        d.fix = "Inline the function as an inline table-valued function or an expression; on SQL Server 2019+ remove what prevents automatic inlining (GETDATE(), table variables, loops).".into();
        d.verify = "Explain again: NonParallelPlanReason is gone.".into();
        out.push(d);
    }
    out
}

/// The table variable an operator reads (`@t`): SQL Server keeps no statistics on those.
fn table_variable_of(n: &PlanNode) -> Option<String> {
    n.object.as_deref().map(table_leaf).filter(|t| t.starts_with('@'))
}

fn table_variable(cx: &Cx) -> Vec<Draft> {
    if cx.engine() != Engine::Mssql {
        return Vec::new();
    }
    let mut out = Vec::new();
    for n in cx.nodes.iter().filter(|n| table_variable_of(n).is_some()) {
        let name = table_variable_of(n).unwrap_or_default();
        let actual = n.act_rows;
        if n.est_rows_total() > 1.5 && actual.is_none_or(|a| a < 10.0 * n.est_rows_for_runs()) {
            continue;
        }
        let mut d = Draft::new("table-variable", InsightArea::Query, cx.subtree(n), vec![n.id]);
        match actual {
            Some(a) if a >= 1_000.0 => {
                d.floor = Severity::Medium;
                d.title =
                    format!("SQL Server planned for 1 row in table variable {name}, which held {}", format::count(a));
            }
            _ => {
                d.cap = Severity::Low;
                d.title = format!("SQL Server plans for 1 row in table variable {name}");
            }
        }
        d.detail = "Table variables have no statistics, so everything joined to it is planned as if it held a single row. That's fine for a handful of rows and badly wrong for thousands.".into();
        d.fix = format!(
            "If {name} can hold more than a few hundred rows, use a temporary table (#{}) instead; it gets statistics. Compatibility level 150+ also defers its compilation so the first run's row count is used.",
            name.trim_start_matches('@')
        );
        d.verify = "Run with an actual plan: the estimate for that table matches its actual rows.".into();
        out.push(d);
    }
    out
}

fn nested_loops(cx: &Cx) -> Vec<Draft> {
    let cross = cross_joins(cx);
    let not_in_ids: Vec<u32> = not_in(cx).iter().flat_map(|d| d.nodes.clone()).collect();
    let mut out = Vec::new();
    for n in
        cx.nodes.iter().filter(|n| n.kind == NodeKind::Join && n.op.to_ascii_lowercase().starts_with("nested loop"))
    {
        if n.children.len() < 2 {
            continue;
        }
        let outer = &n.children[0];
        let inner = &n.children[n.children.len() - 1];
        // Said better elsewhere: key lookups, NOT IN's row count spool, a cross join, MySQL's join buffer (no repeats).
        let mut inner_nodes = Vec::new();
        inner.walk(&mut inner_nodes);
        if inner.kind == NodeKind::Lookup
            || inner_nodes.iter().any(|x| x.op == "Row Count Spool" || x.props.contains_key("using_join_buffer"))
            || cross.contains(&n.id)
            || inner_nodes.iter().any(|x| not_in_ids.contains(&x.id))
        {
            continue;
        }
        // An eager index spool is the eager-spool rule's: it is built once per run, not repeated per row.
        if inner_nodes.iter().any(|x| x.op == "Index Spool" && x.detail.as_deref() == Some("Eager Spool")) {
            continue;
        }
        // Times the inner side is asked for rows: once per outer row.
        let asked =
            if inner.act_executions.is_some() || inner.est_executions > 1.0 { inner.runs() } else { rows_out(outer) };
        // A cache in front of the inner side (PostgreSQL's Memoize and Materialize, SQL Server's lazy spools, MySQL's
        // materialized subqueries) replays rows it holds: its input runs again only on a cache miss or a rebind, which
        // is how often that input ran.
        let materializes = |x: &PlanNode| {
            matches!(x.op.as_str(), "Memoize" | "Table Spool" | "Index Spool" | "Query block" | "Materialized subquery")
                || x.op.starts_with("Materialize")
        };
        let cached_input = if materializes(inner) {
            inner.children.first()
        } else if inner.object.is_none() {
            inner.children.iter().find(|c| materializes(c))
        } else {
            None
        };
        let cache = materializes(inner) || cached_input.is_some();
        let reads = if cache { cached_input.map_or(0.0, |c| c.runs()) } else { asked };
        let outer_name = table_leaf(&first_object(outer).unwrap_or_else(|| outer.op.clone()));
        let inner_name = table_leaf(&first_object(inner).unwrap_or_else(|| inner.op.clone()));
        let hinted = {
            let s = cx.statement();
            s.contains("loop join") || s.contains("option (loop")
        };
        // The optimizer's choice stands unless the plan shows it was made on wrong numbers, or is forced, or each run
        // rereads much of a table: an accurate outer estimate means a loop was costed against a hash join and won.
        let expected = outer.est_rows_for_runs();
        let misjudged = outer.act_rows.is_some_and(|a| a >= MISESTIMATE_FACTOR * expected.max(1.0));
        let leaf = inner_nodes.iter().find(|x| x.object.is_some() && x.kind != NodeKind::Other).copied();
        let per_read = leaf.map(|l| rows_read(l) / l.runs().max(1.0));
        let table = leaf.and_then(|l| l.table_rows).filter(|t| *t > 0.0);
        let rereads = !cache
            && per_read.is_some_and(|per| {
                per >= 100.0
                    && match table {
                        Some(t) => per >= t * INDEX_KEEPS_AT_MOST,
                        None => leaf.is_some_and(|l| l.kind == NodeKind::Scan),
                    }
            });
        // Each outer row meets every row the cache replays, and the join tests its condition on every pair.
        let per_replay = rows_out(inner) / asked.max(1.0);
        let pairs = cache && n.predicate.is_some() && asked >= MANY_LOOPS && per_replay >= 100.0;
        let repeated = reads >= MANY_LOOPS && (hinted || misjudged || rereads);
        if !repeated && !pairs {
            continue;
        }
        let impact = (cx.subtree(inner) + cx.share(n)).min(1.0);
        let mut d = Draft::new("nested-loops", InsightArea::Query, impact, vec![n.id, inner.id]);
        d.cap = Severity::Medium;
        let mysql = cx.engine() == Engine::Mysql;
        d.title = if repeated {
            format!("Repeats its read of {inner_name} {} times, once per row of {outer_name}", format::count(reads))
        } else {
            format!(
                "Compares each of {} rows of {outer_name} with {} rows the {} replays ({} pairs)",
                format::count(asked),
                format::count(per_replay),
                inner.op,
                format::count(asked * per_replay)
            )
        };
        let forced = "The query forces a loop join, so the optimizer didn't weigh a hash or merge join.";
        d.detail = if hinted && repeated {
            forced.to_string()
        } else if misjudged && repeated {
            format!(
                "The optimizer expected {} rows from {outer_name} and chose a nested loop; it got {}. {}",
                format::count(expected),
                rows_text(rows_out(outer)),
                if mysql {
                    "MySQL joins through an index whenever one fits; the estimate decides the join order, so a wrong one can start from the bigger side."
                } else {
                    "At that size one pass with a hash join is usually much cheaper, and the optimizer would have weighed one."
                }
            )
        } else if repeated {
            format!(
                "Each run reads about {} rows of {inner_name}{}, so the loop reads it {} times over. {} of this plan's work.",
                format::count(per_read.unwrap_or(0.0)),
                table
                    .map(|t| format!(" ({} of the table)", format::percent((per_read.unwrap_or(0.0) / t).min(1.0))))
                    .unwrap_or_default(),
                format::count(reads),
                capitalize(&format::percent(impact))
            )
        } else {
            format!(
                "The {} reads {inner_name} once and replays its {} rows for each row of {outer_name}; the join then checks `{}` on every pair. {} of this plan's work.{}",
                inner.op,
                format::count(per_replay),
                format::predicate(n.predicate.as_deref().unwrap_or_default()),
                capitalize(&format::percent(impact)),
                if hinted { format!(" {forced}") } else { String::new() }
            )
        };
        d.fix = if hinted {
            "The query forces a loop join (LOOP JOIN / OPTION (LOOP JOIN)); remove the hint and let the optimizer choose.".into()
        } else if misjudged && repeated {
            if mysql {
                format!(
                    "Fix the estimate for {outer_name}: ANALYZE TABLE, and a histogram on its filtered columns, so MySQL plans the join for the real row count; filtering {outer_name} earlier means fewer rows reach the loop."
                )
            } else {
                format!(
                    "Fix the estimate for {outer_name}: update its statistics. With the right row count the optimizer can choose a hash join by itself."
                )
            }
        } else if repeated && leaf.is_some_and(|l| l.kind == NodeKind::Seek) {
            // It already seeks an index, one that narrows each run too little.
            format!(
                "Give {inner_name} an index that leads with the join column and the columns the inner side filters on, so each run reads only its matching rows; or filter {outer_name} earlier so fewer rows reach the loop."
            )
        } else if repeated {
            format!(
                "Index {inner_name} on the join column so each run reads only its matching rows, or filter {outer_name} earlier so fewer rows reach the loop."
            )
        } else {
            format!(
                "Give the join a condition it can seek or hash on: compare stored columns with = (no functions or computed values around them), indexed on the inner side, or filter {outer_name} earlier so fewer rows reach the loop."
            )
        };
        d.verify = if mysql {
            format!("Explain again: {inner_name} is read with ref or eq_ref access, and far fewer rows reach the loop.")
        } else if hinted {
            "Explain again without the hint: a hash or merge join instead of the loop.".to_string()
        } else if repeated {
            format!("Run with actual rows: a hash join, or far fewer rows read per execution of {inner_name}.")
        } else {
            format!("Explain again: no {} under the join, or a join that seeks its matches.", inner.op)
        };
        out.push(d);
    }
    out
}

fn big_sort(cx: &Cx) -> Vec<Draft> {
    let mut out = Vec::new();
    for n in cx.nodes.iter().filter(|n| n.kind == NodeKind::Sort) {
        if n.warnings.contains(&PlanWarning::Spill)
            || n.detail.as_deref().is_some_and(|d| d.contains("temporary table"))
        {
            continue; // spill and temp-table-sort say more
        }
        let rows = n.children.first().map_or(rows_out(n), rows_out).max(rows_out(n));
        let share = cx.share(n);
        if share < 0.2 || rows < 50_000.0 {
            continue;
        }
        let mut d = Draft::new("big-sort", InsightArea::Index, share, vec![n.id]);
        d.title = format!("Sorting {} rows is {} of this plan's work", format::count(rows), format::percent(share));
        let key =
            n.detail.as_deref().and_then(|d| d.split(" · ").find(|p| p.starts_with("sort key")).map(str::to_string));
        d.detail = format!(
            "The rows arrive in a different order than the query needs{}, so they're all collected and sorted before the first one comes out.",
            key.map(|k| format!(" ({k})")).unwrap_or_default()
        );
        d.fix = "An index whose key order matches the ORDER BY (with the WHERE equality columns first) returns rows already sorted; or sort fewer rows by filtering earlier or limiting the result.".into();
        d.verify = "Explain again: no Sort, or one over far fewer rows.".into();
        out.push(d);
    }
    out
}

fn no_join_predicate(cx: &Cx) -> Vec<Draft> {
    let ids = cross_joins(cx);
    cx.nodes
        .iter()
        .filter(|n| ids.contains(&n.id))
        .copied()
        .collect::<Vec<&PlanNode>>()
        .into_iter()
        // A side of one row (a total, a setting) multiplies nothing: joining to it without a condition is harmless.
        .filter(|n| {
            // Under a TOP, SQL Server estimates each side for the rows it expects to need; the side itself is bigger.
            let per_run = |c: &PlanNode| {
                (rows_out(c) / c.runs().max(1.0)).max(prop_f64(c, "EstimateRowsWithoutRowGoal").unwrap_or(0.0))
            };
            let sides: Vec<f64> = if n.children.len() >= 2 {
                n.children.iter().map(per_run).collect()
            } else {
                // MySQL's table joined through a join buffer: the table, and the rows joined before it.
                let before = cx.parent(n).and_then(|p| {
                    let at = p.children.iter().position(|c| c.id == n.id)?;
                    (at > 0).then(|| rows_out(&p.children[at - 1]))
                });
                vec![rows_read(n) / n.runs().max(1.0), before.unwrap_or(0.0)]
            };
            sides.iter().all(|rows| *rows >= 2.0)
        })
        .take(1)
        .map(|n| {
            let sides: Vec<String> = n.children.iter().filter_map(first_object).map(|o| table_leaf(&o)).collect();
            let impact = cx.subtree(n);
            let mut d = Draft::new("no-join-predicate", InsightArea::Query, impact, vec![n.id]);
            d.title = if sides.len() >= 2 {
                format!("{} and {} are joined without a condition: every row with every row", sides[0], sides[1])
            } else {
                "Two inputs are joined without a condition: every row with every row".into()
            };
            d.detail = format!("{} has no join condition, so its output is the product of both inputs ({} rows planned). That's almost always a missing ON clause.", n.op, format::count(rows_out(n)));
            d.fix = "Add the missing join condition; if you really want every combination, write CROSS JOIN so the intent is clear.".into();
            d.verify = "Explain again: the join has a condition and its row count is reasonable.".into();
            d
        })
        .collect()
}

// ── rules: estimates and statistics ──────────────────────────────────────────────────────────────

fn misestimate(cx: &Cx) -> Vec<Draft> {
    // The plan's misestimates by the one shared rule (`PlanNode::misestimates`), judged here again so a plan saved
    // before that rule reads the same as a fresh one.
    let found: HashMap<u32, f64> = cx.plan.root.misestimates().into_iter().collect();
    // The deepest misestimated operators are the causes; everything above inherits their error.
    let causes: Vec<&PlanNode> = cx
        .nodes
        .iter()
        .filter(|n| found.contains_key(&n.id))
        .filter(|n| {
            let mut below = Vec::new();
            n.children.iter().for_each(|c| c.walk(&mut below));
            !below.iter().any(|b| found.contains_key(&b.id))
        })
        // A table variable estimated at one row is the table-variable rule's: no statistics to refresh.
        .filter(|n| !(table_variable_of(n).is_some() && n.est_rows_total() <= 1.5))
        .copied()
        .collect();
    let mut causes = causes;
    causes.sort_by(|a, b| {
        let w = |n: &PlanNode| found[&n.id].abs() * rows_out(n).max(n.est_rows_for_runs()).log10().max(1.0);
        w(b).total_cmp(&w(a))
    });
    causes
        .into_iter()
        .take(2)
        .map(|n| {
            let factor = found[&n.id];
            let est = n.est_rows_for_runs();
            let act = rows_out(n);
            // MySQL's EXPLAIN ANALYZE estimates the filter in a Filter node above the table it reads.
            let table = n.object.clone().or_else(|| (n.op == "Filter").then(|| first_object(n)).flatten());
            let subject = table.clone().unwrap_or_else(|| n.op.clone());
            let mut above = 0.0;
            // A sort or hash above that ran out of memory was sized for the wrong number of rows.
            let mut spill_above = false;
            // What was planned from this estimate: joins, sorts, aggregates, spools, parallelism above it.
            let mut consumers = false;
            let mut cur = cx.parent(n);
            while let Some(p) = cur {
                above += cx.share(p);
                spill_above |= p.warnings.contains(&PlanWarning::Spill);
                consumers |= matches!(
                    p.kind,
                    NodeKind::Join | NodeKind::Sort | NodeKind::Aggregate | NodeKind::Spool | NodeKind::Parallelism
                ) || p.op.contains("Hash");
                cur = cx.parent(p);
            }
            let impact = (cx.subtree(n) + above).min(1.0);
            let mut d = Draft::new("misestimate", InsightArea::Statistics, impact, vec![n.id]);
            d.floor = if factor.abs() >= 100.0 { Severity::Medium } else { Severity::Low };
            if !consumers {
                // Nothing in this plan was sized from it; the statistics still decide other plans on the table.
                d.floor = Severity::Low;
                d.cap = Severity::Low;
            }
            d.title = format!(
                "Expected {} from {}, got {} ({}× {})",
                plural(est, "row", "rows"),
                table_leaf(&subject),
                rows_text(act),
                rows_text(factor.abs()),
                if factor >= 0.0 { "more" } else { "fewer" }
            );
            let cond = conditions(n);
            // Multi-column statistics only describe plain columns; a filter on an expression (id % 4, lower(x)) needs
            // expression statistics instead, so the advice changes.
            let on_expression = cond.split([' ', '(', ')']).any(|t| matches!(t, "%" | "+" | "-" | "*" | "/"))
                || wrapped_column(&cond, cx.engine()).is_some();
            let cols: Vec<String> = if on_expression { Vec::new() } else { filter_columns(&cond) };
            d.detail = format!(
                "{}{}",
                if consumers {
                    format!(
                        "Everything above this operator was planned for the wrong number of rows: join types, memory grants and parallelism{}.",
                        if spill_above { " (here a sort or hash above it ran out of memory as a result)" } else { "" }
                    )
                } else {
                    "Nothing above it in this plan was sized from that estimate, but the same statistics decide every plan that reads this table.".to_string()
                },
                if cols.len() >= 2 {
                    format!(" The filter combines {}; databases assume such columns are independent, which is wrong when they move together.", format::list(&cols))
                } else if on_expression {
                    " The filter compares an expression, not a plain column, so the column's statistics can't predict it.".to_string()
                } else {
                    String::new()
                }
            );
            let mysql_histograms = !cx
                .prop("server_version")
                .is_some_and(|v| !v.contains("MariaDB") && !crate::version_at_least(v, [8, 0, 0]));
            let (fix, sql) = match (cx.engine(), &table) {
                // Table variables have no statistics, and a temporary table's belong to the session that filled it:
                // nothing Culprit could hand over to run would reach them.
                (Engine::Mssql, Some(t)) if table_variable_of(n).is_some() => (
                    format!("{} is a table variable, so it has no statistics: use a temporary table instead, or add OPTION (RECOMPILE) so the row count at run time is used.", table_leaf(t)),
                    None,
                ),
                (Engine::Mssql, Some(t)) if table_leaf(t).starts_with('#') => (
                    format!("{} is a temporary table: update its statistics in the session that fills it, after loading it, or add OPTION (RECOMPILE) to the query that reads it.", table_leaf(t)),
                    None,
                ),
                (Engine::Mssql, Some(t)) => (
                    format!("Refresh the statistics on {} with a full scan{}.", table_leaf(t), if cols.len() >= 2 { ", and create statistics on the filtered columns together" } else { "" }),
                    Some(if cols.len() >= 2 {
                        format!("UPDATE STATISTICS {} WITH FULLSCAN;\nCREATE STATISTICS st_{}_{} ON {} ({});", qualified(Engine::Mssql, t), table_leaf(t).to_ascii_lowercase(), cols.join("_").to_ascii_lowercase(), qualified(Engine::Mssql, t), cols.iter().map(|c| quote(Engine::Mssql, c)).collect::<Vec<_>>().join(", "))
                    } else {
                        format!("UPDATE STATISTICS {} WITH FULLSCAN;", qualified(Engine::Mssql, t))
                    }),
                ),
                (Engine::Postgres, Some(t)) => (
                    format!(
                        "Analyze {}{}.",
                        table_leaf(t),
                        if cols.len() >= 2 {
                            ", and tell PostgreSQL the filtered columns are related with extended statistics"
                        } else if on_expression {
                            "; on PostgreSQL 14+ statistics on the expression itself help (CREATE STATISTICS … ON (expression) FROM table)"
                        } else {
                            ""
                        }
                    ),
                    Some(if cols.len() >= 2 {
                        format!("ANALYZE {};\nCREATE STATISTICS {}_{}_stats (dependencies, mcv) ON {} FROM {};\nANALYZE {};", qualified(Engine::Postgres, t), table_leaf(t).to_ascii_lowercase(), cols.join("_").to_ascii_lowercase(), cols.iter().map(|c| quote(Engine::Postgres, c)).collect::<Vec<_>>().join(", "), qualified(Engine::Postgres, t), qualified(Engine::Postgres, t))
                    } else {
                        Engine::Postgres.dialect().refresh_statistics(&qualified(Engine::Postgres, t))
                    }),
                ),
                // MariaDB collects histograms with its engine-independent statistics; MySQL has them from 8.0.
                (Engine::Mysql, Some(t)) if cx.mariadb() && !cols.is_empty() => (
                    format!("Refresh the statistics on {}, with column statistics (histograms) on {}.", table_leaf(t), format::list(&cols)),
                    Some(format!("ANALYZE TABLE {} PERSISTENT FOR COLUMNS ({}) INDEXES ();", qualified(Engine::Mysql, t), cols.iter().map(|c| quote(Engine::Mysql, c)).collect::<Vec<_>>().join(", "))),
                ),
                (Engine::Mysql, Some(t)) if mysql_histograms && !cols.is_empty() => (
                    format!("Refresh the statistics on {} and add a histogram on {}.", table_leaf(t), format::list(&cols)),
                    Some(format!("ANALYZE TABLE {} UPDATE HISTOGRAM ON {};", qualified(Engine::Mysql, t), cols.iter().map(|c| quote(Engine::Mysql, c)).collect::<Vec<_>>().join(", "))),
                ),
                (Engine::Mysql, Some(t)) => (
                    format!("Refresh the statistics on {}.", table_leaf(t)),
                    Some(Engine::Mysql.dialect().refresh_statistics(&qualified(Engine::Mysql, t))),
                ),
                (Engine::Sqlite, Some(t)) => (
                    format!("Refresh the statistics on {}.", table_leaf(t)),
                    Some(Engine::Sqlite.dialect().refresh_statistics(&qualified(Engine::Sqlite, t))),
                ),
                (_, None) => ("Refresh statistics on the tables below this operator.".into(), None),
            };
            d.fix = fix;
            d.fix_sql = sql;
            d.verify = "Run with actual rows again: the estimate is within about 10× of the actual count.".into();
            d
        })
        .collect()
}

fn missing_statistics(cx: &Cx) -> Vec<Draft> {
    cx.nodes
        .iter()
        // A table variable never has statistics; the table-variable rule says what to do about it.
        .filter(|n| n.warnings.contains(&PlanWarning::MissingStatistics) && table_variable_of(n).is_none())
        .take(1)
        .map(|n| {
            let cols = n.props.get("ColumnsWithNoStatistics").cloned();
            let mut d = Draft::new("missing-statistics", InsightArea::Statistics, cx.subtree(n), vec![n.id]);
            d.cap = Severity::Medium;
            d.title = format!("No statistics on {}, so its row counts are guesses", cols.clone().unwrap_or_else(|| format!("some columns of {}", table_leaf(&what(n)))));
            d.detail = "Without statistics the optimizer uses fixed guesses for how many rows match, which leads to the wrong join type or memory size. SQL Server creates them by itself unless AUTO_CREATE_STATISTICS is off for the database or it can't write to the database (read-only).".into();
            d.fix = "Check the database's settings: if automatic statistics creation is off, turn it back on (it's on by default); if the database is read-only, create the statistics while it is writable (or on the primary, for a readable secondary) so they come with it.".into();
            d.fix_sql = Some("SELECT name, is_auto_create_stats_on, is_read_only\n  FROM sys.databases\n WHERE database_id = DB_ID();\n-- If is_auto_create_stats_on is 0:\n-- ALTER DATABASE CURRENT SET AUTO_CREATE_STATISTICS ON;".into());
            d.verify = "Explain again: the ColumnsWithNoStatistics warning is gone.".into();
            d
        })
        .collect()
}

fn optimizer_limits(cx: &Cx) -> Vec<Draft> {
    let mut out = Vec::new();
    if let Some(reason) =
        cx.prop("StatementOptmEarlyAbortReason").filter(|r| matches!(*r, "TimeOut" | "MemoryLimitExceeded"))
    {
        let mut d = Draft::new("optimizer-timeout", InsightArea::Query, 0.1, vec![cx.plan.root.id]);
        d.subtree = true;
        d.cap = Severity::Medium;
        let memory = reason.eq_ignore_ascii_case("MemoryLimitExceeded");
        d.title = if memory {
            "The optimizer ran out of memory before it finished choosing a plan".into()
        } else {
            "The optimizer stopped searching before it found the best plan (timeout)".into()
        };
        d.detail = format!(
            "SQL Server reports StatementOptmEarlyAbortReason = {reason}: the query is complex enough that the plan in use is the best one found in the time allowed, not necessarily the best one."
        );
        d.fix = "Make it simpler to optimize: split it into steps with a temporary table, remove joins the result doesn't need, or replace nested views with direct table references.".into();
        d.verify = "Explain again: StatementOptmEarlyAbortReason is gone.".into();
        out.push(d);
    }
    if cx.prop("CardinalityEstimationModelVersion") == Some("70") {
        let mut d = Draft::new("legacy-ce", InsightArea::Server, 0.02, vec![cx.plan.root.id]);
        d.subtree = true;
        d.cap = Severity::Low;
        d.title = "Planned with the legacy cardinality estimator (the SQL Server 2012 model)".into();
        d.detail = "The database has LEGACY_CARDINALITY_ESTIMATION on, a compatibility level below 120, or the query uses the FORCE_LEGACY_CARDINALITY_ESTIMATION hint. The newer estimator handles correlated filters and joins better.".into();
        d.fix = "Test the query with the current estimator (OPTION (USE HINT('FORCE_DEFAULT_CARDINALITY_ESTIMATION'))) and, if it's faster, plan the switch for the database.".into();
        d.verify = "Explain again: CardinalityEstimationModelVersion is 120 or higher.".into();
        out.push(d);
    }
    out
}

fn slow_compile(cx: &Cx) -> Vec<Draft> {
    let (planning, running) = match cx.engine() {
        Engine::Mssql => (cx.prop("CompileTime").and_then(|v| v.parse::<f64>().ok()), None),
        Engine::Postgres => (
            cx.prop("Planning Time").and_then(|v| v.parse::<f64>().ok()),
            cx.prop("Execution Time").and_then(|v| v.parse::<f64>().ok()),
        ),
        Engine::Mysql | Engine::Sqlite => (None, None),
    };
    let Some(planning) = planning else { return Vec::new() };
    let slower_than_running = running.is_some_and(|r| planning > r && planning >= 20.0);
    if planning < 500.0 && !slower_than_running {
        return Vec::new();
    }
    let running = running.or_else(|| elapsed_ms(cx.plan));
    let impact = running.map_or(0.1, |r| planning / (planning + r).max(1.0));
    let mut d = Draft::new("slow-compile", InsightArea::Query, impact, vec![cx.plan.root.id]);
    d.cost_ms = Some(planning);
    d.cap = Severity::Medium;
    // PostgreSQL's first planning on a connection also loads the tables' metadata into the backend's caches, and
    // Culprit's EXPLAIN runs on a fresh connection: a few tens of milliseconds may never recur.
    let cold = cx.engine() == Engine::Postgres;
    if cold && planning < 500.0 {
        d.cap = Severity::Low;
    }
    d.title = match running {
        Some(r) if slower_than_running => {
            format!("Planning takes {}, longer than running it ({})", format::duration(planning), format::duration(r))
        }
        _ => format!("Compiling this plan took {}", format::duration(planning)),
    };
    d.detail = format!(
        "When the plan isn't reused, every run pays this again. Many joins, many partitions or long IN lists make planning expensive.{}",
        if cold {
            " This is one measurement, and the first planning on a new connection also reads the tables' metadata: explain it twice in one session to see the time a warm connection pays."
        } else {
            ""
        }
    );
    d.fix = match cx.engine() {
        Engine::Postgres => "Reuse the plan with prepared statements, keep IN lists short (use = ANY($1) with an array), and make sure partition pruning can happen at plan time.".into(),
        _ => "Make sure the plan is reused: send the query parameterized rather than with literal values, and avoid OPTION (RECOMPILE) on frequent queries.".into(),
    };
    d.verify = "Explain again: compile/planning time is a small fraction of the run.".into();
    vec![d]
}

// ── rules: memory ────────────────────────────────────────────────────────────────────────────────

/// Node prop (SQL Server): a Parallelism exchange wrote its buffers to tempdb. That resolves a deadlock between parallel
/// threads (an order-preserving exchange fed unevenly); it isn't a sort or hash short of memory.
pub const EXCHANGE_SPILL: &str = "ExchangeSpill";

/// A spill smaller than this is at most Low: it costs little next to the work around it.
const SPILL_MATERIAL_BYTES: f64 = 1024.0 * 1024.0;

/// What a spilling operator wrote to disk, when the plan says: tempdb pages (SQL Server), the sort's or hash
/// aggregate's own disk use, or the temporary blocks the operator itself wrote (PostgreSQL with BUFFERS).
fn spilled_bytes(n: &PlanNode) -> Option<f64> {
    let own_temp_pages = n.runtime.as_ref().and_then(|r| r.spill_pages).filter(|p| *p > 0.0);
    let disk_kb = prop_f64(n, "Sort Space Used")
        .filter(|_| n.props.get("Sort Space Type").map(String::as_str) == Some("Disk"))
        .or_else(|| prop_f64(n, "Disk Usage").filter(|kb| *kb > 0.0));
    disk_kb
        .map(|kb| kb * 1024.0)
        .or_else(|| prop_f64(n, "TempdbWritesPages").map(|p| p * 8192.0))
        .or_else(|| own_temp_pages.map(|p| p * 8192.0))
}

fn spill(cx: &Cx) -> Vec<Draft> {
    let mut out = Vec::new();
    for n in cx.nodes.iter().filter(|n| is_true(n.props.get(EXCHANGE_SPILL))) {
        let mut d = Draft::new("spill", InsightArea::Parallelism, cx.share(n), vec![n.id]);
        d.title = match spilled_bytes(n) {
            Some(b) => format!(
                "The {} exchange wrote {} to tempdb to keep its threads from waiting on each other",
                n.op,
                format::bytes(b)
            ),
            None => format!("The {} exchange wrote to tempdb to keep its threads from waiting on each other", n.op),
        };
        d.detail = "SQL Server spills an exchange's buffers to tempdb when its parallel threads would otherwise wait on each other for good, usually at an exchange that keeps rows in order (merging streams) fed unevenly. It isn't a memory grant problem.".into();
        d.fix = "Remove the need for order at that exchange (an ORDER BY or merge join above it that an index or a hash join could serve), and check whether one value holds most of the rows; comparing a run with OPTION (MAXDOP 1) shows what parallelism costs here.".into();
        d.verify = "Run with an actual plan: no ExchangeSpillDetails on the Parallelism operator.".into();
        out.push(d);
    }
    for n in cx.nodes.iter().filter(|n| n.warnings.contains(&PlanWarning::Spill)) {
        let mut d = Draft::new("spill", InsightArea::Memory, cx.share(n), vec![n.id]);
        // The plan's costs were estimated before the spill happened; its size is the evidence of what it cost.
        match spilled_bytes(n) {
            Some(bytes) if bytes < SPILL_MATERIAL_BYTES => d.cap = Severity::Low,
            Some(_) => d.floor = Severity::Medium,
            None => {}
        }
        let underestimated = {
            let mut below = Vec::new();
            n.walk(&mut below);
            below.iter().any(|b| b.misestimate().is_some_and(|m| m >= MISESTIMATE_FACTOR))
        };
        match cx.engine() {
            // EXPLAIN QUERY PLAN reports no spills.
            Engine::Sqlite => continue,
            Engine::Postgres => {
                let disk_kb = prop_f64(n, "Sort Space Used")
                    .filter(|_| n.props.get("Sort Space Type").map(String::as_str) == Some("Disk"));
                let batches = prop_f64(n, "Hash Batches").filter(|b| *b > 1.0);
                let agg_batches = prop_f64(n, "HashAgg Batches").filter(|b| *b > 1.0);
                d.title = match (disk_kb, batches, agg_batches, spilled_bytes(n)) {
                    (Some(kb), _, _, _) => format!(
                        "The sort writes {} to disk because it didn't fit in work_mem",
                        format::bytes(kb * 1024.0)
                    ),
                    (None, Some(b), _, _) => format!(
                        "The hash split its input into {} batches on disk because it didn't fit in work_mem",
                        format::count(b)
                    ),
                    (None, None, _, Some(bytes)) => format!(
                        "{} writes {} of temporary files because it didn't fit in work_mem",
                        n.op,
                        format::bytes(bytes)
                    ),
                    (None, None, Some(b), None) => format!(
                        "{} split its groups into {} batches on disk because they didn't fit in work_mem",
                        n.op,
                        format::count(b)
                    ),
                    (None, None, None, None) => {
                        format!("{} wrote temporary files because it didn't fit in work_mem", n.op)
                    }
                };
                d.detail = format!(
                    "Disk is many times slower than memory for sorting and hashing.{}",
                    if underestimated {
                        " The rows feeding it were badly underestimated, so it was planned with too little memory."
                    } else {
                        ""
                    }
                );
                d.fix = "Give this workload more memory for sorts and hashes, per role or per session rather than server-wide, or add an index that returns rows already ordered so the sort disappears.".into();
                d.fix_sql = Some("-- Try it in your session first:\nSET work_mem = '64MB';\n-- Then for the application's role:\nALTER ROLE app_user SET work_mem = '64MB';".into());
                d.verify =
                    "Explain analyze again: Sort Method is quicksort/top-N (in memory) and Hash Batches is 1.".into();
            }
            // MariaDB's ANALYZE counts a sort's merge passes: it didn't fit in sort_buffer_size.
            Engine::Mysql => {
                let passes = prop_f64(n, "r_sort_passes").unwrap_or(1.0);
                d.title = format!(
                    "The sort needed {} through temporary files because it didn't fit in sort_buffer_size",
                    plural(passes, "merge pass", "merge passes")
                );
                d.detail = format!(
                    "Each merge pass writes the sorted runs to a temporary file and reads them back.{}",
                    if underestimated { " The rows feeding it were badly underestimated." } else { "" }
                );
                d.fix = "Raise sort_buffer_size for the session that runs it rather than server-wide (it's allocated per sort), or add an index that returns rows already in order so the sort disappears.".into();
                d.fix_sql =
                    Some("-- Try it in your session first:\nSET SESSION sort_buffer_size = 8 * 1024 * 1024;".into());
                d.verify = "Run ANALYZE again: r_sort_passes is 0.".into();
            }
            Engine::Mssql => {
                d.title = match prop_f64(n, "TempdbWritesPages") {
                    Some(pages) => {
                        format!("{} ran out of memory and wrote {} to tempdb", n.op, format::bytes(pages * 8192.0))
                    }
                    None => format!("{} ran out of memory and wrote to tempdb", n.op),
                };
                d.detail = format!(
                    "The memory granted to {} was too small for the rows it got, so part of the work went to disk in tempdb.{}",
                    n.op,
                    if underestimated {
                        " The rows feeding it were badly underestimated; the grant was sized for the estimate."
                    } else {
                        ""
                    }
                );
                d.fix = "Fix the row estimate first (update statistics on the tables feeding it); an index that returns rows in the needed order removes the sort altogether. SQL Server 2019+ also grows the grant on later runs (memory grant feedback, compatibility level 150).".into();
                d.verify = "Run with an actual plan: no spill warning on that operator.".into();
            }
        }
        out.push(d);
    }
    out
}

fn memory_grant(cx: &Cx) -> Vec<Draft> {
    if cx.engine() != Engine::Mssql {
        return Vec::new();
    }
    let kb = |key: &str| cx.prop(key).and_then(|v| v.parse::<f64>().ok());
    let mut out = Vec::new();
    let root = cx.plan.root.id;
    if let (Some(granted), Some(used)) = (kb("MemoryGrantedKB"), kb("MemoryMaxUsedKB"))
        && has_actuals(cx.plan)
        && granted >= 10.0 * 1024.0
        && used < 0.25 * granted
    {
        let mut d = Draft::new("memory-grant", InsightArea::Memory, 0.15, vec![memory_node(cx).unwrap_or(root)]);
        d.subtree = true;
        d.floor = if granted >= 1024.0 * 1024.0 { Severity::High } else { Severity::Medium };
        d.title = format!(
            "Reserves {} of memory per run and uses {}",
            format::bytes(granted * 1024.0),
            format::bytes(used * 1024.0)
        );
        d.detail = format!(
            "The grant is sized from row estimates before the query runs; when they're too high, memory sits reserved and other queries wait for it (RESOURCE_SEMAPHORE).{}",
            cx.prop("MemoryGrantWarning").map(|w| format!(" SQL Server flags it too: {w}.")).unwrap_or_default()
        );
        d.fix = "Fix the estimates (update statistics, avoid functions on filtered columns). SQL Server 2019+ shrinks grants on later runs (memory grant feedback); as a last resort cap it with OPTION (MAX_GRANT_PERCENT = 5).".into();
        d.verify = "Run with an actual plan: GrantedMemory is close to MaxUsedMemory.".into();
        out.push(d);
    } else if let Some(desired) = kb("MemoryDesiredKB")
        && !has_actuals(cx.plan)
        && desired >= 1024.0 * 1024.0
    {
        let mut d = Draft::new("memory-grant", InsightArea::Memory, 0.1, vec![memory_node(cx).unwrap_or(root)]);
        d.subtree = true;
        d.title = format!("Asks for {} of memory per run", format::bytes(desired * 1024.0));
        d.detail =
            "Large grants are reserved for the whole run; a few of these at once make other queries queue for memory."
                .into();
        d.fix = "Check the row estimates of the sorts and hashes; sort fewer rows or columns, or cap it with OPTION (MAX_GRANT_PERCENT = n).".into();
        d.verify = "Explain again: the desired memory is lower.".into();
        out.push(d);
    }
    if let Some(wait) = kb("MemoryGrantWaitMs").filter(|w| *w >= 100.0) {
        let mut d = Draft::new("memory-grant", InsightArea::Memory, 0.2, vec![root]);
        d.cost_ms = Some(wait);
        d.floor = Severity::Medium;
        d.title = format!("Waited {} for memory before it could start", format::duration(wait));
        d.detail = "Other queries held the memory it needed. The run was slow before it did any work.".into();
        d.fix = "Find the queries with oversized grants (they show up with this same insight) and shrink them; adding memory only postpones it.".into();
        d.verify = "Run with an actual plan: GrantWaitTime is 0.".into();
        out.push(d);
    }
    out
}

fn memory_node(cx: &Cx) -> Option<u32> {
    cx.nodes
        .iter()
        .filter(|n| n.kind == NodeKind::Sort || n.op.contains("Hash"))
        .max_by(|a, b| a.self_cost.total_cmp(&b.self_cost))
        .map(|n| n.id)
}

// ── rules: parallelism ───────────────────────────────────────────────────────────────────────────

fn parallelism_blocked(cx: &Cx) -> Vec<Draft> {
    // The plan doesn't carry the server's cost threshold for parallelism: below its default (5) no setting would make
    // the plan parallel, and only above the 50 it's commonly raised to does running serially say much.
    const DEFAULT_THRESHOLD: f64 = 5.0;
    const COMMON_THRESHOLD: f64 = 50.0;
    let Some(reason) = cx.prop("NonParallelPlanReason") else { return Vec::new() };
    let cost = cx.plan.total_cost;
    if reason == "TSQLUserDefinedFunctionsNotParallelizable" || cost < DEFAULT_THRESHOLD {
        return Vec::new();
    }
    let plain = match reason {
        "MaxDOPSetToOne" => "MAXDOP is 1 (a query hint, or the database or server setting)",
        "EstimatedDOPIsOne" => "only one CPU is available to it",
        "NoParallelPlansInDesktopOrExpressEdition" => "this SQL Server edition doesn't run parallel plans",
        "CouldNotGenerateValidParallelPlan" => "SQL Server couldn't build a valid parallel plan for it",
        "NonParallelizableIntrinsicFunction" => "the query calls a built-in function that can't run in parallel",
        "TableVariableTransactionsDoNotSupportParallelNestedTransaction" => "it writes to a table variable",
        "NoParallelForMemoryOptimizedTables" => "it touches memory-optimized tables",
        "UpdatingWritebackVariable" => "it assigns to a variable",
        "CLRUserDefinedFunctionRequiresDataAccess" => "it calls a CLR function that reads data",
        _ => "",
    };
    let mut d = Draft::new("parallelism-blocked", InsightArea::Parallelism, 0.1, vec![cx.plan.root.id]);
    d.subtree = true;
    d.cap = if cost >= COMMON_THRESHOLD { Severity::Medium } else { Severity::Low };
    d.title = if plain.is_empty() {
        format!("This plan runs on one CPU: SQL Server reports {reason}")
    } else {
        format!("This plan runs on one CPU because {plain}")
    };
    d.detail = format!(
        "Its estimated cost is {cost:.0}. SQL Server considers a parallel plan above the server's cost threshold for parallelism (5 by default, often raised to 50){}; the whole run happens on a single core instead.",
        if cost >= COMMON_THRESHOLD {
            ", which this plan passes either way"
        } else {
            ", so whether it would split depends on that setting"
        }
    );
    d.fix = match reason {
        "MaxDOPSetToOne" => "Remove OPTION (MAXDOP 1) if the query has it; otherwise check MAXDOP for the database (ALTER DATABASE SCOPED CONFIGURATION) and the server.".into(),
        _ => "Remove what prevents parallelism if the query allows it; otherwise make the single-threaded run cheaper (indexes, fewer rows).".into(),
    };
    d.verify = "Explain again: no NonParallelPlanReason, and Parallelism operators appear.".into();
    vec![d]
}

fn parallel_skew(cx: &Cx) -> Vec<Draft> {
    let mut out = Vec::new();
    for n in &cx.nodes {
        let rows: Vec<f64> = match cx.engine() {
            Engine::Mssql => {
                let (Some(threads), Some(max), Some(total)) =
                    (prop_f64(n, "ThreadCount"), prop_f64(n, "ThreadRowsMax"), n.act_rows)
                else {
                    continue;
                };
                if threads < 2.0 {
                    continue;
                }
                let others = (total - max).max(0.0) / (threads - 1.0);
                let mut v = vec![max];
                v.extend(std::iter::repeat_n(others, threads as usize - 1));
                v
            }
            Engine::Postgres => {
                let Some(workers) =
                    n.props.get("Workers").and_then(|w| serde_json::from_str::<serde_json::Value>(w).ok())
                else {
                    continue;
                };
                let Some(list) = workers.as_array() else { continue };
                let loops_rows: Vec<f64> = list
                    .iter()
                    .filter_map(|w| {
                        let rows = w.get("Actual Rows")?.as_f64()?;
                        let loops = w.get("Actual Loops").and_then(|l| l.as_f64()).unwrap_or(1.0).max(1.0);
                        Some(rows * loops)
                    })
                    .collect();
                if loops_rows.is_empty() {
                    continue;
                }
                let leader = (n.act_rows.unwrap_or(0.0) - loops_rows.iter().sum::<f64>()).max(0.0);
                let mut v = loops_rows;
                v.push(leader);
                v
            }
            Engine::Mysql | Engine::Sqlite => continue,
        };
        let total: f64 = rows.iter().sum();
        let max = rows.iter().cloned().fold(0.0, f64::max);
        let avg = total / rows.len() as f64;
        if total < 10_000.0 || avg <= 0.0 || max < 3.0 * avg {
            continue;
        }
        // Skew costs time only in proportion to the work involved, and a TOP makes uneven threads normal.
        let has_top = cx.nodes.iter().any(|x| x.op == "Top" || x.op == "Limit");
        let mut d = Draft::new("parallel-skew", InsightArea::Parallelism, cx.share(n) * 0.3, vec![n.id]);
        d.cap = if has_top || total < 100_000.0 { Severity::Low } else { Severity::Medium };
        let who = if cx.engine() == Engine::Postgres { "process" } else { "thread" };
        d.title =
            format!("One {who} of {} handled {} of the rows in {}", rows.len(), format::percent(max / total), n.op);
        d.detail = format!("Parallel work is only as fast as its busiest {who}; the others finished early and waited.");
        d.fix = "Uneven work usually means one key value holds most of the rows (check the distribution of the join or filter column) or a parallel scan over a small range; update statistics and consider filtering that value separately.".into();
        d.verify = format!("Run with actual rows: each {who} handles a similar share.");
        out.push(d);
    }
    out.sort_by(|a, b| b.impact.total_cmp(&a.impact));
    out.into_iter().take(1).collect()
}

fn workers_short(cx: &Cx) -> Vec<Draft> {
    cx.nodes
        .iter()
        // A Gather that never ran launched no workers because it never asked for any.
        .filter(|n| n.act_executions.is_none_or(|runs| runs > 0.0))
        .filter_map(|n| {
            let planned = prop_f64(n, "Workers Planned")?;
            let launched = prop_f64(n, "Workers Launched")?;
            (launched < planned).then_some((n, planned, launched))
        })
        .take(1)
        .map(|(n, planned, launched)| {
            let mut d = Draft::new("workers-short", InsightArea::Server, cx.subtree(n), vec![n.id]);
            d.subtree = true;
            d.cap = Severity::Medium;
            d.title = format!("Planned {} parallel workers, only {} started", format::count(planned), format::count(launched));
            d.detail = "The server had no free parallel workers when this ran (max_parallel_workers / max_worker_processes), so it ran with less help than it was planned for.".into();
            d.fix = "If the server has idle CPUs, raise max_parallel_workers (and max_worker_processes, which needs a restart); otherwise reduce how many queries run in parallel at once.".into();
            d.verify = "Explain analyze again: Workers Launched equals Workers Planned.".into();
            d
        })
        .collect()
}

// ── rules: SQL Server specific shapes ────────────────────────────────────────────────────────────

fn eager_spool(cx: &Cx) -> Vec<Draft> {
    let mut out = Vec::new();
    for n in cx.nodes.iter().filter(|n| n.op == "Index Spool" && n.detail.as_deref() == Some("Eager Spool")) {
        let table = first_object(n).unwrap_or_else(|| "the table".into());
        let keys = filter_columns(n.seek_predicate.as_deref().unwrap_or(""));
        let include = output_columns(n);
        let rows = n.children.first().map_or(0.0, rows_out);
        let impact = cx.subtree(n);
        let mut d = Draft::new("eager-spool", InsightArea::Index, impact, vec![n.id]);
        d.floor = Severity::Medium;
        d.title = format!("SQL Server builds a temporary index on {} every time this runs", table_leaf(&table));
        d.detail = format!(
            "The Index Spool copies {} rows of {} into a temporary index in tempdb on each run and drops it afterwards, because no index fits the lookup{}.",
            format::count(rows),
            table_leaf(&table),
            if keys.is_empty() { String::new() } else { format!(" on {}", format::list(&keys)) }
        );
        d.fix = "Create that index permanently, so the work happens once instead of on every run.".into();
        d.fix_sql = index_sql(Engine::Mssql, &table, &keys, &include);
        d.verify = "Explain again: the Index Spool is gone and the lookup seeks the new index.".into();
        out.push(d);
    }
    out
}

/// SQLite built a throwaway index for this statement (`AUTOMATIC COVERING INDEX ON t(x)`): by its own account no index
/// fits, so it builds one on each run and drops it. Its plans carry no row counts, so how much that costs is unknown
/// here: no floor, unlike SQL Server's eager spool.
fn automatic_index(cx: &Cx) -> Vec<Draft> {
    if cx.engine() != Engine::Sqlite {
        return Vec::new();
    }
    let mut out = Vec::new();
    for n in &cx.nodes {
        let Some(on) = n.props.get("Automatic Index") else { continue };
        let Some((named, columns)) = on.split_once('(') else { continue };
        // The step's table, with any alias the plan used already resolved to it.
        let table = n.object.clone().unwrap_or_else(|| named.trim().to_string());
        let keys: Vec<String> = columns
            .trim_end_matches(')')
            .split(',')
            .filter_map(|c| {
                let name: String = c.trim().chars().take_while(|ch| ch.is_alphanumeric() || *ch == '_').collect();
                (!name.is_empty()).then_some(name)
            })
            .collect();
        let mut d = Draft::new("automatic-index", InsightArea::Index, 1.0, vec![n.id]);
        d.title = format!("SQLite builds a temporary index on {} every time this runs", table_leaf(&table));
        d.detail = format!(
            "No index on {} fits this {}, so SQLite builds one{} for each run and throws it away.",
            table_leaf(&table),
            if cx.statement().contains("join") { "join" } else { "search" },
            if keys.is_empty() { String::new() } else { format!(" on {}", format::list(&keys)) }
        );
        d.fix = "Create that index once, so the work happens one time instead of on every run.".into();
        d.fix_sql = index_sql(Engine::Sqlite, &table, &keys, &[]);
        d.verify = "Explain again: no AUTOMATIC INDEX, and a SEARCH using the new index.".into();
        out.push(d);
    }
    out
}

fn many_to_many(cx: &Cx) -> Vec<Draft> {
    cx.nodes
        .iter()
        .filter(|n| n.op == "Merge Join" && is_true(n.props.get("ManyToMany")))
        .take(1)
        .map(|n| {
            let mut d = Draft::new("many-to-many", InsightArea::Index, cx.share(n), vec![n.id]);
            d.cap = Severity::Medium;
            d.title = "The merge join is many-to-many, so it replays duplicates through a tempdb worktable".into();
            d.detail = "When neither input is unique on the join columns, SQL Server rewinds the inner side for every duplicate on the outer side.".into();
            d.fix = "If one side really is unique on the join columns, declare it (a unique index or constraint); otherwise a hash join is usually cheaper.".into();
            d.verify = "Explain again: the Merge Join no longer shows ManyToMany.".into();
            d
        })
        .collect()
}

fn remote_query(cx: &Cx) -> Vec<Draft> {
    cx.nodes
        .iter()
        .filter(|n| n.op.starts_with("Remote"))
        .take(1)
        .map(|n| {
            let mut d = Draft::new("remote-query", InsightArea::Query, cx.subtree(n), vec![n.id]);
            d.cap = Severity::Medium;
            // Remote Insert, Update and Delete send rows the other way: each one is a round trip to the other server.
            let writes = ["Remote Insert", "Remote Update", "Remote Delete"].contains(&n.op.as_str());
            if writes {
                let rows = n.children.first().map_or_else(|| rows_out(n), rows_out);
                d.title = format!("Sends {} to a linked server ({})", plural(rows, "row", "rows"), n.op);
                d.detail = "Each row written through a linked server crosses the network, usually one statement per row on the other side.".into();
                d.fix = "Run the change on the remote server itself (EXEC (…) AT the linked server, or a procedure there) with the rows it needs, rather than row by row from here.".into();
                d.verify = "Explain again: no Remote Insert, Update or Delete over many rows.".into();
            } else {
                d.title = format!("Pulls {} from a linked server", plural(rows_out(n), "row", "rows"));
                d.detail = "Rows from another server travel over the network, and filters that can't be sent along are applied locally after the transfer.".into();
                d.fix = "Filter on the remote side (OPENQUERY with the WHERE clause inside), or copy the data you need locally on a schedule.".into();
                d.verify = "Explain again: far fewer rows come back from the Remote operator.".into();
            }
            d
        })
        .collect()
}

// ── rules: Postgres specific shapes ──────────────────────────────────────────────────────────────

fn heap_fetches(cx: &Cx) -> Vec<Draft> {
    cx.nodes
        .iter()
        .filter(|n| n.op == "Index Only Scan")
        .filter_map(|n| {
            let fetches = prop_f64(n, "Heap Fetches")?;
            (fetches >= 1_000.0 && fetches >= 0.1 * rows_out(n)).then_some((n, fetches))
        })
        .take(1)
        .map(|(n, fetches)| {
            let table = what(n);
            let mut d = Draft::new("heap-fetches", InsightArea::Server, cx.share(n), vec![n.id]);
            d.cap = Severity::Medium;
            d.title = format!("The index-only scan on {} still visits the table {} times", table_leaf(&table), rows_text(fetches));
            d.detail = "PostgreSQL can only skip the table for pages the visibility map marks all-visible, which VACUUM maintains; this table hasn't been vacuumed since many of its rows changed.".into();
            d.fix = format!("Vacuum {}, and check that autovacuum isn't turned off for it (ALTER TABLE … SET (autovacuum_enabled = true)).", table_leaf(&table));
            d.fix_sql = Some(format!("VACUUM (ANALYZE) {};", qualified(Engine::Postgres, &table)));
            d.verify = "Explain analyze again: Heap Fetches is close to 0.".into();
            d
        })
        .collect()
}

fn lossy_bitmap(cx: &Cx) -> Vec<Draft> {
    cx.nodes
        .iter()
        .filter_map(|n| {
            let lossy = prop_f64(n, "Lossy Heap Blocks")?;
            // A BRIN index only ever gives lossy pages: no exact ones means nothing was lost for want of work_mem.
            let exact = prop_f64(n, "Exact Heap Blocks").unwrap_or(0.0);
            (lossy > 0.0 && exact > 0.0).then_some((n, lossy))
        })
        .take(1)
        .map(|(n, lossy)| {
            let rechecked = prop_f64(n, "Rows Removed by Index Recheck").unwrap_or(0.0);
            let mut d = Draft::new("lossy-bitmap", InsightArea::Memory, cx.share(n), vec![n.id]);
            d.cap = Severity::Medium;
            d.title = format!("The bitmap scan on {} ran out of work_mem and went lossy", table_leaf(&what(n)));
            d.detail = format!(
                "For {} pages it only remembered the page, not the rows, so every row on them was re-checked{}.",
                format::count(lossy),
                if rechecked > 0.0 {
                    format!(" ({} rows thrown away)", format::count(rechecked))
                } else {
                    String::new()
                }
            );
            d.fix =
                "Raise work_mem for this workload (per role or session), or make the condition more selective.".into();
            d.fix_sql = Some("SET work_mem = '32MB';  -- test in your session first".into());
            d.verify = "Explain analyze again: no Lossy Heap Blocks.".into();
            d
        })
        .collect()
}

fn jit(cx: &Cx) -> Vec<Draft> {
    if cx.engine() != Engine::Postgres {
        return Vec::new();
    }
    let Some(raw) = cx.prop("JIT") else { return Vec::new() };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(raw) else { return Vec::new() };
    let Some(total) = value.pointer("/Timing/Total").and_then(|v| v.as_f64()) else { return Vec::new() };
    let execution = cx.prop("Execution Time").and_then(|v| v.parse::<f64>().ok());
    if total < 10.0 || execution.is_some_and(|e| total < 0.2 * e) {
        return Vec::new();
    }
    let share = execution.map(|e| (total / e.max(total)).min(1.0));
    // Execution Time includes the compiling. When compiling took at least as long as everything else, the compiled
    // code can't have won that back; below that it may have, and only a run without JIT can tell.
    let dominates = share.is_some_and(|s| s >= 0.5);
    let mut d = Draft::new("jit", InsightArea::Server, share.unwrap_or(0.2), vec![cx.plan.root.id]);
    d.cost_ms = Some(total);
    d.cap = if dominates { Severity::Medium } else { Severity::Low };
    d.title = match execution {
        Some(e) => {
            format!("JIT compilation took {} of the {} this query ran", format::duration(total), format::duration(e))
        }
        None => format!("JIT compilation took {}", format::duration(total)),
    };
    d.detail = format!(
        "PostgreSQL compiled parts of this query to machine code because its estimated cost crossed jit_above_cost. {}",
        if dominates {
            "Compiling took longer than everything else the query did, so it can't have paid for itself here."
        } else {
            "Compiled code evaluates each row faster, so on a long run it can pay for itself; only a run without it tells."
        }
    );
    d.fix = if dominates {
        "Check with JIT off in your session; if it's faster, turn JIT off for this workload's role, or raise jit_above_cost so only long analytical queries use it.".into()
    } else {
        "Compare with JIT off in your session before changing anything.".into()
    };
    d.fix_sql = Some(if dominates {
        "-- In your session, then EXPLAIN ANALYZE again and compare:\nSET jit = off;\n-- If it's faster, for the application's role:\n-- ALTER ROLE app_user SET jit = off;".into()
    } else {
        "-- In your session only, then EXPLAIN ANALYZE again and compare:\nSET jit = off;".into()
    });
    d.verify = "Explain analyze again: no JIT section, and a shorter execution time.".into();
    vec![d]
}

fn correlated_subquery(cx: &Cx) -> Vec<Draft> {
    let mut out = Vec::new();
    for n in &cx.nodes {
        let Some(parent) = cx.parent(n) else { continue };
        // NOT IN explains its own subquery and has the better fix.
        if not_in_work(cx, parent).is_some() {
            continue;
        }
        let (runs, outer, impact) = match cx.engine() {
            Engine::Postgres => {
                if n.props.get("Parent Relationship").map(String::as_str) != Some("SubPlan") {
                    continue;
                }
                // A hashed SubPlan runs once and is probed per row: `(hashed SubPlan 1)` in the parent's condition.
                let name = n.props.get("Subplan Name").map_or("subplan".to_string(), |s| s.to_ascii_lowercase());
                let parent_text =
                    parent.props.values().chain(parent.predicate.iter()).cloned().collect::<Vec<_>>().join(" ");
                if parent_text.to_ascii_lowercase().contains(&format!("hashed {name}")) {
                    continue;
                }
                // Measured runs; else once for each row the parent evaluates the condition on.
                let runs = n.act_executions.unwrap_or_else(|| rows_read(parent));
                // PostgreSQL charges every run of the SubPlan to the parent's own cost.
                (runs, first_object(parent), (cx.share(parent) + cx.subtree(n)).min(1.0))
            }
            Engine::Mysql => {
                if !is_true(n.props.get("dependent")) || is_true(n.props.get("cacheable")) {
                    continue;
                }
                let runs = if n.props.contains_key("cache_state") {
                    // MariaDB's expression cache answers repeated outer values: only measured misses are runs.
                    let Some(runs) = n.act_executions else { continue };
                    runs
                } else if parent.object.is_some() {
                    // Attached to a table's condition: run for each row the table reads.
                    rows_read(parent)
                } else {
                    // In the select list: run for each row the query block returns.
                    parent.est_rows
                };
                let outer = if parent.object.is_some() {
                    parent.object.clone()
                } else {
                    parent
                        .children
                        .iter()
                        .find(|c| !matches!(c.op.as_str(), "Subquery" | "Dependent subquery"))
                        .and_then(first_object)
                };
                // MySQL costs one run of the subquery; the plan's row count says how many there are.
                let one = n.subtree_cost.max(0.0);
                let all = one * runs.max(1.0);
                let impact = if all > 0.0 { all / (cx.total - one + all) } else { 0.0 };
                (runs, outer, impact.clamp(0.0, 1.0))
            }
            Engine::Mssql | Engine::Sqlite => continue,
        };
        if runs < MANY_RUNS {
            continue;
        }
        let mut d = Draft::new("correlated-subquery", InsightArea::Query, impact, vec![n.id]);
        let outer_name = outer.as_deref().map(table_leaf).unwrap_or_else(|| "the outer query".into());
        d.title = format!("A subquery runs {} times, once per row of {outer_name}", format::count(runs));
        d.detail = format!(
            "{} the subquery for every row instead of once for the whole set{}.",
            if cx.engine() == Engine::Mysql {
                "MySQL marks it DEPENDENT and not cacheable, so it re-runs"
            } else {
                "PostgreSQL runs"
            },
            first_object(n).map(|t| format!(", reading {} each time", table_leaf(&t))).unwrap_or_default()
        );
        d.fix = "Rewrite it as a join (or LEFT JOIN … GROUP BY for counts and sums) so it runs once; if it must stay correlated, index the columns it filters on so each run is a quick lookup.".into();
        d.verify =
            "Explain again: the subquery appears once, joined, instead of as a per-row SubPlan / DEPENDENT SUBQUERY."
                .into();
        out.push(d);
    }
    out.into_iter().take(2).collect()
}

// ── rules: MySQL specific shapes ─────────────────────────────────────────────────────────────────

fn join_buffer(cx: &Cx) -> Vec<Draft> {
    let mut out = Vec::new();
    for n in cx
        .nodes
        .iter()
        .filter(|n| n.props.contains_key("using_join_buffer") && (n.predicate.is_some() || n.seek_predicate.is_some()))
    {
        let table = what(n);
        let how = n.props["using_join_buffer"].clone();
        let cond = n.predicate.clone().unwrap_or_default();
        let own = table_leaf(&table).to_ascii_lowercase();
        let alias = n.props.get("table_name").map(|a| a.to_ascii_lowercase());
        // Columns of this table in the join condition: `t`.`col` where t is this table's name or alias.
        let cols: Vec<String> = cond
            .split(" and ")
            .flat_map(|part| {
                part.split(['=', '<', '>'])
                    .map(str::trim)
                    .filter(|side| {
                        let l = side.to_ascii_lowercase();
                        l.contains(&format!("{own}`."))
                            || l.contains(&format!("{own}."))
                            || alias
                                .as_ref()
                                .is_some_and(|a| l.contains(&format!("{a}`.")) || l.contains(&format!("{a}.")))
                    })
                    .map(column_leaf)
                    .collect::<Vec<_>>()
            })
            .collect();
        let impact = cx.share(n).max(cx.subtree(n) * 0.5);
        let mut d = Draft::new("join-buffer", InsightArea::Index, impact, vec![n.id]);
        let leaf = table_leaf(&table);
        let join_cols = if cols.is_empty() { "the join column".to_string() } else { format::list(&cols) };
        // MySQL listed indexes it could use on this table, yet joins through the buffer: it judged a full read
        // cheaper, or those indexes serve another condition. Either way the plan doesn't show an index is missing.
        match n.props.get("possible_keys").filter(|k| !k.is_empty()) {
            Some(keys) => {
                d.title = format!("Joins {leaf} through a join buffer ({how}) over a full read instead of an index");
                d.detail = format!(
                    "MySQL listed {keys} as possible for {leaf}, yet reads all {} rows into a join buffer and matches them there, rather than looking up each row's matches.",
                    format::count(rows_read(n))
                );
                d.fix = format!(
                    "Check whether {keys} starts with {join_cols}: if not, an index that does lets MySQL look rows up directly; if it does, refresh {leaf}'s statistics so MySQL weighs it right."
                );
                d.fix_sql =
                    n.object.as_ref().map(|t| Engine::Mysql.dialect().refresh_statistics(&qualified(Engine::Mysql, t)));
            }
            None => {
                d.title = format!("Joins {leaf} without an index ({how} over a full read)");
                d.detail = format!(
                    "MySQL has no index to look up matching rows of {leaf} for each row it joins, so it reads all {} rows into a join buffer and matches them there.",
                    format::count(rows_read(n))
                );
                d.fix = format!("Index {leaf} on {join_cols} so MySQL can look rows up directly.");
                d.fix_sql = index_sql(Engine::Mysql, &table, &cols, &[]);
            }
        }
        d.verify =
            format!("Explain again: {} is read with ref or eq_ref access, without a join buffer.", table_leaf(&table));
        out.push(d);
    }
    out
}

fn temp_table_sort(cx: &Cx) -> Vec<Draft> {
    let server = if cx.mariadb() { "MariaDB" } else { "MySQL" };
    let in_temp =
        |n: &PlanNode| n.op == "Temporary table" || n.detail.as_deref().is_some_and(|d| d.contains("temporary table"));
    // MySQL marks the grouping step (`grouping_operation`, op Group) or the ordering step (`ordering_operation`, op
    // Sort); MariaDB wraps a "Temporary table" node, sorted when a Sort wraps that.
    let grouping = |n: &PlanNode| matches!(n.op.as_str(), "Group" | "Distinct" | "Temporary table");
    let sorted = |n: &PlanNode| {
        n.detail.as_deref().is_some_and(|d| d.contains("filesort"))
            || cx.nodes.iter().any(|p| p.kind == NodeKind::Sort && p.children.iter().any(|c| c.id == n.id))
    };
    fn grouped_below(n: &PlanNode, in_temp: &dyn Fn(&PlanNode) -> bool) -> bool {
        n.children.iter().any(|c| {
            (matches!(c.op.as_str(), "Group" | "Distinct" | "Temporary table") && in_temp(c))
                || grouped_below(c, in_temp)
        })
    }
    let mut out = Vec::new();
    for n in cx.nodes.iter().filter(|n| in_temp(n)) {
        let rows = n.children.iter().map(rows_out).fold(0.0, f64::max).max(rows_out(n));
        if rows < MANY_ROWS {
            continue;
        }
        // The temporary table's own work and the sort of it, not the reads below it, which happen either way. MySQL
        // leaves most of it uncosted, so this is often small next to the scan feeding it.
        let sort_above = cx.parent(n).filter(|p| p.kind == NodeKind::Sort).map_or(0.0, |p| cx.share(p));
        let impact = (cx.share(n) + sort_above).min(1.0);
        if grouping(n) {
            let mut d = Draft::new("temp-table-sort", InsightArea::Index, impact, vec![n.id]);
            d.title = if sorted(n) {
                format!(
                    "Groups {} rows in an internal temporary table, then sorts them (filesort)",
                    format::count(rows)
                )
            } else {
                format!("Collects {} rows in an internal temporary table", format::count(rows))
            };
            d.detail = format!(
                "The rows don't arrive in the order the GROUP BY / DISTINCT needs, so {server} builds a temporary table (on disk once it outgrows tmp_table_size) and sorts the result."
            );
            d.fix = format!(
                "An index whose leading columns match the GROUP BY lets {server} group in index order with no temporary table; ordering by an aggregate (COUNT(*) DESC) always needs a sort, so keep the grouped set small."
            );
            d.verify = "Explain again: no \"using temporary table\" on the grouping.".into();
            out.push(d);
        } else if !grouped_below(n, &in_temp) {
            // The ordering step's temporary table: the grouping below already reads an index in order, so only the
            // groups are sorted. No index changes that when the order is by an aggregate.
            let mut d = Draft::new("temp-table-sort", InsightArea::Query, impact, vec![n.id]);
            d.cap = Severity::Low;
            d.title = "Sorts the groups in a temporary table (filesort)".into();
            d.detail = format!(
                "Grouping already reads an index in order; {server} then sorts the grouped result, because the query orders by an aggregate or by columns the index doesn't provide."
            );
            d.fix = "Ordering by an aggregate (COUNT(*) DESC) always needs this sort: keep the grouped set small with a selective WHERE, or add a LIMIT so only the top groups are kept.".into();
            d.verify = "Explain again: the sort's input is the groups you expect, not every row.".into();
            out.push(d);
        }
    }
    out
}

fn index_ignored(cx: &Cx) -> Vec<Draft> {
    if cx.engine() != Engine::Mysql {
        return Vec::new();
    }
    let mut out = Vec::new();
    for n in cx.nodes.iter().filter(|n| n.op == "Table scan" && index_could_pay(cx, n)) {
        let keys = possible_keys_for_filter(cx, n);
        if keys.is_empty() {
            continue;
        }
        let kept = filter_keeps(cx, n).unwrap_or(1.0);
        let mut d = Draft::new("index-ignored", InsightArea::Statistics, cx.share(n), vec![n.id]);
        d.cap = Severity::Medium;
        d.title = format!("MySQL could use {} but reads all of {} instead", keys.join(", "), table_leaf(&what(n)));
        d.detail = format!(
            "{} {} of {} rows, yet MySQL judged {} not worth it; that usually means stale statistics or a type mismatch in the comparison.",
            if n.act_rows.is_some() { "The filter kept" } else { "MySQL expects the filter to keep" },
            format::percent(kept),
            format::count(rows_read(n)),
            if keys.len() == 1 { "the index" } else { "those indexes" }
        );
        d.fix = "Refresh the table's statistics and compare the column with a value of its own type; if it still scans, compare with FORCE INDEX to see which is really faster.".into();
        d.fix_sql = n.object.as_ref().map(|t| Engine::Mysql.dialect().refresh_statistics(&qualified(Engine::Mysql, t)));
        d.verify = "Explain again: access_type is ref or range on the index.".into();
        out.push(d);
    }
    out
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {

    #[test]
    fn names_in_scripts_cannot_break_out_of_their_quotes() {
        assert_eq!(quote(Engine::Mssql, "a]; DROP TABLE t; --"), "[a]]; DROP TABLE t; --]");
        assert_eq!(quote(Engine::Mysql, "a`; DROP TABLE t; --"), "`a``; DROP TABLE t; --`");
        assert_eq!(quote(Engine::Postgres, "a\"; DROP TABLE t; --"), "\"a\"\"; DROP TABLE t; --\"");
        assert_eq!(quote(Engine::Postgres, "orders"), "orders");
        // Names read back from plans are unescaped once, so quoting them again round-trips.
        assert_eq!(clean_identifier("[a]]b]"), "a]b");
        assert_eq!(clean_identifier("`a``b`"), "a`b");
        assert_eq!(clean_identifier("\"a\"\"b\""), "a\"b");
        assert_eq!(quote(Engine::Mssql, &clean_identifier("[a]]b]")), "[a]]b]");
    }

    use super::*;
    use crate::model::Properties;

    fn node(id: u32, op: &str, kind: NodeKind) -> PlanNode {
        PlanNode {
            id,
            op: op.into(),
            kind,
            detail: None,
            object: None,
            index: None,
            seek_predicate: None,
            predicate: None,
            est_rows: 1.0,
            est_executions: 1.0,
            act_rows: None,
            act_executions: None,
            rows_read: None,
            table_rows: None,
            subtree_cost: 1.0,
            self_cost: 1.0,
            act_time_ms: None,
            warnings: vec![],
            props: Properties::new(),
            children: vec![],
            runtime: None,
        }
    }

    fn plan(engine: Engine, root: PlanNode) -> Plan {
        let mut p = Plan {
            engine,
            kind: PlanKind::Estimated,
            statement: None,
            total_cost: 0.0,
            root,
            warnings: vec![],
            index_hints: vec![],
            props: Properties::new(),
            raw: None,
            insights: None,
        };
        fn sum(n: &mut PlanNode) -> f64 {
            let children: f64 = n.children.iter_mut().map(sum).sum();
            n.subtree_cost = n.self_cost + children;
            n.subtree_cost
        }
        p.total_cost = sum(&mut p.root);
        p
    }

    fn rules(p: &Plan) -> Vec<String> {
        insights(p).into_iter().map(|i| i.rule).collect()
    }

    #[test]
    fn a_temporary_table_on_the_grouping_differs_from_sorting_the_groups() {
        let scan = || {
            let mut t = node(3, "Table scan", NodeKind::Scan);
            t.est_rows = 499_000.0;
            t.object = Some("shop.orders".into());
            t
        };
        let title = |p: &Plan| insights(p).into_iter().find(|i| i.rule == "temp-table-sort").map(|i| i.title);
        // MySQL: grouping reads an index in order; only the ORDER BY COUNT(*) sorts, in a temporary table.
        let mut group = node(2, "Group", NodeKind::Aggregate);
        group.est_rows = 499_000.0;
        group.children = vec![scan()];
        let mut sort = node(1, "Sort", NodeKind::Sort);
        sort.detail = Some("using filesort, using temporary table".into());
        sort.est_rows = 499_000.0;
        sort.children = vec![group.clone()];
        let t = title(&plan(Engine::Mysql, sort.clone())).expect("the sort of the groups");
        assert!(t.starts_with("Sorts the groups"), "{t}");

        // MySQL: the grouping itself needs a temporary table.
        group.detail = Some("using temporary table".into());
        sort.children = vec![group];
        sort.detail = Some("using filesort".into());
        let t = title(&plan(Engine::Mysql, sort)).expect("grouping in a temporary table");
        assert!(t.starts_with("Groups 499K rows"), "{t}");

        // MariaDB: Sort → Temporary table → table.
        let mut temp = node(2, "Temporary table", NodeKind::Other);
        temp.est_rows = 499_000.0;
        temp.children = vec![scan()];
        let mut sort = node(1, "Sort", NodeKind::Sort);
        sort.est_rows = 499_000.0;
        sort.children = vec![temp];
        let mut p = plan(Engine::Mysql, sort);
        p.props.insert("flavor".into(), "MariaDB".into());
        let found = insights(&p).into_iter().find(|i| i.rule == "temp-table-sort").expect("MariaDB's shape");
        assert!(found.title.contains("then sorts them"), "{}", found.title);
        assert!(found.detail.contains("MariaDB"), "{}", found.detail);
    }

    #[test]
    fn converting_a_parameter_is_not_a_type_mismatch() {
        // SQL Server converts the parameter, not the column: the index works and estimates are fine.
        let mut scan = node(1, "Clustered Index Seek", NodeKind::Seek);
        scan.seek_predicate = Some("[db].[shop].[orders].[id]=CONVERT_IMPLICIT(bigint,[@1],0)".into());
        assert!(!rules(&plan(Engine::Mssql, scan.clone())).contains(&"implicit-conversion".to_string()));
        // `UPDATE shop.products SET price = price + 1 WHERE id = 1`, auto-parameterized.
        scan.seek_predicate = Some("[db].[shop].[products].[id]=CONVERT_IMPLICIT(int,[@2],0)".into());
        assert!(!rules(&plan(Engine::Mssql, scan.clone())).contains(&"implicit-conversion".to_string()));
        scan.seek_predicate =
            Some("[Expr1004]=CONVERT_IMPLICIT(int,[Expr1003],0) AND x=CONVERT_IMPLICIT(int,N'5',0)".into());
        assert!(!rules(&plan(Engine::Mssql, scan)).contains(&"implicit-conversion".to_string()));

        // The column is converted: that's the mismatch, named after the column even after a parameter convert.
        let mut scan = node(1, "Clustered Index Scan", NodeKind::Scan);
        scan.predicate = Some(
            "CONVERT_IMPLICIT(bigint,[@1],0)>0 AND CONVERT_IMPLICIT(nvarchar(200),[db].[shop].[customers].[email],0)=[@P1]"
                .into(),
        );
        let p = plan(Engine::Mssql, scan);
        let found =
            insights(&p).into_iter().find(|i| i.rule == "implicit-conversion").expect("the column is converted");
        assert!(found.title.contains("customers.email"), "{}", found.title);
        assert_eq!(convert_operands("CONVERT_IMPLICIT(nvarchar(36),[m].[c],0)=[@p1]"), vec!["[m].[c]"]);
    }

    #[test]
    fn conversions_the_query_asks_for_are_not_a_type_mismatch() {
        // `SELECT CONVERT(varchar(36), o.id), CONVERT(varchar(30), o.created_at, 126) … WHERE o.archived = 0`: SQL Server
        // warns about each CONVERT in the select list, though nothing is compared with a value of another type.
        let mut scan = node(1, "Clustered Index Scan", NodeKind::Scan);
        scan.predicate = Some("[db].[shop].[orders].[archived] as [o].[archived]=(0)".into());
        let mut p = plan(Engine::Mssql, scan);
        p.warnings = vec![PlanWarning::ImplicitConversion];
        p.props.insert(
            "PlanAffectingConvert".into(),
            "Cardinality Estimate: CONVERT(varchar(36),[o].[id],0)\n\
             Cardinality Estimate: CONVERT(varchar(30),[o].[created_at],126)"
                .into(),
        );
        assert!(!rules(&p).contains(&"implicit-conversion".to_string()));
        p.props.insert(
            "PlanAffectingConvert".into(),
            "Cardinality Estimate: CONVERT_IMPLICIT(varchar(64),[@P1],0)".into(),
        );
        assert!(!rules(&p).contains(&"implicit-conversion".to_string()));
    }

    #[test]
    fn a_plan_captured_before_the_column_changed_type_has_no_mismatch() {
        // Query Store keeps a plan's first XML: after `jobs.name` became nvarchar(64), a recompile with the same shape
        // still shows the varchar-era conversion.
        let mut scan = node(1, "Clustered Index Scan", NodeKind::Scan);
        scan.predicate = Some("CONVERT_IMPLICIT(nvarchar(64),[db].[shop].[jobs].[name],0)=[@p1]".into());
        // A scan of 200K rows: enough work for the mismatch to matter.
        scan.rows_read = Some(200_000.0);
        let mut p = plan(Engine::Mssql, scan);
        p.props.insert(
            "PlanAffectingConvert".into(),
            "Seek Plan: CONVERT_IMPLICIT(nvarchar(64),[db].[shop].[jobs].[name],0)=[@p1]".into(),
        );
        let column = |data_type: &str| ColumnInfo {
            schema: "shop".into(),
            table: "jobs".into(),
            name: "name".into(),
            data_type: data_type.into(),
            nullable: false,
            default: None,
            generated: false,
            ordinal: 2,
        };
        let mut still_varchar = p.clone();
        mark_changed_columns(&mut still_varchar, &[column("varchar(64)")]);
        let found =
            insights(&still_varchar).into_iter().find(|i| i.rule == "implicit-conversion").expect("still varchar");
        assert!(found.severity >= Severity::Medium, "{:?}", found.severity);
        assert!(found.fix.contains("change jobs.name to nvarchar(64)"), "{}", found.fix);

        mark_changed_columns(&mut p, &[column("nvarchar(64)")]);
        assert_eq!(p.props.get(CHANGED_SINCE_CAPTURE).map(String::as_str), Some("jobs.name nvarchar(64)"));
        assert!(!rules(&p).contains(&"implicit-conversion".to_string()));
    }

    #[test]
    fn only_an_implicitly_converted_column_is_a_mismatch() {
        assert!(converts_column("CONVERT_IMPLICIT(nvarchar(255),[db].[shop].[jobs].[run_key],0)=[@par4]"));
        assert!(converts_column("Seek Plan: CONVERT_IMPLICIT(nvarchar(36),[m].[CustomerId],0)=[@p1]"));
        assert!(converts_column("CONVERT_IMPLICIT(nvarchar(20),[status],0)"));
        // Written by the query, or of a value that isn't a column.
        assert!(!converts_column("Cardinality Estimate: CONVERT(varchar(36),[o].[id],0)"));
        assert!(!converts_column("CONVERT_IMPLICIT(varchar(36),[Union1006],0)"));
        assert!(!converts_column("Cardinality Estimate: CONVERT_IMPLICIT(varchar(8),[Expr1002],0)"));
        assert!(!converts_column("CONVERT_IMPLICIT(varchar(64),[@par2],0)"));
        assert!(!converts_column("x=CONVERT_IMPLICIT(int,N'5',0)"));
        assert!(!converts_column("no conversion here"));
    }

    #[test]
    fn identifiers_skip_literals_types_and_parameters() {
        assert_eq!(identifiers("[db].[shop].[orders].[status]=[@P1]"), vec!["status"]);
        assert_eq!(
            identifiers("(date_trunc('day'::text, orders.created_at) = '2026-03-01'::timestamp)"),
            vec!["created_at"]
        );
        assert_eq!(identifiers("(`shop`.`o`.`promo_code` = 'SPRING3')"), vec!["promo_code"]);
        assert_eq!(filter_columns("(status = 'paid') AND (created_at > now())"), vec!["status", "created_at"]);
    }

    #[test]
    fn wrapped_columns_are_found_on_every_engine() {
        assert_eq!(
            wrapped_column("datepart(year,[db].[shop].[orders].[created_at])=(2026)", Engine::Mssql),
            Some(("DATEPART".into(), "created_at".into()))
        );
        assert_eq!(
            wrapped_column("(date_trunc('day'::text, orders.created_at) = '2026-03-01')", Engine::Postgres),
            Some(("DATE_TRUNC".into(), "created_at".into()))
        );
        assert_eq!(
            wrapped_column("(cast(`shop`.`orders`.`created_at` as date) = '2026-03-01')", Engine::Mysql),
            Some(("CAST".into(), "created_at".into()))
        );
        assert_eq!(
            wrapped_column("((o.total)::integer = 5)", Engine::Postgres),
            Some(("::integer".into(), "total".into()))
        );
        // varchar → text casts are how Postgres prints varchar comparisons; the index still works.
        assert_eq!(wrapped_column("((c.email)::text = 'x'::text)", Engine::Postgres), None);
        // A conversion of the parameter, not the column.
        assert_eq!(wrapped_column("[db].[shop].[orders].[status]=CONVERT(varchar(20),[@P1],0)", Engine::Mssql), None);
        // SQL Server seeks through a cast of a datetime column to date (a dynamic seek): nothing blocks the index.
        let to_date = "CONVERT(date,[db].[shop].[orders].[created_at],0)=CONVERT_IMPLICIT(date,[@1],0)";
        assert_eq!(wrapped_column(to_date, Engine::Mssql), None);
        assert!(
            wrapped_column("CONVERT(varchar(10),[db].[shop].[orders].[created_at],120)='x'", Engine::Mssql).is_some()
        );
    }

    #[test]
    fn lookups_need_volume_and_name_the_covering_index() {
        let mut seek = node(1, "Index Seek", NodeKind::Seek);
        seek.object = Some("shop.orders".into());
        seek.index = Some("ix_orders_status".into());
        seek.seek_predicate = Some("[db].[shop].[orders].[status] = 'pending'".into());
        let mut lookup = node(2, "Key Lookup", NodeKind::Lookup);
        lookup.object = Some("shop.orders".into());
        lookup.est_executions = 20_000.0;
        lookup.self_cost = 30.0;
        lookup.props.insert("OutputList".into(), "[db].[shop].[orders].[total], [db].[shop].[orders].[channel]".into());
        let mut join = node(0, "Nested Loops", NodeKind::Join);
        join.children = vec![seek, lookup];
        let p = plan(Engine::Mssql, join);
        let found = insights(&p);
        let i = found.iter().find(|i| i.rule == "key-lookups").expect("key lookups");
        assert_eq!(i.nodes[0], 2);
        assert!(i.title.contains("20K rows"), "{}", i.title);
        // Rebuilding the index from the seek's columns would drop its descending keys, included columns, uniqueness,
        // filter and compression: the fix names the index and the columns, and leaves the statement to the catalog.
        assert_eq!(i.fix_sql, None);
        assert!(i.fix.contains("total and channel to ix_orders_status"), "{}", i.fix);
        assert!(!rules(&p).contains(&"nested-loops".to_string()), "lookups aren't double-reported as loops");

        // A thousand lookups at a sliver of the plan stay Low: no floor lifts them.
        let mut small = p.clone();
        small.root.children[1].est_executions = 1_000.0;
        small.root.children[1].self_cost = 0.5;
        small.root.children[0].self_cost = 100.0;
        let small = plan(Engine::Mssql, small.root);
        let i = insights(&small).into_iter().find(|i| i.rule == "key-lookups").expect("still said");
        assert_eq!(i.severity, Severity::Low);
    }

    #[test]
    fn spills_and_misestimates_explain_each_other() {
        let mut scan = node(2, "Seq Scan", NodeKind::Scan);
        scan.object = Some("public.customers".into());
        scan.predicate = Some("((region = 'eu') AND (segment = 'retail'))".into());
        scan.est_rows = 248.0;
        scan.act_rows = Some(49_997.0);
        scan.warnings = vec![PlanWarning::Misestimate];
        let mut sort = node(1, "Sort", NodeKind::Sort);
        sort.act_rows = Some(49_997.0);
        sort.warnings = vec![PlanWarning::Spill];
        sort.props.insert("Sort Space Type".into(), "Disk".into());
        sort.props.insert("Sort Space Used".into(), "5120".into());
        sort.children = vec![scan];
        let mut p = plan(Engine::Postgres, sort);
        p.kind = PlanKind::Actual;
        let found = insights(&p);
        let spill = found.iter().find(|i| i.rule == "spill").unwrap();
        assert!(spill.title.contains("5.0 MB"), "{}", spill.title);
        assert!(spill.detail.contains("underestimated"));
        let mis = found.iter().find(|i| i.rule == "misestimate").unwrap();
        assert!(mis.title.starts_with("Expected 248 rows from customers, got 50K"), "{}", mis.title);
        assert!(
            mis.fix_sql.as_deref().unwrap().contains("CREATE STATISTICS"),
            "two filtered columns → extended statistics"
        );
    }

    #[test]
    fn coverage_is_said_plainly_per_engine() {
        let mut p = plan(Engine::Mysql, node(0, "Table scan", NodeKind::Scan));
        assert!(coverage_note(&p).unwrap().contains("EXPLAIN ANALYZE adds actual rows"));
        p.props.insert("server_version".into(), "5.7.44-log".into());
        assert!(coverage_note(&p).unwrap().contains("can't measure a run"), "5.7 has no EXPLAIN ANALYZE");
        p.props.insert("server_version".into(), "8.0.36".into());
        assert!(coverage_note(&p).unwrap().contains("EXPLAIN ANALYZE adds actual rows"));
        let mut actual = plan(Engine::Mssql, node(0, "Sort", NodeKind::Sort));
        actual.kind = PlanKind::Actual;
        assert!(coverage_note(&actual).is_none());
    }

    #[test]
    fn severity_follows_impact_within_rule_limits() {
        let scan = |rows: f64, cost: f64| {
            let mut n = node(0, "Seq Scan", NodeKind::Scan);
            n.est_rows = rows;
            n.self_cost = cost;
            plan(Engine::Postgres, n)
        };
        let material = scan(50_000.0, 5_000.0);
        let cx = Cx::new(&material);
        let mut d = Draft::new("legacy-ce", InsightArea::Server, 0.9, vec![0]);
        d.cap = Severity::Low;
        assert_eq!(d.finish(&cx).severity, Severity::Low);
        let floored = || {
            let mut d = Draft::new("spill", InsightArea::Memory, 0.01, vec![0]);
            d.floor = Severity::Medium;
            d
        };
        assert_eq!(floored().finish(&cx).severity, Severity::Medium);
        assert_eq!(Draft::new("scan-filter", InsightArea::Index, 0.5, vec![0]).finish(&cx).severity, Severity::High);

        // The materiality gate comes before any floor: a cheap plan, or operators handling few rows, stay Low.
        let cheap = scan(50_000.0, 40.0);
        assert_eq!(floored().finish(&Cx::new(&cheap)).severity, Severity::Low);
        let few = scan(800.0, 5_000.0);
        assert_eq!(floored().finish(&Cx::new(&few)).severity, Severity::Low);
        // A measured run decides over the estimate: 3 ms is cheap whatever the plan expected.
        let mut fast = scan(50_000.0, 5_000.0);
        fast.props.insert("ElapsedMs".into(), "3".into());
        assert_eq!(floored().finish(&Cx::new(&fast)).severity, Severity::Low);
        // A problem measured in its own time is material by that time.
        let mut d = floored();
        d.cost_ms = Some(400.0);
        assert_eq!(d.finish(&Cx::new(&few)).severity, Severity::Medium);
        // About the whole plan: the busiest operator below counts.
        let mut root = node(0, "Aggregate", NodeKind::Aggregate);
        let mut child = node(1, "Seq Scan", NodeKind::Scan);
        child.est_rows = 80_000.0;
        child.self_cost = 5_000.0;
        root.children = vec![child];
        let whole = plan(Engine::Postgres, root);
        let mut d = floored();
        d.nodes = vec![0];
        assert_eq!(d.finish(&Cx::new(&whole)).severity, Severity::Medium, "the scan is the aggregate's input");
    }

    /// An actual plan of `root`, timed at `ms`, with costs summed bottom-up.
    fn actual(engine: Engine, root: PlanNode, ms: f64) -> Plan {
        let mut p = plan(engine, root);
        p.kind = PlanKind::Actual;
        p.props.insert("ElapsedMs".into(), ms.to_string());
        p
    }

    fn measured(id: u32, op: &str, kind: NodeKind, est: f64, act: f64, runs: f64) -> PlanNode {
        let mut n = node(id, op, kind);
        n.est_rows = est;
        n.act_rows = Some(act);
        n.act_executions = Some(runs);
        n.est_executions = runs.max(1.0);
        n
    }

    fn found(p: &Plan) -> Vec<u32> {
        p.root.misestimates().into_iter().map(|(id, _)| id).collect()
    }

    #[test]
    fn misestimates_compare_per_run_and_ignore_operators_stopped_early() {
        // Under a Limit, PostgreSQL's estimate is for a full run: 20 rows of an expected 5M is the Limit, not a
        // misestimate.
        let mut limit = measured(0, "Limit", NodeKind::Other, 20.0, 20.0, 1.0);
        limit.children = vec![measured(1, "Seq Scan", NodeKind::Scan, 5_000_000.0, 20.0, 1.0)];
        assert!(found(&plan(Engine::Postgres, limit.clone())).is_empty());
        // A sort under it hands over only what the Limit takes, but reads all its input: that count is real.
        let mut sort = measured(1, "Sort", NodeKind::Sort, 5_000_000.0, 20.0, 1.0);
        sort.children = vec![measured(2, "Seq Scan", NodeKind::Scan, 5_000_000.0, 3_000.0, 1.0)];
        limit.children = vec![sort];
        assert_eq!(found(&plan(Engine::Postgres, limit)), vec![2]);
        // More rows than expected is evidence anywhere.
        let mut top = measured(0, "Top", NodeKind::Other, 20.0, 20.0, 1.0);
        top.children = vec![measured(1, "Index Seek", NodeKind::Seek, 10.0, 5_000.0, 1.0)];
        assert_eq!(found(&plan(Engine::Mssql, top)), vec![1]);

        // The inner side of a semi or anti join stops at its first match; merge join inputs stop when the other ends.
        let mut semi = measured(0, "Nested Loop", NodeKind::Join, 900.0, 900.0, 1.0);
        semi.detail = Some("Anti join".into());
        semi.children = vec![
            measured(1, "Seq Scan", NodeKind::Scan, 1_000.0, 1_000.0, 1.0),
            measured(2, "Index Scan", NodeKind::Seek, 400.0, 1_000.0, 1_000.0),
        ];
        assert!(found(&plan(Engine::Postgres, semi)).is_empty());
        let mut merge = measured(0, "Merge Join", NodeKind::Join, 50.0, 50.0, 1.0);
        merge.children = vec![
            measured(1, "Index Scan", NodeKind::Seek, 2_000_000.0, 60.0, 1.0),
            measured(2, "Index Scan", NodeKind::Seek, 50.0, 50.0, 1.0),
        ];
        assert!(found(&plan(Engine::Postgres, merge)).is_empty());

        // An operator that never ran has nothing to compare.
        let never = measured(0, "Index Scan", NodeKind::Seek, 5_000.0, 0.0, 0.0);
        assert!(found(&plan(Engine::Postgres, never)).is_empty());

        // Nested loop: the outer side was misjudged (100 expected, 10K came); the inner seek returned the one row per
        // run it expected, so only the outer side is a misestimate.
        let mut nl = measured(0, "Nested Loops", NodeKind::Join, 100.0, 10_000.0, 1.0);
        let mut inner = measured(2, "Clustered Index Seek", NodeKind::Seek, 1.0, 10_000.0, 10_000.0);
        inner.est_executions = 100.0;
        nl.children = vec![measured(1, "Index Seek", NodeKind::Seek, 100.0, 10_000.0, 1.0), inner];
        let p = plan(Engine::Mssql, nl);
        assert_eq!(found(&p), vec![0, 1]);
        let mis = insights(&p).into_iter().filter(|i| i.rule == "misestimate").collect::<Vec<_>>();
        assert_eq!(mis.len(), 1, "{mis:?}");
        assert_eq!(mis[0].nodes, vec![1]);

        // A parallel scan runs once on each of 8 threads, which SQL Server's estimate counts as one execution.
        let mut parallel = measured(0, "Clustered Index Scan", NodeKind::Scan, 1_000_000.0, 1_000_000.0, 8.0);
        parallel.est_executions = 1.0;
        parallel.props.insert(crate::plan::PARALLEL_THREADS.into(), "8".into());
        assert_eq!(parallel.runs(), 1.0);
        assert!(found(&plan(Engine::Mssql, parallel)).is_empty());

        // A hash join's bitmap pushed into the probe-side scan isn't in the scan's estimate.
        let mut probe = measured(0, "Index Scan", NodeKind::Scan, 1_000_000.0, 30_000.0, 1.0);
        probe.predicate = Some("PROBE([Bitmap1005],[db].[shop].[orders].[id],N'[IN ROW]')".into());
        assert!(found(&plan(Engine::Mssql, probe)).is_empty());
    }

    #[test]
    fn misestimate_advice_fits_the_table_and_the_server() {
        let scan = |object: &str| {
            let mut s = measured(1, "Table Scan", NodeKind::Scan, 10.0, 50_000.0, 1.0);
            s.object = Some(object.into());
            s.predicate = Some("[t].[a]=(1)".into());
            let mut join = measured(0, "Hash Match", NodeKind::Join, 10.0, 50_000.0, 1.0);
            join.children = vec![s];
            actual(Engine::Mssql, join, 400.0)
        };
        let fix = |p: &Plan| insights(p).into_iter().find(|i| i.rule == "misestimate").expect("misestimate");
        // A table variable estimated at one row is the table-variable rule's; never UPDATE STATISTICS on it.
        let mut one = scan("@t");
        one.root.children[0].est_rows = 1.0;
        assert!(!rules(&one).contains(&"misestimate".to_string()));
        assert!(rules(&one).contains(&"table-variable".to_string()));
        let tv = fix(&scan("@t"));
        assert_eq!(tv.fix_sql, None);
        assert!(tv.fix.contains("table variable"), "{}", tv.fix);
        assert_eq!(fix(&scan("#work")).fix_sql, None, "a temporary table's statistics belong to its session");
        assert!(fix(&scan("dbo.orders")).fix_sql.unwrap().starts_with("UPDATE STATISTICS [dbo].[orders]"));
        // No spill above the operator, so no "ran out of memory as a result".
        assert!(!fix(&scan("dbo.orders")).detail.contains("out of memory"));

        let mysql = |flavor: Option<&str>, version: Option<&str>| {
            let mut s = measured(1, "Table scan", NodeKind::Scan, 10.0, 50_000.0, 1.0);
            s.object = Some("shop.orders".into());
            s.predicate = Some("(shop.orders.status = 'x')".into());
            let mut join = measured(0, "Nested loop inner join", NodeKind::Join, 10.0, 50_000.0, 1.0);
            join.children = vec![s];
            let mut p = actual(Engine::Mysql, join, 400.0);
            if let Some(f) = flavor {
                p.props.insert("flavor".into(), f.into());
            }
            if let Some(v) = version {
                p.props.insert("server_version".into(), v.into());
            }
            fix(&p).fix_sql.unwrap()
        };
        assert!(mysql(None, Some("8.0.36")).contains("UPDATE HISTOGRAM ON `status`"));
        assert_eq!(mysql(None, Some("5.7.44")), "ANALYZE TABLE `shop`.`orders`;", "no histograms before 8.0");
        assert!(mysql(Some("MariaDB"), None).contains("PERSISTENT FOR COLUMNS (`status`) INDEXES ()"));

        // A misestimate nothing above depends on changed nothing in this plan.
        let mut root = measured(0, "Filter", NodeKind::Other, 50_000.0, 0.0, 1.0);
        let mut table = measured(1, "Table scan", NodeKind::Scan, 500_000.0, 500_000.0, 1.0);
        table.object = Some("shop.orders".into());
        root.predicate = Some("(orders.promo_code = 'X')".into());
        root.children = vec![table];
        let lone = fix(&actual(Engine::Mysql, root, 80.0));
        assert_eq!(lone.severity, Severity::Low);
        assert!(lone.title.contains("from orders"), "{}", lone.title);
    }

    #[test]
    fn index_advice_from_a_scan_needs_a_scan_that_an_index_would_shrink() {
        let scan = |op: &str, kind: NodeKind, read: f64, kept: f64, filter: &str| {
            let mut n = node(1, op, kind);
            n.object = Some("dbo.orders".into());
            n.predicate = Some(filter.into());
            n.est_rows = kept;
            n.rows_read = Some(read);
            n.self_cost = 50.0;
            let mut p = plan(Engine::Mssql, n);
            p.root.warnings.push(PlanWarning::ResidualPredicate);
            p
        };
        let isnull = "isnull([db].[dbo].[orders].[deleted],(0))=(0)";
        // A seek reading 3 rows: the function blocks nothing worth having.
        assert!(rules(&scan("Index Seek", NodeKind::Seek, 3.0, 2.0, isnull)).is_empty());
        // A scan that keeps most of what it reads: an index wouldn't pay.
        assert!(rules(&scan("Clustered Index Scan", NodeKind::Scan, 2_000_000.0, 1_900_000.0, isnull)).is_empty());
        // A selective scan: now the function is what keeps the index out.
        let selective = scan("Clustered Index Scan", NodeKind::Scan, 2_000_000.0, 3_000.0, isnull);
        assert_eq!(rules(&selective), vec!["non-sargable"]);
        // SQL Server seeks through CONVERT(date, …): the scan needs an index, not a rewrite.
        let to_date = "CONVERT(date,[db].[dbo].[orders].[created_at],0)='2026-03-01'";
        assert_eq!(
            rules(&scan("Clustered Index Scan", NodeKind::Scan, 2_000_000.0, 3_000.0, to_date)),
            vec!["scan-filter"]
        );
        // A leading wildcard on a seek, or on a scan that keeps everything, isn't the problem.
        let like = "[db].[dbo].[orders].[email] like N'%@example.com'";
        assert!(rules(&scan("Index Seek", NodeKind::Seek, 2_000_000.0, 3_000.0, like)).is_empty());
        assert!(rules(&scan("Clustered Index Scan", NodeKind::Scan, 200_000.0, 200_000.0, like)).is_empty());
        let wildcard = insights(&scan("Clustered Index Scan", NodeKind::Scan, 2_000_000.0, 3_000.0, like));
        assert_eq!(wildcard.iter().map(|i| i.rule.as_str()).collect::<Vec<_>>(), vec!["leading-wildcard"]);
        assert!(
            wildcard[0].detail.contains("2,000,000 rows") || wildcard[0].detail.contains("2.0M"),
            "{}",
            wildcard[0].detail
        );
        // A function and a wildcard on one scan: one insight for it.
        let both = format!("upper([db].[dbo].[orders].[email]) like N'%X%' AND {isnull}");
        assert_eq!(
            rules(&scan("Clustered Index Scan", NodeKind::Scan, 2_000_000.0, 3_000.0, &both)),
            vec!["leading-wildcard"]
        );
        // A columnstore is read by segments, not seeks.
        let mut columnstore =
            scan("Clustered Index Scan", NodeKind::Scan, 2_000_000.0, 3_000.0, "[db].[dbo].[orders].[status]=(3)");
        assert_eq!(rules(&columnstore), vec!["scan-filter"]);
        columnstore.root.props.insert("Storage".into(), "ColumnStore".into());
        assert!(rules(&columnstore).is_empty());
        // Said as the plan shows it, not inferred: no claim about which indexes exist.
        let filtered =
            scan("Clustered Index Scan", NodeKind::Scan, 2_000_000.0, 3_000.0, "[db].[dbo].[orders].[status]=(3)");
        let i = insights(&filtered).into_iter().next().unwrap();
        assert!(!i.detail.contains("no index starts with"), "{}", i.detail);
    }

    #[test]
    fn sqlite_says_when_it_builds_an_index_itself() {
        let mut auto = node(7, "Automatic Index", NodeKind::Other);
        auto.object = Some("visits".into());
        auto.props.insert("Automatic Index".into(), "visits(event_id)".into());
        let p = plan(Engine::Sqlite, auto);
        let found: Vec<PlanInsight> = insights(&p).into_iter().filter(|i| i.rule == "automatic-index").collect();
        assert_eq!(found.len(), 1, "{:?}", rules(&p));
        assert!(
            found[0].fix_sql.as_deref().unwrap_or("").starts_with("CREATE INDEX IF NOT EXISTS \"ix_visits_event_id\""),
            "{:?}",
            found[0].fix_sql
        );
        // No rows, no cost: nothing says how much it costs, so it stays Low.
        assert_eq!(found[0].severity, Severity::Low);
    }

    #[test]
    fn mysql_guesses_are_not_evidence() {
        assert!(
            mysql_guessed_filtered(10.0, 1) && mysql_guessed_filtered(11.11, 1) && mysql_guessed_filtered(33.33, 1)
        );
        assert!(mysql_guessed_filtered(1.0, 2) && mysql_guessed_filtered(3.33, 2) && mysql_guessed_filtered(1.23, 2));
        assert!(!mysql_guessed_filtered(1.0, 1), "one condition can't be two guesses multiplied");
        assert!(!mysql_guessed_filtered(0.5, 2) && !mysql_guessed_filtered(4.2, 1));
        let scan = |filtered: &str, possible: Option<&str>| {
            let mut n = node(1, "Table scan", NodeKind::Scan);
            n.object = Some("shop.orders".into());
            n.predicate = Some("(shop.o.promo_code = 'SPRING3')".into());
            n.rows_read = Some(498_573.0);
            n.est_rows = 498_573.0 * filtered.parse::<f64>().unwrap() / 100.0;
            n.self_cost = 50_000.0;
            n.props.insert("filtered".into(), filtered.into());
            if let Some(k) = possible {
                n.props.insert("possible_keys".into(), k.into());
            }
            n
        };
        // 10.00 is MySQL's guess for an equality it has no statistics on: nothing to advise from.
        assert!(rules(&plan(Engine::Mysql, scan("10.00", None))).is_empty());
        let measured_share = rules(&plan(Engine::Mysql, scan("0.50", None)));
        assert_eq!(measured_share, vec!["scan-filter"]);
        // A possible key on the filtered column: MySQL ignored an index; that's index-ignored's, not scan-filter's.
        assert_eq!(rules(&plan(Engine::Mysql, scan("0.50", Some("ix_orders_promo_code")))), vec!["index-ignored"]);
        // In a join, a possible key on the join column can't serve the filter.
        let mut join = node(0, "Nested loop", NodeKind::Join);
        let mut other = node(2, "Unique index lookup", NodeKind::Seek);
        other.object = Some("shop.customers".into());
        join.children = vec![scan("0.50", Some("fk_orders_customer")), other];
        let joined = rules(&plan(Engine::Mysql, join.clone()));
        assert!(!joined.contains(&"index-ignored".to_string()), "{joined:?}");
        assert!(joined.contains(&"scan-filter".to_string()), "{joined:?}");
        join.children[0] = scan("0.50", Some("fk_orders_customer, ix_orders_promo_code"));
        assert!(rules(&plan(Engine::Mysql, join.clone())).contains(&"index-ignored".to_string()));
        // With the catalog, a key counts by the column it leads with, whatever its name.
        let index = |name: &str, columns: &[&str]| {
            serde_json::from_value::<IndexStat>(serde_json::json!({
                "schema": "shop", "table": "orders", "name": name, "kind": "BTREE",
                "columns": columns, "include": [], "filter": null, "is_unique": false, "is_primary_key": false,
                "is_clustered": false, "is_disabled": false, "size_bytes": null, "reads": null, "writes": null,
                "seeks": null, "scans": null, "lookups": null, "last_read": null, "definition": null
            }))
            .unwrap()
        };
        let marked = |keys: &str, catalog: &[IndexStat]| {
            let mut j = join.clone();
            j.children[0] = scan("0.50", Some(keys));
            let mut p = plan(Engine::Mysql, j);
            mark_possible_keys(&mut p, catalog);
            rules(&p)
        };
        let promo = [index("idx_a", &["promo_code", "created_at"]), index("fk_orders_customer", &["customer_id"])];
        assert!(marked("fk_orders_customer, idx_a", &promo).contains(&"index-ignored".to_string()));
        // Named for promo_code, but it leads with customer_id: it can't serve a filter on promo_code.
        let misnamed = [index("ix_orders_promo_code", &["customer_id", "promo_code"])];
        let unusable = marked("ix_orders_promo_code", &misnamed);
        assert!(!unusable.contains(&"index-ignored".to_string()) && unusable.contains(&"scan-filter".to_string()));
        // Above a tenth kept, an index wouldn't pay either way.
        assert!(rules(&plan(Engine::Mysql, scan("15.00", Some("ix_orders_promo_code")))).is_empty());
    }

    #[test]
    fn nested_loops_speak_only_when_the_loop_was_not_the_right_choice() {
        let nl = |outer: PlanNode, inner: PlanNode| {
            let mut n = measured(0, "Nested Loop", NodeKind::Join, 400_000.0, 400_000.0, 1.0);
            n.children = vec![outer, inner];
            actual(Engine::Postgres, n, 300.0)
        };
        let outer = |est: f64| {
            let mut o = measured(1, "Seq Scan", NodeKind::Scan, est, 400_000.0, 1.0);
            o.object = Some("public.orders".into());
            o.self_cost = 100.0;
            o
        };
        let seek = || {
            let mut s = measured(2, "Index Scan", NodeKind::Seek, 1.0, 400_000.0, 400_000.0);
            s.object = Some("public.customers".into());
            s.seek_predicate = Some("(c.id = o.customer_id)".into());
            s.table_rows = Some(200_000.0);
            s.self_cost = 100.0;
            s
        };
        // An accurate outer estimate and a one-row seek per run: the optimizer weighed a hash join and chose this.
        assert!(!rules(&nl(outer(390_000.0), seek())).contains(&"nested-loops".to_string()));
        // It expected 400 rows and got 400K: the loop was chosen on wrong numbers.
        let wrong = insights(&nl(outer(400.0), seek()));
        let loops = wrong.iter().find(|i| i.rule == "nested-loops").expect("misjudged outer side");
        assert!(loops.detail.contains("expected 400 rows"), "{}", loops.detail);
        // Each run rereading most of a table is a problem whatever the estimate.
        let mut rescan = measured(2, "Seq Scan", NodeKind::Scan, 1.0, 400_000.0, 20_000.0);
        rescan.object = Some("public.regions".into());
        rescan.predicate = Some("(r.code = o.region_code)".into());
        rescan.rows_read = Some(20_000.0 * 900.0);
        rescan.table_rows = Some(1_000.0);
        rescan.self_cost = 100.0;
        let mut o = outer(20_000.0);
        o.act_rows = Some(20_000.0);
        let reread = insights(&nl(o, rescan));
        let r = reread.iter().find(|i| i.rule == "nested-loops").expect("rereads");
        assert!(r.fix.contains("Index regions"), "{}", r.fix);

        // Memoize answers repeats from its cache: only the 120 misses ran the index scan.
        let mut memo = measured(2, "Memoize", NodeKind::Spool, 1.0, 400_000.0, 400_000.0);
        let mut below = seek();
        below.act_executions = Some(120.0);
        below.est_executions = 120.0;
        below.act_rows = Some(120.0);
        memo.children = vec![below];
        assert!(!rules(&nl(outer(400.0), memo)).contains(&"nested-loops".to_string()));

        // SQL Server's lazy Table Spool reads its input once and replays it: never a reread of the table.
        let spool_plan = |predicate: Option<&str>| {
            let mut n = node(0, "Nested Loops", NodeKind::Join);
            n.predicate = predicate.map(str::to_string);
            n.self_cost = 6_000.0;
            let mut o = node(1, "Index Scan", NodeKind::Scan);
            o.object = Some("shop.order_items".into());
            o.est_rows = 1_500_000.0;
            let mut spool = node(2, "Table Spool", NodeKind::Spool);
            spool.detail = Some("Lazy Spool".into());
            spool.est_rows = 999.0;
            spool.est_executions = 1_500_001.0;
            let mut agg = node(3, "Hash Match", NodeKind::Aggregate);
            agg.est_rows = 999.0;
            let mut scan = node(4, "Index Scan", NodeKind::Scan);
            scan.object = Some("shop.order_items".into());
            scan.est_rows = 1_500_000.0;
            agg.children = vec![scan];
            spool.children = vec![agg];
            n.children = vec![o, spool];
            plan(Engine::Mssql, n)
        };
        let pairs = insights(&spool_plan(Some("[Expr1003]=[Expr1004]")));
        let p = pairs.iter().find(|i| i.rule == "nested-loops").expect("every pair tested");
        assert!(!p.title.contains("Repeats its read of order_items"), "{}", p.title);
        assert!(p.title.contains("1.5B pairs"), "{}", p.title);
        assert!(!rules(&spool_plan(None)).contains(&"nested-loops".to_string()), "a seekable inner side");

        // An eager index spool is built once per run: eager-spool's, not a repeated read.
        let mut eager = node(0, "Nested Loops", NodeKind::Join);
        let mut o = node(1, "Clustered Index Scan", NodeKind::Scan);
        o.est_rows = 50_000.0;
        let mut spool = node(2, "Index Spool", NodeKind::Spool);
        spool.detail = Some("Eager Spool".into());
        spool.est_executions = 50_000.0;
        let mut feed = node(3, "Clustered Index Scan", NodeKind::Scan);
        feed.object = Some("shop.ticket_events".into());
        feed.est_rows = 2_000_000.0;
        spool.children = vec![feed];
        eager.children = vec![o, spool];
        let eager = rules(&plan(Engine::Mssql, eager));
        assert!(
            eager.contains(&"eager-spool".to_string()) && !eager.contains(&"nested-loops".to_string()),
            "{eager:?}"
        );

        // MySQL joins through an index whenever one fits: the advice is its estimate and join order, not a hash join.
        let mut mysql = measured(0, "Nested loop inner join", NodeKind::Join, 400_000.0, 400_000.0, 1.0);
        let mut c = measured(1, "Table scan", NodeKind::Scan, 400.0, 400_000.0, 1.0);
        c.object = Some("shop.orders".into());
        let mut lookup = measured(2, "Single-row index lookup", NodeKind::Seek, 1.0, 400_000.0, 400_000.0);
        lookup.object = Some("shop.customers".into());
        mysql.children = vec![c, lookup];
        let m = insights(&actual(Engine::Mysql, mysql, 300.0));
        let m = m.iter().find(|i| i.rule == "nested-loops").expect("misjudged outer side");
        assert!(!m.detail.contains("hash join") && !m.fix.contains("merge"), "{} / {}", m.detail, m.fix);
        assert!(m.fix.contains("ANALYZE TABLE"), "{}", m.fix);
    }

    #[test]
    fn spills_say_what_spilled_and_how_much() {
        // An exchange spill resolves a deadlock between parallel threads: no memory-grant advice.
        let mut exchange = node(0, "Parallelism", NodeKind::Parallelism);
        exchange.props.insert(EXCHANGE_SPILL.into(), "true".into());
        exchange.props.insert("TempdbWritesPages".into(), "800".into());
        let p = actual(Engine::Mssql, exchange, 500.0);
        let i = insights(&p).into_iter().find(|i| i.rule == "spill").expect("exchange spill");
        assert_eq!(i.area, InsightArea::Parallelism);
        assert!(!i.fix.contains("grant") && !i.detail.contains("granted"), "{} / {}", i.detail, i.fix);

        // MariaDB's sort merge passes get MariaDB's words.
        let mut sort = node(0, "Sort", NodeKind::Sort);
        sort.warnings = vec![PlanWarning::Spill];
        sort.props.insert("r_sort_passes".into(), "3".into());
        let mut p = actual(Engine::Mysql, sort, 500.0);
        p.props.insert("flavor".into(), "MariaDB".into());
        let i = insights(&p).into_iter().find(|i| i.rule == "spill").expect("sort passes");
        assert!(i.title.contains("3 merge passes") && i.fix.contains("sort_buffer_size"), "{}", i.title);
        assert!(!i.title.contains("tempdb") && !i.fix.contains("memory grant"), "{}", i.fix);

        // PostgreSQL's hash aggregate: its own disk use, not the temp blocks its inputs wrote.
        let mut agg = node(0, "Aggregate", NodeKind::Aggregate);
        agg.warnings = vec![PlanWarning::Spill];
        agg.props.insert("HashAgg Batches".into(), "5".into());
        agg.props.insert("Disk Usage".into(), "7600".into());
        agg.props.insert("Temp Written Blocks".into(), "18432".into());
        let i =
            insights(&actual(Engine::Postgres, agg.clone(), 500.0)).into_iter().find(|i| i.rule == "spill").unwrap();
        assert!(i.title.contains("7.4 MB"), "{}", i.title);
        // Without BUFFERS or Disk Usage, no made-up size.
        agg.props.remove("Disk Usage");
        agg.props.remove("Temp Written Blocks");
        let i = insights(&actual(Engine::Postgres, agg, 500.0)).into_iter().find(|i| i.rule == "spill").unwrap();
        assert!(i.title.contains("5 batches") && !i.title.contains(" 0 B"), "{}", i.title);

        // A 64 KB spill is small next to the work around it.
        let mut small = node(0, "Sort", NodeKind::Sort);
        small.warnings = vec![PlanWarning::Spill];
        small.act_rows = Some(80_000.0);
        small.props.insert("TempdbWritesPages".into(), "8".into());
        let i = insights(&actual(Engine::Mssql, small, 500.0)).into_iter().find(|i| i.rule == "spill").unwrap();
        assert_eq!(i.severity, Severity::Low);
    }

    #[test]
    fn not_in_is_only_row_by_row_probing() {
        let pg = |filter: &str| {
            let mut scan = node(1, "Seq Scan", NodeKind::Scan);
            scan.object = Some("public.customers".into());
            scan.predicate = Some(filter.into());
            scan.est_rows = 100_000.0;
            scan.self_cost = 1_000_000.0;
            let mut sub = node(2, "Seq Scan", NodeKind::Scan);
            sub.props.insert("Parent Relationship".into(), "SubPlan".into());
            sub.props.insert("Subplan Name".into(), "SubPlan 1".into());
            sub.est_rows = 400_000.0;
            scan.children = vec![sub];
            let mut p = plan(Engine::Postgres, scan);
            p.statement = Some("SELECT * FROM customers WHERE id NOT IN (SELECT customer_id FROM orders)".into());
            rules(&p)
        };
        // One build, one hash probe per row: PostgreSQL's hashed SubPlan, before and from 17.
        assert!(pg("(NOT (hashed SubPlan 1))").is_empty());
        assert!(pg("(NOT (ANY (customers.id = (hashed SubPlan 1).col1)))").is_empty());
        // Searched again for each row: that is NOT IN's cost.
        assert_eq!(pg("(NOT (SubPlan 1))"), vec!["not-in"]);
        assert_eq!(pg("(NOT (ANY (customers.id = (SubPlan 1).col1)))"), vec!["not-in"]);

        let mysql = |condition: &str, not_exists: bool| {
            let mut t = node(1, "Table scan", NodeKind::Scan);
            t.object = Some("shop.customers".into());
            t.predicate = Some(condition.into());
            t.est_rows = 199_000.0;
            t.self_cost = 20_000.0;
            if not_exists {
                t.props.insert("not_exists".into(), "true".into());
            }
            let mut p = plan(Engine::Mysql, t);
            p.statement = Some("SELECT * FROM customers WHERE id NOT IN (SELECT customer_id FROM orders)".into());
            rules(&p)
        };
        // MySQL 8.0.17+ runs it as an antijoin.
        assert!(
            mysql("<if>(is_not_null_compl(<subquery2>), <if>(found_match(<subquery2>), false, true), true)", true)
                .is_empty()
        );
        // Materialized once and looked up per row.
        assert!(mysql("(not(<in_optimizer>(customers.id,customers.id in ( <materialize> (select orders.customer_id from orders), <primary_index_lookup>(customers.id in <temporary table> on <auto_key>)))))", false).is_empty());
        // The subquery run for each row.
        assert_eq!(
            mysql(
                "(not(<in_optimizer>(customers.id,<exists>(<index_lookup>(<cache>(customers.id) in orders on fk_orders_customer)))))",
                false
            ),
            vec!["not-in"]
        );
    }

    #[test]
    fn subqueries_count_the_runs_they_really_make() {
        // A hashed SubPlan runs once, however many rows probe it.
        let mut scan = node(1, "Seq Scan", NodeKind::Scan);
        scan.object = Some("public.orders".into());
        scan.est_rows = 500_000.0;
        scan.self_cost = 10_000.0;
        scan.predicate = Some("(ANY (customer_id = (hashed SubPlan 1).col1))".into());
        let mut sub = node(2, "Seq Scan", NodeKind::Scan);
        sub.object = Some("public.customers".into());
        sub.props.insert("Parent Relationship".into(), "SubPlan".into());
        sub.props.insert("Subplan Name".into(), "SubPlan 1".into());
        scan.children = vec![sub];
        let mut p = plan(Engine::Postgres, scan);
        assert!(!rules(&p).contains(&"correlated-subquery".to_string()));
        p.root.predicate = Some("(total > (SubPlan 1))".into());
        assert!(rules(&p).contains(&"correlated-subquery".to_string()));

        // MySQL: a subquery attached to a table's condition runs once per row of that table, not of a sibling.
        let mut table = node(1, "Table scan", NodeKind::Scan);
        table.object = Some("shop.customers".into());
        table.rows_read = Some(199_000.0);
        table.est_rows = 199_000.0;
        let dependent = |id: u32, rows: f64| {
            let mut d = node(id, "Dependent subquery", NodeKind::Other);
            d.props.insert("dependent".into(), "true".into());
            d.props.insert("cacheable".into(), "false".into());
            d.est_rows = rows;
            d.self_cost = 50.0;
            d
        };
        table.children = vec![dependent(2, 3.0), dependent(3, 5.0)];
        let found = insights(&plan(Engine::Mysql, table));
        let titles: Vec<&str> =
            found.iter().filter(|i| i.rule == "correlated-subquery").map(|i| i.title.as_str()).collect();
        assert!(
            !titles.is_empty() && titles.iter().all(|t| t.contains("199K times, once per row of customers")),
            "{titles:?}"
        );
    }

    #[test]
    fn smaller_rules_hold_for_this_plan() {
        // A Gather that never ran started no workers because it never asked for any.
        let mut gather = node(0, "Gather", NodeKind::Parallelism);
        gather.act_executions = Some(0.0);
        gather.act_rows = Some(0.0);
        gather.props.insert("Workers Planned".into(), "2".into());
        gather.props.insert("Workers Launched".into(), "0".into());
        assert!(!rules(&actual(Engine::Postgres, gather.clone(), 50.0)).contains(&"workers-short".to_string()));
        gather.act_executions = Some(1.0);
        assert!(rules(&actual(Engine::Postgres, gather, 50.0)).contains(&"workers-short".to_string()));

        // A BRIN bitmap is lossy by design.
        let mut heap = node(0, "Bitmap Heap Scan", NodeKind::Lookup);
        heap.props.insert("Lossy Heap Blocks".into(), "900".into());
        heap.props.insert("Exact Heap Blocks".into(), "0".into());
        assert!(!rules(&actual(Engine::Postgres, heap.clone(), 50.0)).contains(&"lossy-bitmap".to_string()));
        heap.props.insert("Exact Heap Blocks".into(), "40".into());
        assert!(rules(&actual(Engine::Postgres, heap, 50.0)).contains(&"lossy-bitmap".to_string()));

        // JIT that took most of the run can't have paid off; a fifth of it may have.
        let jit = |jit_ms: f64, run_ms: f64| {
            let mut p = actual(Engine::Postgres, node(0, "Seq Scan", NodeKind::Scan), run_ms);
            p.props.insert("Execution Time".into(), run_ms.to_string());
            p.props.insert("JIT".into(), format!("{{\"Timing\": {{\"Total\": {jit_ms}}}}}"));
            insights(&p).into_iter().find(|i| i.rule == "jit").expect("jit")
        };
        let most = jit(300.0, 400.0);
        assert_eq!(most.severity, Severity::Medium);
        assert!(most.fix_sql.as_deref().unwrap().starts_with("-- In your session"), "{:?}", most.fix_sql);
        let some = jit(100.0, 400.0);
        assert_eq!(some.severity, Severity::Low);
        assert!(!some.fix_sql.as_deref().unwrap().contains("ALTER ROLE"), "{:?}", some.fix_sql);

        // PostgreSQL's planning on Culprit's fresh connection includes loading table metadata.
        let mut p = actual(Engine::Postgres, node(0, "Seq Scan", NodeKind::Scan), 3.0);
        p.props.insert("Planning Time".into(), "30".into());
        p.props.insert("Execution Time".into(), "3".into());
        let compile = insights(&p).into_iter().find(|i| i.rule == "slow-compile").expect("slow compile");
        assert_eq!(compile.severity, Severity::Low);
        assert!(compile.detail.contains("twice in one session"), "{}", compile.detail);

        // The optimizer giving up on a cheap plan costs nothing.
        let mut cheap = plan(Engine::Mssql, node(0, "Clustered Index Seek", NodeKind::Seek));
        cheap.total_cost = 0.01;
        cheap.root.subtree_cost = 0.01;
        cheap.props.insert("StatementOptmEarlyAbortReason".into(), "TimeOut".into());
        let timeout = insights(&cheap).into_iter().find(|i| i.rule == "optimizer-timeout").expect("timeout");
        assert_eq!(timeout.severity, Severity::Low);

        // Below the commonly used cost threshold of 50, running serially may be the server's own choice.
        let blocked = |cost: f64| {
            let mut root = node(0, "Hash Match", NodeKind::Join);
            root.est_rows = 500_000.0;
            root.self_cost = cost;
            let mut p = plan(Engine::Mssql, root);
            p.props.insert("NonParallelPlanReason".into(), "MaxDOPSetToOne".into());
            insights(&p).into_iter().find(|i| i.rule == "parallelism-blocked")
        };
        assert!(blocked(3.0).is_none(), "below the default threshold nothing would go parallel");
        let mid = blocked(30.0).unwrap();
        assert_eq!(mid.severity, Severity::Low);
        assert!(!mid.detail.contains("would normally"), "{}", mid.detail);
        assert_eq!(blocked(200.0).unwrap().severity, Severity::Medium);

        // A cross join to a single row (a total, a setting) multiplies nothing.
        let mut cross = node(0, "Nested Loops", NodeKind::Join);
        cross.warnings = vec![PlanWarning::NoJoinPredicate];
        cross.est_rows = 200_000.0;
        let mut big = node(1, "Clustered Index Scan", NodeKind::Scan);
        big.object = Some("shop.orders".into());
        big.est_rows = 200_000.0;
        let mut total = node(2, "Stream Aggregate", NodeKind::Aggregate);
        total.est_rows = 1.0;
        total.est_executions = 1.0;
        cross.children = vec![big, total];
        assert!(!rules(&plan(Engine::Mssql, cross)).contains(&"no-join-predicate".to_string()));

        // A remote insert sends rows; it doesn't pull them.
        let mut remote = node(0, "Remote Insert", NodeKind::Modify);
        let mut rows = node(1, "Clustered Index Scan", NodeKind::Scan);
        rows.est_rows = 5_000.0;
        remote.children = vec![rows];
        let i = insights(&plan(Engine::Mssql, remote)).into_iter().find(|i| i.rule == "remote-query").unwrap();
        assert!(i.title.starts_with("Sends 5,000 rows") || i.title.starts_with("Sends 5.0K"), "{}", i.title);

        // A table variable never has statistics: missing-statistics leaves it to table-variable.
        let mut tv = node(0, "Table Scan", NodeKind::Scan);
        tv.object = Some("@t".into());
        tv.warnings = vec![PlanWarning::MissingStatistics];
        assert!(!rules(&plan(Engine::Mssql, tv.clone())).contains(&"missing-statistics".to_string()));
        tv.object = Some("dbo.orders".into());
        let stats = insights(&plan(Engine::Mssql, tv)).into_iter().find(|i| i.rule == "missing-statistics").unwrap();
        assert!(stats.fix_sql.unwrap().starts_with("SELECT name, is_auto_create_stats_on"));

        // MySQL's join buffer where possible keys exist: no claim that there is no index.
        let mut buffered = node(0, "Table scan", NodeKind::Scan);
        buffered.object = Some("shop.items".into());
        buffered.predicate = Some("(shop.i.price = shop.o.total)".into());
        buffered.props.insert("using_join_buffer".into(), "hash join".into());
        buffered.props.insert("possible_keys".into(), "ix_items_price".into());
        buffered.rows_read = Some(800_000.0);
        let i = insights(&plan(Engine::Mysql, buffered)).into_iter().find(|i| i.rule == "join-buffer").unwrap();
        assert!(!i.title.contains("without an index") && i.detail.contains("ix_items_price"), "{}", i.title);
        assert!(i.fix_sql.as_deref().unwrap().starts_with("ANALYZE TABLE"), "{:?}", i.fix_sql);
    }

    #[test]
    fn every_rule_has_a_listing() {
        // Keep RULES in step with the rule functions: every id an insight can carry is listed.
        let ids: Vec<&str> = RULES.iter().map(|r| r.0).collect();
        for id in ["key-lookups", "memory-grant", "temp-table-sort", "index-ignored", "jit"] {
            assert!(ids.contains(&id));
        }
        assert!(RULES.len() >= 30);
    }
}
