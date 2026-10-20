//! The `heavy-query` check: Single queries that take a large share of database time, with what their plan spends it on.

use super::*;

pub fn heavy_query_info() -> CheckInfo {
    CheckInfo {
        id: "heavy-query".into(),
        title: "Queries that dominate database time".into(),
        category: Category::Queries,
        engines: vec![Engine::Mssql, Engine::Postgres, Engine::Mysql, Engine::Sqlite],
        requires: vec![],
        description: "Single queries that take a large share of database time, with what their plan spends it on."
            .into(),
    }
}

check!(HeavyQueryCheck: heavy_query_info, heavy_query);

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
        let mut metric = rows_metric(label, r, q.executions);
        if let Some(basis) = crate::support::totals_basis(snapshot, q) {
            metric.label.push_str(&format!(" over {basis}"));
        }
        f.metrics.push(metric);
    }
    if let Some(cpu) = q.cpu_time_ms.filter(|_| q.total_time_ms > 0.0) {
        f.metrics.push(Metric::new("CPU share of its time", (cpu / q.total_time_ms).min(1.0), Unit::Ratio));
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
enum Expect {
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
const MANY_READS_PER_ROW: f64 = 1000.0;

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
fn locking_clause(squeezed: &str) -> Option<&'static str> {
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
pub(super) fn rows_label(engine: Engine, kind: StatementKind) -> Option<&'static str> {
    match (kind, engine) {
        (StatementKind::Write, Engine::Postgres) => Some("Rows changed per run"),
        (StatementKind::Write, Engine::Mssql) => Some("Rows per run"),
        (StatementKind::Write, Engine::Mysql) => None,
        _ => Some("Rows returned per run"),
    }
}

/// Where a statement's time goes when it isn't reading or computing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum IdleKind {
    /// It calls a sleep function: the pause is what it was asked to do.
    Sleep,
    /// It waits for an application lock (GET_LOCK, pg_advisory_lock, sp_getapplock).
    AppLock,
    /// Locks other transactions hold.
    Locks,
    /// The application reading its results.
    Client,
    /// A memory grant.
    Memory,
    /// Pages other sessions are using (latches).
    Latch,
    /// Something the engine doesn't name.
    Unknown,
}

pub(super) struct Idle {
    kind: IdleKind,
    /// One or two sentences: what shows it.
    evidence: String,
    confidence: Confidence,
    /// The engine named the cause (Query Store's waits, MySQL's lock time) rather than Culprit inferring it.
    measured: bool,
    /// The wait's share of the statement's time, where that is what named it: "most of it" only from half on.
    share: Option<f64>,
}

impl Idle {
    fn title_tail(&self) -> String {
        let part = match self.share {
            Some(s) if s < 0.5 => format!("{} of it", format::percent(s)),
            _ => "most of it".to_string(),
        };
        let most = if self.measured { part.clone() } else { "most likely".to_string() };
        match self.kind {
            IdleKind::Sleep => "sleeping".into(),
            IdleKind::AppLock => "waiting for an application lock".into(),
            IdleKind::Locks => format!("{most} waiting for locks"),
            IdleKind::Client => format!("{part} waiting for the application to read the results"),
            IdleKind::Memory => format!("{part} waiting for memory"),
            IdleKind::Latch => format!("{part} waiting for pages other sessions are using"),
            IdleKind::Unknown => "most of it waiting rather than working".into(),
        }
    }

    /// What to do about where the time goes. Where it waits for a lock, how to find the holder is the one every check
    /// that names a lock wait gives ([`find_lock_holder`]): Live only for waits that come often enough to catch.
    fn fix(&self, snapshot: &Snapshot, q: &QueryStat) -> Fix {
        let holder = || find_lock_holder(snapshot, q, None);
        let (summary, sql) = match self.kind {
            IdleKind::AppLock => {
                let LockHolder { lead, sql } = holder();
                (
                    format!(
                        "{lead} Shorten the work done while the lock is held, or take the lock without waiting (a try-lock, or a lock timeout) and retry later."
                    ),
                    sql,
                )
            }
            IdleKind::Locks => {
                let LockHolder { lead, sql } = holder();
                (
                    format!(
                        "{lead} Keep those transactions short: commit sooner, and don't wait for a user or another service inside a transaction. An index or a rewrite of this statement won't shorten the wait."
                    ),
                    sql,
                )
            }
            _ => (self.advice(), None),
        };
        Fix { summary, sql, cost: None }
    }

    fn advice(&self) -> String {
        match self.kind {
            IdleKind::Sleep => "Find who sends it (a health check, a test, a retry loop with a back-off) and whether the pause belongs in the database at all: while it sleeps it holds a connection, and any locks its transaction took.".into(),
            IdleKind::AppLock | IdleKind::Locks => String::new(),
            IdleKind::Client => "The application reads the results slowly, or row by row while it does other work. Read all rows first and process them afterwards, or fetch fewer.".into(),
            IdleKind::Memory => "It waits for a memory grant while other queries hold the memory: find the queries with the largest grants (Health) and reduce them, usually with better estimates or an index that avoids a big sort or hash.".into(),
            IdleKind::Latch => "Sessions queue for the same pages: typically inserts at the end of an index on an increasing key, or tempdb allocation. See the hot-rows and last-page findings, and Live while it happens.".into(),
            IdleKind::Unknown => "Find what it waits for while it runs: Live shows each session's wait and any blocking chain. Typical causes are locks other transactions hold, a slow client reading the results, or a call outside the database.".into(),
        }
    }

    fn verify(&self) -> Expect {
        match self.kind {
            IdleKind::Sleep | IdleKind::AppLock => Expect::TimeOrStops,
            _ => Expect::Average,
        }
    }
}

/// Sleep functions, by the name to show.
pub(super) const SLEEPS: [(&str, &str); 6] = [
    ("pg_sleep(", "pg_sleep"),
    ("pg_sleep_for(", "pg_sleep_for"),
    ("pg_sleep_until(", "pg_sleep_until"),
    ("sleep(", "SLEEP"),
    ("waitfor delay", "WAITFOR"),
    ("waitfor time", "WAITFOR"),
];

/// Application locks a statement waits for until another session releases them.
pub(super) const APP_LOCKS: [(&str, &str); 5] = [
    ("get_lock(", "GET_LOCK"),
    ("pg_advisory_lock(", "pg_advisory_lock"),
    ("pg_advisory_xact_lock(", "pg_advisory_xact_lock"),
    ("pg_advisory_lock_shared(", "pg_advisory_lock_shared"),
    ("sp_getapplock", "sp_getapplock"),
];

/// Runs this long or longer are worth explaining by what they wait for.
pub(super) const IDLE_MIN_MS: f64 = 100.0;
/// Generous costs of reading: a page from a slow disk, a row examined from one (MySQL). Reads that can't take a
/// tenth of a run's time even at these rates don't explain it.
const PAGE_READ_MS: f64 = 10.0;
const ROW_READ_MS: f64 = 1.0;
/// CPU under this share of the time: the statement isn't computing for most of it.
const CPU_SMALL: f64 = 0.2;
/// Pages (rows examined, MySQL) a run of a read that returns nothing reads before its title says so: many times what a
/// lookup that finds nothing reads (an index's few levels).
const NOTHING_READS: f64 = 100.0;
/// A wait that takes at least this share of a statement's time is what its runs spend most of their time on.
const WAIT_MOST: f64 = 0.5;

/// What a statement's dominant wait is about, where the fix depends on it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WaitKind {
    /// Reading pages from storage: they weren't in memory.
    Storage,
    Other,
}

/// The wait that takes most of a statement's time by the engine's own count of its waits (Query Store's per query,
/// PostgreSQL's time reading and writing data files, MySQL's lock time), in a sentence: "85% of its time is spent
/// waiting for data to come from storage because it wasn't in memory (Query Store: Buffer IO)." Not the statement's own
/// work (CPU, a parallel plan's threads waiting for each other), nor a category with no plain meaning. Query Store sums
/// a parallel plan's waits over its threads, which can pass the statement's time: then it is "most of" it. With the
/// wait's share of the statement's time.
fn dominant_wait(snapshot: &Snapshot, q: &QueryStat) -> Option<(WaitKind, f64, String)> {
    if q.total_time_ms <= 0.0 {
        return None;
    }
    let w = q
        .waits
        .iter()
        .filter(|w| w.wait_ms >= q.total_time_ms * WAIT_MOST)
        .max_by(|a, b| a.wait_ms.total_cmp(&b.wait_ms))?;
    let key = culprit_core::waits::category_key(&w.category);
    let doing = match key {
        "io" if snapshot.server.engine == Engine::Postgres => "reading and writing data files",
        "io" => "waiting for data to come from storage because it wasn't in memory",
        "lock" => "waiting for locks other transactions hold",
        "latch" => "waiting for pages other sessions are using",
        "log" => "waiting for its changes to reach the transaction log",
        "network" => "waiting for the application to read the results",
        "memory" => "waiting for memory",
        "compilation" => "compiling its plan",
        _ => return None,
    };
    let share = w.wait_ms / q.total_time_ms;
    let part =
        if share >= 1.0 { "Most of its time".to_string() } else { format!("{} of its time", format::percent(share)) };
    let source = snapshot.workload.as_ref().map_or("the statistics", |w| w.source.as_str());
    let kind = if key == "io" { WaitKind::Storage } else { WaitKind::Other };
    // A part of its time the source keeps only in its totals: said over them where the share is the scans'.
    let sentence = format!("{part} is spent {doing} ({source}: {}).", w.wait_type.as_deref().unwrap_or(&w.category));
    Some((kind, share, over_totals(snapshot, q, sentence)))
}

