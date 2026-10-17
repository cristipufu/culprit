use std::fmt::Write;

use culprit_core::{
    Category, Engine, Finding, Fix, Metric, NodeKind, Plan, PlanNode, QueryRef, QueryStat, Report, Snapshot,
    SubjectKind, Unit, Workload, format,
};

use crate::queries::IdleKind;
use crate::support::{first_verb, main_statement, names_table, write_targets};

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
            // What it costs as every list of statements says it (`Workload::cost_of`): over Culprit's scans where they
            // measured it, with the totals beside where those differ twofold; when it ran as well as how often, a
            // burst on its day and a rate a day only where its runs came that way.
            let Some(cost) = s.workload.as_ref().map(|w| w.cost_of(q)) else { continue };
            // One run or two far apart: what they took, never an average that hides it (`QueryStat::few_runs`).
            let took = q
                .few_runs_took()
                .map_or_else(|| format!("{} average", format::duration(cost.mean_ms)), |t| format!("which took {t}"));
            let _ = writeln!(
                out,
                "Query `{}`{} — {} of DB time, {}, {took}{}{}:\n",
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
    if let Some(why) = finding_no_share_reason(plan, node, Some(q)) {
        let _ = write!(out, " {why}");
    }
    let _ = writeln!(out, "\n");
}

/// Checks whose findings are about where a statement's plan does its work, so the plan's working step is worth naming
/// when the finding names none. A finding about something else (a result's size, how often a statement runs, what it
/// waits for, its locks, a table's rows) names its step itself or has none, and so does a plan insight: one that names
/// no step is about the whole plan (it runs on one CPU, the optimizer gave up, its compile).
const ABOUT_THE_PLAN: &[&str] = &[
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
/// mirrors it (`noShareReason` in `app/src/lib/plan-format.ts`).
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
/// Where its runs measured a misestimate, that stays the reason. Mirrored by the app's `findingNoShareReason`.
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
        CheapPlanRuns::MoreReads { expected, reads, .. } => format!(
            "Its costs rest on an estimate of {} a run, and its runs read far more than that needs ({} {} a run), so no \
             share of them is put on one step.",
            rows(expected),
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
            // What a run or two took, where an average would hide it (`QueryStat::few_runs`).
            match q.and_then(|q| Some((q.executions, q.few_runs_took()?))) {
                Some((runs, took)) if runs <= 1.0 => format!("its one run took {took}"),
                Some((_, took)) => format!("its runs took {took}"),
                None => format!("its runs take {} on average", format::duration(ms)),
            }
        ),
        CheapPlanRuns::Cheap | CheapPlanRuns::Unmeasured | CheapPlanRuns::NotCheap => {
            "The plan as a whole is cheap, so no share of its cost is put on one step.".into()
        }
    }
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

/// What a coding agent does about the finding in the project: where the change lives (the application's statements, its
/// schema, the server's settings, the database's upkeep, the connection pool) and whether there is one to make. Only for
/// a finding about what the database holds now (a setting, an index, statistics) does it say the next scan should no
/// longer have it: a count kept since a reset, a forced plan that helps or a statistic no query reads stays on the list
/// after the right change, so the check is the finding's own "How to verify", or what a later scan can show.
fn codebase(report: &Report, finding: &Finding) -> String {
    let s = &report.snapshot;
    let statements: Vec<&QueryStat> =
        finding.queries.iter().filter_map(|r| s.workload.as_ref()?.query(&r.id)).collect();
    // What a driver or a pool sends isn't in the application's code (`is_session_statement`).
    let app: Vec<&QueryStat> =
        statements.iter().copied().filter(|q| !culprit_core::sql::is_session_statement(&q.text)).collect();
    let once = !finding.inactive && !app.is_empty() && app.iter().all(|q| crate::ran_once(q));
    let sql = FixSql::read(finding.fix.as_ref().and_then(|f| f.sql.as_deref()).unwrap_or_default(), s.server.engine);
    let mut said: Vec<String> = Vec::new();
    if finding.inactive {
        // Unchanged since Culprit's first scan, that scan is only the latest they can have run: they may have stopped
        // long before it, and the deploy that changed them may be an earlier one.
        let by_first_scan = finding
            .last_seen
            .as_deref()
            .zip(s.workload.as_ref())
            .is_some_and(|(last, w)| crate::refine::last_ran_by_first_scan(w, last));
        said.push(format!(
            "{} Before changing anything, find the code that sends them today: if it no longer does, or no longer has \
             the problem, there is nothing to do.",
            if by_first_scan {
                "The statements behind this finding haven't run since Culprit's first scan, and may have stopped long \
                 before it: a deploy before that scan may have changed or fixed them."
            } else {
                "The statements behind this finding stopped running before the scan's data ends: a deploy may have \
                 changed or fixed them."
            }
        ));
    } else if once {
        said.push(format!(
            "{} once in {}: an export, a backfill or a query someone ran by hand looks like this. Change nothing unless \
             it keeps running.",
            if app.len() == 1 { "The statement behind this finding ran" } else { "Each statement behind this finding ran" },
            s.workload.as_ref().map(|w| w.window_phrase()).unwrap_or_else(|| "the scan's window".into())
        ));
    } else {
        where_the_change_is(s, finding, &sql, &statements, &app, &mut said);
    }
    // A fix that asks for no change has none to wait for: the next scan shows where the finding stands.
    let declines = !finding.inactive && !once && finding.fix.as_ref().is_some_and(advises_no_change);
    said.push(verify_line(s, finding, &sql, &statements, once, declines));
    said.retain(|s| !s.is_empty());
    said.join(" ")
}

