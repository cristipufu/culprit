//! The `heavy-query` check: Single queries that take a large share of database time, with what their plan spends it on.

mod fix;
mod idle;
mod rewrite;
#[cfg(test)]
mod tests;

use super::*;
use crate::evidence::{QueryMeasure, Requirement as Evidence};
use fix::*;
pub(super) use idle::idle_time;
pub(crate) use idle::{IdleKind, idle_kind};
use idle::{NOTHING_READS, WaitKind, dominant_wait};
pub(crate) use rewrite::late_filter_rewrite;
use rewrite::{ancestors, narrowed_later_fix};

pub fn heavy_query_info() -> CheckInfo {
    CheckInfo {
        id: "heavy-query".into(),
        title: "Queries that dominate database time".into(),
        category: Category::Queries,
        engines: vec![Engine::Mssql, Engine::Postgres, Engine::Yugabyte, Engine::Mysql, Engine::Sqlite],
        requires: vec![],
        description: "Single queries that take a large share of database time, with what their plan spends it on."
            .into(),
    }
}

check!(HeavyQueryCheck: heavy_query_info, heavy_query, &[Evidence::Query(QueryMeasure::Work)]);

pub fn heavy_query(snapshot: &Snapshot) -> Vec<Finding> {
    let Some(workload) = &snapshot.workload else {
        return Vec::new();
    };
    let mut queries: Vec<&QueryStat> = workload
        .queries
        .iter()
        .filter(|q| !is_monitoring_query(q) && heavy_share(snapshot, q) >= HEAVY_SHARE)
        .collect();
    queries.sort_by(|a, b| heavy_share(snapshot, b).total_cmp(&heavy_share(snapshot, a)));
    // A statement whose cost is behind it (it stopped running, or ran once) never takes the place of one that runs:
    // the six are statements that run, and history follows them.
    let (history, running): (Vec<&QueryStat>, Vec<&QueryStat>) =
        queries.into_iter().partition(|q| crate::query_is_history(snapshot, q));
    running
        .into_iter()
        .take(HEAVY_QUERIES)
        .chain(history.into_iter().take(HEAVY_QUERIES))
        .map(|q| heavy_query_finding(snapshot, q))
        .collect()
}

/// The heavy statements that get a finding of their own, of those that run (and as many of those whose cost is behind
/// them).
const HEAVY_QUERIES: usize = 6;

