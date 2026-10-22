//! Compact, agent-shaped JSON for reports, plans, sessions and result sets. Long database text is clipped so a single
//! tool result never floods the agent's context.

use std::collections::BTreeMap;

use culprit_core::{
    Category, Changes, Error, Finding, LiveState, Plan, PlanNode, QueryStat, Report, Result, ResultSet, Severity,
    Workload, format,
};
use serde_json::{Value, json};

use crate::tools::TopBy;

const TEXT_SHORT: usize = 300;
const TEXT_FULL: usize = 8_000;
const CELL: usize = 1_000;

/// Collapse whitespace and cut to `max` characters.
fn clip(text: &str, max: usize) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max {
        return flat;
    }
    let mut cut: String = flat.chars().take(max).collect();
    cut.push('…');
    cut
}

fn round(value: f64, places: i32) -> f64 {
    let factor = 10f64.powi(places);
    (value * factor).round() / factor
}

fn pct(ratio: f64) -> f64 {
    round(ratio * 100.0, 1)
}

fn window(w: &Workload) -> Value {
    json!({
        "source": w.source,
        "hours": round(w.measurement_hours(), 1),
        "cumulative_since_stats_reset": !w.is_windowed(),
        "application_log": w.log,
        "start": w.window_start,
        "end": w.window_end,
    })
}

fn scan_header(report: &Report) -> Value {
    let s = &report.snapshot;
    json!({
        "connection": s.connection,
        "engine": s.server.engine,
        "environment": s.environment,
        "server": format!("{} {}", s.server.product, s.server.version),
        "database": s.server.database,
        "scanned_at": s.taken_at,
        "window": s.workload.as_ref().map(window),
        "headline": report.summary.headline,
    })
}

/// The tool that shows more of what a dashboard tile is about; none for what an engine without a server (SQLite) can't
/// answer: sessions, waits, deadlocks, periods to compare.
pub fn next_tool(key: culprit_core::TileKey, engine: culprit_core::Engine) -> Option<&'static str> {
    use culprit_core::TileKey;
    if !engine.has_server()
        && matches!(
            key,
            TileKey::DbTime
                | TileKey::Waits
                | TileKey::Health
                | TileKey::Now
                | TileKey::Deadlocks
                | TileKey::Storage
                | TileKey::Connections
                | TileKey::Replication
        )
    {
        return None;
    }
    Some(match key {
        TileKey::DbTime => "what_changed",
        TileKey::Findings => "findings",
        TileKey::TopQueries => "top_queries",
        TileKey::Waits | TileKey::Health => "waits",
        TileKey::Now => "blocking",
        TileKey::Deadlocks => "deadlocks",
        TileKey::Indexes => "index_advice",
        TileKey::Plans => "plan_insights",
        TileKey::Statistics => "scan",
        TileKey::Storage | TileKey::Connections | TileKey::Replication => "waits",
        TileKey::Failures => "findings",
    })
}

pub fn findings(report: &Report, category: Option<Category>, min: Severity, limit: usize) -> Value {
    let matching: Vec<&Finding> =
        report.findings.iter().filter(|f| category.is_none_or(|c| f.category == c) && f.severity >= min).collect();
    let shown: Vec<Value> = matching
        .iter()
        .take(limit)
        .map(|f| {
            json!({
                "id": f.id,
                "severity": f.severity,
                "confidence": f.confidence,
                "category": f.category,
                "title": f.title,
                "impact": f.impact.label,
                "db_time_pct": f.impact.db_time_share.map(pct),
                "inactive": f.inactive.then_some(true),
            })
        })
        .collect();
    let locked: Vec<Value> = report
        .locked
        .iter()
        .map(|l| json!({ "check": l.check.title, "needs": l.missing.iter().map(|c| c.label()).collect::<Vec<_>>() }))
        .collect();
    json!({
        "scan": scan_header(report),
        "total": matching.len(),
        "findings": shown,
        "locked_checks": (!locked.is_empty()).then_some(locked),
    })
}