/// The sentences for a finding whose statements still run: where its fix is made.
fn where_the_change_is(
    s: &Snapshot,
    finding: &Finding,
    sql: &FixSql,
    statements: &[&QueryStat],
    app: &[&QueryStat],
    said: &mut Vec<String>,
) {
    let fix = finding.fix.as_ref();
    let declines = fix.is_some_and(advises_no_change);
    // An index advised for statements that stay as they are: the index is the whole change.
    let index_only = finding.category == Category::Indexes && sql.only_schema();
    // The statements to find in the code, and how a sentence refers back to them.
    let (these, their, them) = match (app.is_empty(), quotes_its_statement(finding)) {
        (false, _) => ("the statements above", "their", "them"),
        (true, true) => ("the statement quoted under *Caveats*", "its", "it"),
        (true, false) => ("the statements this finding is about", "their", "them"),
    };
    if app.is_empty() && !statements.is_empty() {
        said.push(
            "The statements above come from the driver or the connection pool, not from the application's own code, \
             so there is no query to find: the change is in how the project sets up its pool (its connection string or \
             pool options)."
                .into(),
        );
    } else if let Some(waited) = waited_rather_than_worked(s, finding, app).filter(|_| !app.is_empty()) {
        said.push(waited.sentence(s, app));
    } else if sql.fallback {
        said.push(format!(
            "The change the fix prefers is in the application's code: find where it sends {these} (search for {their} \
             table and column names) and change {them} the way this project ships query changes (an ORM change or a \
             query rewrite). The fix's SQL is only the fallback it names, for when the application can't change."
        ));
    } else if !app.is_empty() && !declines && !sql.in_the_application && !sql.only_runs_on_the_database() && !index_only
    {
        said.push(
            "Where the fix changes the statements above, find where the application sends them (search for their \
             table and column names) and change them the way this project ships query changes (an ORM change or a \
             query rewrite)."
                .into(),
        );
    }
    if declines {
        said.push(if sql.schema {
            format!(
                "The fix doesn't recommend this change as things stand: make no schema change for this finding unless \
                 the condition the fix gives holds, and then ship it {}",
                schema_shipping(s, sql)
            )
        } else {
            "The fix asks for no change as things stand: there is nothing to do in the codebase for this finding."
                .into()
        });
        return;
    }
    let configuration = finding.category == Category::Configuration && !(sql.upkeep || sql.schema || sql.plan_pin);
    // The setting is the whole fix: said as "the fix is", not "the setting in the fix's SQL".
    let whole = (configuration || app.is_empty()) && !sql.fallback;
    if configuration && !sql.settings() {
        said.push(Scope::Server.sentence(true, sql));
    }
    for (scope, sets) in
        [(Scope::Server, sql.setting), (Scope::Database, sql.database_setting), (Scope::Role, sql.role_setting)]
    {
        if sets {
            said.push(scope.sentence(whole, sql));
        }
    }
    if sql.local {
        said.push(format!(
            "{} lasts only until the end of the transaction it runs in, so it goes in the application's code, inside \
             the transaction that runs the statement, just before it.",
            if sql.settings() || configuration { "`SET LOCAL` is the exception: it" } else { "The fix's `SET LOCAL`" }
        ));
    }
    if sql.upkeep {
        // Only SQLite's PRAGMA optimize is made to run as connections close: other upkeep reads whole tables.
        let on_close = fix.is_some_and(|f| {
            [Some(&f.summary), f.sql.as_ref()]
                .into_iter()
                .flatten()
                .any(|t| t.to_ascii_lowercase().contains("pragma optimize"))
        });
        // A fix that says the database makes the upkeep itself ("No update is needed…") offers it, not asks for it.
        if fix.is_some_and(offers_optional_upkeep) {
            said.push(
                "The fix's upkeep is optional, as it says: the database makes it on its own. If you run it, run it \
                 against the database outside busy hours; it isn't a change to the application's code."
                    .into(),
            );
        } else {
            said.push(format!(
                "{} run it against the database outside busy hours, and if the fix says to repeat it, do that where \
                 this project runs its recurring database work ({}).",
                if sql.only_upkeep() {
                    "The fix is upkeep of the database, not a change to the application's code:"
                } else {
                    "The upkeep in the fix's SQL (statistics, vacuuming, a rebuild) isn't a change to the application's \
                     code:"
                },
                if on_close {
                    "a scheduled job, or where the application opens and closes its connections, which is where \
                     SQLite's `PRAGMA optimize` belongs"
                } else {
                    "a scheduled job"
                }
            ));
        }
    }
    if sql.plan_pin {
        said.push(
            "The fix's Query Store procedure runs against the database (Query Store keeps the plan it forces or the \
             hint it sets), not in the application's code: run it there."
                .into(),
        );
    }
    if sql.event_session {
        // How to stop the session, where the SQL says it (its last line, a comment).
        let stop = if sql.session_drop {
            " Once it has caught what you need, remove it with the DROP in the SQL's last line."
        } else {
            ""
        };
        said.push(if sql.other {
            format!(
                "The fix's event session runs on the database, not in the application's code: start it there; it \
                 changes nothing by itself.{stop}"
            )
        } else {
            format!(
                "The fix's SQL starts an event session on the database{}: run it there, not in the application's code; \
                 it changes nothing by itself.{stop} The statement to change is the one it catches: find the code that \
                 sends that one (the application, a job or a migration) and change it the way the fix says.",
                if sql.reads { " and reads what it catches" } else { "" }
            )
        });
    } else if sql.reads {
        said.push(
            "The fix's query only reads: run it against the database and act on what it shows, the way the fix says; \
             it changes nothing by itself."
                .into(),
        );
    }
    if sql.schema {
        let what = if sql.index { "index change" } else { "schema change" };
        said.push(format!("Ship the fix's {what} {}", schema_shipping(s, sql)));
    }
    if sql.in_the_application {
        // The fix's SQL offers a change in the application among its options: its comments say which.
        let other = sql.settings() || sql.upkeep || sql.plan_pin || sql.schema || sql.diagnostic() || sql.other;
        said.push(format!(
            "The fix's {} in the application's code, as its SQL's comments say: find where the application sends \
             {these} (search for {their} table and column names) and change {them} the way this project ships query \
             changes (an ORM change or a query rewrite).",
            if other { "other option is a change" } else { "change is" }
        ));
    }
    if said.is_empty() && app.is_empty() && matches!(finding.subject.kind, SubjectKind::Table | SubjectKind::Index) {
        // Every table the finding is about, and the statements its summary quotes, if it quotes any (a deadlock's).
        let tables = finding_tables(finding);
        let names = format::list(&tables.iter().map(|t| format!("`{t}`")).collect::<Vec<_>>());
        said.push(if finding.summary.contains('“') {
            format!(
                "The statements to look for are quoted under *What's happening*: find the code that sends them \
                 (search for {names}) and make the change the fix describes there."
            )
        } else {
            format!(
                "Where the fix changes how the application reads or writes {names}, search the code for the \
                 statements that touch {}.",
                if tables.len() == 1 { "it" } else { "them" }
            )
        });
    }
    if said.is_empty() {
        said.push(if quotes_its_statement(finding) {
            "The statement to look for is quoted under *Caveats*, as recorded: find where the application sends it \
             (search for its table and column names) and make the change the fix describes there."
                .into()
        } else {
            "No statement is listed under *Queries involved*: the fix above says where the change is.".into()
        });
    }
}