pub(super) fn heavy_query_finding(snapshot: &Snapshot, q: &QueryStat) -> Finding {
    // Weighed, rated and graded on what Culprit's scans measured of it where they did (`query_impact`), the same
    // figures the title, the summary and every list of statements state.
    let impact = query_impact(snapshot, &[q]).expect("workload present");
    let handle = query_handle(q);
    let time = time_noun(snapshot);
    let mut f = finding("heavy-query", &q.id, Category::Queries, subject(SubjectKind::Query, &q.id));
    let scans = snapshot.workload.as_ref().and_then(|w| w.scans_of(q));
    let recent = recent_entry(snapshot, q);
    apply_impact(snapshot, &mut f, &impact);
    f.severity = severity_for_impact(snapshot, &[q], &impact);
    f.confidence = Confidence::High;
    // Runs a day at the pace it ran: over Culprit's scans where they measured it, a burst's own total, never
    // extrapolated to a day.
    let per_day_runs = scans.map(|s| s.per_day(s.runs)).or_else(|| rate_per_day_of(snapshot, &[q], q.executions));
    let kind = kind_of(q);
    // A statement that stopped running is told in the past.
    let stopped = crate::query_stopped(snapshot, q).is_some();
    // Rows returned measure waste only for a read that returns what it finds: an aggregate returns one row per group
    // whatever it reads, and a write returns none.
    let reads_per_row = match (q.reads_per_execution(), q.rows_per_execution()) {
        (Some(reads), Some(rows)) if kind == StatementKind::Read => Some(reads / rows.max(1.0)),
        _ => None,
    };
    // A read whose runs return less than a row: per row, its reads would be counted against a row it rarely returns,
    // so they're said per run, once they are more than a lookup that finds nothing reads.
    let reads_for_nothing = match (q.reads_per_execution(), q.rows_per_execution()) {
        (Some(reads), Some(rows)) if kind == StatementKind::Read && rows < 1.0 && reads >= NOTHING_READS => {
            Some((reads, if rows <= 0.0 { "nothing" } else { "less than a row" }))
        }
        _ => None,
    };
    let idle = idle_time(snapshot, q);
    let hot =
        q.plan.as_ref().and_then(|p| Some((p, culprit_core::insights::working_step(p)?))).filter(|_| idle.is_none());
    // A plan that does no real work (a point lookup), run cheaply as measured: its time is the call itself, so no step
    // of it is the lead.
    let cheap_plan = q.plan.as_ref().is_some_and(|p| culprit_core::insights::runs_cheaply(p, q));
    // A residual filter may explain the reads, but only an actual plan can establish its row counts.
    let lookups = reads_for_nothing
        .filter(|_| hot.is_none() && idle.is_none())
        .and_then(|(reads, nothing)| residual_read(q.plan.as_ref()?, reads, nothing));
    // Costed under the bar every step's share needs, yet not cheap because a step reads a whole table: no share of the
    // time is put on that step, but it is the one the fix is about.
    let whole_scan = q
        .plan
        .as_ref()
        .filter(|_| hot.is_none() && idle.is_none() && lookups.is_none() && !cheap_plan)
        .and_then(|p| Some((p, whole_table_scan(p)?)));
    let share = format::percent(impact.share);
    let mysql = snapshot.server.engine == Engine::Mysql;
    let (reads_verb, reads_unit) = if mysql { ("examines", "rows") } else { ("reads", "pages") };
    let (takes, read, returns) = match (stopped, mysql) {
        (true, true) => ("took", "examined", "returned"),
        (true, false) => ("took", "read", "returned"),
        (false, _) => ("takes", reads_verb, "returns"),
    };
    // A statement that stopped running took its share until it stopped, whatever it spent the time on.
    let until = if stopped { " until it stopped" } else { "" };
    f.title = match (&idle, reads_per_row, reads_for_nothing) {
        (Some(idle), _, _) => {
            format!("{} {takes} {share} of {time}{until}, {}", capitalize(&handle), idle.title_tail())
        }
        (None, _, Some((reads, nothing))) => {
            format!("{} {read} {} {reads_unit} a run to return {nothing}", capitalize(&handle), format::count(reads))
        }
        (None, Some(r), _) if r >= MANY_READS_PER_ROW => {
            format!("{} {read} {} {reads_unit} for every row it {returns}", capitalize(&handle), format::count(r))
        }
        _ => format!("{} {takes} {share} of {time}{until}", capitalize(&handle)),
    };
    // With time the source couldn't attribute, the share is of what it could.
    let of = if snapshot.workload.as_ref().is_some_and(|w| w.untracked_time_ms.is_some()) {
        format!("of the {time} {} could attribute", snapshot.workload.as_ref().map(|w| w.source.as_str()).unwrap_or(""))
    } else {
        format!("of all {time}")
    };
    let window = snapshot.workload.as_ref().map(window_phrase).unwrap_or_default();
    let mut summary = match &recent {
        Some(s) => format!(
            "Its entry in {} is recent: {} of its {} runs came during {} of Culprit's own scans, where it ran about {} times a day at {} on average and took {share} of database time. Over {window} it is {}, a share diluted by all the time before its entry began counting.",
            snapshot.workload.as_ref().map_or("the statistics", |w| w.source.as_str()),
            format::count(s.runs),
            format::count(q.executions),
            s.length(),
            format::count(s.per_day(s.runs)),
            format::duration(s.time_ms / s.runs.max(1.0)),
            format::percent(snapshot.workload.as_ref().map_or(0.0, |w| w.share_of(q.total_time_ms, q)))
        ),
        // What runs now, where the source's totals go back further than Culprit's scans: their figures, with the
        // totals' beside them where those differ twofold.
        None => scans_sentence(snapshot, q, &of).unwrap_or_else(|| {
            let (runs, when) = statement_runs(snapshot, q);
            // Two runs far apart: what each took, never an average that hides it.
            let at = match q.few_runs_took().filter(|_| q.executions > 1.0) {
                Some(took) => format!(", taking {took}"),
                None => format!(
                    " at {}{}",
                    format::duration(q.mean_time_ms),
                    if q.executions > 1.0 { " on average" } else { "" }
                ),
            };
            format!("It ran {runs}{at}{when}: {share} {of} in {window}.")
        }),
    };
    let wait = dominant_wait(snapshot, q).filter(|_| idle.is_none());
    if let Some(idle) = &idle {
        summary.push(' ');
        summary.push_str(&idle.evidence);
    } else if let Some((plan, node)) = hot {
        // MySQL counts the rows a statement examined: what the step reads, measured, when it is the plan's only table.
        let examined = q.reads_per_execution().filter(|_| mysql);
        let step = describe_node(plan, node, examined);
        // A share only where the plan can state one: beside a step it puts no cost on, a scan reads as "100%".
        match plan.step_share(node) {
            Some(s) => summary.push_str(&format!(" {} of its estimated cost is {step}.", format::percent(s))),
            None => summary.push_str(&uncosted_step(snapshot, plan, &step)),
        }
    } else if let Some(lookups) = &lookups {
        summary.push(' ');
        summary.push_str(&lookups.evidence);
    } else if cheap_plan && kind == StatementKind::Write {
        // A write's plan costs next to nothing whatever its runs cost: the time is the write itself.
        summary.push(' ');
        summary.push_str(&write_cost(snapshot, q));
    } else if cheap_plan {
        match (&wait, q.reads_per_execution()) {
            // Time spent waiting for storage isn't how often it runs alone: the few pages it reads come from disk.
            (Some((WaitKind::Storage, ..)), Some(reads)) => summary.push_str(&format!(
                " Its plan is cheap: each run {reads_verb} about {} {reads_unit}, and no step of it is where the time goes.",
                format::count(reads.round().max(1.0))
            )),
            _ => summary
                .push_str(" Its plan is cheap, so the share comes from how often it runs, not from any step of it."),
        }
    } else if let Some((plan, node)) = whole_scan {
        // No share of the cost is put on the scan under the bar, but that every run reads its table is the plan's own.
        let step = describe_node(plan, node, q.reads_per_execution().filter(|_| mysql));
        summary.push_str(&format!(
            " Every run includes {step}: a scan reads the whole table, and costs more as the table grows."
        ));
    }
    if let Some((_, _, wait)) = &wait {
        summary.push(' ');
        summary.push_str(wait);
    }
    let trigger_info = (kind == StatementKind::Write).then(|| target_table_triggers(snapshot, q)).flatten();
    if let Some((table_name, event_noun, ref names)) = trigger_info {
        summary.push_str(&format!(
            " {table_name} also fires {} on {event_noun} ({}), whose work runs inside this statement's time.",
            if names.len() == 1 { "a trigger" } else { "triggers" },
            names.join(", ")
        ));
    }
    f.summary = summary;
    let workload = snapshot.workload.as_ref();
    match (workload, &scans) {
        // What the scans measured, and the totals since the source began counting where they differ twofold (never
        // for a recent entry, whose totals are those runs diluted by the time before it began).
        (Some(w), Some(s)) => {
            let totals = format!("over {}", w.window_phrase());
            let beside = !s.is_recent(q);
            f.metrics.push(Metric::new("Share of DB time over Culprit's scans", s.share(), Unit::Ratio));
            if beside && s.share_differs(w, q) {
                let label = format!("{} {totals}", share_metric_label(snapshot));
                f.metrics.push(Metric::new(label, w.share_of(q.total_time_ms, q), Unit::Ratio));
            }
            // Runs that came in bursts are counted, never a rate a day.
            if s.per_day(s.runs) >= 1.0 && !w.cadence(q).is_burst() {
                f.metrics.push(Metric::new("Runs per day over Culprit's scans", s.per_day(s.runs), Unit::Count));
            } else {
                f.metrics.push(Metric::new("Runs over Culprit's scans", s.runs, Unit::Count));
            }
            if beside && s.runs_differ(w, q) && !w.cadence(q).is_burst() {
                let label = format!("Runs per day {totals}");
                f.metrics.push(Metric::new(label, w.per_day(q.executions), Unit::Count));
            }
            f.metrics.extend(average_metrics(snapshot, q));
        }
        _ => {
            f.metrics.push(Metric::new(share_metric_label(snapshot), impact.share, Unit::Ratio));
            match workload.and_then(|w| Some((w.runs_per_day_of(q), w.per_day_measured(q.executions)?))) {
                Some((Some(runs), _)) => f.metrics.push(Metric::new("Runs per day", runs, Unit::Count)),
                // A day or more of window, but not runs that came a day at a time: what was counted.
                Some((None, _)) => f.metrics.push(Metric::new("Runs", q.executions, Unit::Count)),
                None => {}
            }
            f.metrics.extend(average_metrics(snapshot, q));
        }
    }
    if let Some(r) = q.reads_per_execution() {
        // MySQL counts rows examined, the other engines pages.
        let (label, unit) =
            if mysql { ("Rows examined per run", Unit::Rows) } else { ("Pages read per run", Unit::Pages) };
        let label = crate::support::totals_basis(snapshot, q)
            .map_or_else(|| label.to_string(), |basis| format!("{label} over {basis}"));
        f.metrics.push(Metric::new(label, r, unit));
    }
    if let (Some(r), Some(label)) = (q.rows_per_execution(), rows_label(snapshot.server.engine, kind)) {
        let mut metric = rows_metric(&label, r, q.executions);
        if let Some(basis) = crate::support::totals_basis(snapshot, q) {
            metric.label.push_str(&format!(" over {basis}"));
        }
        f.metrics.push(metric);
    }
    if let Some(cpu) = q.cpu_time_ms.filter(|_| q.total_time_ms > 0.0) {
        f.metrics.push(Metric::new("CPU share of its time", (cpu / q.total_time_ms).min(1.0), Unit::Ratio));
    }
    if let Some((_, _, names)) = trigger_info {
        f.metrics.push(Metric::text("Triggers on table", names.join(", ")));
    }
    let mut plan_node = hot.map(|(_, n)| n.id);
    match &idle {
        Some(idle) => {
            if idle.kind == IdleKind::Sleep || idle.kind == IdleKind::AppLock {
                // A deliberate pause holds a connection, not the server's resources: listed, ranked low.
                f.severity = Severity::Low;
                f.impact.score = f.impact.score.min(base_score(Severity::Low));
            }
            f.confidence = idle.confidence;
            f.fix = Some(idle.fix(snapshot, q));
            f.verify = vec![verify_line(snapshot, q, idle.verify())];
        }
        None => {
            // The next steps tied to one operator: measure a seek's residual filter, or inspect a whole-table read.
            let step_fix = match (&lookups, whole_scan) {
                (Some(lookups), _) => Some((lookups.check.id, lookups.fix(snapshot))),
                (None, Some((plan, node))) => scan_step_fix(snapshot, plan, node, q, kind).map(|fix| (node.id, fix)),
                _ => None,
            };
            let (fix, lever) = match step_fix {
                Some((node, fix)) => {
                    plan_node = Some(node);
                    (fix, Shows::CheaperRuns)
                }
                None => heavy_fix_and_lever(snapshot, hot, per_day_runs, q, kind, cheap_plan),
            };
            f.fix = Some(fix);
            let expect = match (&lookups, &wait) {
                (Some(_), _) => Expect::Measure,
                // A read that waits for storage: what the change shortens is the wait, not the pages a run reads.
                (None, Some((WaitKind::Storage, wait_share, _)))
                    if matches!(kind, StatementKind::Read | StatementKind::Aggregate) =>
                {
                    Expect::Storage(if *wait_share >= 1.0 { "most".to_string() } else { format::percent(*wait_share) })
                }
                _ if culprit_core::sql::is_session_statement(&q.text) => Expect::Session,
                // A fix that cuts runs leaves what one run reads as it was (or more, once the cheap runs are skipped
                // or a batch carries more rows): what moves is how often it runs, and its time with it.
                _ if super::chatty_query::counter_advice(snapshot, q).is_some() => Expect::Counter,
                _ if lever == Shows::FewerRuns => Expect::FewerRuns,
                _ => Expect::CheaperRuns,
            };
            f.verify = vec![verify_line(snapshot, q, expect)];
        }
    }
    if statistics_maintenance_table(&q.text).is_some() {
        f.summary.push_str(" This is optimizer statistics maintenance, not an update of application rows.");
        f.fix = Some(advice("Check whether this was an automatic refresh or scheduled maintenance. Keep necessary statistics current; review an unnecessarily frequent schedule or full scan only with evidence of its cost.".into()));
        f.verify = vec!["Compare the frequency and duration of subsequent statistics refreshes under comparable load, together with query performance. A completed maintenance operation can remain in Query Store's history after any scheduling change.".into()];
    }
    if let Some(rewrite) = q.plan.as_ref().and_then(|p| late_filter_rewrite(snapshot, q, p)) {
        plan_node = rewrite.nodes.first().copied();
        f.fix = Some(advice(rewrite.fix));
        f.verify = vec![rewrite.verify];
        if !index_work_is_material(snapshot, &[q]) {
            f.actionability = culprit_core::Actionability::Information;
        }
    }
    f.queries = vec![QueryRef { id: q.id.clone(), plan_node, note: None }];
    f
}