pub fn lint_sql(report: &Report, findings: &[Finding]) -> Value {
    let shown: Vec<Value> = findings
        .iter()
        .map(|f| {
            json!({
                "id": f.id,
                "severity": f.severity,
                "confidence": f.confidence,
                "category": f.category,
                "title": f.title,
                "summary": f.summary,
                "impact": f.impact.label,
                "fix": f.fix.as_ref().map(|x| json!({
                    "summary": x.summary,
                    "sql": x.sql,
                    "cost": x.cost,
                })),
                "fix_sql": f.fix.as_ref().and_then(|x| x.sql.as_deref()),
                "verify": f.verify,
            })
        })
        .collect();
    json!({
        "connection": report.snapshot.connection,
        "engine": report.snapshot.server.engine,
        "database": report.snapshot.server.database,
        "total": findings.len(),
        "findings": shown,
    })
}

pub fn finding(report: &Report, f: &Finding) -> Value {
    json!({
        "id": f.id,
        "title": f.title,
        "severity": f.severity,
        "confidence": f.confidence,
        "category": f.category,
        "query_ids": f.queries.iter().map(|q| q.id.as_str()).collect::<Vec<_>>(),
        "fix_sql": f.fix.as_ref().and_then(|x| x.sql.as_deref()),
        "brief": culprit_engine::agent_brief(report, f),
    })
}

fn query_row(w: &Workload, q: &QueryStat) -> Value {
    // Share, runs a day and average as every surface states them (`Workload::cost_of`): over Culprit's scans where they
    // measured it (`measured_over`), with the source's totals beside them where those differ twofold (`totals`).
    let cost = w.cost_of(q);
    let scans = w.scans_of(q).filter(|_| w.statement_time_per_day(q).is_some());
    json!({
        "id": q.id,
        "db_time_pct": pct(cost.share),
        // Per day only where its runs came that way, over 20 hours or more; null for a burst or a few runs, with the
        // measured runs and their window.
        "runs_per_day": scans.map(|s| s.per_day(s.runs)).or_else(|| w.runs_per_day_of(q)).map(|v| round(v, 1)),
        "runs": round(q.executions, 0),
        // How often it ran in words, as the brief says it: a burst by when it ran ("750 runs on Oct 11, 15:51–15:55
        // UTC"), never a rate a day.
        "how_often": cost.runs,
        "window_hours": round(w.measurement_hours(), 2),
        "measured_over": scans.map(|s| s.phrase()),
        "totals": cost.totals,
        "avg_ms": round(cost.mean_ms, 3),
        "max_ms": q.max_time_ms.map(|v| round(v, 3)),
        "cpu_ms_per_run": q.cpu_time_ms.filter(|_| q.executions > 0.0).map(|c| round(c / q.executions, 3)),
        "rows_per_run": q.rows_per_execution().map(|v| round(v, 1)),
        "reads_per_run": q.reads_per_execution().map(|v| round(v, 1)),
        "plans": q.plans.len().max(usize::from(q.plan.is_some())),
        "object": q.object_name,
        "text": clip(&q.text, TEXT_SHORT),
    })
}

pub fn top_queries(report: &Report, by: TopBy, limit: usize) -> Value {
    let Some(w) = report.snapshot.workload.as_ref() else {
        let note = report.summary.no_workload.clone().unwrap_or_else(|| "This scan has no query history.".into());
        return json!({ "scan": scan_header(report), "queries": [], "note": note });
    };
    let key = |q: &QueryStat| match by {
        // Its share as every surface states it: over Culprit's scans where they measured it.
        TopBy::Time => w.statement_share(q),
        TopBy::Cpu => q.cpu_time_ms.unwrap_or(0.0),
        TopBy::Reads => q.logical_reads.unwrap_or(0.0),
        TopBy::Executions => q.executions,
    };
    let mut queries: Vec<&QueryStat> = w.queries.iter().collect();
    queries.sort_by(|a, b| key(b).total_cmp(&key(a)));
    json!({
        "scan": scan_header(report),
        "queries": queries.into_iter().take(limit).map(|q| query_row(w, q)).collect::<Vec<_>>(),
    })
}