/// What a setting the fix's SQL makes applies to, which says where it is changed.
#[derive(Clone, Copy)]
enum Scope {
    /// The server (`ALTER SYSTEM`, `SET PERSIST`, `SET GLOBAL`, `sp_configure`), or the platform's control for it.
    Server,
    /// One database, kept in it (`ALTER DATABASE … SET`, a SQL Server database option, `ALTER DATABASE SCOPED
    /// CONFIGURATION`).
    Database,
    /// One role's new connections (`ALTER ROLE … SET`).
    Role,
}

impl Scope {
    /// Where the change is made: the server's configuration, or SQL run once against the database (by its owner or a
    /// migration), which a parameter group or configuration file must not take over for the whole server. `whole`: the
    /// setting is the fix, not one statement of its SQL.
    fn sentence(self, whole: bool, sql: &FixSql) -> String {
        let server = "the server's configuration (infrastructure as code, the platform's parameter group, a configuration \
                      file), which is for settings of the whole server";
        // Where nothing manages the server's configuration, the change is made by hand: the fix's SQL, run by a login
        // allowed to change the server's settings, or the steps the fix gives where it has no SQL for them.
        let by_hand = match (sql.setting, whole) {
            (true, true) => {
                "Where nothing manages it, run the fix's SQL against the server as a login allowed to change its \
                 settings, the way the fix says."
            }
            (true, false) => {
                "Where nothing manages it, run the setting's statements from the fix's SQL against the server as a \
                 login allowed to change its settings."
            }
            (false, _) => "Where nothing manages it, make the change by hand the way the fix says.",
        };
        match self {
            Scope::Server if whole => format!(
                "The fix is a setting of the database server, not a change to the application's code: apply it where \
                 this project manages the server's configuration (infrastructure as code, the platform's parameter \
                 group, the server's configuration file). {by_hand}"
            ),
            Scope::Server => format!(
                "The setting in the fix's SQL belongs to the database server, not to the application's code: apply it \
                 where this project manages the server's configuration (infrastructure as code, the platform's \
                 parameter group, the server's configuration file). {by_hand}"
            ),
            Scope::Database => format!(
                "{} once against the database as its owner, by hand or shipped as a migration. It isn't a change to \
                 the application's code, nor to {server}.",
                if whole {
                    "The fix is a setting kept in this database alone: run its SQL"
                } else {
                    "The database setting in the fix's SQL (`ALTER DATABASE`) is kept in this database alone: run it"
                }
            ),
            Scope::Role => format!(
                "The role setting in the fix's SQL (`ALTER ROLE … SET`) applies to that role's new connections alone: run \
                 it once against the database, as that role or one allowed to change it, by hand or shipped as a \
                 migration{}. It isn't a change to the application's code, nor to {server}.",
                if sql.app_role {
                    ", with the role the application connects as (its connection string names it) in place of \
                     `<app_role>`"
                } else {
                    ""
                }
            ),
        }
    }
}