/// Where a statement's time goes when it isn't work (`idle_time`), for the views that word a finding by it (the brief).
pub(crate) fn idle_kind(snapshot: &Snapshot, q: &QueryStat) -> Option<IdleKind> {
    idle_time(snapshot, q).map(|i| i.kind)
}

/// When a statement's time isn't work: it sleeps, waits for a lock, or does too little reading and computing to fill
/// its run time. Uses the engine's own account (Query Store waits, MySQL lock time) where there is one. Its evidence
/// rests on figures the source keeps only in its totals (reads a run, its run time, its waits): where the summary leads
/// with Culprit's scans, it says what they are over ([`over_totals`]).
pub(super) fn idle_time(snapshot: &Snapshot, q: &QueryStat) -> Option<Idle> {
    let mut idle = idle_time_of(snapshot, q)?;
    if !matches!(idle.kind, IdleKind::Sleep | IdleKind::AppLock) {
        idle.evidence = over_totals(snapshot, q, idle.evidence);
    }
    Some(idle)
}

/// A sentence of figures from statement `q`'s totals, led by what they are over where its share and runs are said over
/// Culprit's scans ([`crate::support::totals_basis`]): "Over the 1277 days since statistics were reset, it read …".
fn over_totals(snapshot: &Snapshot, q: &QueryStat, sentence: String) -> String {
    let Some(basis) = crate::support::totals_basis(snapshot, q) else { return sentence };
    // A pronoun or an adverb leads in lower case after the basis; a name (MySQL) or a figure stays as it is.
    let lead = match sentence.split_whitespace().next() {
        Some("It" | "Only" | "Most") => {
            let mut chars = sentence.chars();
            chars.next().map(|c| c.to_lowercase().chain(chars).collect()).unwrap_or_default()
        }
        _ => sentence,
    };
    format!("Over {basis}, {lead}")
}

