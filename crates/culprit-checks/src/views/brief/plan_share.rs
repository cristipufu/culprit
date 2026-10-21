//! Plan step selection and step cost-share explanations shared by the agent brief, plan viewer (`Plan.view`), CLI tree,
//! and exports.

use super::*;

/// Checks whose findings are about where a statement's plan does its work, so the plan's working step is worth naming
/// when the finding names none. A finding about something else (a result's size, how often a statement runs, what it
/// waits for, its locks, a table's rows) names its step itself or has none, and so does a plan insight: one that names
/// no step is about the whole plan (it runs on one CPU, the optimizer gave up, its compile).
pub(crate) const ABOUT_THE_PLAN: &[&str] = &[
    "heavy-query",
    "regression",
    "plan-regression",
    "parameter-sensitive",
    "plan-warnings",
    "temp-spill",
    "memory-grant",
    "missing-index",
    "filtered-scan",
    "scan-heavy",
    "missing-histogram",
    "tuning-advice",
];

/// The step of `plan` a reader of `finding` looks at, for the statement `r` refers to: the step the finding names, else,
/// for a finding about where a statement's plan does its work, the plan's working step, where the plan can state its
/// share. None for a finding about something else (a result's size, how often a statement runs, what it waits for): the
/// plan's costliest step isn't what it is about, and naming it would point the reader at a step the finding never
/// weighed. Every surface that shows a finding's step (the brief, the exports) picks it here.
pub fn finding_step<'a>(finding: &Finding, r: &QueryRef, plan: &'a Plan) -> Option<&'a PlanNode> {
    if let Some(named) = r.plan_node.and_then(|id| plan.nodes().into_iter().find(|n| n.id == id)) {
        return Some(named);
    }
    if !ABOUT_THE_PLAN.contains(&finding.check.as_str()) {
        return None;
    }
    culprit_core::insights::working_step(plan).filter(|n| plan.step_share(n).is_some())
}

/// The share of its plan's cost a finding's step is said to have: `Plan::step_share`, which states one only in a plan
/// that does real work, as heavy-query says it (a cheap plan's costliest step isn't where its statement's time goes,
/// even where it reads a whole table), and never for a step whose cost rests on a guess. Every surface that shows a
/// finding's step (the brief, the exports, the app's finding view) states this share.
pub fn finding_step_share(plan: &Plan, node: &PlanNode) -> Option<f64> {
    plan.step_share(node)
}

/// Said for a step planned on a default guess for a parameter, or above one.
const GUESSED_ROWS: &str = "The rows it works on rest on the planner's default guess for a parameter, not on the table's statistics, so no \
     share is given.";
/// Said for a step a loop runs once for each row of a step planned on such a guess.
const GUESSED_RUNS: &str = "It runs once for each row of a step whose rows rest on the planner's default guess for a \
                            parameter, so how often it runs is that guess, and no share is given.";

/// Why a plan that weighs its steps states no share of its cost for step `node` (`Plan::step_share`), as a sentence to
/// follow the step's name; `None` where it states one, or on a plan without costs (which has no shares to explain).
/// The true reason, with what the statement `q`'s runs measured where it is known: a plan costed for far fewer rows
/// than its runs handle isn't "cheap", and nor is one that reads a whole table big enough to matter on every run (a
/// catalog's or a small table's whole read is no reason, and isn't said). Every surface that says
/// why a step has no share (the brief, the exports, the plan viewer, the CLI's tree) says it from here; the app
/// consumes it through `Plan.view`.
pub fn no_share_reason(plan: &Plan, node: &PlanNode, q: Option<&QueryStat>) -> Option<String> {
    if !plan.weighed() || plan.step_share(node).is_some() {
        return None;
    }
    let uncosted = plan.uncosted_steps();
    if !uncosted.is_empty() {
        let steps: Vec<String> = uncosted.iter().map(|n| step_name(n)).collect();
        let (has, its) = if steps.len() == 1 { ("has", "its") } else { ("have", "their") };
        return Some(format!("{} {has} no cost of {its} own in the plan, so no share is given.", format::list(&steps)));
    }
    match plan.guess(node) {
        Some(culprit_core::Guess::Rows) => return Some(GUESSED_ROWS.into()),
        Some(culprit_core::Guess::Runs) => return Some(GUESSED_RUNS.into()),
        None => {}
    }
    if !culprit_core::insights::does_real_work(plan) {
        return Some(cheap_plan_reason(plan, q));
    }
    Some(format!("{} has no cost of its own in the plan, so no share is given.", step_name(node)))
}

