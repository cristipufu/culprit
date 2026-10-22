//! Rules over every check's findings: the pipeline's refine stage (see [`crate::pipeline`]). Each rule fixes, once for
//! every check and engine, what no single check can see on its own: two checks describing one cause, or a finding that
//! rests on statements whose cost is behind them. A new rule declares its decision in the appropriate submodule and
//! adds a line in [`rules`].

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use culprit_core::{Engine, Finding, QueryStat, Severity, Snapshot, SubjectKind, format};

use crate::pipeline::Refine;
use crate::{signals, support};

mod coverage;
mod indexes;
mod lifecycle;
mod locks;
mod maintenance;
mod merge;
mod presentation;
mod queries;
#[cfg(test)]
mod review_tests;
mod writes;
#[cfg(test)]
use coverage::explained_heavy_query;
#[cfg(test)]
use lifecycle::stopped_running;
#[cfg(test)]
use presentation::caveats_repeat_nothing;

/// The built-in rules, in the order they run.
pub(crate) fn rules() -> Vec<Arc<dyn Refine>> {
    vec![
        // One cause, one finding.
        Arc::new(indexes::Rule(indexes::Policy::MissingScan)),
        Arc::new(indexes::Rule(indexes::Policy::PlanRequest)),
        Arc::new(coverage::Rule(coverage::Policy::Rewrite)),
        Arc::new(coverage::Rule(coverage::Policy::SpillSort)),
        Arc::new(queries::Rule(queries::Policy::SpillResult)),
        Arc::new(coverage::Rule(coverage::Policy::ForeignKey)),
        Arc::new(indexes::Rule(indexes::Policy::AutomaticForeignKey)),
        Arc::new(indexes::Rule(indexes::Policy::Histogram)),
        Arc::new(coverage::Rule(coverage::Policy::Regression)),
        Arc::new(queries::Rule(queries::Policy::PlanStory)),
        Arc::new(queries::Rule(queries::Policy::ScanRegression)),
        Arc::new(writes::Rule(writes::Policy::DeadRows)),
        Arc::new(writes::Rule(writes::Policy::ColdUpdates)),
        Arc::new(writes::Rule(writes::Policy::LargeValues)),
        Arc::new(locks::Rule(locks::Policy::Episode)),
        Arc::new(locks::Rule(locks::Policy::Chain)),
        Arc::new(maintenance::Rule(maintenance::Policy::Compiles)),
        Arc::new(maintenance::Rule(maintenance::Policy::Statistics)),
        Arc::new(maintenance::Rule(maintenance::Policy::Slot)),
        Arc::new(coverage::Rule(coverage::Policy::Network)),
        Arc::new(coverage::Rule(coverage::Policy::Heavy)),
        // Statements whose cost is behind them.
        Arc::new(lifecycle::Rule(lifecycle::Policy::Stopped)),
        Arc::new(lifecycle::Rule(lifecycle::Policy::Creation)),
        Arc::new(lifecycle::Rule(lifecycle::Policy::Once)),
        Arc::new(lifecycle::Rule(lifecycle::Policy::Gone)),
        Arc::new(lifecycle::Rule(lifecycle::Policy::Verification)),
        Arc::new(lifecycle::Rule(lifecycle::Policy::IndexWork)),
        Arc::new(presentation::Rule(presentation::Policy::AbsoluteCost)),
        // Wording.
        Arc::new(presentation::Rule(presentation::Policy::Names)),
        Arc::new(presentation::Rule(presentation::Policy::Caveats)),
    ]
}

/// A finding whose share of database time is its statements' but measures none of it: a guess at what the optimizer
/// may misjudge (a column with no histogram, statistics behind their table) or an engine's estimate of what an index
/// would save (Azure's index advice), and the list of forced plans. It never explains a heavy query away. SQL Server's
/// own plan advice stands on what it measured: the regression behind a plan to force, a forced plan's scan.
fn measures_no_time(f: &Finding) -> bool {
    matches!(f.check.as_str(), "missing-histogram" | "stale-stats")
        || (f.check == "tuning-advice"
            && !f.id.starts_with("tuning-advice:force:")
            && !f.id.starts_with("tuning-advice:forced-scan:"))
}