pub fn query(report: &Report, id: &str) -> Result<Value> {
    let w = report.snapshot.workload.as_ref().ok_or_else(|| Error::not_found("This scan has no query history"))?;
    let q = w.query(id).ok_or_else(|| {
        Error::not_found(format!("No query '{id}' in the latest scan")).with_hint("Call top_queries for the ids")
    })?;
    let mut row = query_row(w, q);
    row["text"] = json!(clip(&q.text, TEXT_FULL));
    row["parameters"] = json!(q.parameters);
    row["executions"] = json!(q.executions);
    row["total_ms"] = json!(round(q.total_time_ms, 1));
    row["stddev_ms"] = json!(q.stddev_time_ms.map(|v| round(v, 3)));
    row["first_seen"] = json!(q.first_seen);
    row["last_seen"] = json!(q.last_seen);
    // Fastest run and, where the engine keeps them (MySQL), percentiles, a run-time histogram and lock time.
    row["spread"] = json!(q.spread);
    // Failed and cancelled runs, planning and JIT time, log bytes and estimated row width, where the engine keeps them.
    row["costs"] = json!(q.costs);
    let ms = |v: Option<f64>| v.map(|v| round(v, 3));
    row["plan_variants"] = json!(
        q.plans
            .iter()
            .map(|p| json!({
                "plan_id": p.id,
                "executions": p.executions,
                "avg_ms": round(p.mean_time_ms, 3),
                "min_ms": ms(p.min_time_ms),
                "max_ms": ms(p.max_time_ms),
                "stddev_ms": ms(p.stddev_time_ms),
                "compiled_for": p.compiled,
                "variant_query": p.variant_query,
                "last_seen": p.last_seen,
                "forced": p.is_forced.then_some(true),
            }))
            .collect::<Vec<_>>()
    );
    row["captured_plan"] = json!(q.plan.as_ref().map(plan_summary));
    row["findings"] = json!(
        report
            .findings
            .iter()
            .filter(|f| f.queries.iter().any(|r| r.id == id))
            .map(|f| json!({ "id": f.id, "title": f.title, "severity": f.severity }))
            .collect::<Vec<_>>()
    );
    Ok(json!({ "scan": scan_header(report), "query": row }))
}

fn plan_summary(plan: &Plan) -> Value {
    let hot = hottest(plan);
    json!({
        "kind": plan.kind,
        "hottest": hot.map(node_label),
        "hottest_cost_pct": hot.and_then(|hot| share(plan, hot)).map(pct),
        "warnings": warnings(plan),
    })
}

fn warnings(plan: &Plan) -> Vec<&'static str> {
    let mut all: Vec<&'static str> = plan.warnings.iter().map(|w| w.label()).collect();
    for node in plan.nodes() {
        for w in &node.warnings {
            if !all.contains(&w.label()) {
                all.push(w.label());
            }
        }
    }
    all
}

/// Where the plan's work is, measured the way `share` measures it: the step with the most time of its own in an actual
/// plan that carries per-operator timing, else the costliest. Never an exchange (its cost or time is passing rows
/// between threads), none in a plan too cheap for any of its steps to matter, and none where the plan can't state the
/// step's share (beside a step it puts no cost on, or on rows that rest on a default guess).
fn hottest(plan: &Plan) -> Option<&PlanNode> {
    if !culprit_core::insights::does_real_work(plan) {
        return None;
    }
    let hot = match culprit_core::self_times(plan) {
        Some(own) => {
            let time = |n: &PlanNode| own.get(&n.id).copied().unwrap_or(0.0);
            plan.nodes()
                .into_iter()
                .filter(|n| n.kind != culprit_core::NodeKind::Parallelism)
                .max_by(|a, b| time(a).total_cmp(&time(b)))
        }
        None => plan.hottest(),
    };
    hot.filter(|hot| share(plan, hot).is_some())
}

/// Each operator's share of the time in actual plans that carry per-operator timing, of the estimated cost otherwise,
/// where the plan can state one (`Plan::step_share`).
fn share(plan: &Plan, node: &PlanNode) -> Option<f64> {
    if let Some(own) = culprit_core::self_times(plan) {
        let total: f64 = own.values().sum();
        return (total > 0.0).then(|| own.get(&node.id).copied().unwrap_or(0.0) / total);
    }
    plan.step_share(node)
}

fn node_label(node: &PlanNode) -> String {
    let mut label = node.op.clone();
    if let Some(object) = &node.object {
        label.push(' ');
        label.push_str(object);
    }
    if let Some(index) = &node.index {
        label.push_str(&format!(" ({index})"));
    }
    label
}