fn idle_time_of(snapshot: &Snapshot, q: &QueryStat) -> Option<Idle> {
    let text = squeeze(&q.text);
    let named = |list: &[(&str, &'static str)]| -> Option<&'static str> {
        list.iter()
            .find(|(pattern, _)| if pattern.ends_with('(') { calls(&text, pattern) } else { text.contains(pattern) })
            .map(|(_, name)| *name)
    };
    if let Some(name) = named(&SLEEPS) {
        return Some(Idle {
            kind: IdleKind::Sleep,
            evidence: format!(
                "It calls {name}: the time is a deliberate pause, not work, and no index or rewrite makes it shorter."
            ),
            confidence: Confidence::High,
            measured: true,
            share: None,
        });
    }
    if let Some(name) = named(&APP_LOCKS) {
        return Some(Idle {
            kind: IdleKind::AppLock,
            evidence: format!(
                "It calls {name}, which waits until the session holding that lock releases it: its time is how long others hold the lock, not work it does."
            ),
            confidence: Confidence::High,
            measured: true,
            share: None,
        });
    }
    if q.mean_time_ms < IDLE_MIN_MS || q.total_time_ms <= 0.0 {
        return None;
    }
    let engine = snapshot.server.engine;
    let run = format::duration(q.mean_time_ms);
    let read_cost = if engine == Engine::Mysql { ROW_READ_MS } else { PAGE_READ_MS };
    let reads = q.reads_per_execution();
    let reads_small =
        reads.is_some_and(|r| r * read_cost < q.mean_time_ms * 0.1) && q.temp_writes.is_none_or(|t| t <= 0.0);
    let cpu_share = q.cpu_time_ms.map(|c| c / q.total_time_ms);
    let cpu_small = cpu_share.is_some_and(|s| s < CPU_SMALL);
    let unit = if engine == Engine::Mysql { "rows examined" } else { "pages" };
    let reads_text = reads.map(|r| format!("{} {unit} per run", format::count(r))).unwrap_or_default();

    // MySQL's own lock time (row locks too from 8.0.28; the slow query log's lock time).
    let lock_ms = q.spread.as_ref().and_then(|s| s.lock_ms).or_else(|| {
        (engine == Engine::Mysql).then(|| q.waits.iter().filter(|w| w.category == "Lock").map(|w| w.wait_ms).sum())
    });
    if engine == Engine::Mysql
        && let Some(lock) = lock_ms.filter(|l| *l >= q.total_time_ms * 0.5)
    {
        return Some(Idle {
            kind: IdleKind::Locks,
            evidence: format!(
                "{} counts {} of its {} waiting for locks.",
                snapshot.server.name(),
                format::percent((lock / q.total_time_ms).min(1.0)),
                format::duration(q.total_time_ms)
            ),
            confidence: Confidence::High,
            measured: true,
            share: None,
        });
    }

    // Query Store's waits per query (summed over parallel threads, so a category names the cause, not a share).
    if engine == Engine::Mssql && cpu_small {
        let top = q
            .waits
            .iter()
            .filter(|w| w.category != "CPU" && w.wait_ms >= q.total_time_ms * 0.25)
            .max_by(|a, b| a.wait_ms.total_cmp(&b.wait_ms));
        // Categories as the SQL Server provider folds Query Store's (`wait_category`).
        let kind = match top.map(|w| w.category.as_str()) {
            Some("Lock") => Some(IdleKind::Locks),
            Some("Network") => Some(IdleKind::Client),
            Some("Memory") => Some(IdleKind::Memory),
            Some("Latch") => Some(IdleKind::Latch),
            // Reading from disk, writing the log, compiling or a parallel plan's threads are the statement's own work.
            Some("IO" | "Log" | "Parallelism" | "Compilation") => return None,
            Some(_) | None => None,
        };
        let cpu = format::percent(cpu_share.unwrap_or(0.0));
        if let (Some(kind), Some(w)) = (kind, top) {
            return Some(Idle {
                kind,
                evidence: format!(
                    "Only {cpu} of its time is CPU, and Query Store records {} of {} waits for it.",
                    format::duration(w.wait_ms),
                    w.wait_type.as_deref().unwrap_or(&w.category)
                ),
                confidence: Confidence::High,
                measured: true,
                // Summed over a parallel plan's threads it can pass the statement's time: then it is most of it.
                share: Some((w.wait_ms / q.total_time_ms).min(1.0)),
            });
        }
        if reads_small {
            return Some(Idle {
                kind: IdleKind::Unknown,
                evidence: format!(
                    "Only {cpu} of its time is CPU, and {reads_text} take far less than {run}: most of each run is spent waiting."
                ),
                confidence: Confidence::Medium,
                measured: false,
                share: None,
            });
        }
        return None;
    }
    if cpu_small && reads_small {
        return Some(Idle {
            kind: IdleKind::Unknown,
            evidence: format!(
                "Only {} of its time is CPU, and {reads_text} take far less than {run}: most of each run is spent waiting.",
                format::percent(cpu_share.unwrap_or(0.0))
            ),
            confidence: Confidence::Medium,
            measured: false,
            share: None,
        });
    }
    // No CPU time (PostgreSQL, the slow query log): a write, or a read that locks the rows it finds, that reads a
    // handful of pages can't compute for seconds, so it waited, most likely for a row lock. A plain read could be
    // computing (functions), so it isn't called waiting.
    let kind = kind_of(q);
    if cpu_share.is_none() && reads_small && matches!(kind, StatementKind::Write | StatementKind::Locking) {
        // MySQL counted its lock time and it wasn't most of the run: waiting, but not for what MySQL calls locks.
        if engine == Engine::Mysql && lock_ms.is_some() {
            return Some(Idle {
                kind: IdleKind::Unknown,
                evidence: format!(
                    "It examined {reads_text}, which takes milliseconds, yet ran {run}: most of that time it was waiting, and {} doesn't say for what.",
                    snapshot.server.name()
                ),
                confidence: Confidence::Medium,
                measured: false,
                share: None,
            });
        }
        let source = match engine {
            Engine::Postgres => "pg_stat_statements doesn't record what a statement waited for",
            _ => "the statistics don't record what it waited for",
        };
        // A locking read asks for the lock on every row it finds: that's the one it most likely waited for.
        let lock = match locking_clause(&text) {
            Some(clause) if kind == StatementKind::Locking => {
                format!("for the lock its {clause} takes on the rows it finds, which another transaction held")
            }
            _ if kind == StatementKind::Locking => {
                "for the lock it takes on the rows it finds, which another transaction held".to_string()
            }
            _ => "for a row lock another transaction held".to_string(),
        };
        return Some(Idle {
            kind: IdleKind::Locks,
            evidence: format!(
                "It read {reads_text}, which takes milliseconds, yet ran {run}: almost all of that time it was waiting, most likely {lock} ({source})."
            ),
            confidence: Confidence::Medium,
            measured: false,
            share: None,
        });
    }
    None
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
fn advice(summary: String) -> Fix {
    Fix { summary, sql: None, cost: None }
}

/// The fix for a heavy statement no single step explains, with what it moves: fewer runs (the pool's settings, a
/// cache, a skipped or batched call), or cheaper runs. The verify step expects what it moves ([`verify_line`]).
fn heavy_fix_and_lever(
    snapshot: &Snapshot,
    hot: Option<(&Plan, &PlanNode)>,
    per_day_runs: Option<f64>,
    q: &QueryStat,
    kind: StatementKind,
    cheap_plan: bool,
) -> (Fix, Shows) {
    // The driver or the pool sends these around the application's statements: its settings are the lever.
    if culprit_core::sql::is_session_statement(&q.text) {
        return (advice(session_fix(snapshot.server.engine, &q.text)), Shows::FewerRuns);
    }
    if let Some((plan, node)) = hot {
        if let Some(fix) = scan_step_fix(snapshot, plan, node, q, kind) {
            return (fix, Shows::CheaperRuns);
        }
        // "Most of the work" only from half of what the plan costs, and a share only where the plan can state one.
        let most = match plan.step_share(node) {
            Some(share) if share >= 0.5 => "Most of the work",
            Some(_) => "The largest part of the work",
            None => "The step the plan costs highest",
        };
        // A covering index or a sorted index only where the plan's own rule says the step is worth it: covering a
        // seek that finds a row or two copies the selected columns into the index for nothing.
        if culprit_core::insights::has_insight(plan, "key-lookups", node)
            && let Some(i) = crate::views::plan_advice::catalog_insights(snapshot, q, plan)
                .into_iter()
                .find(|i| i.rule == "key-lookups" && i.nodes.contains(&node.id))
        {
            return (Fix { summary: i.fix, sql: i.fix_sql, cost: None }, Shows::CheaperRuns);
        }
        if culprit_core::insights::has_insight(plan, "big-sort", node) {
            return (
                advice(format!(
                    "{most} is sorting. An index whose key order matches the sort lets the database read rows already sorted."
                )),
                Shows::CheaperRuns,
            );
        }
    }
    // Storage waiting is observed; whether it comes from cache pressure, cold pages or latency is not established.
    if matches!(kind, StatementKind::Read | StatementKind::Aggregate)
        && let Some((WaitKind::Storage, ..)) = dominant_wait(snapshot, q)
    {
        return (advice(storage_fix(snapshot, q, kind, cheap_plan)), Shows::CheaperRuns);
    }
    // How often it runs is the cost on the same condition the summary says so: runs measured cheap. Only a statement
    // with no plan to judge its runs by is judged by its count alone.
    if cheap_plan {
        return (
            advice(fewer_runs_fix(
                snapshot,
                q,
                kind,
                "Each run is fast and its plan is already cheap; the cost is in how often it runs.",
            )),
            Shows::FewerRuns,
        );
    }
    if q.plan.as_ref().is_none_or(|p| !p.weighed())
        && per_day_runs.is_some_and(|r| r >= 100_000.0)
        && snapshot.workload.as_ref().map_or(q.mean_time_ms, |w| w.statement_mean_ms(q)) < 5.0
    {
        return (
            advice(fewer_runs_fix(snapshot, q, kind, "Each run is fast; the cost is in how often it runs.")),
            Shows::FewerRuns,
        );
    }
    let fix = advice(match kind {
        StatementKind::Write => super::chatty_query::counter_advice(snapshot,q).unwrap_or_else(|| "Find this statement in the application and check that it finds the rows it changes through an index, and changes no more rows per run than it needs (split large updates into batches).".into()),
        StatementKind::Locking => "Find this statement in the application: it locks the rows it finds until its transaction ends. Check that it locks only the rows the transaction goes on to change, and that the transaction commits soon after.".into(),
        // A result worth keeping up to date is one that folds many rows into each it returns, not a few-row lookup
        // that groups.
        StatementKind::Aggregate if folds_many_rows(q) => "Find this statement in the application and check whether it aggregates more rows than it needs, or whether the result could be kept up to date instead of recomputed on every run.".into(),
        StatementKind::Aggregate => "Find this statement in the application and check whether it aggregates more rows than it needs.".into(),
        _ => "Find this statement in the application and check whether it reads more rows or columns than it needs.".into(),
    });
    (fix, Shows::CheaperRuns)
}

/// Whether an aggregate's runs read many pages (rows examined, MySQL) for each row they return.
fn folds_many_rows(q: &QueryStat) -> bool {
    match (q.reads_per_execution(), q.rows_per_execution()) {
        (Some(reads), Some(rows)) => reads / rows.max(1.0) >= MANY_READS_PER_ROW,
        _ => false,
    }
}

/// The fix for a step that scans a table with a filter, by the plan's own rules for it: the statement needs about what
/// the scan keeps (read less), a condition later in the plan narrows it (that condition's index), or the filter keeps
/// few rows where the filtered-scan check would suggest an index. None when the plan says none of these.
fn scan_step_fix(snapshot: &Snapshot, plan: &Plan, node: &PlanNode, q: &QueryStat, kind: StatementKind) -> Option<Fix> {
    if node.kind != NodeKind::Scan || node.predicate.is_none() {
        return None;
    }
    let table = node.object.as_deref().unwrap_or("the table");
    // An index only where the filtered-scan check would suggest one: enough rows, a filter that keeps few by an
    // estimate that isn't a guess, columns an index can seek on, and no index already leading with them.
    let index_pays = scan_groups(snapshot)
        .iter()
        .any(|g| g.queries.iter().any(|(scanned, id)| scanned.id == q.id && *id == node.id));
    match node.kept_share() {
        // The bar every index suggestion uses: past it the scan is the right plan, and the fix is to read less, once
        // the rows the statement returns say it needs what the scan keeps.
        Some(kept) if kept > culprit_core::INDEX_KEEPS_AT_MOST => {
            let kept_rows = node.rows_read_total().map(|read| read * kept);
            // The nearest condition above the scan: where the plan narrows what the scan keeps.
            let narrowing = ancestors(plan, node.id).into_iter().find_map(|n| n.predicate.as_deref());
            match (kept_rows, q.rows_handled_per_execution()) {
                // Runs return an order of magnitude fewer rows than the scan keeps: they're narrowed later. An
                // aggregate returns one row per group whatever it reads, so its rows say so only where a condition
                // above the scan narrows them before they're grouped.
                (Some(k), Some(returned))
                    if returned * culprit_core::MISESTIMATE_FACTOR < k
                        && (kind != StatementKind::Aggregate || narrowing.is_some()) =>
                {
                    Some(narrowed_later_fix(snapshot, plan, q, table, kept, returned, narrowing))
                }
                // About as many as the scan keeps: what the caller asks for. Without the rows the runs return,
                // nothing says so, and nothing is claimed.
                (Some(_), Some(_)) => Some(advice(format!(
                    "The plan reads all of {table} and keeps about {} of the rows, too many for an index to help. Read only the rows and columns the caller needs, or page through them.",
                    format::percent(kept)
                ))),
                _ => None,
            }
        }
        _ if index_pays => Some(advice(format!(
            "The plan scans {table} and filters afterwards. An index on the filtered columns turns that into a seek; open the plan to see the predicate."
        ))),
        _ => None,
    }
}

/// Fewer runs, for a statement whose runs are fast and whose cost is how many there are. A read's answer can be cached,
/// or skipped when it's known, unless its runs mostly find nothing: a check whether a row exists, whose answer goes
/// stale the moment one is written. A write has to happen, so its levers are skipping it where it changes nothing,
/// more rows per statement, and an index no read uses ([`write_fix`]).
fn fewer_runs_fix(snapshot: &Snapshot, q: &QueryStat, kind: StatementKind, opening: &str) -> String {
    match kind {
        StatementKind::Write => write_fix(snapshot, q),
        _ if finds_nothing_mostly(q, kind) => format!("{opening} {}", finds_nothing(snapshot, q)),
        _ => format!(
            "{opening} Check whether the application needs every run: cache the result, batch the calls, or skip the statement when its answer is already known."
        ),
    }
}

/// What a write does to rows, in sentences: ("inserts", "insert") for an INSERT.
fn write_verbs(q: &QueryStat) -> (&'static str, &'static str) {
    match first_verb(main_statement(&q.text)).map(|(_, v)| v) {
        // Its rows are the ones it inserted and the ones it updated on a conflict.
        Some("insert") if upsert(q) != Upsert::No => ("inserts or updates", "insert or update"),
        Some("insert") => ("inserts", "insert"),
        Some("update") => ("updates", "update"),
        Some("delete") => ("deletes", "delete"),
        _ => ("changes", "change"),
    }
}

/// What an INSERT does with a row whose key is already there, where it updates it: PostgreSQL's `ON CONFLICT … DO
/// UPDATE`. Every such run writes a new version of the row (and its WAL), even one that sets a column to the value it
/// has, which a get-or-create does only so that RETURNING gives back the row there.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Upsert {
    /// It inserts, or skips the row (`DO NOTHING`).
    No,
    /// `DO UPDATE` with new values (`SET c = EXCLUDED.c`).
    Update,
    /// `DO UPDATE` that sets each column to itself (`SET salt = app_salts.salt`): the row is rewritten unchanged.
    GetOrCreate,
}

fn upsert(q: &QueryStat) -> Upsert {
    let text = squeeze(main_statement(&q.text));
    let Some(at) = text.find(" do update set ").filter(|_| text.contains(" on conflict")) else { return Upsert::No };
    let rest = &text[at + " do update set ".len()..];
    let end = [" where ", " returning "].iter().filter_map(|k| rest.find(k)).min().unwrap_or(rest.len());
    let leaf = |side: &str| side.trim().rsplit('.').next().unwrap_or("").to_string();
    let to_itself = rest[..end].split(',').all(|assignment| {
        assignment.split_once('=').is_some_and(|(column, value)| {
            let value = value.trim();
            !value.starts_with("excluded.") && !leaf(column).is_empty() && leaf(column) == leaf(value)
        })
    });
    if to_itself { Upsert::GetOrCreate } else { Upsert::Update }
}

/// Whether an INSERT writes one row of values a run (`INSERT … VALUES (…)`, one list): then the runs that change a row
/// are exactly as many as the rows it changed.
fn one_row_a_run(q: &QueryStat) -> bool {
    let text = squeeze(main_statement(&q.text));
    text.starts_with("insert") && text.contains(" values(") && !text.contains("),(") && !text.contains(" select ")
}

/// The share of a write's runs that change nothing, from the rows they change on average: at least 1 − rows a run (a
/// run that changes something changes a row or more), exactly that where each run writes one row of values; with
/// whether it is exact. `None` for runs that change half a row or more on average.
fn runs_changing_nothing(q: &QueryStat) -> Option<(f64, bool)> {
    let rows = q.rows_handled_per_execution().filter(|r| *r < 0.5)?;
    Some((1.0 - rows, one_row_a_run(q)))
}

/// What a write's runs cost, from its own counters, for a plan with nothing to make faster: the rows a run changes
/// (and how many runs change none), and the log it writes where the engine counts it. "Each run inserts 0.25 rows on
/// average (75% of its runs insert nothing) and writes 1.7 KB of WAL, 99% of all the WAL the statements wrote: 6.7 KB
/// and 2.7 full-page images (…) for each row it inserts."
fn write_cost(snapshot: &Snapshot, q: &QueryStat) -> String {
    // A statement that stopped running is told in the past.
    let stopped = crate::all_stopped(snapshot, &[q]);
    let (verb, verb_base) = match write_verbs(q) {
        (verb, base) if stopped => (past_tense(verb), past_tense(base)),
        (verb, base) => (verb.to_string(), base.to_string()),
    };
    let mut out = if upsert(q) == Upsert::No && runs_changing_nothing(q).is_some() {
        if stopped {
            "Its plan had nothing to make faster: checking for rows still cost work when a run changed nothing."
                .to_string()
        } else {
            "Its plan has nothing to make faster: checking for rows still costs work when a run changes nothing."
                .to_string()
        }
    } else if stopped {
        "Its plan had nothing to make faster: what a run cost was the write itself (its log, its index entries, the checks of its keys).".to_string()
    } else {
        "Its plan has nothing to make faster: what a run costs is the write itself (its log, its index entries, the checks of its keys).".to_string()
    };
    let rows = q.rows_handled_per_execution();
    let postgres = snapshot.server.engine == Engine::Postgres;
    let log_word = if postgres { "WAL" } else { "transaction log" };
    let costs = q.costs.as_ref();
    let log = costs.and_then(|c| c.log_bytes).filter(|b| *b > 0.0 && q.executions > 0.0);
    match rows {
        Some(rows) => {
            let n = rows_a_run(rows);
            // Culprit's scans keep a statement's runs and time, not its rows: where its summary leads with them, what a
            // run changes is said over the totals it comes from (a recent entry's are mostly those scans' runs).
            let over = crate::support::totals_basis(snapshot, q).map(|b| format!(" over {b}")).unwrap_or_default();
            out.push_str(&format!(" Each run {verb} {n} row{} on average{over}", if n == "1" { "" } else { "s" }));
            if upsert(q) == Upsert::GetOrCreate {
                out.push_str(
                    " (a run that finds the row there rewrites it: its DO UPDATE sets a column to the value it has, which still writes a new version of the row)",
                );
            }
            if let Some((share, exact)) = runs_changing_nothing(q) {
                out.push_str(&format!(
                    " ({}{} of its runs {verb_base} nothing)",
                    if exact { "" } else { "at least " },
                    format::percent(share)
                ));
            }
        }
        None if log.is_some() => out.push_str(" Each run"),
        None => return out,
    }
    let Some(log) = log else {
        out.push('.');
        return out;
    };
    let share = costs
        .and_then(|c| c.log_share)
        .filter(|s| *s >= 0.5)
        .map(|s| format!(", {} of all the {log_word} the statements wrote", format::percent(s)))
        .unwrap_or_default();
    out.push_str(&format!(
        "{} {} {} of {log_word}{share}",
        if rows.is_some() { " and" } else { "" },
        if stopped { "wrote" } else { "writes" },
        format::bytes(log / q.executions)
    ));
    let images = costs.and_then(|c| c.log_full_pages).filter(|n| *n > 0.0 && postgres);
    const IMAGE: &str = "full-page images (a page logged whole the first time it changes after a checkpoint)";
    // Per row it changes, where that differs from per run.
    let per_row = rows.filter(|r| format::count(*r) != "1").map(|r| r * q.executions).filter(|r| *r >= 1.0);
    match (per_row, images) {
        (Some(changed), Some(images)) => out.push_str(&format!(
            ": {} and {} {IMAGE} for each row it {verb}.",
            format::bytes(log / changed),
            format::count(images / changed)
        )),
        (Some(changed), None) => out.push_str(&format!(": {} for each row it {verb}.", format::bytes(log / changed))),
        (None, Some(images)) => out.push_str(&format!(", with {} {IMAGE}.", format::count(images / q.executions))),
        (None, None) => out.push('.'),
    }
    out
}

/// A write's verbs ([`write_verbs`]) in the past, for a statement that stopped running: "inserts or updates" →
/// "inserted or updated".
fn past_tense(verbs: &str) -> String {
    verbs
        .split(' ')
        .map(|w| match w.trim_end_matches('s') {
            "insert" => "inserted",
            "update" => "updated",
            "delete" => "deleted",
            "change" => "changed",
            _ => w,
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Rows a run, as a count reads them, and to two decimals under one: 0.25 rows a run, not "0.3", beside "75% of its
/// runs".
fn rows_a_run(rows: f64) -> String {
    if rows > 0.0 && rows < 1.0 {
        let text = format!("{rows:.2}");
        text.trim_end_matches('0').trim_end_matches('.').to_string()
    } else {
        format::count(rows)
    }
}

/// The levers for a write whose runs are cheap and many. Where most runs change nothing (an insert of a row that is
/// already there), skipping them; otherwise more rows per statement, in the engine's terms, and in one transaction
/// unless another transaction can insert the same key (holding it longer makes that one wait longer); and dropping an
/// index only where the table has one no read uses.
fn write_fix(snapshot: &Snapshot, q: &QueryStat) -> String {
    if let Some(advice) = super::chatty_query::counter_advice(snapshot, q) {
        return advice;
    }
    let engine = snapshot.server.engine;
    let (_, verb) = write_verbs(q);
    let table = write_targets(q, engine)
        .into_iter()
        .find_map(|o| snapshot.tables.iter().find(|t| names_table(&o, &t.schema, &t.name)));
    // That its plan has nothing to make faster only where a plan says so; a statement judged by its count alone has none.
    // A statement that stopped running is told in the past.
    let cheap = q.plan.as_ref().is_some_and(|p| culprit_core::insights::runs_cheaply(p, q));
    let nothing = upsert(q) == Upsert::No && runs_changing_nothing(q).is_some();
    let opening = match (crate::all_stopped(snapshot, &[q]), cheap, nothing) {
        (true, _, true) => {
            "Each run was fast, but most of its runs changed nothing: the cost was in sending them anyway."
        }
        (false, _, true) => {
            "Each run is fast, but most of its runs change nothing: the cost is in sending them anyway."
        }
        (true, true, false) => {
            "Each run was fast and its plan had nothing to make faster: what cost was the write itself, every time it ran."
        }
        (true, false, false) => "Each run was fast; what cost was the write itself, every time it ran.",
        (false, true, false) => {
            "Each run is fast and its plan has nothing to make faster: what costs is the write itself, every time it runs."
        }
        (false, false, false) => "Each run is fast; what costs is the write itself, every time it runs.",
    };
    let lever = match (upsert(q), runs_changing_nothing(q)) {
        // A get-or-create rewrites the row it finds on every run that finds one: the levers are not sending it when
        // the application has the row, and not rewriting it when it is there.
        (Upsert::GetOrCreate, _) => "It is a get-or-create: its DO UPDATE sets a column to the value it has only so that RETURNING gives back the row already there, and that still writes a new version of the row (and its WAL) on every run that finds one. Skip the call where the application already has the row (it read or created it earlier and keeps it), or evaluate ON CONFLICT DO NOTHING RETURNING followed by a separate SELECT when no row is returned. That is not an equivalent atomic replacement: account for the transaction isolation level, concurrent deletion, a missing row and serialization failures with appropriate locking or retries. Preserve any required UPDATE-trigger effects; keep the atomic upsert if the application depends on them.".to_string(),
        (Upsert::Update, _) => "On a conflict it updates the row it finds, writing a new version of it even when the values are the same: give its DO UPDATE a WHERE that updates only a row whose values differ (`WHERE t.c IS DISTINCT FROM EXCLUDED.c`), and skip the call where the application knows nothing changed.".to_string(),
        (Upsert::No, Some((share, exact))) => {
            let skip = if verb == "insert" {
                "skip the statement where the application already knows the row exists (it read the row, or wrote it earlier, and keeps the value for as long as it holds), and send it only when the row may be missing.".to_string()
            } else {
                format!(
                    "skip the statement where the application already knows there is nothing to {verb} (the rows aren't there, or already hold the values)."
                )
            };
            // What its runs did, in the past once it stopped; the advice stays for the code that sends it.
            let did = if crate::all_stopped(snapshot, &[q]) { past_tense(verb) } else { verb.to_string() };
            format!(
                "{}{} of its runs {did} nothing{}: {skip}",
                if exact { "" } else { "At least " },
                format::percent(share),
                crate::support::totals_basis(snapshot, q).map(|basis| format!(" over {basis}")).unwrap_or_default()
            )
        }
        (Upsert::No, None) => {
            let many = match verb {
                "update" | "delete" => many_keys(engine, &verb.to_ascii_uppercase()),
                _ => bulk_insert(engine).to_string(),
            };
            // Several rows in one transaction hold their locks until it commits: where another transaction can insert
            // the same key, it waits that much longer.
            let transaction = match table {
                Some(t) if verb == "insert" && inserts_repeatable_key(snapshot, q, t) => "",
                _ => ", in one transaction",
            };
            format!(
                "{} several rows per statement where the application has them together ({many}){transaction}.",
                capitalize(verb)
            )
        }
    };
    // An index no read uses still costs every write an entry: the one index advice a write's count supports.
    let unused: Vec<String> = table
        .map(|t| {
            snapshot
                .indexes_on(&t.schema, &t.name)
                .filter(|i| crate::indexes::is_unused(snapshot, i))
                .map(|i| i.name.clone())
                .collect()
        })
        .unwrap_or_default();
    let indexes = match unused.as_slice() {
        [] => String::new(),
        [one] => format!(
            " {one} has had no reads {}, yet every write keeps it up to date: dropping it (see unused-index) saves that work.",
            since_phrase(snapshot)
        ),
        many => format!(
            " {} have had no reads {}, yet every write keeps them up to date: dropping them (see unused-index) saves that work.",
            format::list(many),
            since_phrase(snapshot)
        ),
    };
    format!("{opening} {lever}{indexes}")
}

/// What to do about a read whose runs mostly find nothing. Caching its answer isn't one: "not there" stops being true
/// the moment the row is written. Where its plan seeks a unique index, that index is named: it is the one an insert
/// handling the duplicate-key error relies on, and it exists already.
fn finds_nothing(snapshot: &Snapshot, q: &QueryStat) -> String {
    let unique = q.plan.as_ref().and_then(|plan| {
        plan.nodes().into_iter().filter(|n| n.kind == NodeKind::Seek).find_map(|n| {
            let (schema, table) = crate::indexes::split_object(n.object.as_deref()?)?;
            let name = n.index.as_deref()?.trim_matches(['[', ']', '"', '`']);
            let index = snapshot
                .indexes_on(&schema, &table)
                .find(|i| i.name.eq_ignore_ascii_case(name) && (i.is_unique || i.is_primary_key) && !i.is_disabled)?;
            Some(match index.filter.as_deref() {
                Some(filter) => format!("{} on {schema}.{table} (unique for the rows where `{filter}`)", index.name),
                None => format!("{} on {schema}.{table}", index.name),
            })
        })
    });
    let instead = match unique {
        Some(index) => format!(
            "this plan seeks {index}, a unique index that exists already, so an insert that handles its duplicate-key error (or an upsert) needs no read first"
        ),
        None => "with a unique index on those columns, an insert that handles the duplicate-key error (or an upsert) needs no read first".to_string(),
    };
    format!(
        "Most of its runs find nothing. If it checks whether a row exists before writing one (a duplicate or idempotency check), let the write do the check instead: {instead}. If it polls for work, ask less often when it finds none, or have the writer notify the poller."
    )
}

/// Whether a read's runs mostly return no row.
fn finds_nothing_mostly(q: &QueryStat, kind: StatementKind) -> bool {
    kind == StatementKind::Read && q.rows_per_execution().is_some_and(|r| r < 0.5)
}

/// The fix for a read whose runs spend most of their time waiting for storage. Waiting alone cannot distinguish
/// cache pressure, cold pages or storage latency; preserve that uncertainty before suggesting more memory.
fn storage_fix(snapshot: &Snapshot, q: &QueryStat, kind: StatementKind, cheap_plan: bool) -> String {
    let engine = snapshot.server.engine;
    let (verb, unit) = if engine == Engine::Mysql { ("examines", "rows") } else { ("reads", "pages") };
    let reads = q.reads_per_execution().map(|r| format::count(r.round().max(1.0)));
    let opening = match (cheap_plan, reads) {
        (true, Some(reads)) => {
            format!("Each run {verb} about {reads} {unit} in total, and most of its elapsed time is storage waiting.")
        }
        (_, Some(reads)) => format!(
            "Each run {verb} about {reads} {unit} in total, and most of its elapsed time is storage waiting. Check whether it reads unnecessary rows or columns."
        ),
        (_, None) => "Most of its elapsed time is storage waiting.".to_string(),
    };
    let mut fix = format!(
        "{opening} Compare physical reads and storage latency under representative load to distinguish slow I/O, a cold cache and repeated cache misses. The wait share alone does not establish eviction or insufficient memory. Check independent cache-pressure evidence before considering a memory or service-tier change."
    );
    if finds_nothing_mostly(q, kind) {
        fix.push(' ');
        fix.push_str(&finds_nothing(snapshot, q));
    } else if cheap_plan {
        fix.push_str(" Fewer runs (a result cached where it's safe to reuse, calls batched) also save the reads.");
    }
    fix
}

/// A seek followed by a filter, beside a statement that reads many pages and returns little. The plan's estimates
/// and the statement's recorded reads are separate evidence: pages cannot establish how many rows a lookup handled.
struct ResidualRead<'a> {
    check: &'a PlanNode,
    schema: String,
    table: String,
    evidence: String,
}

fn residual_read<'a>(plan: &'a Plan, reads: f64, nothing: &str) -> Option<ResidualRead<'a>> {
    let nodes = plan.nodes();
    let (seek, check) = nodes
        .iter()
        .copied()
        .filter(|n| n.kind == NodeKind::Lookup && n.op.contains("Lookup") && n.predicate.is_some())
        .find_map(|lookup| {
            let seek = nodes.iter().copied().find(|s| {
                s.kind == NodeKind::Seek && s.index.is_some() && s.object.is_some() && s.object == lookup.object
            })?;
            Some((seek, lookup))
        })
        .or_else(|| {
            let seek = nodes.iter().copied().find(|s| {
                s.kind == NodeKind::Seek && s.index.is_some() && s.object.is_some() && s.predicate.is_some()
            })?;
            Some((seek, seek))
        })?;
    let mut keys = predicate_columns(seek.seek_predicate.as_deref().unwrap_or_default());
    keys.equality.append(&mut keys.range);
    let checked_columns = predicate_columns(check.predicate.as_deref()?);
    let mut checked = checked_columns.equality.clone();
    checked.extend(checked_columns.range.into_iter().take(1));
    if keys.equality.is_empty()
        || checked.is_empty()
        || checked.iter().any(|c| keys.equality.iter().any(|k| k.eq_ignore_ascii_case(c)))
    {
        return None;
    }
    let (schema, table) = crate::indexes::split_object(seek.object.as_deref()?)?;
    let index = seek.index.as_deref().unwrap_or_default().trim_matches(['[', ']', '"', '`']);
    let expected = seek.est_rows_total().round().max(1.0);
    let expected = format!("{} row{} from {index}", format::count(expected), if expected == 1.0 { "" } else { "s" });
    let sought = format::list(&keys.equality);
    let checks = format::list(&checked);
    let shape = if std::ptr::eq(seek, check) {
        format!("The captured plan estimates {expected} (on {sought}) and checks each row it finds for {checks}")
    } else {
        format!(
            "The captured plan estimates {expected} (on {sought}), then looks up each row it finds in the table to check {checks}"
        )
    };
    let (verb, unit) = if plan.engine == Engine::Mysql { ("examine", "rows") } else { ("read", "pages") };
    let nothing = if nothing == "nothing" { "nothing".to_string() } else { format!("{nothing} each") };
    Some(ResidualRead {
        check,
        schema,
        table,
        evidence: format!(
            "{shape}. The statement's runs {verb} {} {unit} each and return {nothing}; those totals do not show how many rows each plan step handled.",
            format::count(reads)
        ),
    })
}

impl ResidualRead<'_> {
    fn fix(&self, snapshot: &Snapshot) -> Fix {
        let capture = if snapshot.server.engine.spec().abilities().last_actual_plan {
            "Read a retained actual plan when the server has one; otherwise capture a representative actual plan in a test environment."
        } else {
            "Capture an actual execution plan for a representative run in a test environment."
        };
        advice(format!(
            "{capture} Check how many rows each step reads and keeps, and collect IO measurements for the same run to locate the reads on {}.{}. Use that evidence to decide whether a change to statistics or the access path is justified.",
            self.schema, self.table
        ))
    }
}

/// An enabled index on `schema.table` whose first key is `column`: one that holds every row first, since a filtered
/// index serves only the statements whose conditions match its filter.
fn index_leading_with<'a>(
    snapshot: &'a Snapshot,
    schema: &'a str,
    table: &'a str,
    column: &str,
) -> Option<&'a culprit_core::IndexStat> {
    let leading: Vec<&culprit_core::IndexStat> = snapshot
        .indexes_on(schema, table)
        .filter(|i| !i.is_disabled && i.key_names().first().is_some_and(|k| k.eq_ignore_ascii_case(column)))
        .collect();
    leading.iter().find(|i| i.filter.is_none()).or_else(|| leading.first()).copied()
}

