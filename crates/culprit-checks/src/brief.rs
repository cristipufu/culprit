use std::fmt::Write;

use culprit_core::{Finding, Metric, Report, Unit, format};

/// A self-contained markdown brief a coding agent can act on without access to the database.
pub fn agent_brief(report: &Report, finding: &Finding) -> String {
    let s = &report.snapshot;
    let mut out = String::new();
    let _ = writeln!(out, "# {}\n", finding.title);
    let _ = writeln!(
        out,
        "Database: **{}** ({}, database `{}`, {} environment). Finding `{}`: {} impact, {} confidence. Scanned {}.\n",
        s.connection,
        s.server.product,
        s.server.database,
        format!("{:?}", s.environment).to_lowercase(),
        finding.id,
        format!("{:?}", finding.severity).to_lowercase(),
        format!("{:?}", finding.confidence).to_lowercase(),
        s.taken_at
    );
    let _ = writeln!(out, "## What's happening\n\n{}\n", finding.summary);

    if !finding.metrics.is_empty() {
        let _ = writeln!(out, "## Evidence\n");
        for m in &finding.metrics {
            let _ = writeln!(out, "- {}: {}", m.label, metric_text(m));
        }
        let _ = writeln!(out);
    }

    let queries: Vec<_> =
        finding.queries.iter().filter_map(|r| s.workload.as_ref()?.query(&r.id).map(|q| (r, q))).take(5).collect();
    if !queries.is_empty() {
        let _ = writeln!(out, "## Queries involved\n");
        for (r, q) in queries {
            let share = s.workload.as_ref().map(|w| w.share(q.total_time_ms)).unwrap_or(0.0);
            // Per day only over a day of data; under that, the measured runs and their window.
            let how_often = s
                .workload
                .as_ref()
                .map(|w| w.how_often(q.executions))
                .unwrap_or_else(|| format!("{} runs", format::count(q.executions)));
            let _ = writeln!(
                out,
                "Query `{}`{} — {} of DB time, {how_often}, {} average:\n",
                q.id,
                q.object_name.as_deref().map(|o| format!(" in `{o}`")).unwrap_or_default(),
                format::percent(share),
                format::duration(q.mean_time_ms)
            );
            let _ = writeln!(out, "```sql\n{}\n```\n", q.text.trim());
            if let Some(plan) = &q.plan {
                let node = r
                    .plan_node
                    .and_then(|id| plan.nodes().into_iter().find(|n| n.id == id))
                    .unwrap_or_else(|| plan.hottest());
                let pct = if plan.total_cost > 0.0 { node.self_cost / plan.total_cost } else { 0.0 };
                let _ = writeln!(
                    out,
                    "Plan: the most expensive step ({} of estimated cost) is `{}`{}{}.\n",
                    format::percent(pct),
                    node.op,
                    node.object.as_deref().map(|o| format!(" on `{o}`")).unwrap_or_default(),
                    node.predicate
                        .as_deref()
                        .map(|p| format!(" filtering `{}`", format::predicate(p)))
                        .unwrap_or_default()
                );
            }
        }
    }

    if let Some(fix) = &finding.fix {
        let _ = writeln!(out, "## Suggested fix\n\n{}\n", fix.summary);
        if let Some(sql) = &fix.sql {
            let _ = writeln!(out, "```sql\n{sql}\n```\n");
        }
        if let Some(cost) = &fix.cost {
            let _ = writeln!(out, "Cost: {cost}\n");
        }
    }
    if !finding.verify.is_empty() {
        let _ = writeln!(out, "## How to verify\n");
        for v in &finding.verify {
            let _ = writeln!(out, "- {v}");
        }
        let _ = writeln!(out);
    }
    if !finding.caveats.is_empty() {
        let _ = writeln!(out, "## Caveats\n");
        for c in &finding.caveats {
            let _ = writeln!(out, "- {}", c.replace('\n', "\n  "));
        }
        let _ = writeln!(out);
    }
    let _ = writeln!(
        out,
        "## What to do in the codebase\n\nFind where the application issues the queries above (search for their table and column names), implement the fix the way this project ships schema and query changes (a migration, an ORM change, or a query rewrite), and keep production-safe options such as ONLINE/CONCURRENTLY. After deploying, run `culprit scan {}` and confirm `{}` no longer appears.",
        s.connection, finding.id
    );
    out
}

fn metric_text(m: &Metric) -> String {
    match m.unit {
        Unit::Ms => format::duration(m.value),
        Unit::Bytes => format::bytes(m.value),
        Unit::Ratio => format::percent(m.value),
        Unit::Days => format!("{:.0} days", m.value),
        Unit::Text => m.text.clone().unwrap_or_default(),
        Unit::Count | Unit::Rows | Unit::Pages => format::count(m.value),
    }
}