/// SQL Server writes names in plans and DMVs with the database in front (`[shop_db].[dbo].[orders].[email]`). In
/// sentences that's noise, and in a shared report it names the database: prose keeps `[dbo].[orders].[email]`. Fix
/// scripts are left as they are.
pub(crate) fn without_database(text: &str, database: &str) -> String {
    if database.is_empty() {
        return text.to_string();
    }
    let prefix = format!("[{}].[", database.replace(']', "]]"));
    text.replace(&prefix, "[")
}

/// The sentences of a text: split after a full stop, a question or an exclamation mark that a space and a capital
/// letter, a digit or a quote follow ("e.g. the" and "1.5 s" stay whole).
pub(crate) fn sentences(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    for w in chars.windows(3) {
        let [(at, end), (_, space), (_, next)] = [w[0], w[1], w[2]];
        if matches!(end, '.' | '?' | '!')
            && space == ' '
            && (next.is_uppercase() || next.is_ascii_digit() || matches!(next, '`' | '"' | '“'))
        {
            let sentence = text[start..at + 1].trim();
            if !sentence.is_empty() {
                out.push(sentence);
            }
            start = at + 1;
        }
    }
    let last = text[start..].trim();
    if !last.is_empty() {
        out.push(last);
    }
    out
}

/// A sentence as [`caveats_repeat_nothing`] compares it: lower case, spaces collapsed, no final stop.
fn sentence_key(sentence: &str) -> String {
    sentence.split_whitespace().collect::<Vec<_>>().join(" ").trim_end_matches(['.', '!', '?', ':', ';']).to_lowercase()
}

fn table_leaf(name: &str) -> String {
    name.rsplit('.').next().unwrap_or(name).trim_matches(['[', ']', '"', '`']).to_ascii_lowercase()
}

/// A query that stopped running long before the window ended was most likely changed or fixed by a deploy. Its history still
/// dominates a multi-day window, so without this the top of the list would be yesterday's problem. "Long" is relative to how
/// often the query ran ([`query_quiet`]: silence of 20 typical intervals, so a daily job is not mistaken for a fixed one),
/// and "stopped" is said only once that silence covers [`STOPPED_MIN_HOURS`]: a request every two hours, or a batch that
/// ran an hour ago, comes back. Returns how long it has been quiet (seconds) and when it last ran.
pub(crate) fn query_stopped(snapshot: &Snapshot, q: &QueryStat) -> Option<(f64, String)> {
    query_quiet(snapshot, q).filter(|(silence, _)| silent_long_enough(*silence))
}

/// Whether a finding on `queries` is told in the past: every one stopped running ([`query_stopped`]), the test
/// [`stopped_running`] marks the finding inactive by. Checks pick their title's verb by it ("spent", "was read").
pub(crate) fn all_stopped(snapshot: &Snapshot, queries: &[&QueryStat]) -> bool {
    !queries.is_empty() && queries.iter().all(|q| query_stopped(snapshot, q).is_some())
}

/// Hours of silence before "stopped" is said, of a statement or of the requests a statement made (a missing index):
/// the same 20 hours a sentence needs to say "a day" ([`culprit_core::PER_DAY_MIN_HOURS`]). Under that, a finding says
/// how long it has been quiet and stays active.
pub(crate) const STOPPED_MIN_HOURS: f64 = culprit_core::PER_DAY_MIN_HOURS;

/// Whether a silence of `seconds` before the scan's data ends is long enough to say "stopped" ([`STOPPED_MIN_HOURS`]).
pub(crate) fn silent_long_enough(seconds: f64) -> bool {
    seconds >= STOPPED_MIN_HOURS * 3600.0
}