/// Advice for a scan that keeps most of its table while the statement returns far fewer rows: a condition later in the
/// plan narrows them, so the index belongs to that condition, not to the scan's filter. An OR across two tables can use
/// an index on neither; written as a UNION of two queries, each side can use one of its own: the one its table has, or
/// one to add where the index rules allow it.
fn narrowed_later_fix(
    snapshot: &Snapshot,
    plan: &Plan,
    _q: &QueryStat,
    table: &str,
    kept: f64,
    returned: f64,
    narrowing: Option<&str>,
) -> Fix {
    // Rounded as the evidence rounds it: 2.5 rows a run are "about 2.5", not "about 3".
    let rows = if returned < 1.0 {
        "less than a row a run".to_string()
    } else {
        let n = format::count(returned);
        let n = n.strip_suffix(".0").unwrap_or(&n);
        format!("about {n} row{} a run", if n == "1" { "" } else { "s" })
    };
    let start = format!(
        "The plan reads all of {table} and keeps about {} of its rows, but the statement returns {rows}: they are narrowed later in the plan",
        format::percent(kept)
    );
    match narrowing {
        Some(condition) if or_across_tables(condition) => or_rewrite_fix(snapshot, plan, condition, &start),
        Some(condition) => advice(format!(
            "{start}, by `{}`. Where that condition can be applied to {table} itself, an index on the columns it compares lets the plan read only the rows needed.",
            super::plan_warnings::truncate(condition, 140)
        )),
        None => advice(format!(
            "{start} (by a join or a limit). Open the plan to see where; a condition on {table} itself, with an index on the columns it compares, lets the plan read only the rows needed."
        )),
    }
}