/// The plan as an indented tree, one operator per line: cost share, operator, rows, predicates and warnings.
pub fn plan_tree(plan: &Plan) -> String {
    let mut out = String::new();
    fn visit(plan: &Plan, node: &PlanNode, depth: usize, out: &mut String) {
        let cost = share(plan, node).map(|s| format!("{:>4}", format::percent(s))).unwrap_or_else(|| "   -".into());
        let mut line = format!("{cost}  {}{}", "  ".repeat(depth), node_label(node));
        if plan.weighed() {
            line.push_str(&format!("  rows est {}", format::count(node.est_rows_total())));
        }
        if let Some(actual) = node.act_rows {
            line.push_str(&format!(" actual {}", format::count(actual)));
        }
        if let Some(read) = node.rows_read {
            line.push_str(&format!(" read {}", format::count(read)));
        }
        if let Some(seek) = &node.seek_predicate {
            line.push_str(&format!("  seek: {}", clip(seek, 160)));
        }
        if let Some(filter) = &node.predicate {
            line.push_str(&format!("  filter: {}", clip(filter, 160)));
        }
        if !node.warnings.is_empty() {
            let labels: Vec<&str> = node.warnings.iter().map(|w| w.label()).collect();
            line.push_str(&format!("  ⚠ {}", labels.join(", ")));
        }
        out.push_str(&line);
        out.push('\n');
        for child in &node.children {
            visit(plan, child, depth + 1, out);
        }
    }
    visit(plan, &plan.root, 0, &mut out);
    // Shares of the estimated cost: say why a step has none ("-").
    if culprit_core::self_times(plan).is_none()
        && let Some(note) = culprit_engine::export::shares_note(plan, None)
    {
        out.push_str(&format!("   -  {note}\n"));
    }
    out
}

/// Insights for a plan, computed if the plan doesn't carry them yet.
fn insight_set(plan: &Plan) -> culprit_core::insights::PlanInsights {
    plan.insights.clone().unwrap_or_else(|| culprit_core::insights::PlanInsights {
        items: culprit_core::insights::insights(plan),
        note: culprit_core::insights::coverage_note(plan),
    })
}

pub fn plan_insights(plan: &Plan) -> Value {
    let found = insight_set(plan);
    json!({
        "engine": plan.engine,
        "kind": plan.kind,
        "insights": found.items.iter().map(|i| json!({
            "rule": i.rule,
            "severity": i.severity,
            // The share of the plan's work where the plan states one (`insights::stated_share`): null where the rule
            // ranks by its own weight, the plan has no costs or is cheap, or the step's cost rests on a guess.
            "impact": culprit_core::insights::stated_share(plan, i).map(|s| round(s, 3)),
            "operators": i.nodes,
            "title": i.title,
            "detail": i.detail,
            "fix": i.fix,
            "fix_sql": i.fix_sql,
            "verify": i.verify,
        })).collect::<Vec<_>>(),
        "note": found.note,
        "tree": plan_tree(plan),
    })
}

pub fn plan(plan: &Plan) -> Value {
    let mut value = plan_value(plan);
    if let Some(summary) = culprit_core::actual_summary(plan) {
        value["actual"] = json!({
            "elapsed_ms": summary.elapsed_ms.map(|v| round(v, 1)),
            "cpu_ms": summary.cpu_ms.map(|v| round(v, 1)),
            "logical_reads": summary.logical_reads,
            "physical_reads": summary.physical_reads,
            "rows": summary.rows,
            "memory_granted_kb": summary.memory_granted_kb,
            "memory_used_kb": summary.memory_used_kb,
            "spilled": summary.spilled,
            "slowest_operator": summary.slowest_node,
            "slowest_operator_ms": summary.slowest_ms.map(|v| round(v, 1)),
            "worst_estimate_operator": summary.worst_estimate_node,
            "worst_estimate_factor": summary.worst_estimate.map(|v| round(v, 1)),
            "shares": if culprit_core::self_times(plan).is_some() { "share of the time" } else { "share of the estimated cost" },
            "last_ran": plan.props.get("LastRunAt"),
        });
    }
    value
}