/// "the way this project ships schema changes (a migration), keeping …": the options the fix's SQL already has, and what
/// they need from a migration.
fn schema_shipping(s: &Snapshot, sql: &FixSql) -> String {
    let mut out = "the way this project ships schema changes (a migration), keeping the options the fix's SQL already \
                   has."
        .to_string();
    if sql.concurrently {
        out.push_str(
            " `CONCURRENTLY` can't run inside a transaction, and most migration tools wrap each migration in one: run \
             this one without it (Rails `disable_ddl_transaction!`, Django `atomic = False`, a non-transactional Flyway \
             migration).",
        );
    }
    // A file the application opens itself: building an index holds its write lock to the end.
    if sql.creates_index && !s.server.engine.has_server() {
        let _ = write!(
            out,
            " {} builds an index under the database's write lock, so the application's writes wait until it finishes: \
             run it when writes are quiet.",
            s.server.engine.label()
        );
    }
    out
}

/// What a later scan can show after the change, and how to look: the finding's own *How to verify*, else its numbers
/// beside this scan's. Where the source keeps totals since a reset, the runs from before the change stay in what any of
/// them reads, whatever the finding says to look at; a statement that stopped running has nothing to check until it
/// runs again.
fn verify_line(
    s: &Snapshot,
    finding: &Finding,
    sql: &FixSql,
    statements: &[&QueryStat],
    once: bool,
    declines: bool,
) -> String {
    let (scan, id) = (format!("`culprit scan {}`", s.connection), &finding.id);
    let how = if finding.verify.is_empty() {
        format!("compare {} with this scan's", compared(finding))
    } else {
        format!("check `{id}` the way *How to verify* says")
    };
    let workload = s.workload.as_ref().filter(|_| !statements.is_empty());
    // A source that keeps totals since its last reset keeps the earlier runs in them (`stay`: which ones, and the verb).
    // Said once a brief: not where the finding's own *How to verify* says it already.
    let kept = |stay: &str| {
        workload
            .filter(|w| !w.windowed && !says_running_totals(finding, w))
            .map(|w| {
                format!(
                    ": {} keeps running totals, so {stay} in them and their numbers move only as new runs add up",
                    w.source
                )
            })
            .unwrap_or_default()
    };
    if declines {
        return if finding.verify.is_empty() {
            format!("A later {scan} shows whether `{id}` still applies.")
        } else {
            format!("A later {scan} shows where `{id}` stands: check it the way *How to verify* says.")
        };
    }
    let one = statements.len() <= 1;
    let (runs, does) = if one { ("it runs", "it does") } else { ("they run", "they do") };
    // The slowest run and the spread are kept since the reset whatever runs after it: a scan can't show the change.
    if finding.check == "unstable-query"
        && (finding.verify.is_empty() || finding.inactive)
        && let Some(w) = workload.filter(|w| !w.windowed)
    {
        let kept = format!(
            "the slowest run and the spread behind `{id}` are kept by {} since its last reset, so they stay as they \
             are",
            w.source
        );
        // A statement that no longer runs has nothing to catch (its *How to verify* says so): the fix says what runs
        // in its place, if anything.
        if finding.inactive {
            return if finding.verify.first().is_some_and(|v| v.starts_with("Nothing to check")) {
                format!("If it runs again, even a later {scan} can't show a change: {kept}.")
            } else {
                format!(
                    "There is nothing to check unless it runs again, and even then a later {scan} can't show a \
                     change: {kept}."
                )
            };
        }
        return format!(
            "A later {scan} can't show the change: {kept}. Catch the next slow runs instead, the way the fix says."
        );
    }
    // Statements that stopped: their *How to verify* says there is nothing to check unless they run again, once.
    if finding.inactive && finding.verify.first().is_some_and(|v| v.starts_with("Nothing to check")) {
        return String::new();
    }
    if finding.inactive {
        let kept = kept("the earlier runs stay");
        return format!("There is nothing to check unless {runs} again: if {does}, run {scan} and {how}{kept}.");
    }
    if once {
        let kept = kept("its first run stays");
        return format!("If it runs again, run {scan} and {how}{kept}.");
    }
    let kept = kept("the runs from before the change stay");
    match workload {
        // A finding about what the database holds now (a setting, an index, statistics): the next scan reads it as it
        // is then, and nothing kept from before the change is in it.
        None if finding.verify.is_empty()
            && (finding.category == Category::Configuration
                || matches!(finding.subject.kind, SubjectKind::Index | SubjectKind::Setting)
                || sql.schema
                || sql.upkeep
                || sql.settings()) =>
        {
            format!("After the change is live, run {scan}: `{id}` should be gone.")
        }
        Some(w) if w.windowed && finding.verify.is_empty() => format!(
            "After any change is live, run {scan} once its window covers only runs from after the change, and {how}."
        ),
        _ => format!("After any change is live, run {scan} and {how}{kept}."),
    }
}