fn or_rewrite_fix(snapshot: &Snapshot, plan: &Plan, condition: &str, start: &str) -> Fix {
    let sides = union_sides(snapshot, plan, condition)
        .map(|sides| format!(" Existing coverage: {}.", sides.join("; ")))
        .unwrap_or_default();
    advice(format!(
        "{start}, by `{}`. This OR spans different tables, so a single index on the scan's filter does not address it. Evaluate a UNION rewrite or an equivalent EXISTS form that lets each branch use its own access path. Preserve the original joins, permission checks, duplicate handling, NULL behavior, grouping and ordering. Compare representative rewritten plans and reads before deciding whether a branch needs another index.{sides} Keep this structural risk under review as table size or call volume grows.",
        super::plan_warnings::truncate(condition, 140)
    ))
}

/// Existing index coverage for each branch. Returned rows after joins do not establish a branch's selectivity,
/// so a rewrite must be evaluated before recommending any new index.
fn union_sides(snapshot: &Snapshot, plan: &Plan, condition: &str) -> Option<Vec<String>> {
    if snapshot.notes.iter().any(|n| n.collector == "indexes") && !snapshot.server.has(Capability::IndexUsage) {
        return None;
    }
    let mut sides = Vec::new();
    for branch in or_branches(condition) {
        let mut tables = qualifiers(&unquoted(branch));
        tables.sort();
        tables.dedup();
        let [qualifier] = tables.as_slice() else { return None };
        let cols = predicate_columns(branch);
        let column = cols.equality.first().or(cols.range.first())?;
        let node = table_named(plan, qualifier)?;
        let (schema, table) = crate::indexes::split_object(node.object.as_deref()?)?;
        sides.push(match index_leading_with(snapshot, &schema, &table, column) {
            Some(index) => format!(
                "{} on {schema}.{table} leads with {column}{}",
                index.name,
                index
                    .filter
                    .as_deref()
                    .map(|f| format!(" (partial index: verify the branch implies {f})"))
                    .unwrap_or_default()
            ),
            None => format!("{schema}.{table} has no index that leads with {column}"),
        });
    }
    (sides.len() >= 2).then_some(sides)
}