/// A running statement for agents: elapsed, how far along, the busiest operator and per-operator rows so far.
pub fn progress(progress: &culprit_core::QueryProgress) -> Value {
    let label =
        |id: u32| progress.plan.as_ref().and_then(|p| p.nodes().into_iter().find(|n| n.id == id).map(node_label));
    json!({
        "session": progress.session,
        "running": progress.running,
        "state": progress.state,
        "wait": progress.wait,
        "elapsed_ms": progress.elapsed_ms,
        "cpu_ms": progress.cpu_ms,
        "logical_reads": progress.logical_reads,
        "rows_examined": progress.rows_examined,
        "percent": progress.percent.map(|p| round(p * 100.0, 1)),
        "percent_basis": progress.percent_basis,
        "phase": progress.phase,
        "busiest_operator": progress.hot_node.and_then(label),
        "operators": progress.nodes.iter().map(|n| json!({
            "id": n.node_id,
            "operator": label(n.node_id),
            "state": n.state,
            "rows": n.rows,
            "estimated_rows": round(n.est_rows, 0),
        })).collect::<Vec<_>>(),
        "plan": progress.plan.as_ref().map(plan_tree),
        "note": progress.note,
        "statement": progress.statement.as_deref().map(|s| clip(s, 2000)),
    })
}

fn plan_value(plan: &Plan) -> Value {
    let found = insight_set(plan);
    json!({
        "insights": found.items.iter().map(|i| json!({
            "rule": i.rule,
            "severity": i.severity,
            "operators": i.nodes,
            "title": i.title,
        })).collect::<Vec<_>>(),
        "engine": plan.engine,
        "kind": plan.kind,
        "total_cost": round(plan.total_cost, 4),
        "operators": plan.nodes().len(),
        "virtual_indexes": plan.props.get("VirtualIndexes"),
        "warnings": warnings(plan),
        "index_hints": plan.index_hints.iter().map(|h| json!({
            "table": h.table,
            "equality": h.equality,
            "inequality": h.inequality,
            "include": h.include,
            "estimated_improvement_pct": round(h.impact_pct, 1),
        })).collect::<Vec<_>>(),
        "tree": plan_tree(plan),
    })
}

pub fn live(live: &LiveState) -> Value {
    let mut sessions: Vec<_> = live.sessions.iter().collect();
    sessions.sort_by(|a, b| {
        b.blocked_by
            .is_some()
            .cmp(&a.blocked_by.is_some())
            .then(b.elapsed_ms.unwrap_or(0.0).total_cmp(&a.elapsed_ms.unwrap_or(0.0)))
    });
    let blocking: Vec<&str> = live.sessions.iter().filter_map(|s| s.blocked_by.as_deref()).collect();
    json!({
        "taken_at": live.taken_at,
        "sessions_total": live.sessions.len(),
        "blocked": live.sessions.iter().filter(|s| s.blocked_by.is_some()).count(),
        "sessions": sessions.into_iter().take(50).map(|s| json!({
            "id": s.id,
            "state": s.state,
            "user": s.user,
            "application": s.application,
            "elapsed_ms": s.elapsed_ms.map(|v| round(v, 0)),
            "wait": s.wait,
            "wait_ms": s.wait_ms.map(|v| round(v, 0)),
            "blocked_by": s.blocked_by,
            "blocking_others": blocking.contains(&s.id.as_str()).then_some(true),
            "transaction_ms": s.transaction_ms.map(|v| round(v, 0)),
            "sql": s.sql.as_deref().map(|t| clip(t, TEXT_SHORT)),
        })).collect::<Vec<_>>(),
        "waits": live.waits.iter().take(10).map(|w| json!({ "category": w.category, "type": w.wait_type, "ms": round(w.wait_ms, 0) })).collect::<Vec<_>>(),
    })
}

/// Strip quoting so `[dbo].[Orders]`, `"public"."orders"` and `` `shop`.`orders` `` compare equal.
fn bare(name: &str) -> String {
    name.chars().filter(|c| !matches!(c, '[' | ']' | '"' | '`')).collect::<String>().to_lowercase()
}