/// Why a finding's step states no share ([`finding_step_share`]): [`no_share_reason`], except that a step the finding
/// is about which reads its table whole on a cheap plan is said as that read, whatever the table's size. The finding is
/// what shows that read matters; the size bar of `insights::whole_read_matters` is for plans no finding points at.
/// Where its runs measured a misestimate, that stays the reason. Included in the app's `Plan.view` projection.
pub fn finding_no_share_reason(plan: &Plan, node: &PlanNode, q: Option<&QueryStat>) -> Option<String> {
    use culprit_core::insights::CheapPlanRuns;
    let why = no_share_reason(plan, node, q)?;
    let cheap = plan.uncosted_steps().is_empty() && !plan.has_guess() && !culprit_core::insights::does_real_work(plan);
    let misestimated = matches!(
        culprit_core::insights::cheap_plan_runs(plan, q),
        CheapPlanRuns::MoreRows { .. } | CheapPlanRuns::MoreReads { .. }
    );
    Some(if cheap && !misestimated && node.reads_table_whole() { whole_read_reason(plan, node) } else { why })
}

/// "…too small for a share…, but `Seq Scan` reads all 3.9K rows of public.apps on every run."
fn whole_read_reason(plan: &Plan, scan: &PlanNode) -> String {
    let what = scan.object.as_deref().unwrap_or("its table");
    let all = match scan.table_rows.filter(|r| *r > 0.0) {
        Some(n) => format!("all {} row{} of {what}", format::count(n), if format::count(n) == "1" { "" } else { "s" }),
        None => format!("all of {what}"),
    };
    format!(
        "Its estimated cost ({}) is too small for a share of it to say where the time goes, but `{}` reads {all} on \
         every run.",
        cost_text(plan.total_cost.max(plan.root.subtree_cost)),
        scan.op
    )
}

/// Why no step of a plan that weighs its steps shows a share of its cost, said once under the plan wherever its steps
/// are listed (the plan viewer, the CLI's tree, the exports, MCP, the readings); `None` when every costed step shows
/// one. `q` is the statement it was planned for, where known: its runs say whether a cheap plan's runs are cheap.
pub fn plan_shares_note(plan: &Plan, q: Option<&QueryStat>) -> Option<String> {
    if !plan.weighed() {
        return None;
    }
    if !plan.uncosted_steps().is_empty() {
        return no_share_reason(plan, &plan.root, q);
    }
    if !culprit_core::insights::does_real_work(plan) {
        return Some(cheap_plan_reason(plan, q));
    }
    let guesses = plan.guesses();
    if guesses.is_empty() {
        return None;
    }
    Some(if guesses.iter().any(|(_, g)| *g == culprit_core::Guess::Runs) {
        "No share is given for a step whose rows rest on the planner's default guess for a parameter, nor for the steps \
         above it or run once for each of its rows."
            .into()
    } else {
        "No share is given for a step whose rows rest on the planner's default guess for a parameter, nor for the steps \
         above it."
            .into()
    })
}