/// One rewrite observation shared by Findings, Plans and the advisor, independent of today's table size.
pub(crate) fn late_filter_rewrite(
    snapshot: &Snapshot,
    q: &QueryStat,
    plan: &Plan,
) -> Option<culprit_core::insights::PlanInsight> {
    if is_monitoring_query(q)
        || !matches!(kind_of(q), StatementKind::Read | StatementKind::Aggregate)
        || plan.nodes().iter().any(|n| n.kind == NodeKind::Modify)
    {
        return None;
    }
    let returned = q.rows_handled_per_execution();
    let mut scans: Vec<_> = plan
        .nodes()
        .into_iter()
        .filter(|n| {
            n.kind == NodeKind::Scan
                && n.reads_table_whole()
                && n.object.as_deref().is_some_and(|o| !culprit_core::insights::is_system_object(o))
        })
        .collect();
    scans.sort_by(|a, b| b.table_rows.unwrap_or(0.0).total_cmp(&a.table_rows.unwrap_or(0.0)));
    for node in scans {
        let Some(kept) = node.kept_share().filter(|k| *k > culprit_core::INDEX_KEEPS_AT_MOST) else { continue };
        let Some(condition) =
            ancestors(plan, node.id).into_iter().filter_map(|n| n.predicate.as_deref()).find(|p| or_across_tables(p))
        else {
            continue;
        };
        let table = node.object.as_deref()?;
        let fix = match returned {
            Some(returned) => narrowed_later_fix(snapshot, plan, q, table, kept, returned, Some(condition)),
            None => or_rewrite_fix(
                snapshot,
                plan,
                condition,
                &format!("The captured plan reads {table} before a filter across tables is applied"),
            ),
        };
        return Some(culprit_core::insights::PlanInsight {
            rule: "cross-table-or".into(), area: culprit_core::insights::InsightArea::Query,
            severity: Severity::Low, impact: 0.0, fixed_weight: true, nodes: vec![node.id],
            title: "A filter across tables can require a whole-table read".into(),
            detail: format!("The captured plan scans {table} before an OR across tables narrows its rows. This structure can become expensive as the tables grow; the plan does not measure the benefit or semantics of a rewrite."),
            fix: fix.summary, fix_sql: None,
            verify: "Compare results, representative parameters, plans, reads and execution time for the original and rewritten query. Preserve permission checks and duplicate/NULL behavior. Evaluate index keys only from the rewritten branch predicates and measured selectivity.".into(),
            index_hint: None,
        });
    }
    None
}

/// The branches of a condition's top-level OR, outer parentheses taken off: `((a.x = $1) OR (s.y = $2))` →
/// `(a.x = $1)`, `(s.y = $2)`. Quoted text is never split.
fn or_branches(condition: &str) -> Vec<&str> {
    let mut text = condition.trim();
    while text.starts_with('(') && text.ends_with(')') && closes_at_end(text) {
        text = text[1..text.len() - 1].trim();
    }
    let bytes = text.as_bytes();
    let word = |c: Option<&u8>| c.is_some_and(|c| c.is_ascii_alphanumeric() || *c == b'_');
    let (mut depth, mut quoted, mut start, mut at) = (0i32, false, 0, 0);
    let mut parts = Vec::new();
    while at < bytes.len() {
        match bytes[at] {
            b'\'' => quoted = !quoted,
            b'(' if !quoted => depth += 1,
            b')' if !quoted => depth -= 1,
            b'o' | b'O'
                if !quoted
                    && depth == 0
                    && bytes.get(at + 1).is_some_and(|c| c.eq_ignore_ascii_case(&b'r'))
                    && !word(at.checked_sub(1).and_then(|i| bytes.get(i)))
                    && !word(bytes.get(at + 2)) =>
            {
                parts.push(text[start..at].trim());
                at += 2;
                start = at;
                continue;
            }
            _ => {}
        }
        at += 1;
    }
    parts.push(text[start..].trim());
    parts
}

/// Whether the parenthesis opening `text` is the one its last character closes.
fn closes_at_end(text: &str) -> bool {
    let (mut depth, mut quoted) = (0i32, false);
    for (i, c) in text.char_indices() {
        match c {
            '\'' => quoted = !quoted,
            '(' if !quoted => depth += 1,
            ')' if !quoted => {
                depth -= 1;
                if depth == 0 {
                    return i == text.len() - 1;
                }
            }
            _ => {}
        }
    }
    false
}

/// The step of a plan that reads the table a condition calls `name`: by its alias (PostgreSQL's), or by its name.
fn table_named<'a>(plan: &'a Plan, name: &str) -> Option<&'a PlanNode> {
    plan.nodes().into_iter().find(|n| {
        let Some(object) = n.object.as_deref() else { return false };
        let leaf = object.rsplit('.').next().unwrap_or(object).trim_matches(['[', ']', '"', '`']);
        n.props.get("Alias").is_some_and(|a| a.eq_ignore_ascii_case(name)) || leaf.eq_ignore_ascii_case(name)
    })
}

/// The steps above the one with `id`, nearest first.
fn ancestors(plan: &Plan, id: u32) -> Vec<&PlanNode> {
    fn path<'a>(n: &'a PlanNode, id: u32, out: &mut Vec<&'a PlanNode>) -> bool {
        if n.id == id {
            return true;
        }
        let found = n.children.iter().any(|c| path(c, id, out));
        if found {
            out.push(n);
        }
        found
    }
    let mut out = Vec::new();
    path(&plan.root, id, &mut out);
    out
}

/// Whether a condition ORs a comparison on one table's column with a comparison on another's (`a.owner_id = $1 OR
/// s.email = $2`): an index on either table serves only its own side, so the plan reads both in full.
fn or_across_tables(condition: &str) -> bool {
    let text = unquoted(condition);
    // Each OR as a word (`… $1) OR ((s.email …`), and the comparisons on either side of it: the last column before it
    // and the first after it.
    let bytes = text.as_bytes();
    let word = |c: &u8| c.is_ascii_alphanumeric() || *c == b'_';
    (0..bytes.len().saturating_sub(1)).any(|at| {
        bytes[at] == b'o'
            && bytes[at + 1] == b'r'
            && (at == 0 || !word(&bytes[at - 1]))
            && bytes.get(at + 2).is_none_or(|c| !word(c))
            && matches!(
                (qualifiers(&text[..at]).pop(), qualifiers(&text[at + 2..]).into_iter().next()),
                (Some(a), Some(b)) if a != b
            )
    })
}

/// A condition lowercased, with its names unquoted and its quoted literals left out: `'black or white'` is a value, not
/// a condition.
fn unquoted(condition: &str) -> String {
    let mut text = String::new();
    let mut quoted = false;
    for c in condition.to_ascii_lowercase().chars() {
        match c {
            '\'' => quoted = !quoted,
            '`' | '[' | ']' | '"' => {}
            _ if !quoted => text.push(c),
            _ => {}
        }
    }
    text
}