pub fn tables(report: &Report) -> Value {
    let s = &report.snapshot;
    let mut indexes: BTreeMap<(String, String), usize> = BTreeMap::new();
    for i in &s.indexes {
        *indexes.entry((bare(&i.schema), bare(&i.table))).or_default() += 1;
    }
    let mut tables: Vec<_> = s.tables.iter().collect();
    tables.sort_by(|a, b| b.total_bytes.unwrap_or(0.0).total_cmp(&a.total_bytes.unwrap_or(0.0)));
    json!({
        "scan": scan_header(report),
        "tables": tables.into_iter().take(300).map(|t| json!({
            "table": format!("{}.{}", t.schema, t.name),
            "rows": t.rows,
            "size": t.total_bytes.map(format::bytes),
            "indexes": indexes.get(&(bare(&t.schema), bare(&t.name))).copied().unwrap_or(0),
            "primary_key": t.has_primary_key,
        })).collect::<Vec<_>>(),
    })
}

pub fn table(report: &Report, wanted: &str) -> Result<Value> {
    let s = &report.snapshot;
    let wanted = bare(wanted);
    let matches: Vec<_> = s
        .tables
        .iter()
        .filter(|t| bare(&format!("{}.{}", t.schema, t.name)) == wanted || bare(&t.name) == wanted)
        .collect();
    let t = match matches.as_slice() {
        [one] => *one,
        [] => {
            return Err(Error::not_found(format!("No table '{wanted}' in the latest scan"))
                .with_hint("Call schema without `table` for the list"));
        }
        many => {
            let names: Vec<String> = many.iter().map(|t| format!("{}.{}", t.schema, t.name)).collect();
            return Err(Error::invalid(format!("'{wanted}' matches {}; pass schema.table", names.join(", "))));
        }
    };
    let (schema, name) = (bare(&t.schema), bare(&t.name));
    let mine = |s2: &str, t2: &str| bare(s2) == schema && bare(t2) == name;
    let usage = index_usage(report, &schema, &name);
    let columns: Vec<Value> = {
        let mut cols: Vec<_> = s.columns.iter().filter(|c| mine(&c.schema, &c.table)).collect();
        cols.sort_by_key(|c| c.ordinal);
        cols.into_iter()
            .map(|c| json!({ "name": c.name, "type": c.data_type, "nullable": c.nullable, "default": c.default, "generated": c.generated.then_some(true) }))
            .collect()
    };
    let indexes: Vec<Value> = s
        .indexes
        .iter()
        .filter(|i| mine(&i.schema, &i.table))
        .map(|i| {
            let used_by = usage.get(&bare(&i.name)).cloned().unwrap_or_default();
            json!({
                "name": i.name,
                "kind": i.kind,
                "columns": i.columns,
                "include": (!i.include.is_empty()).then_some(&i.include),
                "filter": i.filter,
                "unique": i.is_unique,
                "primary_key": i.is_primary_key.then_some(true),
                "disabled": i.is_disabled.then_some(true),
                "size": i.size_bytes.map(format::bytes),
                "reads": i.reads,
                "writes": i.writes,
                "last_read": i.last_read,
                "used_by_captured_plans": used_by,
            })
        })
        .collect();
    let foreign_keys: Vec<Value> = s
        .foreign_keys
        .iter()
        .filter(|f| mine(&f.schema, &f.table))
        .map(|f| {
            let mut obj = json!({
                "name": f.name,
                "columns": f.columns,
                "references": format!("{}.{}({})", f.ref_schema, f.ref_table, f.ref_columns.join(", ")),
            });
            if let Some(rule) = &f.delete_rule {
                obj["delete_rule"] = json!(rule);
            }
            obj
        })
        .collect();
    let referenced_by: Vec<Value> = s
        .foreign_keys
        .iter()
        .filter(|f| mine(&f.ref_schema, &f.ref_table))
        .map(|f| {
            let mut obj = json!({
                "name": f.name,
                "from": format!("{}.{}({})", f.schema, f.table, f.columns.join(", ")),
                "columns": f.ref_columns,
            });
            if let Some(rule) = &f.delete_rule {
                obj["delete_rule"] = json!(rule);
            }
            obj
        })
        .collect();
    let triggers: Vec<Value> = t
        .storage
        .as_ref()
        .map(|st| st.triggers().into_iter().map(|(name, events)| json!({ "name": name, "events": events })).collect())
        .unwrap_or_default();
    let partition_bounds: Vec<&str> = t.storage.as_ref().map(|st| st.partition_bounds()).unwrap_or_default();
    let planned = s.workload.as_ref().map(|w| w.queries.iter().filter(|q| q.plan.is_some()).count()).unwrap_or(0);
    let mut out = json!({
        "scan": scan_header(report),
        "table": format!("{}.{}", t.schema, t.name),
        "rows": t.rows,
        "size": t.total_bytes.map(format::bytes),
        "writes": json!({ "inserts": t.inserts, "updates": t.updates, "deletes": t.deletes }),
        "columns": columns,
        "indexes": indexes,
        "foreign_keys": foreign_keys,
        "note": format!("used_by_captured_plans covers the {planned} query plans captured in this scan; an index missing there may still serve other queries."),
    });
    if !referenced_by.is_empty() {
        out["referenced_by"] = json!(referenced_by);
    }
    if !triggers.is_empty() {
        out["triggers"] = json!(triggers);
    }
    if let Some(parent) = &t.partition_of {
        out["partition_of"] = json!(parent);
    }
    if !partition_bounds.is_empty() {
        out["partition_bounds"] = json!(partition_bounds);
    }
    Ok(out)
}