/// What a change shows in a later scan, for the verify step ([`verify_line`]).
pub(super) enum Expect {
    /// The cause is not known yet: get operator measurements before prescribing a change.
    Measure,
    /// Less of its time waiting for storage, which is this share of it now.
    Storage(String),
    /// A fix that makes each run cheaper: its reads and time per run drop.
    CheaperRuns,
    /// A fix that cuts runs (a cache, a skipped or batched call, a rarer poll): fewer runs and less time in all, while
    /// what one run reads and takes stays, or rises.
    FewerRuns,
    /// A counter's calls may be required to allocate numbers and return state atomically.
    Counter,
    /// A session statement: evaluate avoidable overhead without removing required session or transaction work.
    Session,
    /// Its average run, toward what its reads take (a wait taken away).
    Average,
    /// Its time, or that it stops running (a pause or an application lock taken away).
    TimeOrStops,
}

/// How a later scan shows the change. A windowed source (Query Store, a log) covers the runs since, so its share,
/// reads and averages move; on a source of running totals two of Culprit's scans show it, in What changed, which
/// carries runs and time and no reads ([`verify_between_scans`]).
fn verify_line(snapshot: &Snapshot, q: &QueryStat, expect: Expect) -> String {
    let mysql = snapshot.server.engine == Engine::Mysql;
    let reads = if mysql { "rows examined" } else { "pages read" };
    if matches!(expect, Expect::Measure) {
        return format!(
            "Use an actual plan to check row counts, and collect IO measurements for the same run to locate the reads. After a change supported by that evidence, compare equivalent runs: {reads} and time per run should fall without changing the result."
        );
    }
    let between = match &expect {
        Expect::Measure => unreachable!(),
        Expect::Storage(now) => {
            format!("its time per run should drop, as less of it waits for storage ({now} of it now)")
        }
        Expect::CheaperRuns => Shows::CheaperRuns.between().to_string(),
        Expect::FewerRuns => Shows::FewerRuns.between().to_string(),
        Expect::Counter => "compare time and avoidable calls for equivalent work, preserving numbering, metadata updates, returned values and transaction guarantees".into(),
        Expect::Session => "compare avoidable session-call overhead for equivalent useful work, preserving transaction guarantees and session isolation; required calls need not decrease".to_string(),
        Expect::Average => "its time per run should drop toward what its reads take".to_string(),
        Expect::TimeOrStops => "its time should drop, or it should stop running".to_string(),
    };
    if let Some(line) = verify_between_scans(snapshot, q, &between) {
        return line;
    }
    let time = time_noun(snapshot);
    match expect {
        Expect::Measure => unreachable!(),
        Expect::Storage(now) => format!(
            "Compare equivalent runs before and after a justified change: time per run and storage wait time per run should fall ({now} of elapsed time was storage waiting). If the change avoids calls instead, compare total runs and total database time for the same useful work."
        ),
        Expect::CheaperRuns => format!(
            "After the change ships, re-scan: this query's {reads} and time per run should drop, and its share of {time} with them."
        ),
        Expect::FewerRuns => format!(
            "After the change ships, re-scan: this query should run fewer times and take a smaller share of {time}, while what one run reads and takes can stay as it is, or rise."
        ),
        Expect::Counter => "Compare time and any avoidable calls under equivalent workload, checking that numbering, metadata updates, returned values and transaction guarantees remain correct. Required atomic calls need not decrease.".into(),
        Expect::Session => {
            "Compare avoidable session-call overhead for equivalent useful work. Verify transaction guarantees and session isolation; required calls need not decrease.".to_string()
        }
        Expect::Average => "Re-scan after the change: its average duration drops toward what its reads take.".into(),
        Expect::TimeOrStops => "Re-scan after the change: this statement's time drops or it stops running.".into(),
    }
}

