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
use crate::plan::{NodeKind, Plan, PlanKind, PlanNode, PlanWarning};

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
    ("not-in", ALL, "NOT IN that can't become an anti-join"),
    ("scalar-udf", &[Engine::Mssql], "A scalar function called once per row"),
    ("table-variable", &[Engine::Mssql], "A table variable estimated at one row"),
    ("parallelism-blocked", &[Engine::Mssql], "An expensive plan that can't go parallel"),
    ("parallel-skew", &[Engine::Mssql, Engine::Postgres], "One thread or worker doing most of the work"),
    ("eager-spool", &[Engine::Mssql], "A temporary index built on every run"),
    ("spill", &[Engine::Mssql, Engine::Postgres], "A sort or hash that ran out of memory and wrote to disk"),
    ("memory-grant", &[Engine::Mssql], "A memory grant far larger than what the query uses, or a wait for one"),
    ("misestimate", ALL, "Actual rows far from the estimate"),
    ("nested-loops", ALL, "A nested loop repeating its inner side thousands of times"),
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

/// Compute and attach insights (and the coverage note) to a plan.
pub fn annotate(plan: &mut Plan) {
    plan.insights = Some(PlanInsights { items: insights(plan), note: coverage_note(plan) });
}

/// What this plan can't show, in one sentence, so an empty list isn't mistaken for a clean bill of health.
pub fn coverage_note(plan: &Plan) -> Option<String> {
    let actual = has_actuals(plan);
    // MariaDB's own plan reader sets the flavor; plans of logged statements carry the server's version instead.
    let mariadb = plan.props.get("flavor").is_some_and(|f| f == "MariaDB")
        || plan.props.get("server_version").is_some_and(|v| v.contains("MariaDB"));
    match (plan.engine, actual) {
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
            "Without EXPLAIN ANALYZE there are no actual rows, spills or timings to check. Postgres plans never carry index suggestions; Culprit reads filters instead.".into(),
        ),
        (Engine::Postgres, true) => Some(
            "Postgres plans carry no index suggestions or memory grants; index advice comes from the filters.".into(),
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
            out.push(d.finish());
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
        Cx { plan, nodes, parent, total: if total > 0.0 { total } else { 1.0 } }
    }

    fn engine(&self) -> Engine {
        self.plan.engine
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
            title: String::new(),
            detail: String::new(),
            fix: String::new(),
            fix_sql: None,
            verify: String::new(),
        }
    }

    fn finish(self) -> PlanInsight {
        let by_impact = if self.impact >= 0.3 {
            Severity::High
        } else if self.impact >= 0.08 {
            Severity::Medium
        } else {
            Severity::Low
        };
        PlanInsight {
            rule: self.rule.into(),
            area: self.area,
            severity: by_impact.max(self.floor).min(self.cap),
            impact: (self.impact * 1000.0).round() / 1000.0,
            nodes: self.nodes,
            title: self.title,
            detail: self.detail,
            fix: self.fix,
            fix_sql: self.fix_sql,
            verify: self.verify,
        }
    }
}

// ── helpers ──────────────────────────────────────────────────────────────────────────────────────

/// Rows an operator hands to its parent across all its executions.
fn rows_out(n: &PlanNode) -> f64 {
    n.act_rows.unwrap_or_else(|| n.est_rows_total())
}