/// Whether the finding's own *How to verify* says that `w` keeps running totals (the one sentence every verify step on
/// such a source uses), or that the counters it compares do ("The scan counters of orders keep totals since they
/// started"), so the brief doesn't say it again.
fn says_running_totals(finding: &Finding, w: &culprit_core::Workload) -> bool {
    let sentence = format!("{} keeps running totals", w.source);
    finding.verify.iter().any(|v| v.contains(&sentence) || v.contains("keep totals since"))
}

/// Whether a re-scan can show the finding gone (`culprit verify`, the step every export adds after *How to verify*).
/// Not for statements that stopped (there is nothing to ship), nor for one whose own steps compare two scans of a
/// source of running totals: those totals keep it on the list after the right change.
pub fn gone_after_rescan(report: &Report, finding: &Finding) -> bool {
    // A finding that reads a plan (an index the plan then seeks) is gone from the next scan whatever the totals keep.
    let says_gone = finding.verify.iter().any(|v| v.contains("this finding should be gone"));
    !finding.inactive
        && (says_gone || report.snapshot.workload.as_ref().is_none_or(|w| !says_running_totals(finding, w)))
}

/// "`id`'s numbers under *Evidence*": what of a finding without its own verify step a later scan shows to compare.
fn compared(finding: &Finding) -> String {
    if finding.metrics.is_empty() {
        format!("`{}`'s summary", finding.id)
    } else {
        format!("`{}`'s numbers under *Evidence*", finding.id)
    }
}