/// "Rows returned per run" as a number, or, for so few that a number per run would read as none ("0.0" for 1 row in
/// 6.5K runs), as what was counted: "1 row in 6.5K runs".
fn rows_metric(label: &str, per_run: f64, runs: f64) -> Metric {
    let rows = per_run * runs;
    if per_run > 0.0 && per_run < 0.05 && runs > 0.0 {
        let n = format::count(rows.round().max(1.0));
        return Metric::text(
            label.trim_end_matches(" per run"),
            format!("{n} row{} in {} runs", if n == "1" { "" } else { "s" }, format::count(runs)),
        );
    }
    if per_run > 0.0 && per_run < 1.0 {
        return Metric::text(label, rows_a_run(per_run));
    }
    Metric::new(label, per_run, Unit::Rows)
}

/// Reads per row returned from which a read's title says so, and an aggregate's result is worth keeping up to date
/// instead of recomputing: thousands of pages (rows examined, MySQL) folded into each row it returns.
pub(super) const MANY_READS_PER_ROW: f64 = 1000.0;

/// The costliest step of a plan that puts no cost on another step doing work ([`Plan::uncosted_steps`]: MySQL's
/// filesort and temporary tables), or whose rows rest on a guess, in plain words: " Its only step with a cost is a table
/// scan of shop.orders that examines 500K rows a run (MySQL puts no cost on the sort)." No share is said:
/// [`Plan::step_share`] gives none.
fn uncosted_step(snapshot: &Snapshot, plan: &Plan, step: &str) -> String {
    let mut steps: Vec<String> = Vec::new();
    for n in plan.uncosted_steps() {
        let temporary = [Some(n.op.as_str()), n.detail.as_deref()]
            .into_iter()
            .flatten()
            .any(|t| t.to_ascii_lowercase().contains("temporary"));
        let what = match n.kind {
            _ if temporary => "the temporary table",
            NodeKind::Sort => "the sort",
            NodeKind::Aggregate => "the grouping",
            _ => "the spool",
        };
        if !steps.iter().any(|s| s == what) {
            steps.push(what.to_string());
        }
    }
    if steps.is_empty() {
        return format!(" The step its plan costs highest is {step}.");
    }
    let costed = plan.nodes().into_iter().filter(|n| n.self_cost > 0.0).count();
    let lead = if costed <= 1 {
        "Its only step with a cost is"
    } else {
        "Of the steps its plan puts a cost on, the costliest is"
    };
    format!(" {lead} {step} ({} puts no cost on {}).", snapshot.server.name(), format::list(&steps))
}

