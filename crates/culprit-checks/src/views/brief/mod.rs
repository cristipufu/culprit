use std::fmt::Write;

use culprit_core::{
    Category, Engine, Finding, Fix, Metric, NodeKind, Plan, PlanNode, QueryRef, QueryStat, Report, Snapshot,
    SubjectKind, Unit, Workload, format,
};

use crate::queries::IdleKind;
use crate::support::{first_verb, main_statement, names_table, write_targets};

mod codebase;
mod plan_share;

pub use codebase::gone_after_rescan;
pub(crate) use plan_share::ABOUT_THE_PLAN;
pub use plan_share::{finding_no_share_reason, finding_step, finding_step_share, no_share_reason, plan_shares_note};

use codebase::codebase;

/// A self-contained markdown brief a coding agent can act on without access to the database.
pub fn agent_brief(report: &Report, finding: &Finding) -> String {
    let s = &report.snapshot;
    let mut out = String::new();
    let _ = writeln!(out, "# {}\n", finding.title);
    let _ = writeln!(
        out,
        "Database: **{}** ({}, database `{}`, {} environment). Finding `{}`: {}, {} impact, {} confidence. Scanned {}.\n",
        s.connection,
        s.server.product,
        s.server.database,
        format!("{:?}", s.environment).to_lowercase(),
        finding.id,
        finding.actionability.label(),
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
            // What it costs as every list of statements says it (`Workload::cost_of`): over Culprit's scans where they
            // measured it, with the totals beside where those differ twofold; when it ran as well as how often, a
            // burst on its day and a rate a day only where its runs came that way.
            let Some(cost) = s.workload.as_ref().map(|w| w.cost_of(q)) else { continue };
            let period_note = cost
                .activity
                .as_ref()
                .map(|_| {
                    let basis = cost.over.clone().unwrap_or_else(|| s.workload.as_ref().unwrap().window_phrase());
                    format!(" (share and average over {basis}; the observed calls have their own period)")
                })
                .unwrap_or_default();
            // One run or two far apart: what they took, never an average that hides it (`QueryStat::few_runs`).
            let took = q
                .few_runs_took()
                .map_or_else(|| format!("{} average", format::duration(cost.mean_ms)), |t| format!("which took {t}"));
            let _ = writeln!(
                out,
                "Query `{}`{} — {} of DB time, {}, {took}{period_note}{}{}:\n",
                q.id,
                q.object_name.as_deref().map(|o| format!(" in `{o}`")).unwrap_or_default(),
                format::percent(cost.share),
                cost.runs,
                cost.totals.map(|t| format!(" ({t})")).unwrap_or_default(),
                // A statement that stopped running: these are what it ran before, not what it runs now.
                if finding.inactive { " (it no longer runs)" } else { "" }
            );
            let _ = writeln!(out, "```sql\n{}\n```\n", q.text.trim());
            if let Some(plan) = &q.plan {
                plan_line(&mut out, finding, r, q, plan);
            }
        }
    }

    if let Some(fix) = &finding.fix {
        let _ = writeln!(out, "## Suggested fix\n\n{}\n", for_the_agent(&fix.summary));
        if let Some(sql) = &fix.sql {
            let _ = writeln!(out, "```sql\n{sql}\n```\n");
        }
        if let Some(cost) = &fix.cost {
            let _ = writeln!(out, "Cost: {cost}\n");
        }
    }
    if !finding.verify.is_empty() {
        let _ = writeln!(out, "## How to verify\n");
        // A finding names no connection (it is shared and exported); the brief is read where Culprit runs, so its
        // commands name this one.
        let command = crate::queries::WHAT_CHANGED_COMMAND;
        let named = command.replace("<connection>", &report.snapshot.connection);
        for v in &finding.verify {
            let _ = writeln!(out, "- {}", v.replace(command, &named));
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
    let _ = writeln!(out, "## What to do in the codebase\n\n{}", codebase(report, finding));
    out
}

/// The share of database time a finding's statement is said to take, on every surface that lists its statements (the
/// brief, the exports): the one its findings are weighed by ([`Workload::statement_share`]), over Culprit's scans where
/// they measured it, otherwise its time weighed by how long its plan was cached (`share_of`).
pub fn query_share(workload: &Workload, _finding: &Finding, q: &QueryStat) -> f64 {
    workload.statement_share(q)
}

/// "Plan: the most expensive step (64% of estimated cost) is `Seq Scan` on `public.orders` …": the step the reader looks
/// at, with its share only where the plan can state one, and why there is none where it can't.
fn plan_line(out: &mut String, finding: &Finding, r: &QueryRef, q: &QueryStat, plan: &Plan) {
    let Some(node) = finding_step(finding, r, plan) else { return };
    let on = node.object.as_deref().map(|o| format!(" on `{o}`")).unwrap_or_default();
    if !plan.weighed() {
        // A plan without costs (SQLite): the step the finding names, with no share it doesn't have, and what the step
        // does, which is its point ("USING AUTOMATIC COVERING INDEX").
        let _ = writeln!(
            out,
            "Plan: the step to look at is `{}`{on}{}.\n",
            node.op,
            node.detail.as_deref().map(|d| format!(": `{}`", d.replace('`', ""))).unwrap_or_default()
        );
        return;
    }
    let share = finding_step_share(plan, node);
    // "The most expensive" only for the plan's costliest step, and only where the plan can state its share. An exchange
    // (Gather Merge) is never it, so where one is costed higher the step is the costliest that works on the data.
    let exchange_costs_more =
        plan.nodes().iter().any(|n| n.kind == NodeKind::Parallelism && n.self_cost > node.self_cost);
    let what = match (share, plan.hottest()) {
        (Some(_), Some(h)) if h.id == node.id && exchange_costs_more => {
            "the most expensive step that works on the data"
        }
        (Some(_), Some(h)) if h.id == node.id => "the most expensive step",
        _ => "the step this finding is about",
    };
    let _ = write!(
        out,
        "Plan: {what}{} is `{}`{on}{}.",
        share.map(|s| format!(" ({} of estimated cost)", format::percent(s))).unwrap_or_default(),
        node.op,
        node.predicate.as_deref().map(|p| format!(" filtering `{}`", format::predicate(p))).unwrap_or_default()
    );
    if let Some(why) = finding_no_share_reason(plan, node, Some(q))
        .filter(|_| plan.uncosted_steps().is_empty() || !finding.summary.contains("no cost"))
    {
        let why = if plan.uncosted_steps().is_empty() {
            why.as_str()
        } else {
            "The database leaves some of this plan's work without a cost estimate, so no step is given a share."
        };
        let _ = write!(out, " {why}");
    }
    let _ = writeln!(out, "\n");
}

/// Fix text written for a person reading the finding, said to the coding agent reading the brief: it is the brief
/// already, so it isn't told to copy one.
fn for_the_agent(text: &str) -> String {
    text.replace(" (copy the brief for your coding agent)", "").replace("(copy the brief for your coding agent) ", "")
}

/// Metrics that total a wait: said to the second under an hour (`signals::total_wait`), the way their findings' titles
/// and summaries say them ("2 min 32 s", where a rounded "3 min" would disagree with the title above it).
pub const TOTAL_WAIT_METRICS: &[&str] = &["Time waiting", "Time waiting since start"];

/// A finding's metric as its *Evidence* line says it, on every surface that lists one (the brief, the exports).
pub fn metric_text(m: &Metric) -> String {
    match m.unit {
        Unit::Ms if TOTAL_WAIT_METRICS.contains(&m.label.as_str()) => crate::signals::total_wait(m.value),
        Unit::Ms => format::duration(m.value),
        Unit::Bytes => format::bytes(m.value),
        Unit::Ratio => format::percent(m.value),
        // "Counting for: 9.5 days" as the summary says it, never rounded up past what was counted.
        Unit::Days => format::days(m.value),
        Unit::Text => m.text.clone().unwrap_or_default(),
        Unit::Count | Unit::Rows | Unit::Pages => format::count(m.value),
    }
}