/// What the statements of a fix's SQL do, and the routes its comments offer besides them (a change in the application,
/// the SQL as only a fallback for one). A comment's words count only as a route: what it mentions in passing is no
/// statement.
#[derive(Default)]
struct FixSql {
    /// A setting of the server (`SET PERSIST`, `SET GLOBAL`, `ALTER SYSTEM`, `sp_configure`).
    setting: bool,
    /// A setting kept in one database (`ALTER DATABASE … SET`, `ALTER DATABASE SCOPED CONFIGURATION`).
    database_setting: bool,
    /// A setting of one role (`ALTER ROLE … SET`, `ALTER USER … SET`).
    role_setting: bool,
    /// It names the application's role as the placeholder `<app_role>`.
    app_role: bool,
    /// `SET LOCAL`: a setting for one transaction, which only the application's code can make.
    local: bool,
    /// Upkeep run against the database: statistics refreshed, a vacuum, an index rebuilt.
    upkeep: bool,
    /// A schema change: an index created, dropped, altered or disabled, a table altered.
    schema: bool,
    /// The schema change is to an index.
    index: bool,
    /// It creates an index.
    creates_index: bool,
    /// It builds or drops an index `CONCURRENTLY`, which PostgreSQL can't do inside a transaction.
    concurrently: bool,
    /// A Query Store procedure: a plan forced or unforced, a hint set.
    plan_pin: bool,
    /// An event session started on the database to catch statements as they run.
    event_session: bool,
    /// A comment says how to stop the session (`-- … DROP EVENT SESSION …`).
    session_drop: bool,
    /// A statement that only reads (`sql::reads_only`): what the database keeps, or what an event session caught.
    reads: bool,
    /// Anything else: a query rewrite, a batch, an EXPLAIN of the statement.
    other: bool,
    /// A comment offers a change made in the application ("2. Or in the application, … append OPTION (…)").
    in_the_application: bool,
    /// A comment calls the SQL the fallback for a change in the application ("Prefer fixing the app. As a fallback:").
    fallback: bool,
}

impl FixSql {
    fn read(sql: &str, engine: Engine) -> FixSql {
        let (comments, code): (Vec<&str>, Vec<&str>) = sql.lines().partition(|l| l.trim_start().starts_with("--"));
        let routes = comments.join(" ").to_ascii_lowercase();
        let mut out = FixSql {
            in_the_application: routes.contains("in the application"),
            fallback: routes.contains("fallback"),
            session_drop: routes.contains("drop event session"),
            app_role: sql.contains("<app_role>"),
            ..FixSql::default()
        };
        for statement in code.join("\n").split(';') {
            // A statement run only if a condition holds (`IF NOT EXISTS (…) CREATE EVENT SESSION …`) does what it does.
            let statement = guarded(statement);
            let words: Vec<String> = statement
                .split(|c: char| c.is_whitespace() || c == '(' || c == ',')
                .filter(|w| !w.is_empty())
                .map(str::to_ascii_uppercase)
                .collect();
            let Some(first) = words.first().map(String::as_str) else { continue };
            let word = |i: usize| words.get(i).map(String::as_str).unwrap_or_default();
            let has = |w: &str| words.iter().any(|x| x == w);
            let called = if matches!(first, "EXEC" | "EXECUTE") { word(1) } else { first };
            let procedure = called.rsplit('.').next().unwrap_or(called);
            if procedure.starts_with("SP_QUERY_STORE_") {
                out.plan_pin = true;
            } else if procedure == "SP_CONFIGURE" || first == "RECONFIGURE" {
                out.setting = true;
            } else if procedure == "SP_UPDATESTATS" {
                out.upkeep = true;
            } else if first == "SET" {
                match word(1) {
                    "LOCAL" => out.local = true,
                    "PERSIST" | "PERSIST_ONLY" | "GLOBAL" => out.setting = true,
                    w if w.starts_with("@@GLOBAL") => out.setting = true,
                    _ => out.other = true,
                }
            } else if first == "ALTER" && word(1) == "SYSTEM" {
                out.setting = true;
            } else if first == "ALTER" && word(1) == "DATABASE" && has("SET") {
                out.database_setting = true;
            } else if first == "ALTER" && matches!(word(1), "ROLE" | "USER") && has("SET") {
                out.role_setting = true;
            } else if matches!(first, "CREATE" | "ALTER") && word(1) == "EVENT" && word(2) == "SESSION" {
                out.event_session = true;
            } else if (first == "ALTER" && word(1) == "INDEX" && (has("REBUILD") || has("REORGANIZE")))
                || (first == "UPDATE" && word(1) == "STATISTICS")
                || matches!(first, "ANALYZE" | "VACUUM" | "REINDEX" | "OPTIMIZE" | "CLUSTER" | "PRAGMA" | "DBCC")
            {
                out.upkeep = true;
            } else if matches!(first, "CREATE" | "DROP" | "ALTER")
                && (has("INDEX") || matches!(word(1), "TABLE" | "STATISTICS"))
            {
                out.schema = true;
                out.index |= has("INDEX");
                out.creates_index |= first == "CREATE" && has("INDEX");
                out.concurrently |= has("CONCURRENTLY");
            } else if culprit_core::sql::reads_only(statement, engine) {
                out.reads = true;
            } else {
                out.other = true;
            }
        }
        out
    }