/// The costliest step that reads a whole table, in a plan that weighs its steps: what keeps a plan costed under the bar
/// from running cheaply ([`culprit_core::insights::runs_cheaply`]), since a scan costs more as its table grows.
fn whole_table_scan(plan: &Plan) -> Option<&PlanNode> {
    if !plan.weighed() {
        return None;
    }
    // A scan that stops at its first rows (under a TOP, in index order) doesn't read its table whole.
    plan.nodes().into_iter().filter(|n| n.reads_table_whole()).max_by(|a, b| a.self_cost.total_cmp(&b.self_cost))
}

/// What a statement does with the rows it touches, as far as its text says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StatementKind {
    /// Returns the rows it finds.
    Read,
    /// Returns one row per group, or one row in all: COUNT, SUM, GROUP BY, DISTINCT.
    Aggregate,
    /// INSERT, UPDATE, DELETE, MERGE: changes rows and returns none.
    Write,
    /// A read that locks the rows it finds until its transaction ends (`SELECT … FOR UPDATE`, `FOR SHARE`, SQL Server's
    /// UPDLOCK): it waits for a transaction holding them, as a write does.
    Locking,
    Other,
}

pub(crate) fn statement_kind(text: &str) -> StatementKind {
    let lower = squeeze(culprit_core::sql::without_parameter_list(text));
    // The statement a WITH list leads to: `WITH b AS (SELECT …) DELETE …` is a delete.
    let body = main_statement(&lower).trim_start_matches(|c: char| c.is_whitespace() || c == '(');
    if ["insert", "update", "delete", "merge", "replace"].iter().any(|v| body.starts_with(v)) {
        return StatementKind::Write;
    }
    if !(body.starts_with("select") || body.starts_with("with")) {
        return StatementKind::Other;
    }
    if locking_clause(&lower).is_some() {
        return StatementKind::Locking;
    }
    let aggregate = ["count(", "sum(", "avg(", "min(", "max(", "string_agg(", "array_agg(", "group_concat("]
        .iter()
        .any(|f| calls(&lower, f))
        || lower.contains(" group by ")
        || lower.contains("select distinct ");
    if aggregate { StatementKind::Aggregate } else { StatementKind::Read }
}