/// For each index on the table: which captured query plans read it, and with which operators.
fn index_usage(report: &Report, schema: &str, table: &str) -> BTreeMap<String, Vec<Value>> {
    let mut out: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    let Some(w) = report.snapshot.workload.as_ref() else { return out };
    let mut queries: Vec<&QueryStat> = w.queries.iter().filter(|q| q.plan.is_some()).collect();
    queries.sort_by(|a, b| b.total_time_ms.total_cmp(&a.total_time_ms));
    for q in queries {
        let mut seen: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for node in q.plan.as_ref().map(|p| p.nodes()).unwrap_or_default() {
            let (Some(object), Some(index)) = (node.object.as_deref(), node.index.as_deref()) else { continue };
            let parts: Vec<String> = bare(object).split('.').map(str::to_string).collect();
            let node_table = parts.last().map(String::as_str).unwrap_or_default();
            let node_schema = (parts.len() >= 2).then(|| parts[parts.len() - 2].as_str());
            if node_table != table || node_schema.is_some_and(|s| s != schema) {
                continue;
            }
            let ops = seen.entry(bare(index)).or_default();
            if !ops.contains(&node.op) {
                ops.push(node.op.clone());
            }
        }
        for (index, ops) in seen {
            out.entry(index).or_default().push(json!({
                "query_id": q.id,
                "db_time_pct": pct(w.statement_share(q)),
                "operators": ops,
            }));
        }
    }
    out
}

pub fn result_set(rs: &ResultSet) -> Value {
    let rows: Vec<Vec<Value>> = rs
        .rows
        .iter()
        .map(|row| {
            row.iter()
                .map(|cell| match cell {
                    Value::String(s) if s.chars().count() > CELL => {
                        Value::String(format!("{}…", s.chars().take(CELL).collect::<String>()))
                    }
                    other => other.clone(),
                })
                .collect()
        })
        .collect();
    json!({
        "columns": rs.columns,
        "rows": rows,
        "truncated": rs.truncated,
        "elapsed_ms": round(rs.elapsed_ms, 1),
        "messages": rs.messages,
    })
}

/// What changed between two periods, compact: statuses, runs and time per run on both sides, plan switches.
pub fn changes(c: &Changes) -> Value {
    let side = |s: &Option<culprit_core::PeriodStats>| {
        s.as_ref().map(|s| {
            json!({
                "runs": s.executions,
                "avg_ms": round(s.mean_ms, 3),
                "total_ms": round(s.total_ms, 1),
            })
        })
    };
    json!({
        "connection": c.connection,
        "source": c.source,
        "recent": c.recent,
        "baseline": c.baseline,
        "baseline_coverage": round(c.baseline_coverage, 2),
        "summary": c.summary,
        "min_runs": c.min_executions,
        "threshold": c.threshold,
        "notes": c.notes,
        "unchanged": c.unchanged,
        "queries": c
            .queries
            .iter()
            .map(|q| json!({
                "query_id": q.query_id,
                "status": q.status,
                "headline": q.headline,
                "ratio": q.ratio.map(|r| round(r, 2)),
                "impact_ms": round(q.impact_ms, 1),
                "recent_db_time_pct": pct(q.recent_share),
                "baseline": side(&q.baseline),
                "recent": side(&q.recent),
                "plan_switch": q.plan_switch,
                "reason": q.reason,
                "may_be_explained_by": q.context,
                "text": clip(&q.text, 400),
            }))
            .collect::<Vec<_>>(),
        "events": c.events.iter().map(event).collect::<Vec<_>>(),
    })
}