/// Silence of 20 typical intervals (at least an hour) before the window ends, out of the query's own rhythm (a burst
/// that began inside the window, a gap it already came back from); how long ([`query_stopped`]) decides what it means.
/// Under [`STOPPED_MIN_HOURS`], a finding stays active and says how long it has been quiet ([`stopped_running`]).
fn query_quiet(snapshot: &Snapshot, q: &QueryStat) -> Option<(f64, String)> {
    const QUIET_INTERVALS: f64 = 20.0;
    const MIN_QUIET_SECONDS: f64 = 3600.0;
    // A day and a bit: long enough that a nightly job would have come back.
    const DAILY_SECONDS: f64 = 26.0 * 3600.0;
    // One run has no rhythm to stop: that's `ran_once`, not a deploy. The slow query log keeps only slow runs: one
    // not logged since may still run, faster than long_query_time.
    if ran_once(q) || crate::queries::slow_log_only(snapshot) {
        return None;
    }
    let workload = snapshot.workload.as_ref()?;
    let end = workload.window_end.as_deref().and_then(support::parse_epoch_seconds)?;
    let window_start = end - workload.measurement_hours() * 3600.0;
    let last_text = q.last_seen.clone()?;
    let last = support::parse_epoch_seconds(&last_text)?;
    let first = q.first_seen.as_deref().and_then(support::parse_epoch_seconds).unwrap_or(window_start);
    let interval = (last - first).max(1.0) / q.executions.max(1.0);
    let silence = end - last;
    if silence < (interval * QUIET_INTERVALS).max(MIN_QUIET_SECONDS) {
        return None;
    }
    // Runs that came in one burst inside the window (a nightly job, a batch: they began after it started and covered
    // a small part of it) have no rhythm the window shows: quiet since is how such a job looks, until a day has passed
    // without it coming back. A query running from the window's start (within a bucket of it) ran before it too.
    let bucket = workload
        .timeline
        .as_ref()
        .and_then(|t| {
            Some(support::parse_epoch_seconds(t.starts.get(1)?)? - support::parse_epoch_seconds(t.starts.first()?)?)
        })
        .unwrap_or(900.0);
    let began_inside = first - window_start > bucket.max(900.0);
    if began_inside && (last - first) < workload.measurement_hours() * 3600.0 * 0.5 && silence < DAILY_SECONDS {
        return None;
    }
    // A query that runs in bursts (an eval run, a batch) is quiet between them: being silent no longer than a gap it
    // already came back from is its rhythm, not a deploy.
    if let Some(gap) = longest_return(q, workload)
        && silence <= gap
    {
        return None;
    }
    Some((silence, last_text))
}

/// Once `q` stopped (see `query_stopped`), the statement that may have replaced it: one still running that writes the
/// same table the same way (an insert into it, for an insert), and wasn't already running before `q` began; the one
/// that runs most. Only for writes: many statements read a table, and none of them says it took a read's place.
pub(crate) fn query_successor<'a>(snapshot: &'a Snapshot, q: &QueryStat) -> Option<&'a QueryStat> {
    let engine = snapshot.server.engine;
    let table = support::write_targets(q, engine)
        .into_iter()
        .find_map(|o| snapshot.tables.iter().find(|t| support::names_table(&o, &t.schema, &t.name)))?;
    let verb = support::first_verb(support::main_statement(&q.text)).map(|(_, v)| v)?;
    support::app_queries(snapshot)
        .filter(|o| o.id != q.id && o.executions > 0.0 && !query_is_history(snapshot, o))
        .filter(|o| support::first_verb(support::main_statement(&o.text)).is_some_and(|(_, v)| v == verb))
        .filter(|o| support::writes_table(o, engine, &table.schema, &table.name))
        // One that was already running before this one began didn't replace it.
        .filter(|o| match (o.first_seen.as_deref(), q.first_seen.as_deref()) {
            (Some(theirs), Some(ours)) => {
                support::parse_epoch_seconds(theirs).zip(support::parse_epoch_seconds(ours)).is_none_or(|(a, b)| a >= b)
            }
            _ => true,
        })
        .max_by(|a, b| a.executions.total_cmp(&b.executions))
}

/// A statement seen once in the window: an export, a backfill, a migration or a query someone ran by hand looks like
/// this. One run shows what it cost, not that it will run again, so it isn't a pattern to fix.
pub(crate) fn ran_once(q: &QueryStat) -> bool {
    q.executions <= 1.0
}