/// [`statement_kind`] for a statement, with what its plan says: a PostgreSQL plan that locks the rows it returns
/// (LockRows) is a locking read whatever the text shows of it.
pub(crate) fn kind_of(q: &QueryStat) -> StatementKind {
    let kind = statement_kind(&q.text);
    let locks_rows = q.plan.as_ref().is_some_and(|p| p.nodes().iter().any(|n| n.op.eq_ignore_ascii_case("LockRows")));
    if matches!(kind, StatementKind::Read | StatementKind::Aggregate) && locks_rows {
        StatementKind::Locking
    } else {
        kind
    }
}

/// The clause by which a read locks the rows it finds, as written: `FOR UPDATE`, `FOR NO KEY UPDATE`, `FOR SHARE`,
/// `FOR KEY SHARE`, MySQL's `LOCK IN SHARE MODE`, SQL Server's UPDLOCK, XLOCK and HOLDLOCK hints. From squeezed text.
pub(super) fn locking_clause(squeezed: &str) -> Option<&'static str> {
    const CLAUSES: [(&str, &str); 8] = [
        ("for update", "FOR UPDATE"),
        ("for no key update", "FOR NO KEY UPDATE"),
        ("for share", "FOR SHARE"),
        ("for key share", "FOR KEY SHARE"),
        ("lock in share mode", "LOCK IN SHARE MODE"),
        ("updlock", "UPDLOCK"),
        ("xlock", "XLOCK"),
        ("holdlock", "HOLDLOCK"),
    ];
    let word = |c: Option<char>| c.is_some_and(|c| c.is_ascii_alphanumeric() || c == '_');
    CLAUSES.iter().find_map(|(clause, name)| {
        squeezed
            .match_indices(clause)
            .any(|(at, _)| {
                !word(squeezed[..at].chars().next_back()) && !word(squeezed[at + clause.len()..].chars().next())
            })
            .then_some(*name)
    })
}