    /// A setting, of the server, a database or a role.
    fn settings(&self) -> bool {
        self.setting || self.database_setting || self.role_setting
    }

    /// A statement that changes nothing: an event session that catches statements, or a read.
    fn diagnostic(&self) -> bool {
        self.event_session || self.reads
    }

    /// Every statement is upkeep.
    fn only_upkeep(&self) -> bool {
        self.upkeep
            && !(self.settings()
                || self.local
                || self.schema
                || self.plan_pin
                || self.diagnostic()
                || self.other
                || self.in_the_application)
    }

    /// Every statement changes the schema, and no comment offers a change in the application: the statements stay as
    /// they are.
    fn only_schema(&self) -> bool {
        self.schema
            && !(self.settings()
                || self.local
                || self.upkeep
                || self.plan_pin
                || self.diagnostic()
                || self.other
                || self.in_the_application)
    }

    /// The fix's SQL is the whole change and none of it is the application's: upkeep, a plan forced in Query Store, or
    /// a diagnostic run on the database. The statements stay as they are.
    fn only_runs_on_the_database(&self) -> bool {
        (self.upkeep || self.plan_pin || self.diagnostic())
            && !(self.settings() || self.local || self.schema || self.other || self.in_the_application)
    }
}

/// The statement an `IF [NOT] EXISTS (…)` runs, without its condition; any other statement as it is.
fn guarded(statement: &str) -> &str {
    let trimmed = statement.trim_start();
    let is_if = trimmed.get(..3).is_some_and(|w| w.eq_ignore_ascii_case("if ") || w.eq_ignore_ascii_case("if\n"));
    let Some(open) = trimmed.find('(').filter(|_| is_if) else { return statement };
    let mut depth = 0;
    for (at, c) in trimmed[open..].char_indices() {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return &trimmed[open + at + 1..];
                }
            }
            _ => {}
        }
    }
    statement
}

/// A fix that says the change isn't worth making as things stand ("usually not worth having"), or that there is nothing
/// to change ("Nothing to create", "Nothing to do while they help").
fn advises_no_change(fix: &Fix) -> bool {
    let text = fix.summary.to_ascii_lowercase();
    text.contains("not worth") || text.starts_with("nothing to ") || text.contains(". nothing to ")
}

/// A fix whose SQL the database runs on its own anyway ("No update is needed before a query uses these statistics: SQL
/// Server refreshes them…"): running it is an option, not the change.
fn offers_optional_upkeep(fix: &Fix) -> bool {
    let text = fix.summary.to_ascii_lowercase();
    text.starts_with("no ") && text.split(':').next().is_some_and(|lead| lead.contains(" is needed"))
}

/// Where a finding's time went when it wasn't work, with a fix that changes no statement.
#[derive(Clone, Copy)]
enum Waited {
    /// Run-time spread with no SQL to run: the slow runs may have waited (for a lock, another transaction, storage)
    /// rather than done more work.
    SlowRuns,
    /// A statement whose time is waiting for locks other transactions hold.
    Locks,
    /// A statement whose time is waiting for an application lock (GET_LOCK, pg_advisory_lock, sp_getapplock).
    AppLock,
    /// A statement whose time is a pause it asks for (pg_sleep, WAITFOR).
    Sleep,
}

/// How the finding's statements spent the time its fix is about when it wasn't work, where the fix changes no
/// statement: the slow runs of unstable-query and parameter-sensitive, and a heavy query that waited for locks, for an
/// application lock or on a pause of its own (`queries::idle_kind`, the way heavy-query read it).
fn waited_rather_than_worked(s: &Snapshot, finding: &Finding, app: &[&QueryStat]) -> Option<Waited> {
    if finding.fix.as_ref().is_none_or(|f| f.sql.is_some()) {
        return None;
    }
    match finding.check.as_str() {
        "unstable-query" | "parameter-sensitive" => Some(Waited::SlowRuns),
        // Its time is waiting on locks other transactions hold: the fix says making it faster won't help.
        "lock-time" => Some(Waited::Locks),
        "heavy-query" => match crate::queries::idle_kind(s, app.first()?)? {
            IdleKind::Locks => Some(Waited::Locks),
            IdleKind::AppLock => Some(Waited::AppLock),
            IdleKind::Sleep => Some(Waited::Sleep),
            _ => None,
        },
        _ => None,
    }
}