/// Whether a statement's cost is behind it: it stopped running, or it ran once. Advice (an index to add, a plan to
/// look at) is for statements that keep running.
pub(crate) fn query_is_history(snapshot: &Snapshot, q: &QueryStat) -> bool {
    ran_once(q) || query_stopped(snapshot, q).is_some()
}

/// The longest stretch of empty timeline buckets the query came back after, widened by the two partial buckets around
/// it (when in them it ran isn't known). None when it never paused for a whole bucket.
fn longest_return(q: &QueryStat, workload: &culprit_core::Workload) -> Option<f64> {
    let trend = q.trend.as_ref()?;
    let timeline = workload.timeline.as_ref()?;
    let at = |i: usize| timeline.starts.get(i).and_then(|s| support::parse_epoch_seconds(s));
    let width = at(1)? - at(0)?;
    let active: Vec<usize> =
        trend.executions.iter().enumerate().filter(|(_, e)| e.is_some_and(|e| e > 0.0)).map(|(i, _)| i).collect();
    let empty = active.windows(2).map(|w| w[1] - w[0] - 1).max().filter(|&n| n >= 1)?;
    Some((empty + 2) as f64 * width)
}

/// A finding is about queries when its subject is one (lock time, tuning advice, parameter sensitivity…) or its check
/// reads plans: what it says rests on how those statements ran.
fn about_queries(f: &Finding) -> bool {
    // A family's statements are literal copies of one: each runs once and is gone by construction, the family runs on.
    // Structured error events retain their own dates, including untimed failures after the last timed execution.
    // Linking that old execution for context must not make a newly logged failure history.
    if matches!(f.check.as_str(), "query-family" | "logged-errors") {
        return false;
    }
    const QUERY_DRIVEN: [&str; 11] = [
        // An index request or a table's scans stand on the statements that asked for them or read it.
        "missing-index",
        "scan-heavy",
        "plan-warnings",
        "plan-insights",
        "filtered-scan",
        "heavy-query",
        "chatty-query",
        "unstable-query",
        "plan-regression",
        "regression",
        "temp-spill",
    ];
    !f.queries.is_empty() && (f.subject.kind == SubjectKind::Query || QUERY_DRIVEN.contains(&f.check.as_str()))
}

/// Whether fix SQL creates something in the database: an index or statistics (`CREATE …`, `ALTER TABLE … ADD`, an
/// index rebuilt with more columns). An event session (`CREATE EVENT SESSION`) only watches, and a setting or a plan
/// forced creates no object. Comments don't count.
pub(crate) fn creates_something(sql: &str) -> bool {
    let text: Vec<&str> = sql.lines().map(|line| line.split("--").next().unwrap_or("")).collect();
    let words: Vec<String> = text.join(" ").split_whitespace().map(|w| w.to_ascii_lowercase()).collect();
    words.windows(2).any(|w| match (w[0].as_str(), w[1].as_str()) {
        ("create", next) => next != "event",
        ("alter", "table") | ("alter", "index") => {
            words.iter().any(|w| w == "add" || w == "rebuild" || w.starts_with("include"))
        }
        _ => false,
    })
}

/// Whether a statement's last run is only the oldest of Culprit's scans that tracked it. A source that keeps no last
/// run (pg_stat_statements) is dated by the scans: one whose count hasn't grown since the first of them last ran by
/// then at the latest, and may have stopped long before (see `culprit_engine::trends::last_seen_from_scans`).
pub(crate) fn last_ran_by_first_scan(workload: &culprit_core::Workload, last: &str) -> bool {
    let first = workload.timeline.as_ref().and_then(|t| t.starts.first()).and_then(|s| support::parse_epoch_seconds(s));
    !workload.is_windowed()
        && first.zip(support::parse_epoch_seconds(last)).is_some_and(|(first, last)| (first - last).abs() < 1.0)
}

// ── indexes ──

/// Whether the index a missing-index request creates (`missing_keys`, in its DDL's order) serves the scan a plan shows
/// filtering on `scan_keys`: the index leads with a column the scan filters on, so the scan's rows can be sought by it.
/// An index that holds the scan's column only further along (`(customer_id, status)` for a scan on `status`) could at
/// best be scanned, so that scan keeps its own finding and its own index.
pub(crate) fn missing_index_serves_scan(missing_keys: &[String], scan_keys: &[String]) -> bool {
    missing_keys.first().is_some_and(|lead| scan_keys.iter().any(|k| k.eq_ignore_ascii_case(lead)))
}