/// Whether squeezed text calls `function` (written with its opening parenthesis), as a whole name.
pub(super) fn calls(squeezed: &str, function: &str) -> bool {
    squeezed.match_indices(function).any(|(at, _)| {
        squeezed[..at].chars().next_back().is_none_or(|c| !(c.is_ascii_alphanumeric() || c == '_' || c == '.'))
    })
}

/// "Rows returned per run", or what the engine's row count means for a write. MySQL counts no rows sent for a write.
pub(super) fn rows_label(engine: Engine, kind: StatementKind) -> Option<std::borrow::Cow<'static, str>> {
    if kind == StatementKind::Write {
        engine.spec().statement_metrics().write_rows_label
    } else {
        Some("Rows returned per run".into())
    }
}

/// Whether the workload adds up only the runs the slow query log kept (those over long_query_time), so a share is of
/// the logged slow runs' time, not of all database time.
pub(crate) fn slow_log_only(snapshot: &Snapshot) -> bool {
    snapshot.workload.as_ref().is_some_and(|w| {
        // The collector says so; scans before it did are read from the setting.
        w.min_logged_ms.is_some()
            || (w.kind() == Some(culprit_core::WorkloadKind::SlowLog) && logs_slow_runs_only(snapshot))
    })
}

/// Whether the slow query log keeps only runs over long_query_time: unless it is 0, which logs every run.
pub(crate) fn logs_slow_runs_only(snapshot: &Snapshot) -> bool {
    !snapshot
        .setting("long_query_time")
        .and_then(|s| s.value.trim().parse::<f64>().ok())
        .is_some_and(|seconds| seconds <= 0.0)
}