/// The table each qualified column of [`unquoted`] text names, in order: `a.owner_id` → `a`,
/// `db.app.messages.tenant_id` → `messages`.
fn qualifiers(text: &str) -> Vec<String> {
    text.split(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '$' | '@')))
        .filter_map(|word| {
            let (qualifier, column) = word.rsplit_once('.')?;
            let table = qualifier.rsplit('.').next()?;
            let starts = |s: &str| s.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_');
            (starts(table) && starts(column)).then(|| table.to_string())
        })
        .collect()
}

/// What to change for a statement the driver or the connection pool sends ([`culprit_core::sql::is_session_statement`]):
/// the pool's or the driver's settings, never the application's code, and each kind of statement has its own.
fn session_fix(engine: Engine, text: &str) -> String {
    let pool = "The driver or the connection pool sends this around the application's statements, not the application's own code";
    match first_keyword(text).as_str() {
        // PostgreSQL's poolers reset a session with DISCARD ALL (or RESET ALL) between one use and the next.
        "discard" | "reset" if engine == Engine::Postgres => "This resets session state and is commonly sent by a driver or connection pool; the statement statistics do not identify its caller or pooling mode. Inspect the application's connection lifecycle and the driver or pool configuration, and benchmark any change against the measured absolute cost shown here. Keep resets that prevent session state leaking between users. Npgsql arranges its reset on close and sends it on reuse; PgBouncer normally resets on release in session pooling, and normally does not in transaction pooling. Consider changing reset behavior only after verifying that every caller safely handles retained session state.".into(),
        "begin" | "start" | "commit" | "rollback" | "end" | "savepoint" | "release" => format!(
            "{pool}: check whether it wraps every statement in a transaction of its own (each commit waits for the log to reach disk), and run the statements that belong together in one transaction."
        ),
        "select" => format!(
            "{pool}: it checks that a connection is still alive. Check how often the pool does that (on every checkout, or on a timer) and check less often."
        ),
        _ if engine == Engine::Mssql => format!(
            "{pool}: a pooled connection returns to the login's defaults each time it is reused (sp_reset_connection), so options set per session are sent again on every checkout. Set them once instead: as the database's or the login's defaults, or in the connection string where the driver allows."
        ),
        _ => format!(
            "{pool}: look at what the pool runs each time it hands out a connection (session settings, a reset), and set what the application needs once instead: in the connection string, or as the role's or the server's defaults."
        ),
    }
}