/// The table a finding about a table names, as `schema.table` lowercased: its subject, or for hot-rows (whose subject
/// is the index most of the waiting was on) its id.
fn finding_table(f: &Finding) -> String {
    match f.id.strip_prefix("hot-rows:") {
        Some(table) => table.to_ascii_lowercase(),
        None => f.subject.name.to_ascii_lowercase(),
    }
}

/// Takes the finding at `from` out of `findings` and lets `into` (the finding that names the cause) absorb what it
/// adds: its queries, its rank and, unless its statements are history (they stopped running, or ran once), its
/// severity. The finding after `from` takes its place.
#[cfg(test)]
fn absorb(snapshot: &Snapshot, findings: &mut Vec<Finding>, into: usize, from: usize, note: Option<String>) {
    let decision = merge::Absorption::decide(snapshot, findings, into, from);
    decision.apply_state(findings);
    decision.render(findings, note);
    decision.finish(findings);
}

/// A finding's share of database time when it is the share of the statements it lists, as the check gave it (its
/// statements' own `query_impact`): `None` for a share of something else (a scan's part of a plan, a capped or
/// recomputed figure) or a finding without one.
fn share_of_listed(snapshot: &Snapshot, f: &Finding) -> Option<f64> {
    let workload = snapshot.workload.as_ref()?;
    let share = f.impact.db_time_share?;
    let listed: Vec<&QueryStat> = f.queries.iter().map(|r| workload.query(&r.id)).collect::<Option<Vec<_>>>()?;
    let theirs = support::query_impact(snapshot, &listed)?.share;
    ((theirs - share).abs() <= 0.0005).then_some(share)
}

/// The position of the first base finding of `check` about `table` (`schema.table`, lowercased).
fn position_on(findings: &[Finding], check: &str, table: &str) -> Option<usize> {
    let base_id = format!("{check}:{table}");
    findings.iter().position(|f| f.check == check && finding_table(f) == table && f.id.eq_ignore_ascii_case(&base_id))
}

fn leaf(table: &str) -> &str {
    table.rsplit('.').next().unwrap_or(table)
}

fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    chars.next().map(|c| c.to_uppercase().collect::<String>() + chars.as_str()).unwrap_or_default()
}