/// One timeline event, compact: exact `at`, or `after`..`at` when Culprit noticed it between two scans.
fn event(e: &culprit_core::ChangeEvent) -> Value {
    let mut v = json!({
        "id": e.id,
        "kind": e.kind,
        "title": clip(&e.title, 300),
        "at": e.at,
        "source": e.source,
    });
    let map = v.as_object_mut().expect("object");
    for (key, value) in
        [("after", &e.after), ("until", &e.until), ("table", &e.table), ("object", &e.object), ("by", &e.by)]
    {
        if let Some(value) = value {
            map.insert(key.into(), json!(clip(value, 200)));
        }
    }
    if let Some(detail) = &e.detail {
        map.insert("detail".into(), json!(clip(detail, 400)));
    }
    if e.action.is_some() {
        map.insert("action".into(), json!(e.action));
    }
    if e.minor {
        map.insert("minor".into(), json!(true));
    }
    v
}

pub fn events(log: &culprit_engine::EventLog) -> Value {
    json!({
        "connection": log.connection,
        "start": log.start,
        "end": log.end,
        "events": log.events.iter().map(event).collect::<Vec<_>>(),
        "notes": log.notes,
    })
}

/// Each metric as average, worst and limit rather than every point; the episodes carry the times.
pub fn cloud(s: &culprit_core::CloudSignals) -> Value {
    let metrics: Vec<Value> = s
        .series
        .iter()
        .filter(|m| !m.points.is_empty())
        .map(|m| {
            let values: Vec<f64> = m.points.iter().map(|p| p.value).collect();
            let mean = values.iter().sum::<f64>() / values.len() as f64;
            let worst = if m.floor {
                values.iter().copied().fold(f64::INFINITY, f64::min)
            } else {
                values.iter().copied().fold(f64::NEG_INFINITY, f64::max)
            };
            let latest = m.points.last().map(|p| round(p.value, 2));
            json!({
                "metric": m.metric,
                "label": m.label,
                "unit": m.unit,
                "avg": round(mean, 2),
                if m.floor { "min" } else { "max" }: round(worst, 2),
                "latest": latest,
                "limit": m.limit.map(|l| round(l, 2)),
                "limit_label": m.limit_label,
            })
        })
        .collect();
    json!({
        "provider": s.provider,
        "service": s.service,
        "resource": s.resource,
        "tier": s.tier,
        "start": s.start,
        "end": s.end,
        "period_seconds": s.period_seconds,
        "summary": s.summary,
        "episodes": s.episodes,
        "metrics": metrics,
        "notes": s.notes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clipping_collapses_whitespace_and_marks_the_cut() {
        assert_eq!(clip("SELECT  a,\n  b\tFROM t", 100), "SELECT a, b FROM t");
        assert_eq!(clip("abcdef", 3), "abc…");
        assert_eq!(bare("[dbo].[Orders]"), "dbo.orders");
        assert_eq!(bare("`shop`.`orders`"), "shop.orders");
    }

    #[test]
    fn a_step_the_plan_puts_no_cost_on_gets_no_share() {
        // MySQL costs the scan and not the filesort above it: the scan isn't "100%" of the work, nor the hottest step.
        let text = std::fs::read_to_string(format!("{}/../../fixtures/mysql-demo.json", env!("CARGO_MANIFEST_DIR")))
            .expect("fixture");
        let snapshot: culprit_core::Snapshot = serde_json::from_str(&text).expect("snapshot");
        let plan =
            snapshot.workload.as_ref().and_then(|w| w.query("19bd4ff468839ea3")?.plan.clone()).expect("its plan");
        let tree = plan_tree(&plan);
        assert!(!tree.contains('%'), "{tree}");
        assert!(tree.contains("has no cost of its own in the plan, so no share is given"), "{tree}");
        let summary = plan_summary(&plan);
        assert!(summary["hottest"].is_null() && summary["hottest_cost_pct"].is_null(), "{summary}");
    }
}