/// Rows an operator read: what the engine reports read, else what it returned.
fn rows_read(n: &PlanNode) -> f64 {
    n.rows_read.unwrap_or_else(|| rows_out(n))
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

/// A name quoted for a script, with its closing delimiter doubled so a name can never end the quoting early.
fn quote(engine: Engine, name: &str) -> String {
    match engine {
        Engine::Mssql => format!("[{}]", name.replace(']', "]]")),
        Engine::Mysql => format!("`{}`", name.replace('`', "``")),
        Engine::Postgres => {
            let simple = name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
                && !name.starts_with(|c: char| c.is_ascii_digit());
            if simple { name.to_string() } else { format!("\"{}\"", name.replace('"', "\"\"")) }
        }
    }
}

fn qualified(engine: Engine, table: &str) -> String {
    table.split('.').map(|p| quote(engine, &clean_identifier(p))).collect::<Vec<_>>().join(".")
}

fn table_leaf(table: &str) -> String {
    clean_identifier(table.rsplit('.').next().unwrap_or(table))
}

/// A `CREATE INDEX` to copy: online/concurrent where the engine has it. Culprit never runs it.
fn index_sql(engine: Engine, table: &str, keys: &[String], include: &[String]) -> Option<String> {
    if keys.is_empty() {
        return None;
    }
    let name = format!("ix_{}_{}", table_leaf(table), keys.join("_")).to_ascii_lowercase();
    let name: String =
        name.chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' { c } else { '_' }).take(60).collect();
    let cols = keys.iter().map(|k| quote(engine, k)).collect::<Vec<_>>().join(", ");
    let include: Vec<String> =
        include.iter().filter(|c| !keys.iter().any(|k| k.eq_ignore_ascii_case(c))).map(|c| quote(engine, c)).collect();
    let target = qualified(engine, table);
    Some(match engine {
        Engine::Mssql => {
            let mut sql = format!("CREATE NONCLUSTERED INDEX {}\n    ON {target} ({cols})", quote(engine, &name));
            if !include.is_empty() {
                sql.push_str(&format!("\n    INCLUDE ({})", include.join(", ")));
            }
            sql.push_str(
                ";\n-- Add WITH (ONLINE = ON) on Enterprise or Azure SQL to keep the table available while it builds.",
            );
            sql
        }
        Engine::Postgres => {
            let mut sql = format!("CREATE INDEX CONCURRENTLY {}\n    ON {target} ({cols})", quote(engine, &name));
            if !include.is_empty() {
                sql.push_str(&format!("\n    INCLUDE ({})", include.join(", ")));
            }
            sql.push(';');
            sql
        }
        Engine::Mysql => format!(
            "CREATE INDEX {}\n    ON {target} ({cols})\n    ALGORITHM = INPLACE LOCK = NONE;",
            quote(engine, &name)
        ),
    })
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
        let keys = seek.map(|s| filter_columns(&conditions(s))).unwrap_or_default();
        let impact = cx.share(n) + cx.parent(n).filter(|p| p.kind == NodeKind::Join).map_or(0.0, |p| cx.share(p));
        let mut nodes = vec![n.id];
        nodes.extend(seek.map(|s| s.id));
        let mut d = Draft::new("key-lookups", InsightArea::Index, impact, nodes);
        d.floor = Severity::Medium;
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
        d.fix = if heap {
            format!(
                "{table} has no clustered index, so every lookup follows a row pointer. Give it a clustered index, or add the looked-up columns to the index as INCLUDE columns."
            )
        } else {
            match &index {
                Some(ix) => format!(
                    "Add the looked-up columns to {ix} as included columns so the seek answers the query by itself, or select fewer columns."
                ),
                None => "Add the looked-up columns to the index as included columns, or select fewer columns.".into(),
            }
        };
        d.fix_sql = match (&index, keys.is_empty()) {
            (Some(ix), false) if !cols.is_empty() && !heap => Some(format!(
                "CREATE NONCLUSTERED INDEX {}\n    ON {} ({})\n    INCLUDE ({})\n    WITH (DROP_EXISTING = ON);",
                quote(Engine::Mssql, ix),
                qualified(Engine::Mssql, &table),
                keys.iter().map(|k| quote(Engine::Mssql, k)).collect::<Vec<_>>().join(", "),
                cols.iter().map(|k| quote(Engine::Mssql, k)).collect::<Vec<_>>().join(", ")
            )),
            _ => None,
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
            if h.impact_pct >= 50.0 {
                d.floor = Severity::Medium;
            }
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

fn scan_filter(cx: &Cx) -> Vec<Draft> {
    let hinted: Vec<String> = cx.plan.index_hints.iter().map(|h| table_leaf(&h.table).to_ascii_lowercase()).collect();
    let mut out = Vec::new();
    // MySQL's JSON estimate: a scan that expects to keep a tenth of what it examines.
    let mysql_estimate = |n: &PlanNode| {
        n.kind == NodeKind::Scan
            && prop_f64(n, "filtered").is_some_and(|f| f <= 10.0)
            && n.rows_read.is_some_and(|r| r >= 10_000.0)
            && n.predicate.is_some()
    };
    // PostgreSQL's estimate: a sequential scan of a big table expected to keep almost none of it (the table's size
    // comes from the planner's statistics, filled in with the plan).
    let postgres_estimate = |n: &PlanNode| {
        cx.engine() == Engine::Postgres
            && n.kind == NodeKind::Scan
            && n.act_rows.is_none()
            && n.predicate.is_some()
            && n.table_rows.is_some_and(|t| t >= 10_000.0 && rows_out(n) * 10.0 <= t)
    };
    for n in cx
        .nodes
        .iter()
        .filter(|n| n.warnings.contains(&PlanWarning::ResidualPredicate) || mysql_estimate(n) || postgres_estimate(n))
    {
        let Some(table) = n.object.clone() else { continue };
        if hinted.contains(&table_leaf(&table).to_ascii_lowercase()) {
            continue;
        }
        // MySQL's EXPLAIN ANALYZE puts the filter in a Filter node above the scan.
        let filter_node = cx.parent(n).filter(|p| p.op == "Filter");
        let read = rows_read(n);
        let kept = filter_node.map_or_else(|| rows_out(n), rows_out);
        let filter = n.predicate.clone().or_else(|| filter_node.and_then(|f| f.predicate.clone())).unwrap_or_default();
        // A conversion, function or leading wildcard explains the scan better; their rules say so.
        let explained = conditions(n).contains("CONVERT_IMPLICIT")
            || wrapped_column(&filter).is_some()
            || wildcard_at(&filter).is_some();
        if explained || kept * 10.0 > read {
            continue;
        }
        let cols = filter_columns(&filter);
        let impact = cx.share(n).max(cx.subtree(n) * 0.5);
        let mut d = Draft::new("scan-filter", InsightArea::Index, impact, vec![n.id]);
        d.title = format!("Reads {} of {table} to keep {}", plural(read, "row", "rows"), rows_text(kept));
        let estimated = n.act_rows.is_none();
        d.detail = format!(
            "{} {} checks `{}` on every row it reads{}, so most of the reading is thrown away. {} of this plan's work.",
            if estimated { "As planned, the" } else { "The" },
            n.op,
            format::predicate(&filter),
            if cols.is_empty() {
                String::new()
            } else {
                format!(" because no index starts with {}", format::list(&cols))
            },
            capitalize(&format::percent(impact))
        );
        d.fix = if cols.is_empty() {
            "Add an index on the filtered columns so the database can go straight to the matching rows.".into()
        } else {
            format!("Add an index on {} so the database can go straight to the matching rows.", format::list(&cols))
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
    let plan_lines: Vec<String> =
        cx.prop("PlanAffectingConvert").map(|v| v.lines().map(str::to_string).collect()).unwrap_or_default();
    let node = cx
        .nodes
        .iter()
        // A converted column is the problem; a converted parameter, variable or literal costs nothing.
        .filter(|n| converts_column(&conditions(n)) || n.warnings.contains(&PlanWarning::ImplicitConversion))
        .max_by(|a, b| a.self_cost.total_cmp(&b.self_cost))
        .copied();
    if plan_lines.is_empty() && node.is_none() {
        return Vec::new();
    }
    let seek_line = plan_lines.iter().find(|l| l.starts_with("Seek Plan"));
    let text =
        seek_line.cloned().or_else(|| plan_lines.first().cloned()).or_else(|| node.map(conditions)).unwrap_or_default();
    let blocks_seek = seek_line.is_some() || node.is_some_and(|n| n.kind == NodeKind::Scan);
    let column = convert_operands(&text)
        .into_iter()
        .find(|op| is_column_operand(op))
        .and_then(column_label)
        .or_else(|| convert_column(&text));
    let impact = node.map_or(0.05, |n| cx.share(n));
    let mut d =
        Draft::new("implicit-conversion", InsightArea::Query, impact, node.map(|n| vec![n.id]).unwrap_or_default());
    let col = column.clone().unwrap_or_else(|| "a column".into());
    if blocks_seek {
        d.floor = Severity::Medium;
        d.title = format!("A type mismatch on {col} stops SQL Server from seeking an index");
        d.detail = format!(
            "The value it's compared with has a different type, so SQL Server converts {col} on every row (`{}`) and can't seek the index. The usual cause is an application sending strings as nvarchar to a varchar column.",
            format::predicate(&convert_expression(&text))
        );
    } else {
        d.cap = Severity::Low;
        d.title = format!("A type mismatch on {col} turns row estimates into guesses");
        d.detail = "SQL Server converts the column before comparing it, which only affects how many rows it expects here; the index can still be used.".into();
    }
    d.fix = "Send the parameter with the column's exact type: VarChar instead of NVarChar (SqlDbType.VarChar in .NET, sql.VarChar in Node's mssql, a non-Unicode mapping in your ORM).".into();
    d.verify = "Explain again: no CONVERT_IMPLICIT in the plan, and the scan became a seek.".into();
    vec![d]
}

/// Each CONVERT_IMPLICIT operand in a predicate as written: `[db].[s].[t].[c]`, `[@1]`, `N'x'`, `[Expr1004]`.
fn convert_operands(text: &str) -> Vec<&str> {
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
            out.push(operand[..end].trim());
        }
        rest = after;
    }
    out
}

/// A CONVERT_IMPLICIT operand that is a column, not a parameter (`@1`), variable, literal or computed expression.
fn is_column_operand(op: &str) -> bool {
    let last = op.rsplit('.').next().unwrap_or(op).trim_matches(|c| c == '[' || c == ']' || c == '"');
    let computed = last.len() > 4 && last.starts_with("Expr") && last[4..].chars().all(|c| c.is_ascii_digit());
    !last.is_empty()
        && !op.contains('(')
        && !op.starts_with('\'')
        && !op.starts_with("N'")
        && !last.starts_with('@')
        && !last.starts_with(|c: char| c.is_ascii_digit() || c == '-')
        && !computed
}

/// Whether a predicate converts a column (the case that costs an index seek or a good estimate).
fn converts_column(text: &str) -> bool {
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

/// `(function, column)` for the first function wrapped around a column in a filter, or a cast on a column.
fn wrapped_column(text: &str) -> Option<(String, String)> {
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
            if args.to_ascii_lowercase().contains("convert_implicit") {
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
    for n in cx.nodes.iter().filter(|n| matches!(n.kind, NodeKind::Scan | NodeKind::Seek)) {
        let Some(filter) = n.predicate.as_deref() else { continue };
        let Some((func, col)) = wrapped_column(filter) else { continue };
        if rows_read(n) < 1_000.0 && n.table_rows.is_none_or(|t| t < 1_000.0) {
            continue;
        }
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
            "Each row's {col} is computed before it can be compared (`{}`), so an index on {col} can't narrow the search: {} reads {} rows. {} of this plan's work.",
            format::predicate(filter),
            n.op,
            format::count(rows_read(n)),
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
            }
        };
        d.verify = format!("Explain again: {} is read with a seek or index condition on {col}.", table_leaf(&table));
        out.push(d);
    }
    out
}

fn leading_wildcard(cx: &Cx) -> Vec<Draft> {
    let mut out = Vec::new();
    for n in cx.nodes.iter().filter(|n| matches!(n.kind, NodeKind::Scan | NodeKind::Seek)) {
        let Some(filter) = n.predicate.as_deref() else { continue };
        let Some(at) = wildcard_at(filter) else { continue };
        let column = identifiers(&filter[..at]).into_iter().last().unwrap_or_else(|| "the column".into());
        let table = what(n);
        let impact = cx.share(n).max(cx.subtree(n) * 0.5);
        let mut d = Draft::new("leading-wildcard", InsightArea::Query, impact, vec![n.id]);
        d.title = format!("LIKE '%…' on {column} reads all of {}", table_leaf(&table));
        d.detail = format!(
            "A pattern that starts with a wildcard can't use an index, so {} checks every row's {column} ({} rows).",
            n.op,
            format::count(rows_read(n).max(n.table_rows.unwrap_or(0.0)))
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
            "Explain again: {} is read through an index (a Bitmap Index Scan on Postgres).",
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

fn not_in(cx: &Cx) -> Vec<Draft> {
    let mut out = Vec::new();
    for n in &cx.nodes {
        let hit = match cx.engine() {
            Engine::Mssql => {
                n.detail.as_deref().is_some_and(|d| d.contains("Anti Semi Join"))
                    && n.children
                        .iter()
                        .any(|c| c.op == "Row Count Spool" || c.children.iter().any(|g| g.op == "Row Count Spool"))
            }
            Engine::Postgres => n.predicate.as_deref().is_some_and(|p| {
                let l = p.to_ascii_lowercase();
                (l.contains("not (any") || l.contains("not (hashed subplan")) && l.contains("subplan")
            }),
            Engine::Mysql => n.predicate.as_deref().is_some_and(|p| {
                let l = p.to_ascii_lowercase();
                l.contains("is_not_null_compl") || (l.contains("<not>") && l.contains("<in_optimizer>"))
            }),
        };
        if !hit {
            continue;
        }
        let impact = cx.subtree(n).max(cx.share(n));
        let mut d = Draft::new("not-in", InsightArea::Query, impact, vec![n.id]);
        d.title = match cx.engine() {
            Engine::Mssql => {
                "NOT IN against a column that can be NULL makes SQL Server check for NULLs row by row".into()
            }
            Engine::Postgres => {
                "NOT IN (subquery) can't run as an anti-join, so Postgres tests every row against the subquery".into()
            }
            Engine::Mysql => "NOT IN (subquery) needs NULL handling MySQL can't turn into an anti-join".into(),
        };
        d.detail = match cx.engine() {
            Engine::Mssql => format!("Because the subquery's column allows NULL, NOT IN must prove no NULL exists before it can return anything, which adds the Row Count Spool under {}.", n.op),
            Engine::Postgres => "If the subquery ever returns NULL, NOT IN returns nothing at all; that rule stops Postgres from using a Hash Anti Join, so it probes the subquery for each row instead.".into(),
            Engine::Mysql => "NOT IN has to treat a NULL from the subquery specially, so MySQL materializes it and checks NULLs separately instead of a straight anti-join.".into(),
        };
        d.fix = "Rewrite as NOT EXISTS, which means what you almost certainly intend and runs as an anti-join: `WHERE NOT EXISTS (SELECT 1 FROM orders o WHERE o.customer_id = c.id)`. Declaring the column NOT NULL also helps.".into();
        d.verify = match cx.engine() {
            Engine::Mssql => "Explain again: an Anti Semi Join without a Row Count Spool.".into(),
            Engine::Postgres => {
                "Explain again: a Hash Anti Join (or Nested Loop Anti Join) instead of a SubPlan filter.".into()
            }
            Engine::Mysql => "Explain again: an antijoin instead of <in_optimizer> checks.".into(),
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
    for n in cx.nodes.iter().filter(|n| n.props.contains_key("UserDefinedFunctions")) {
        let names = n.props["UserDefinedFunctions"].clone();
        let calls = rows_out(n).max(n.children.first().map_or(0.0, rows_out));
        let impact = cx.subtree(n).max(0.1);
        let mut d = Draft::new("scalar-udf", InsightArea::Query, impact, vec![n.id]);
        d.floor = if calls >= MANY_RUNS { Severity::Medium } else { Severity::Low };
        d.title = format!("{names} runs once for every row (about {} calls per run)", format::count(calls));
        d.detail = format!(
            "A scalar function runs like a small separate query for each row{}. Its cost doesn't even show in the plan's numbers, which makes it easy to miss.",
            if blocks_parallel { ", and it keeps the whole plan on one CPU" } else { "" }
        );
        d.fix = "Inline it: rewrite the function as an inline table-valued function or a plain expression in the query. SQL Server 2019+ inlines scalar functions automatically unless they use things like GETDATE(), table variables or loops.".into();
        d.verify = "Explain again: no UserDefinedFunction in the plan, and no NonParallelPlanReason.".into();
        out.push(d);
    }
    if out.is_empty() && blocks_parallel {
        let mut d = Draft::new("scalar-udf", InsightArea::Query, 0.1, vec![cx.plan.root.id]);
        d.floor = Severity::Medium;
        d.title = "A scalar function keeps this plan on one CPU and runs once per row".into();
        d.detail = "SQL Server reports TSQLUserDefinedFunctionsNotParallelizable: a T-SQL scalar function in the query can't run in parallel, and it's called for each row.".into();
        d.fix = "Inline the function as an inline table-valued function or an expression; on SQL Server 2019+ remove what prevents automatic inlining (GETDATE(), table variables, loops).".into();
        d.verify = "Explain again: NonParallelPlanReason is gone.".into();
        out.push(d);
    }
    out
}

fn table_variable(cx: &Cx) -> Vec<Draft> {
    if cx.engine() != Engine::Mssql {
        return Vec::new();
    }
    let mut out = Vec::new();
    for n in cx.nodes.iter().filter(|n| n.object.as_deref().is_some_and(|o| table_leaf(o).starts_with('@'))) {
        let name = table_leaf(n.object.as_deref().unwrap_or_default());
        let actual = n.act_rows;
        if n.est_rows_total() > 1.5 && actual.is_none_or(|a| a < 10.0 * n.est_rows_total()) {
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
        let runs = inner
            .act_executions
            .or_else(|| (inner.est_executions > 1.0).then_some(inner.est_executions))
            .unwrap_or_else(|| rows_out(outer));
        if runs < MANY_LOOPS {
            continue;
        }
        let inner_name = first_object(inner).unwrap_or_else(|| inner.op.clone());
        let outer_name = first_object(outer).unwrap_or_else(|| outer.op.clone());
        let impact = cx.subtree(inner).max(cx.share(n));
        let mut d = Draft::new("nested-loops", InsightArea::Query, impact, vec![n.id, inner.id]);
        d.cap = Severity::Medium;
        d.title = format!(
            "Repeats its read of {} {} times, once per row of {}",
            table_leaf(&inner_name),
            format::count(runs),
            table_leaf(&outer_name)
        );
        let expected = outer.est_rows_total();
        let misjudged = outer.act_rows.is_some_and(|a| a >= 10.0 * expected.max(1.0));
        d.detail = if misjudged {
            format!(
                "The optimizer expected {} rows from {} and chose a nested loop; it got {}. At that size one pass with a hash join is usually much cheaper.",
                format::count(expected),
                table_leaf(&outer_name),
                rows_text(rows_out(outer))
            )
        } else {
            format!(
                "A nested loop is the right choice for a few rows; at {} repetitions a hash join over a single pass of {} is usually cheaper.",
                format::count(runs),
                table_leaf(&inner_name)
            )
        };
        let hinted = {
            let s = cx.statement();
            s.contains("loop join") || s.contains("option (loop")
        };
        d.fix = if hinted {
            "The query forces a loop join (LOOP JOIN / OPTION (LOOP JOIN)); remove the hint and let the optimizer choose.".into()
        } else if misjudged {
            format!(
                "Fix the estimate for {}: update its statistics. With the right row count the optimizer picks a hash join by itself.",
                table_leaf(&outer_name)
            )
        } else {
            format!(
                "Filter {} earlier so fewer rows reach the loop, or make sure the join columns are indexed on both sides so a merge or hash join is an option.",
                table_leaf(&outer_name)
            )
        };
        d.verify =
            format!("Run with actual rows: a hash join, or far fewer executions of {}.", table_leaf(&inner_name));
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
        .take(1)
        .map(|n| {
            let sides: Vec<String> = n.children.iter().filter_map(first_object).map(|o| table_leaf(&o)).collect();
            let impact = cx.subtree(n).max(0.1);
            let mut d = Draft::new("no-join-predicate", InsightArea::Query, impact, vec![n.id]);
            d.floor = Severity::Medium;
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
    // The deepest misestimated operators are the causes; everything above inherits their error.
    let flagged: Vec<&PlanNode> =
        cx.nodes.iter().filter(|n| n.warnings.contains(&PlanWarning::Misestimate)).copied().collect();
    let causes: Vec<&PlanNode> = flagged
        .iter()
        .filter(|n| {
            let mut below = Vec::new();
            n.children.iter().for_each(|c| c.walk(&mut below));
            !below.iter().any(|b| b.warnings.contains(&PlanWarning::Misestimate))
        })
        .copied()
        .collect();
    let mut causes = causes;
    causes.sort_by(|a, b| {
        let w =
            |n: &PlanNode| n.misestimate().unwrap_or(1.0).abs() * rows_out(n).max(n.est_rows_total()).log10().max(1.0);
        w(b).total_cmp(&w(a))
    });
    causes
        .into_iter()
        .take(2)
        .map(|n| {
            let factor = n.misestimate().unwrap_or(1.0);
            let est = n.est_rows_total();
            let act = rows_out(n);
            let subject = n.object.clone().unwrap_or_else(|| n.op.clone());
            let above: f64 = {
                let mut chain = 0.0;
                let mut cur = cx.parent(n);
                while let Some(p) = cur {
                    chain += cx.share(p);
                    cur = cx.parent(p);
                }
                chain
            };
            let impact = (cx.subtree(n) + above).clamp(0.1, 1.0);
            let mut d = Draft::new("misestimate", InsightArea::Statistics, impact, vec![n.id]);
            d.floor = if factor.abs() >= 100.0 { Severity::Medium } else { Severity::Low };
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
                || wrapped_column(&cond).is_some();
            let cols: Vec<String> = if on_expression { Vec::new() } else { filter_columns(&cond) };
            let spilled = cx.nodes.iter().any(|x| x.warnings.contains(&PlanWarning::Spill));
            d.detail = format!(
                "Everything above this operator was planned for the wrong number of rows: join types, memory grants and parallelism{}.{}",
                if spilled { " (here a sort or hash ran out of memory as a result)" } else { "" },
                if cols.len() >= 2 {
                    format!(" The filter combines {}; databases assume such columns are independent, which is wrong when they move together.", format::list(&cols))
                } else if on_expression {
                    " The filter compares an expression, not a plain column, so the column's statistics can't predict it.".to_string()
                } else {
                    String::new()
                }
            );
            let table = n.object.clone();
            let (fix, sql) = match (cx.engine(), &table) {
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
                            ", and tell Postgres the filtered columns are related with extended statistics"
                        } else if on_expression {
                            "; on Postgres 14+ statistics on the expression itself help (CREATE STATISTICS … ON (expression) FROM table)"
                        } else {
                            ""
                        }
                    ),
                    Some(if cols.len() >= 2 {
                        format!("ANALYZE {};\nCREATE STATISTICS {}_{}_stats (dependencies, mcv) ON {} FROM {};\nANALYZE {};", qualified(Engine::Postgres, t), table_leaf(t).to_ascii_lowercase(), cols.join("_").to_ascii_lowercase(), cols.iter().map(|c| quote(Engine::Postgres, c)).collect::<Vec<_>>().join(", "), qualified(Engine::Postgres, t), qualified(Engine::Postgres, t))
                    } else {
                        format!("ANALYZE {};", qualified(Engine::Postgres, t))
                    }),
                ),
                (Engine::Mysql, Some(t)) => (
                    format!("Refresh {}'s statistics{}.", table_leaf(t), if cols.is_empty() { String::new() } else { format!(" and add a histogram on {}", format::list(&cols)) }),
                    Some(if cols.is_empty() {
                        format!("ANALYZE TABLE {};", qualified(Engine::Mysql, t))
                    } else {
                        format!("ANALYZE TABLE {} UPDATE HISTOGRAM ON {};", qualified(Engine::Mysql, t), cols.iter().map(|c| quote(Engine::Mysql, c)).collect::<Vec<_>>().join(", "))
                    }),
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
        .filter(|n| n.warnings.contains(&PlanWarning::MissingStatistics))
        .take(1)
        .map(|n| {
            let cols = n.props.get("ColumnsWithNoStatistics").cloned();
            let mut d = Draft::new("missing-statistics", InsightArea::Statistics, cx.subtree(n), vec![n.id]);
            d.cap = Severity::Medium;
            d.title = format!("No statistics on {}, so its row counts are guesses", cols.clone().unwrap_or_else(|| format!("some columns of {}", table_leaf(&what(n)))));
            d.detail = "Without statistics the optimizer uses fixed guesses for how many rows match, which leads to the wrong join type or memory size.".into();
            d.fix = "Turn automatic statistics creation back on (it's on by default), or create statistics on those columns.".into();
            d.fix_sql = Some("ALTER DATABASE CURRENT SET AUTO_CREATE_STATISTICS ON;".into());
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
        Engine::Mysql => (None, None),
    };
    let Some(planning) = planning else { return Vec::new() };
    let slower_than_running = running.is_some_and(|r| planning > r && planning >= 20.0);
    if planning < 500.0 && !slower_than_running {
        return Vec::new();
    }
    let mut d = Draft::new("slow-compile", InsightArea::Query, 0.1, vec![cx.plan.root.id]);
    d.cap = Severity::Medium;
    d.title = match running {
        Some(r) if slower_than_running => {
            format!("Planning takes {}, longer than running it ({})", format::duration(planning), format::duration(r))
        }
        _ => format!("Compiling this plan took {}", format::duration(planning)),
    };
    d.detail = "When the plan isn't reused, every run pays this again. Many joins, many partitions or long IN lists make planning expensive.".into();
    d.fix = match cx.engine() {
        Engine::Postgres => "Reuse the plan with prepared statements, keep IN lists short (use = ANY($1) with an array), and make sure partition pruning can happen at plan time.".into(),
        _ => "Make sure the plan is reused: send the query parameterized rather than with literal values, and avoid OPTION (RECOMPILE) on frequent queries.".into(),
    };
    d.verify = "Explain again: compile/planning time is a small fraction of the run.".into();
    vec![d]
}

// ── rules: memory ────────────────────────────────────────────────────────────────────────────────

fn spill(cx: &Cx) -> Vec<Draft> {
    let mut out = Vec::new();
    for n in cx.nodes.iter().filter(|n| n.warnings.contains(&PlanWarning::Spill)) {
        let impact = cx.share(n).max(0.1);
        let mut d = Draft::new("spill", InsightArea::Memory, impact, vec![n.id]);
        d.floor = Severity::Medium;
        let underestimated = {
            let mut below = Vec::new();
            n.walk(&mut below);
            below.iter().any(|b| b.misestimate().is_some_and(|m| m >= 10.0))
        };
        match cx.engine() {
            Engine::Postgres => {
                let disk_kb = prop_f64(n, "Sort Space Used")
                    .filter(|_| n.props.get("Sort Space Type").map(String::as_str) == Some("Disk"));
                let batches = prop_f64(n, "Hash Batches").filter(|b| *b > 1.0);
                let temp_kb = prop_f64(n, "Temp Written Blocks").map(|b| b * 8.0);
                d.title = match (disk_kb, batches) {
                    (Some(kb), _) => format!(
                        "The sort writes {} to disk because it didn't fit in work_mem",
                        format::bytes(kb * 1024.0)
                    ),
                    (None, Some(b)) => format!(
                        "The hash split its input into {} batches on disk because it didn't fit in work_mem",
                        format::count(b)
                    ),
                    _ => format!(
                        "{} writes {} of temporary files because it didn't fit in work_mem",
                        n.op,
                        format::bytes(temp_kb.unwrap_or(0.0) * 1024.0)
                    ),
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
            _ => {
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
    let Some(reason) = cx.prop("NonParallelPlanReason") else { return Vec::new() };
    if reason == "TSQLUserDefinedFunctionsNotParallelizable" || cx.plan.total_cost < 30.0 {
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
    d.cap = Severity::Medium;
    d.title = if plain.is_empty() {
        format!("This plan runs on one CPU: SQL Server reports {reason}")
    } else {
        format!("This plan runs on one CPU because {plain}")
    };
    d.detail = format!(
        "With a cost of {:.0} it would normally be split across CPUs; the whole run happens on a single core instead.",
        cx.plan.total_cost
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
            Engine::Mysql => continue,
        };
        let total: f64 = rows.iter().sum();
        let max = rows.iter().cloned().fold(0.0, f64::max);
        let avg = total / rows.len() as f64;
        if total < 10_000.0 || avg <= 0.0 || max < 3.0 * avg {
            continue;
        }
        // Skew costs time only in proportion to the work involved, and a TOP makes uneven threads normal.
        let has_top = cx.nodes.iter().any(|x| x.op == "Top" || x.op == "Limit");
        let mut d = Draft::new("parallel-skew", InsightArea::Parallelism, (cx.share(n) * 0.3).max(0.02), vec![n.id]);
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
        .filter_map(|n| {
            let planned = prop_f64(n, "Workers Planned")?;
            let launched = prop_f64(n, "Workers Launched")?;
            (launched < planned).then_some((n, planned, launched))
        })
        .take(1)
        .map(|(n, planned, launched)| {
            let mut d = Draft::new("workers-short", InsightArea::Server, cx.subtree(n).max(0.1), vec![n.id]);
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

fn many_to_many(cx: &Cx) -> Vec<Draft> {
    cx.nodes
        .iter()
        .filter(|n| n.op == "Merge Join" && is_true(n.props.get("ManyToMany")))
        .take(1)
        .map(|n| {
            let mut d = Draft::new("many-to-many", InsightArea::Index, cx.share(n).max(0.05), vec![n.id]);
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
            let mut d = Draft::new("remote-query", InsightArea::Query, cx.subtree(n).max(0.05), vec![n.id]);
            d.cap = Severity::Medium;
            d.title = format!("Pulls {} from a linked server", plural(rows_out(n), "row", "rows"));
            d.detail = "Rows from another server travel over the network, and filters that can't be sent along are applied locally after the transfer.".into();
            d.fix = "Filter on the remote side (OPENQUERY with the WHERE clause inside), or copy the data you need locally on a schedule.".into();
            d.verify = "Explain again: far fewer rows come back from the Remote operator.".into();
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
            let mut d = Draft::new("heap-fetches", InsightArea::Server, cx.share(n).max(0.05), vec![n.id]);
            d.cap = Severity::Medium;
            d.title = format!("The index-only scan on {} still visits the table {} times", table_leaf(&table), rows_text(fetches));
            d.detail = "Postgres can only skip the table for pages the visibility map marks all-visible, which VACUUM maintains; this table hasn't been vacuumed since many of its rows changed.".into();
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
            (lossy > 0.0).then_some((n, lossy))
        })
        .take(1)
        .map(|(n, lossy)| {
            let rechecked = prop_f64(n, "Rows Removed by Index Recheck").unwrap_or(0.0);
            let mut d = Draft::new("lossy-bitmap", InsightArea::Memory, cx.share(n).max(0.05), vec![n.id]);
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
    let mut d =
        Draft::new("jit", InsightArea::Server, execution.map_or(0.2, |e| (total / e).min(1.0)), vec![cx.plan.root.id]);
    d.cap = Severity::Medium;
    d.title = match execution {
        Some(e) => {
            format!("JIT compilation took {} of the {} this query ran", format::duration(total), format::duration(e))
        }
        None => format!("JIT compilation took {}", format::duration(total)),
    };
    d.detail = "Postgres compiled parts of this query to machine code because its estimated cost crossed jit_above_cost; for queries that run briefly, compiling costs more than it saves.".into();
    d.fix = "Turn JIT off for this workload, or raise jit_above_cost so only long analytical queries use it.".into();
    d.fix_sql = Some("ALTER ROLE app_user SET jit = off;".into());
    d.verify = "Explain analyze again: no JIT section, and a shorter execution time.".into();
    vec![d]
}

fn correlated_subquery(cx: &Cx) -> Vec<Draft> {
    let mut out = Vec::new();
    for n in &cx.nodes {
        let (runs, outer) = match cx.engine() {
            Engine::Postgres => {
                if n.props.get("Parent Relationship").map(String::as_str) != Some("SubPlan") {
                    continue;
                }
                let parent = cx.parent(n);
                let not_in = parent.and_then(|p| p.predicate.as_deref()).is_some_and(|f| {
                    let l = f.to_ascii_lowercase();
                    l.contains("not (") && l.contains("subplan")
                });
                if not_in {
                    continue; // not-in explains it and has the better fix
                }
                let runs = n.act_executions.unwrap_or_else(|| parent.map_or(0.0, rows_out));
                (runs, parent.and_then(first_object))
            }
            Engine::Mysql => {
                if !is_true(n.props.get("dependent")) || is_true(n.props.get("cacheable")) {
                    continue;
                }
                let parent = cx.parent(n);
                let outer = parent.and_then(|p| p.children.iter().find(|c| c.id != n.id).and_then(first_object));
                let runs = parent.and_then(|p| p.children.iter().find(|c| c.id != n.id)).map_or(0.0, rows_out);
                (runs, outer)
            }
            Engine::Mssql => continue,
        };
        if runs < MANY_RUNS {
            continue;
        }
        let impact = cx.subtree(n).max(0.1);
        let mut d = Draft::new("correlated-subquery", InsightArea::Query, impact, vec![n.id]);
        d.floor = Severity::Medium;
        let outer_name = outer.as_deref().map(table_leaf).unwrap_or_else(|| "the outer query".into());
        d.title = format!("A subquery runs {} times, once per row of {outer_name}", format::count(runs));
        d.detail = format!(
            "{} the subquery for every row instead of once for the whole set{}.",
            if cx.engine() == Engine::Mysql {
                "MySQL marks it DEPENDENT and not cacheable, so it re-runs"
            } else {
                "Postgres runs"
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
        d.floor = Severity::Medium;
        d.title = format!("Joins {} without an index ({how} over a full read)", table_leaf(&table));
        d.detail = format!(
            "MySQL has no index to look up matching rows of {} for each row it joins, so it reads all {} rows into a join buffer and matches them there.",
            table_leaf(&table),
            format::count(rows_read(n))
        );
        d.fix = if cols.is_empty() {
            format!("Index the join column of {} so MySQL can look rows up directly.", table_leaf(&table))
        } else {
            format!("Index {} on {} so MySQL can look rows up directly.", table_leaf(&table), format::list(&cols))
        };
        d.fix_sql = index_sql(Engine::Mysql, &table, &cols, &[]);
        d.verify =
            format!("Explain again: {} is read with ref or eq_ref access, without a join buffer.", table_leaf(&table));
        out.push(d);
    }
    out
}

fn temp_table_sort(cx: &Cx) -> Vec<Draft> {
    let mariadb = cx.plan.props.get("flavor").is_some_and(|f| f == "MariaDB")
        || cx.plan.props.get("server_version").is_some_and(|v| v.contains("MariaDB"));
    let server = if mariadb { "MariaDB" } else { "MySQL" };
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
        if rows < 10_000.0 {
            continue;
        }
        let impact = cx.subtree(n).max(0.1);
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
    for n in cx.nodes.iter().filter(|n| n.op == "Table scan") {
        let Some(possible) = n.props.get("possible_keys") else { continue };
        let filtered = prop_f64(n, "filtered").unwrap_or(100.0);
        if rows_read(n) < 10_000.0 || filtered >= 20.0 {
            continue;
        }
        let mut d = Draft::new("index-ignored", InsightArea::Statistics, cx.share(n).max(0.05), vec![n.id]);
        d.cap = Severity::Medium;
        d.title = format!("MySQL could use {possible} but reads all of {} instead", table_leaf(&what(n)));
        d.detail = format!(
            "It expects the filter to keep {} of {} rows, yet judged the index not worth it; that usually means stale statistics or a type mismatch in the comparison.",
            format::percent(filtered / 100.0),
            format::count(rows_read(n))
        );
        d.fix = "Refresh the table's statistics and compare the column with a value of its own type; if it still scans, compare with FORCE INDEX to see which is really faster.".into();
        d.fix_sql = n.object.as_ref().map(|t| format!("ANALYZE TABLE {};", qualified(Engine::Mysql, t)));
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
            wrapped_column("datepart(year,[db].[shop].[orders].[created_at])=(2026)"),
            Some(("DATEPART".into(), "created_at".into()))
        );
        assert_eq!(
            wrapped_column("(date_trunc('day'::text, orders.created_at) = '2026-03-01')"),
            Some(("DATE_TRUNC".into(), "created_at".into()))
        );
        assert_eq!(
            wrapped_column("(cast(`shop`.`orders`.`created_at` as date) = '2026-03-01')"),
            Some(("CAST".into(), "created_at".into()))
        );
        assert_eq!(wrapped_column("((o.total)::integer = 5)"), Some(("::integer".into(), "total".into())));
        // varchar → text casts are how Postgres prints varchar comparisons; the index still works.
        assert_eq!(wrapped_column("((c.email)::text = 'x'::text)"), None);
        // A conversion of the parameter, not the column.
        assert_eq!(wrapped_column("[db].[shop].[orders].[status]=CONVERT(varchar(20),[@P1],0)"), None);
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
        let sql = i.fix_sql.as_deref().unwrap();
        assert!(
            sql.contains("INCLUDE ([total], [channel])") && sql.contains("([status])") && sql.contains("DROP_EXISTING")
        );
        assert!(!rules(&p).contains(&"nested-loops".to_string()), "lookups aren't double-reported as loops");
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
        let mut d = Draft::new("legacy-ce", InsightArea::Server, 0.9, vec![0]);
        d.cap = Severity::Low;
        assert_eq!(d.finish().severity, Severity::Low);
        let mut d = Draft::new("spill", InsightArea::Memory, 0.01, vec![0]);
        d.floor = Severity::Medium;
        assert_eq!(d.finish().severity, Severity::Medium);
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