// ── pressure, config, maintenance ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::support::{finding, subject};
    use culprit_core::{Category, QueryRef};

    fn fixture(name: &str) -> Snapshot {
        let text =
            std::fs::read_to_string(format!("{}/../../fixtures/{name}", env!("CARGO_MANIFEST_DIR"))).expect("fixture");
        serde_json::from_str(&text).expect("snapshot")
    }

    fn query<'a>(s: &'a mut Snapshot, id: &str) -> &'a mut QueryStat {
        s.workload.as_mut().expect("workload").queries.iter_mut().find(|q| q.id == id).expect("query")
    }

    /// A finding of `check` about the statements `ids`, weighing `share` of database time.
    fn about(check: &str, key: &str, ids: &[&str], share: f64) -> Finding {
        let mut f = finding(check, key, Category::Queries, subject(SubjectKind::Query, key));
        f.impact.db_time_share = Some(share);
        f.queries = ids.iter().map(|id| QueryRef { id: id.to_string(), plan_node: None, note: None }).collect();
        f
    }

    // The fresh MySQL reading: missing-histogram's guess about two statements' estimates (16% of database time, the
    // two together) took away both heavy queries (8.5% and 7.9%), the planted scan with them. A finding explains a
    // heavy query only by the time it measured in that statement.
    #[test]
    fn a_heavy_query_is_explained_only_by_the_time_a_finding_measured_in_it() {
        let s = fixture("mysql-demo.json");
        let w = s.workload.as_ref().unwrap();
        let ids = ["19bd4ff468839ea3", "c7b5ac9bb4bcecfe"];
        let shares: Vec<f64> =
            ids.iter().map(|id| w.query(id).map(|q| w.share_of(q.total_time_ms, q)).unwrap()).collect();
        let (a, b) = (shares[0], shares[1]);
        let heavy = |at: usize| about("heavy-query", ids[at], &ids[at..=at], shares[at]);
        let left = |cause: Finding| {
            let mut findings = vec![heavy(0), heavy(1), cause];
            explained_heavy_query(&s, &mut findings);
            findings.into_iter().filter(|f| f.check == "heavy-query").map(|f| f.id).collect::<Vec<_>>()
        };
        // A guess at estimates measures no time: both stay.
        assert_eq!(left(about("missing-histogram", "shop.orders", &ids, a + b)).len(), 2);
        assert_eq!(left(about("stale-stats", "shop.orders", &ids, a + b)).len(), 2);
        assert_eq!(left(about("tuning-advice", "create-index:shop.orders(promo_code)", &ids, a + b)).len(), 2);
        // A scan both statements make is each one's time: both go.
        assert!(left(about("filtered-scan", "shop.orders(promo_code)", &ids, a + b)).is_empty());
        // Waits that are a third of their time together are a third of each one's: under half, both stay, though
        // the total passes half of either.
        assert!((a + b) / 3.0 >= a.min(b) * 0.5, "{a} {b}");
        assert_eq!(left(about("hot-rows", "shop.orders", &ids, (a + b) / 3.0)).len(), 2);
        // SQL Server's plan to force stands on the regression it measured.
        let force = about("tuning-advice", &format!("force:{}", ids[0]), &ids[..1], a);
        assert_eq!(left(force), vec![format!("heavy-query:{}", ids[1])]);
    }

    // The PostgreSQL demo: the sort that spills and the result it returns are one statement, one share: big-result
    // and temp-spill each said 45% of database time, with two fixes.
    #[test]
    fn a_spill_carries_the_big_result_it_sorts() {
        let report = crate::analyze(fixture("postgres-demo.json"));
        let id = "9065391607317686058";
        assert!(!report.findings.iter().any(|f| f.id == format!("big-result:{id}")));
        let spill = report.findings.iter().find(|f| f.id == format!("temp-spill:{id}")).expect("the spill");
        assert!(
            spill.summary.contains(" Each run also sends 247K rows (≈ 17.4 MB) to the application"),
            "{}",
            spill.summary
        );
        let fix = &spill.fix.as_ref().unwrap().summary;
        assert!(
            fix.contains(
                "Where the application doesn't use every row it receives, return only what's used: page the results"
            ),
            "{fix}"
        );
        // The spill's step names the two scans to compare; the rows' step reads its rows between them.
        assert!(
            spill.verify.iter().any(|v| v
                == "If it now returns fewer rows, the rows it returns per run between those two scans should drop to what a page or a job actually uses."),
            "{:?}",
            spill.verify
        );
        assert!(spill.metrics.iter().any(|m| m.label == "Rows per run"));
        assert_eq!(report.findings.iter().filter(|f| f.queries.iter().any(|q| q.id == id)).count(), 1);
    }

    // The fresh SQL Server reading: plan-regression:244 said to force the fast plan and filtered-scan to create an
    // index, for one statement whose plan began scanning when the index it sought went away.
    #[test]
    fn a_scan_that_came_with_a_plan_change_carries_the_regression() {
        let mut s = fixture("mssql-demo.json");
        let q = query(&mut s, "8");
        q.plan_id = Some("47".into());
        for p in q.plans.iter_mut() {
            p.logical_reads = Some(p.executions * if p.id == "47" { 5_700.0 } else { 3.0 });
        }
        let mut scan = about("filtered-scan", "shop.orders(shipped_at)", &["8"], 0.12);
        scan.queries[0].plan_node = Some(2);
        scan.summary = "One query plans to read every row of shop.orders.".into();
        let mut regression = about("plan-regression", "8", &["8"], 0.12);
        regression.facts.plan_change = Some(culprit_core::FindingPlanChange { from: "8".into(), to: "47".into() });
        regression.summary = "Query Store saw this query run with plan 8 at 9.5 ms on average until Oct 11, 10:44 UTC, and with plan 47 at 32 ms since Oct 11, 10:46 UTC (93 runs). The plan choice changed, typically after a statistics update.".into();
        regression.metrics.push(culprit_core::Metric::new("Fast plan", 9.5, culprit_core::Unit::Ms));
        let mut findings = vec![regression.clone(), scan.clone()];
        queries::Rule(queries::Policy::ScanRegression).refine(&s, &mut findings);
        assert_eq!(findings.len(), 1, "{:?}", findings.iter().map(|f| &f.id).collect::<Vec<_>>());
        assert_eq!(
            findings[0].summary,
            "One query plans to read every row of shop.orders. Query Store saw this query run with plan 8 at 9.5 ms on average until Oct 11, 10:44 UTC, and with plan 47 at 32 ms since Oct 11, 10:46 UTC (93 runs). The scan came with the plan change: plan 8 read about 3 pages a run, plan 47 reads about 5.7K."
        );
        assert!(findings[0].metrics.iter().any(|m| m.label == "Fast plan"));
        // A scan the plan doesn't show (SQL Server's index request alone) is another story: the regression stays.
        scan.queries[0].plan_node = None;
        let mut findings = vec![regression, scan];
        queries::Rule(queries::Policy::ScanRegression).refine(&s, &mut findings);
        assert_eq!(findings.len(), 2);
    }

    // The aptabase-us reading: an insert stopped and another took its place, named only in one finding's fix.
    #[test]
    fn a_stopped_statement_names_what_may_have_replaced_it() {
        let mut s = fixture("postgres-demo.json");
        let gone = query(&mut s, "3432126458605143571");
        gone.text = "INSERT INTO events (customer_id, kind) VALUES ($1, $2) ON CONFLICT DO NOTHING".into();
        gone.plan = None;
        // Quiet for the last four hours of the window: too soon to say it stopped. It stays active and says so.
        gone.last_seen = Some("2026-10-11T08:00:00Z".into());
        let next = query(&mut s, "2885972271926804069");
        next.text =
            "INSERT INTO events (customer_id, kind) VALUES ($1, $2) ON CONFLICT (id) DO UPDATE SET kind = $2".into();
        next.plan = None;
        let mut findings = vec![about("heavy-query", "3432126458605143571", &["3432126458605143571"], 0.2)];
        stopped_running(&s, &mut findings);
        assert!(!findings[0].inactive);
        assert!(
            findings[0].caveats[0].starts_with("This query hasn't run in the last ")
                && findings[0].caveats[0].ends_with("too short a time to say it stopped."),
            "{:?}",
            findings[0].caveats
        );
        // Quiet since the morning before: a day of silence, so it stopped.
        query(&mut s, "3432126458605143571").last_seen = Some("2026-10-10T08:00:00Z".into());
        let mut findings = vec![about("heavy-query", "3432126458605143571", &["3432126458605143571"], 0.2)];
        stopped_running(&s, &mut findings);
        assert!(findings[0].inactive);
        assert!(
            findings[0].caveats[0].ends_with(
                "Query 2885972271926804069, an insert into events too, still runs and may be what replaced it: check that one."
            ),
            "{:?}",
            findings[0].caveats
        );
        // A read of the same table took no write's place.
        query(&mut s, "2885972271926804069").text = "SELECT kind FROM events WHERE customer_id = $1".into();
        let mut findings = vec![about("heavy-query", "3432126458605143571", &["3432126458605143571"], 0.2)];
        stopped_running(&s, &mut findings);
        assert!(!findings[0].caveats[0].contains("replaced"), "{:?}", findings[0].caveats);
    }
}