impl Waited {
    /// Where the work is, in the code around the statements rather than in them.
    fn sentence(self, s: &Snapshot, app: &[&QueryStat]) -> String {
        let short = "keep those transactions short: commit right after the write, with no network calls or other slow \
                     work before the commit";
        match self {
            Waited::SlowRuns => {
                // The rows an insert's foreign keys check are share-locked: what holds them holds the insert, in
                // the fix's own words for this engine.
                let referenced = referenced_tables(s, app);
                format!(
                    "The fix changes no statement: the slow runs may have waited rather than worked (the summary says \
                     for what). Where they waited for another transaction, it is one of those the fix names: find the \
                     code that writes to {}{} inside a transaction, and {short}.",
                    written_tables(s, app),
                    if referenced.is_empty() {
                        String::new()
                    } else {
                        format!(", or holds the rows it references in {referenced} the way the fix says,")
                    }
                )
            }
            Waited::Locks => format!(
                "The fix changes no statement: the finding reads its time as waiting for locks other transactions hold \
                 (the summary says how sure it is), not as work, so no rewrite or index of it shortens it. Find the \
                 code that changes or locks rows of {} inside a transaction, and {short}.",
                written_tables(s, app)
            ),
            Waited::AppLock => "The fix changes no statement: its time is waiting for an application lock another \
                 session holds, not work. Find the code that takes the same lock and shorten the work it does while it \
                 holds it, or have this statement take the lock without waiting (a try-lock or a lock timeout) and retry \
                 later."
                .into(),
            Waited::Sleep => "Its time is a pause the statement asks for, not work: no rewrite or index makes it \
                 shorter. Find the code that sends it (a health check, a test, a retry loop with a back-off) and decide \
                 whether the pause belongs in the database at all: while it sleeps it holds a connection, and any locks \
                 its transaction took."
                .into(),
        }
    }
}

/// The tables a finding about tables is about, as `schema.table`: a deadlock's every table, an index's table (the
/// application reads and writes the table, not its index: a subject `schema.table.index`), else its subject.
fn finding_tables(finding: &Finding) -> Vec<String> {
    let deadlocked = crate::deadlocks::deadlock_tables(finding);
    if !deadlocked.is_empty() {
        return deadlocked;
    }
    let name = &finding.subject.name;
    match name.rsplit_once('.').filter(|_| finding.subject.kind == SubjectKind::Index) {
        Some((table, _)) => vec![table.to_string()],
        None => vec![name.clone()],
    }
}

/// A caveat quotes one of the finding's statements as the source recorded it ("One of them, as recorded:" and its
/// text): where no statement is listed, that is the one to look for in the code.
fn quotes_its_statement(finding: &Finding) -> bool {
    finding.caveats.iter().any(|c| c.contains(", as recorded:\n"))
}

/// "`public.apps`": the tables an insert's foreign keys reference, whose rows it checks and share-locks.
fn referenced_tables(s: &Snapshot, app: &[&QueryStat]) -> String {
    let engine = s.server.engine;
    let mut names: Vec<String> = Vec::new();
    for q in app.iter().filter(|q| first_verb(main_statement(&q.text)).is_some_and(|(_, v)| v == "insert")) {
        for target in write_targets(q, engine) {
            for k in s.foreign_keys.iter().filter(|k| names_table(&target, &k.schema, &k.table)) {
                let name = format!("`{}.{}`", k.ref_schema, k.ref_table);
                if !names.contains(&name) {
                    names.push(name);
                }
            }
        }
    }
    format::list(&names)
}

/// "`public.app_salts`": the tables the statements write (`write_targets`: their plans' modifications, else their
/// text's target), else the tables their plans read; "the same tables" when nothing names one.
fn written_tables(s: &Snapshot, app: &[&QueryStat]) -> String {
    let mut found: Vec<String> = app.iter().flat_map(|q| write_targets(q, s.server.engine)).collect();
    if found.is_empty() {
        found = app
            .iter()
            .filter_map(|q| q.plan.as_ref())
            .flat_map(|p| p.nodes())
            .filter_map(|n| n.object.clone())
            .collect();
    }
    let mut names: Vec<String> = Vec::new();
    for name in found {
        if !names.contains(&name) {
            names.push(name);
        }
    }
    if names.is_empty() {
        return "the same tables".into();
    }
    format::list(&names.iter().map(|n| format!("`{n}`")).collect::<Vec<_>>())
}

/// Fix text written for a person reading the finding, said to the coding agent reading the brief: it is the brief
/// already, so it isn't told to copy one.
fn for_the_agent(text: &str) -> String {
    text.replace(" (copy the brief for your coding agent)", "").replace("(copy the brief for your coding agent) ", "")
}

/// "`Sort` (using filesort)": a step and what it does.
fn step_name(node: &PlanNode) -> String {
    match node.detail.as_deref() {
        Some(detail) => format!("`{}` ({})", node.op, detail.replace('`', "")),
        None => format!("`{}`", node.op),
    }
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