/// A fix in words only: what the application changes.
pub(super) fn advice(summary: String) -> Fix {
    Fix { summary, sql: None, cost: None }
}

/// Active triggers on a write statement's target table that fire for the statement's DML event (`INSERT`, `UPDATE`,
/// `DELETE`, `MERGE`/`REPLACE`).
fn target_table_triggers<'a>(snapshot: &'a Snapshot, q: &QueryStat) -> Option<(&'a str, &'static str, Vec<String>)> {
    let verb = first_verb(main_statement(&q.text)).map(|(_, v)| v)?;
    let (wanted, event_noun): (&[char], &'static str) = match verb {
        "insert" => (&['I'], "insert"),
        "update" => (&['U'], "update"),
        "delete" => (&['D'], "delete"),
        "merge" | "replace" => (&['I', 'U', 'D'], "write"),
        _ => return None,
    };
    let target = culprit_core::sql::tables_in(&q.text, snapshot.server.engine).into_iter().next()?;
    let leaf = target.rsplit('.').next().unwrap_or(&target);
    let table = snapshot.tables.iter().find(|t| t.name.eq_ignore_ascii_case(leaf))?;
    let storage = table.storage.as_ref()?;
    let mut names: Vec<String> = storage
        .triggers()
        .into_iter()
        .filter(|(_, events)| events.is_empty() || wanted.iter().any(|w| events.contains(*w)))
        .map(|(name, _)| name.to_string())
        .collect();
    names.sort();
    names.dedup();
    (!names.is_empty()).then_some((table.name.as_str(), event_noun, names))
}