#[cfg(test)]
mod tables_tests {
    use super::*;
    use crate::support::{finding, subject};
    use culprit_core::{Category, Fix, QueryRef};

    fn fixture(name: &str) -> Snapshot {
        let text =
            std::fs::read_to_string(format!("{}/../../fixtures/{name}", env!("CARGO_MANIFEST_DIR"))).expect("fixture");
        serde_json::from_str(&text).expect("snapshot")
    }

    // CX-10d: a caveat repeated the fix ("… already leads with …; the recommended index differs from it only by …").
    // Whatever check wrote it, a caveat sentence the summary or the fix already says goes; the rest of it stays.
    #[test]
    fn a_caveat_repeats_nothing_the_summary_or_the_fix_says() {
        let s = fixture("postgres-demo.json");
        let mut f = finding("tuning-advice", "x", Category::Indexes, subject(SubjectKind::Table, "app.job_runs"));
        f.summary = "The index helps. It costs every write.".into();
        f.fix = Some(Fix { summary: "Nothing to create: ix_a already leads with a.".into(), sql: None, cost: None });
        f.caveats = vec![
            "Nothing to create: ix_a already leads with a".into(),
            "It costs every write. Measure it first.".into(),
            "Quoted, as recorded:\nIt costs every write.".into(),
            "Something new.".into(),
        ];
        let mut findings = vec![f];
        caveats_repeat_nothing(&s, &mut findings);
        assert_eq!(
            findings[0].caveats,
            vec!["Measure it first.", "Quoted, as recorded:\nIt costs every write.", "Something new."]
        );
        // "e.g. the" and "1.5 s" are inside one sentence.
        assert_eq!(
            sentences("Use one, e.g. the first. It takes 1.5 s. Done"),
            vec!["Use one, e.g. the first.", "It takes 1.5 s.", "Done"]
        );
    }