/// A statement's first keyword, past leading comments, lowercased.
fn first_keyword(text: &str) -> String {
    let mut rest = text.trim_start();
    loop {
        if let Some(after) = rest.strip_prefix("/*") {
            rest = after.split_once("*/").map_or("", |(_, r)| r).trim_start();
        } else if let Some(after) = rest.strip_prefix("--") {
            rest = after.split_once('\n').map_or("", |(_, r)| r).trim_start();
        } else {
            break;
        }
    }
    rest.split(|c: char| !c.is_ascii_alphabetic()).next().unwrap_or("").to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;
    use culprit_core::{PlanKind, Properties};

    fn node(id: u32, op: &str, kind: NodeKind, children: Vec<PlanNode>) -> PlanNode {
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
            subtree_cost: 0.0,
            self_cost: 0.0,
            act_time_ms: None,
            runtime: None,
            warnings: vec![],
            props: Properties::new(),
            children,
        }
    }

    /// An app list as a PostgreSQL generic plan: apps and app_shares read in full, and `a.owner_id = $1 OR s.email =
    /// $2` above the join, where the 5.9K apps the scan keeps become a row or two.
    fn apps_plan() -> Plan {
        let mut apps = node(7, "Seq Scan", NodeKind::Scan, vec![]);
        apps.object = Some("public.apps".into());
        apps.props.insert("Alias".into(), "a".into());
        apps.predicate = Some("(a.deleted_at IS NULL)".into());
        (apps.est_rows, apps.rows_read, apps.table_rows) = (5_880.0, Some(6_285.0), Some(6_285.0));
        (apps.self_cost, apps.subtree_cost) = (186.43, 186.43);
        let mut shares = node(5, "Seq Scan", NodeKind::Scan, vec![]);
        shares.object = Some("public.app_shares".into());
        shares.props.insert("Alias".into(), "s".into());
        (shares.est_rows, shares.rows_read, shares.table_rows) = (845.0, Some(845.0), Some(845.0));
        (shares.self_cost, shares.subtree_cost) = (19.45, 19.45);
        let hash = node(6, "Hash", NodeKind::Other, vec![apps]);
        let mut join = node(4, "Hash Join", NodeKind::Join, vec![shares, hash]);
        join.predicate = Some("(((a.owner_id)::text = $1) OR ((s.email)::text = $2))".into());
        (join.est_rows, join.self_cost, join.subtree_cost) = (11.0, 75.72, 281.6);
        let mut root = node(0, "Sort", NodeKind::Sort, vec![join]);
        (root.est_rows, root.self_cost, root.subtree_cost) = (11.0, 0.21, 286.19);
        Plan {
            engine: Engine::Postgres,
            kind: PlanKind::Generic,
            statement: None,
            total_cost: 286.19,
            root,
            warnings: vec![],
            index_hints: vec![],
            props: Properties::new(),
            raw: None,
            insights: None,
        }
    }

    const APPS_QUERY: &str = "SELECT a.id, a.name FROM apps a LEFT JOIN app_shares s ON s.app_id = a.id WHERE (a.owner_id = $1 OR s.email = $2) AND a.deleted_at IS NULL GROUP BY a.id, a.name ORDER BY a.name";

    fn postgres_demo() -> Snapshot {
        serde_json::from_str(include_str!("../../../../../fixtures/postgres-demo.json")).expect("fixture")
    }

    /// An index of the demo's on `public.<table>` (`columns`), as the catalog would list it.
    fn index(snapshot: &Snapshot, table: &str, name: &str, columns: &[&str]) -> culprit_core::IndexStat {
        let mut index = snapshot.indexes[0].clone();
        (index.schema, index.table, index.name) = ("public".into(), table.into(), name.into());
        index.columns = columns.iter().map(|c| c.to_string()).collect();
        (index.include, index.filter, index.is_disabled) = (vec![], None, false);
        index
    }

    /// The fix for the scan of apps, for runs that return `rows` each.
    fn scan_fix_on(snapshot: &Snapshot, rows: f64) -> Fix {
        let plan = apps_plan();
        let mut q = snapshot.workload.as_ref().expect("workload").queries[0].clone();
        q.text = APPS_QUERY.into();
        q.rows = Some(rows * q.executions);
        q.plan = Some(plan.clone());
        let scan = plan.nodes().into_iter().find(|n| n.id == 7).expect("the scan of apps");
        heavy_fix_and_lever(snapshot, Some((&plan, scan)), None, &q, statement_kind(&q.text), false).0
    }

    fn scan_fix(rows: f64) -> String {
        scan_fix_on(&postgres_demo(), rows).summary
    }

    #[test]
    fn a_scan_narrowed_later_is_not_too_many_rows_for_an_index() {
        // The scan keeps 94% of apps, but a run returns two rows: the OR above the join narrows them.
        let fix = scan_fix(2.0);
        assert!(!fix.contains("too many for an index"), "{fix}");
        assert!(fix.contains("returns about 2 rows a run: they are narrowed later in the plan"), "{fix}");
        assert!(fix.contains("OR spans different tables") && fix.contains("UNION rewrite"), "{fix}");
        // Runs that return about what the scan keeps need those rows: no index serves them.
        let fix = scan_fix(5_000.0);
        assert!(fix.contains("keeps about 94% of the rows, too many for an index to help"), "{fix}");
        // The aptabase-us reading: "returns about 3 rows a run" beside 2.5 in the evidence. Rounded as it is.
        assert!(scan_fix(2.5).contains("the statement returns about 2.5 rows a run"), "{}", scan_fix(2.5));
        assert!(scan_fix(1.96).contains("the statement returns about 2 rows a run"), "{}", scan_fix(1.96));
    }

    // The fresh PostgreSQL demo: `SELECT … FOR UPDATE` that waited 38 s for a row lock was a read, told to read fewer
    // rows. A read that locks what it finds waits as a write does.
    #[test]
    fn rewrite_observation_is_shared_without_premature_index_sql() {
        let s = postgres_demo();
        let mut q = s.queries()[0].clone();
        q.text = APPS_QUERY.into();
        q.executions = 1000.0;
        q.rows = Some(2000.0);
        q.total_time_ms = 1.0;
        q.mean_time_ms = 0.001;
        q.plan = Some(apps_plan());
        let p = q.plan.as_ref().unwrap();
        let insight =
            crate::query_plan_insights(&s, &q, p).items.into_iter().find(|i| i.rule == "cross-table-or").unwrap();
        assert_eq!(insight.area, culprit_core::insights::InsightArea::Query);
        assert_eq!(insight.severity, Severity::Low);
        assert!(insight.fix_sql.is_none());
        assert!(insight.fix.contains("permission checks") && insight.verify.contains("selectivity"));
        let f = heavy_query_finding(&s, &q);
        assert_eq!(f.actionability, culprit_core::Actionability::Information);
        assert!(f.fix.unwrap().sql.is_none());
        assert_eq!(f.queries[0].plan_node, insight.nodes.first().copied());
        for text in [format!("{APPS_QUERY} FOR UPDATE"), "UPDATE apps SET name = $1".into()] {
            let mut writes = q.clone();
            writes.text = text;
            assert!(
                late_filter_rewrite(&s, &writes, p).is_none(),
                "read rewrites do not change locking or write semantics"
            );
        }
        let mut with_query = s.clone();
        with_query.workload.as_mut().unwrap().queries = vec![q.clone()];
        let report = crate::analyze(with_query);
        let advisor = crate::index_advice(&report);
        assert!(advisor.notes.iter().any(|note| note.contains("Query rewrites to evaluate") && note.contains(&q.id)));
        // Larger tables keep the structural concern, without claiming that a rewrite has measured benefit.
        let mut large = p.clone();
        large.root.children[0].children[1].children[0].table_rows = Some(6_285_000.0);
        large.root.children[0].children[1].children[0].rows_read = Some(6_285_000.0);
        large.root.children[0].children[1].children[0].est_rows = 5_880_000.0;
        assert!(late_filter_rewrite(&s, &q, &large).is_some());
        let mut without_rows = q.clone();
        without_rows.rows = None;
        let observation = late_filter_rewrite(&s, &without_rows, &large).unwrap();
        assert!(!observation.fix.contains("statement returns"), "no invented row count");
        // Engine-owned objects are never candidates for a rewrite or index change.
        for n in &mut large.root.children[0].children {
            n.object = Some("pg_catalog.pg_class".into());
            for c in &mut n.children {
                c.object = Some("pg_catalog.pg_class".into());
            }
        }
        assert!(late_filter_rewrite(&s, &q, &large).is_none());
    }

    #[test]
    fn a_read_that_locks_its_rows_is_a_locking_statement() {
        for text in [
            "SELECT id FROM orders WHERE id = $1 FOR UPDATE",
            "SELECT id FROM jobs WHERE state = $1 LIMIT $2 FOR UPDATE SKIP LOCKED",
            "SELECT id FROM orders WHERE id = $1 FOR NO KEY UPDATE",
            "SELECT * FROM accounts WHERE id = ? LOCK IN SHARE MODE",
            "SELECT [id] FROM [shop].[orders] WITH (UPDLOCK, ROWLOCK) WHERE [id] = @P1",
        ] {
            assert_eq!(statement_kind(text), StatementKind::Locking, "{text}");
        }
        for (text, kind) in [
            ("SELECT updated_for, for_update FROM t WHERE x = $1", StatementKind::Read),
            ("SELECT count(*) FROM orders WHERE status = $1", StatementKind::Aggregate),
            (
                "WITH b AS (SELECT id FROM jobs LIMIT 10) DELETE FROM jobs USING b WHERE jobs.id = b.id",
                StatementKind::Write,
            ),
        ] {
            assert_eq!(statement_kind(text), kind, "{text}");
        }
        // A plan that locks the rows it returns says so whatever the text shows.
        let mut q = postgres_demo().workload.unwrap().queries[0].clone();
        q.text = "SELECT id FROM orders WHERE id = $1".into();
        let mut plan = apps_plan();
        plan.root.op = "LockRows".into();
        q.plan = Some(plan);
        assert_eq!(kind_of(&q), StatementKind::Locking);
    }

    #[test]
    fn each_side_of_a_union_names_the_index_it_has_or_the_one_it_needs() {
        // apps has an index on owner_id; app_shares' key leads with app_id, so the email side has none.
        let mut snapshot = postgres_demo();
        let owner = index(&snapshot, "apps", "idx_apps_owner", &["owner_id"]);
        let key = index(&snapshot, "app_shares", "PK_app_shares", &["app_id", "email"]);
        snapshot.indexes.extend([owner, key]);
        let fix = scan_fix_on(&snapshot, 2.0);
        assert!(!fix.summary.contains("an index of its own"), "{}", fix.summary);
        assert!(fix.summary.contains("idx_apps_owner on public.apps leads with owner_id"), "{}", fix.summary);
        assert!(fix.summary.contains("public.app_shares has no index that leads with email"), "{}", fix.summary);
        assert!(fix.sql.is_none(), "measure branch selectivity after a rewrite before proposing DDL");
        assert!(fix.summary.contains("Preserve the original joins, permission checks"));
        let mut unavailable = snapshot.clone();
        unavailable.server.capabilities.retain(|c| *c != Capability::IndexUsage);
        unavailable
            .notes
            .push(culprit_core::CollectorNote { collector: "indexes".into(), message: "unavailable".into() });
        let unknown = scan_fix_on(&unavailable, 2.0);
        assert!(!unknown.summary.contains("has no index"), "unavailable definitions do not prove absence");
        // With an index on email too, each side names its own and there is nothing to create.
        snapshot.indexes.push(index(&snapshot, "app_shares", "app_shares_email_idx", &["email"]));
        let fix = scan_fix_on(&snapshot, 2.0);
        assert!(fix.summary.contains("app_shares_email_idx on public.app_shares leads with email"), "{}", fix.summary);
        assert!(fix.sql.is_none(), "{:?}", fix.sql);
    }

    #[test]
    fn a_whole_table_read_under_the_cost_bar_gets_the_scan_steps_advice() {
        // Costed at 177, under the 250 a step's share needs, yet not cheap: it reads all of apps on every run. The fix
        // comes from that scan (the OR as a UNION), not "kept up to date instead of recomputed" for a 2-row lookup.
        let mut snapshot = postgres_demo();
        snapshot.indexes.push(index(&snapshot, "apps", "idx_apps_owner", &["owner_id"]));
        let mut plan = apps_plan();
        (plan.total_cost, plan.root.subtree_cost) = (177.44, 177.44);
        let q = &mut snapshot.workload.as_mut().expect("workload").queries[0];
        q.text = APPS_QUERY.into();
        (q.mean_time_ms, q.executions) = (1.44, q.total_time_ms / 1.44);
        (q.rows, q.logical_reads) = (Some(2.5 * q.executions), Some(60.0 * q.executions));
        q.plan = Some(plan);
        let id = q.id.clone();
        let f = heavy_query(&snapshot).into_iter().find(|f| f.queries[0].id == id).expect("the heavy query");
        let fix = f.fix.expect("a fix");
        assert!(fix.summary.contains("UNION rewrite"), "{}", fix.summary);
        assert!(!fix.summary.contains("kept up to date"), "{}", fix.summary);
        assert!(!f.summary.contains("of its estimated cost"), "no share under the bar: {}", f.summary);
        // The summary names the scan the fix is about.
        assert!(
            f.summary.ends_with(
                "Every run includes a seq scan of public.apps that reads 6.3K rows: a scan reads the whole table, and costs more as the table grows."
            ),
            "{}",
            f.summary
        );
        assert_eq!(f.queries[0].plan_node, Some(7), "the step the fix is about");
    }

    #[test]
    fn a_plan_that_reads_a_whole_table_is_not_called_cheap() {
        // Costed under what reading 10K rows costs, a millisecond a run and ten pages per row: cheap only while no
        // step reads a table in full, which costs more as the table grows.
        let snapshot: Snapshot =
            serde_json::from_str(include_str!("../../../../../fixtures/postgres-demo.json")).expect("fixture");
        let mut plan = apps_plan();
        (plan.total_cost, plan.root.subtree_cost) = (100.0, 100.0);
        let mut q = snapshot.workload.as_ref().expect("workload").queries[0].clone();
        (q.mean_time_ms, q.logical_reads, q.rows) = (1.0, Some(20.0 * q.executions), Some(2.0 * q.executions));
        assert!(!culprit_core::insights::runs_cheaply(&plan, &q));
        // The same plan finding its rows through indexes.
        fn seek(n: &mut PlanNode) {
            if n.kind == NodeKind::Scan {
                n.kind = NodeKind::Seek;
            }
            n.children.iter_mut().for_each(seek);
        }
        seek(&mut plan.root);
        assert!(culprit_core::insights::runs_cheaply(&plan, &q));
    }

    #[test]
    fn an_or_across_tables_is_told_from_one_within_a_table() {
        assert!(or_across_tables("(((a.owner_id)::text = $1) OR ((s.email)::text = $2))"));
        assert!(or_across_tables(
            "[db].[app].[messages].[tenant_id]=[@par0] OR [db].[app].[conversations].[account_id]=[@par1]"
        ));
        assert!(!or_across_tables("((a.status = 1) OR (a.status = 2)) AND b.id = a.owner_id"));
        assert!(!or_across_tables("(s.label = 'black or white') AND (a.id = s.app_id)"));
        assert!(!or_across_tables("(a.deleted_at IS NULL)"));
    }

    #[test]
    fn a_pools_statements_get_the_pools_own_lever() {
        // PostgreSQL's DISCARD ALL is the pool's reset: its setting, not transactions or pings.
        let discard = session_fix(Engine::Postgres, "DISCARD ALL");
        assert!(
            discard.contains("do not identify its caller or pooling mode") && discard.contains("Keep resets"),
            "{discard}"
        );
        assert!(!discard.contains("sp_reset_connection"), "{discard}");
        assert!(
            discard.contains("Npgsql") && discard.contains("PgBouncer") && discard.contains("benchmark"),
            "{discard}"
        );
        // sp_reset_connection is SQL Server's.
        let set = session_fix(Engine::Mssql, "SET TRANSACTION ISOLATION LEVEL READ COMMITTED");
        assert!(set.contains("sp_reset_connection"), "{set}");
        let commit = session_fix(Engine::Postgres, "COMMIT");
        assert!(commit.contains("transaction of its own") && !commit.contains("sp_reset_connection"), "{commit}");
        let ping = session_fix(Engine::Mysql, "/* ping */ SELECT 1");
        assert!(ping.contains("still alive"), "{ping}");
    }
}