/// Why a plan that does no real work puts a share of its cost on no step, as its statement's runs measured it
/// (`insights::cheap_plan_runs`): its costs rest on a guess, on an estimate its runs far exceed, or under the bar while
/// a scan reads a whole table that can matter (`insights::whole_read_matters`: `SCAN_ROWS` rows or more, not the
/// catalog's), or its runs are slow; else the plan is cheap.
fn cheap_plan_reason(plan: &Plan, q: Option<&QueryStat>) -> String {
    use culprit_core::insights::CheapPlanRuns;
    // Whether the plan is cheap rests on its total, and a guess anywhere is in the total.
    if plan.has_guess() {
        return "The plan's costs rest on the planner's default guess for a parameter, so no share of them is put on \
                one step."
            .into();
    }
    let mysql = plan.engine == Engine::Mysql;
    let rows = |n: f64| format!("{} row{}", format::count(n), if format::count(n) == "1" { "" } else { "s" });
    match culprit_core::insights::cheap_plan_runs(plan, q) {
        CheapPlanRuns::MoreRows { expected, rows: actual } => format!(
            "Its costs rest on an estimate of {} a run, and its runs {} far more ({} a run), so no share of them is put \
             on one step.",
            rows(expected),
            if q.is_some_and(QueryStat::writes_rows) { "change" } else { "return" },
            rows(actual)
        ),
        CheapPlanRuns::MoreReads { reads } => format!(
            "Its runs read {} {} a run; the plan's estimated cost is too small to say where that reading happens, so \
             no share of it is put on one step.",
            format::count(reads.round()),
            if mysql { "rows examined" } else { "pages" }
        ),
        CheapPlanRuns::ScansTable(id) => match plan.nodes().into_iter().find(|n| n.id == id) {
            Some(scan) => whole_read_reason(plan, scan),
            None => format!(
                "Its estimated cost ({}) is too small for a share of it to say where the time goes, but a scan reads all \
                 of its table on every run.",
                cost_text(plan.total_cost.max(plan.root.subtree_cost))
            ),
        },
        CheapPlanRuns::Slow(ms) => format!(
            "The plan is estimated to be cheap, yet {}: the time isn't in what it costs, so no share of its cost is put \
             on one step.",
            plan_run_times(q, ms)
        ),
        CheapPlanRuns::Cheap | CheapPlanRuns::Unmeasured | CheapPlanRuns::NotCheap => {
            "The plan as a whole is cheap, so no share of its cost is put on one step.".into()
        }
    }
}

/// What the captured plan's completed runs took. A handful with one outlier is said as a total and a maximum,
/// rather than an average that describes none of them. Past tense also holds for statements that stopped.
fn plan_run_times(q: Option<&QueryStat>, mean: f64) -> String {
    let Some(q) = q else { return format!("its runs averaged {}", format::duration(mean)) };
    let (runs, total, max) = match q.captured_plan_variant() {
        Some(p) => (p.executions, p.total_time_ms, p.max_time_ms),
        None => (q.executions, q.total_time_ms, q.max_time_ms),
    };
    if runs == 1.0 {
        return format!("its one run took {}", format::duration(total));
    }
    if let Some(max) = max.filter(|_| total > 0.0).map(|m| m.clamp(0.0, total)) {
        if runs == 2.0 && max >= (total - max) * 2.0 {
            return format!("its runs took {} and {}", format::duration(max), format::duration(total - max));
        }
        if runs <= 10.0 && max >= mean * 2.0 {
            return format!(
                "its {} runs took {} in all, with the slowest taking {}",
                format::count(runs),
                format::duration(total),
                format::duration(max)
            );
        }
    }
    format!("its runs averaged {}", format::duration(mean))
}

/// A plan's estimated cost as the engine writes it: whole units from 10, else three significant digits (SQL Server's
/// point lookups cost hundredths).
fn cost_text(cost: f64) -> String {
    if cost >= 10.0 {
        format!("{cost:.0}")
    } else {
        let digits = (2 - cost.max(1e-9).log10().floor() as i32).clamp(0, 6) as usize;
        format!("{cost:.digits$}")
    }
}

/// "`Sort` (using filesort)": a step and what it does.
fn step_name(node: &PlanNode) -> String {
    match node.detail.as_deref() {
        Some(detail) => format!("`{}` ({})", node.op, detail.replace('`', "")),
        None => format!("`{}`", node.op),
    }
}