    // CX-11: large-rows listed four statements and claimed the share of three: folding log-volume's statement in kept
    // the larger of the two shares. A finding's share counts every statement it lists.
    #[test]
    fn a_finding_that_absorbs_another_counts_every_statement_it_lists() {
        let s = fixture("postgres-demo.json");
        let w = s.workload.as_ref().unwrap();
        let ids: Vec<String> = w.queries.iter().take(3).map(|q| q.id.clone()).collect();
        let share = |of: &[String]| {
            let qs: Vec<&QueryStat> = of.iter().filter_map(|id| w.query(id)).collect();
            support::query_impact(&s, &qs).unwrap().share
        };
        let about = |check: &str, of: &[String], share: Option<f64>| {
            let mut f = finding(check, &of.join("+"), Category::Queries, subject(SubjectKind::Table, "public.x"));
            f.impact.db_time_share = share;
            f.queries = of.iter().map(|id| QueryRef { id: id.clone(), plan_node: None, note: None }).collect();
            f
        };
        let (two, one) = (ids[..2].to_vec(), ids[2..].to_vec());
        let mut findings =
            vec![about("large-rows", &two, Some(share(&two))), about("log-volume", &one, Some(share(&one)))];
        absorb(&s, &mut findings, 0, 1, None);
        assert_eq!(findings.len(), 1);
        assert!((findings[0].impact.db_time_share.unwrap() - share(&ids)).abs() < 1e-9, "{:?}", findings[0].impact);
        // A finding with no share of its own counts its statements too once it takes one in.
        let mut findings = vec![about("hot-rows", &two, None), about("lock-time", &one, Some(share(&one)))];
        absorb(&s, &mut findings, 0, 1, None);
        assert!((findings[0].impact.db_time_share.unwrap() - share(&ids)).abs() < 1e-9, "{:?}", findings[0].impact);
        // A share that isn't its statements' (a scan's part of a plan) is kept as the larger of the two.
        let mut findings =
            vec![about("filtered-scan", &two, Some(0.001)), about("log-volume", &one, Some(share(&one)))];
        absorb(&s, &mut findings, 0, 1, None);
        assert_eq!(findings[0].impact.db_time_share, Some(0.001f64.max(share(&one))));
    }
}

#[cfg(test)]
mod prose_tests {
    use super::without_database;

    #[test]
    fn prose_names_lose_the_database() {
        assert_eq!(
            without_database("CONVERT_IMPLICIT(nvarchar(200),[shop_db].[dbo].[customers].[email],0)", "shop_db"),
            "CONVERT_IMPLICIT(nvarchar(200),[dbo].[customers].[email],0)"
        );
        assert_eq!(without_database("[other].[dbo].[t]", "shop_db"), "[other].[dbo].[t]");
    }
}
