//! Compact, agent-shaped JSON for reports, plans, sessions and result sets. Long database text is clipped so a single
//! tool result never floods the agent's context.

use std::collections::BTreeMap;

use culprit_core::{
    Category, Error, Finding, LiveState, Plan, PlanNode, QueryStat, Report, Result, ResultSet, Severity, Workload,
    format,
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
        "hours": round(w.window_hours, 1),
        "cumulative_since_stats_reset": !w.windowed,
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
    json!({
        "id": q.id,
        "db_time_pct": pct(w.share(q.total_time_ms)),
        "runs_per_day": round(w.per_day(q.executions), 1),
        "avg_ms": round(q.mean_time_ms, 3),
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
        return json!({ "scan": scan_header(report), "queries": [], "note": "This login can't read query statistics." });
    };
    let key = |q: &QueryStat| match by {
        TopBy::Time => q.total_time_ms,
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
    let w = report.snapshot.workload.as_ref().ok_or_else(|| Error::not_found("This scan has no query statistics"))?;
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
    row["plan_variants"] = json!(
        q.plans
            .iter()
            .map(|p| json!({
                "plan_id": p.id,
                "executions": p.executions,
                "avg_ms": round(p.mean_time_ms, 3),
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
    let hot = plan.hottest();
    json!({
        "kind": plan.kind,
        "hottest": node_label(hot),
        "hottest_cost_pct": share(plan, hot).map(pct),
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

fn share(plan: &Plan, node: &PlanNode) -> Option<f64> {
    (plan.total_cost > 0.0).then(|| node.self_cost / plan.total_cost)
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
        line.push_str(&format!("  rows est {}", format::count(node.est_rows_total())));
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
    out
}

pub fn plan(plan: &Plan) -> Value {
    json!({
        "engine": plan.engine,
        "kind": plan.kind,
        "total_cost": round(plan.total_cost, 4),
        "operators": plan.nodes().len(),
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
        .map(|f| json!({ "name": f.name, "columns": f.columns, "references": format!("{}.{}({})", f.ref_schema, f.ref_table, f.ref_columns.join(", ")) }))
        .collect();
    let planned = s.workload.as_ref().map(|w| w.queries.iter().filter(|q| q.plan.is_some()).count()).unwrap_or(0);
    Ok(json!({
        "scan": scan_header(report),
        "table": format!("{}.{}", t.schema, t.name),
        "rows": t.rows,
        "size": t.total_bytes.map(format::bytes),
        "writes": json!({ "inserts": t.inserts, "updates": t.updates, "deletes": t.deletes }),
        "columns": columns,
        "indexes": indexes,
        "foreign_keys": foreign_keys,
        "note": format!("used_by_captured_plans covers the {planned} query plans captured in this scan; an index missing there may still serve other queries."),
    }))
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
                "db_time_pct": pct(w.share(q.total_time_ms)),
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
}
