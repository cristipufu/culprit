use super::*;

#[test]
fn a_scan_without_query_history_says_why() {
    let mut snapshot: Snapshot = serde_json::from_str(include_str!("../../../../fixtures/mssql-demo.json")).unwrap();
    assert_eq!(snapshot.no_workload_reason(), None);
    snapshot.workload = None;
    snapshot.server.missing.retain(|m| m.capability != Capability::QueryStore);
    assert_eq!(snapshot.no_workload_reason().as_deref(), Some("This scan has no query history."));
    snapshot.server.missing.push(MissingCapability {
        capability: Capability::QueryStore,
        reason: "Query Store is OFF on this database.".into(),
        fix: None,
    });
    assert_eq!(snapshot.no_workload_reason().as_deref(), Some("Query Store is OFF on this database."));
}

#[test]
fn percentiles_never_pass_the_slowest_run() {
    let mut s = Spread { p50_ms: Some(10.0), p95_ms: Some(40.0), p99_ms: Some(43.7), ..Default::default() };
    s.clamp_to(Some(43.4));
    assert_eq!((s.p50_ms, s.p95_ms, s.p99_ms), (Some(10.0), Some(40.0), Some(43.4)));
    s.clamp_to(None);
    assert_eq!(s.p99_ms, Some(43.4));
}

/// The app reads each engine's facts (abilities, session actions, the reasons it refuses) from the engine files
/// themselves: written next to the generated types (`cargo test -p culprit-core`), so the browser demo and every
/// view answer as the engine does and can't drift from it.
#[test]
fn engine_facts_are_exported_for_the_app() {
    let table: std::collections::BTreeMap<String, crate::engines::EngineFacts> =
        Engine::ALL.iter().map(|e| (wire_name(e), e.facts())).collect();
    assert_eq!(table.len(), Engine::ALL.len());
    // Every ability an engine lacks has its own reason.
    for e in Engine::ALL {
        for a in Ability::ALL {
            if !e.abilities().has(a) {
                assert!(!e.spec().refusal(a).message.is_empty(), "{e} {a:?}");
            }
        }
    }
    if let Ok(dir) = std::env::var("TS_RS_EXPORT_DIR") {
        // In the engines' own order (`Engine::ALL`), which the app keeps wherever it lists them.
        struct InOrder;
        impl serde::Serialize for InOrder {
            fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                use serde::ser::SerializeMap;
                let mut map = serializer.serialize_map(Some(Engine::ALL.len()))?;
                for e in Engine::ALL {
                    map.serialize_entry(&wire_name(&e), &e.facts())?;
                }
                map.end()
            }
        }
        let text = serde_json::to_string_pretty(&InOrder).expect("facts") + "\n";
        std::fs::write(std::path::Path::new(&dir).join("engines.json"), text).expect("app/src/gen/engines.json");
    }
}

#[test]
fn how_often_never_extrapolates_a_short_window() {
    let w = |hours: f64, windowed: bool| Workload {
        counter_set: None,
        history_inputs: Vec::new(),
        measurement: None,
        source: "Query Store".into(),
        kind: Some(WorkloadKind::QueryStore),
        windowed,
        window_start: None,
        window_end: None,
        window_hours: hours,
        total_time_ms: 0.0,
        total_cpu_ms: None,
        total_executions: 0.0,
        query_count: 0.0,
        queries: vec![],
        timeline: None,
        untracked_time_ms: None,
        hourly_time_ms: None,
        min_logged_ms: None,
        log: None,
    };
    // 4 runs in 10 hours is not "~9.6 runs a day".
    assert_eq!(w(10.0, true).how_often(4.0), "4 runs over the last 10 hours");
    assert_eq!(w(10.0, true).per_day_measured(4.0), None);
    assert_eq!(w(0.5, false).how_often(1.0), "1 run over the 30 minutes since statistics were reset");
    assert_eq!(w(48.0, true).how_often(4200.0), "about 2.1K runs a day");
    // A cumulative source that can't say when it started counting has no window to put a rate on.
    let mut unknown = w(0.0, false);
    assert!(unknown.window_unknown());
    assert_eq!(unknown.how_often(3.0), "3 runs over the time since statistics were last reset");
    assert_eq!(unknown.rate_per_day(3.0), None);
    unknown.window_start = Some("2026-10-01T00:00:00Z".into());
    assert!(!unknown.window_unknown());
    // An application's log covers its own period, which needn't end at the scan; one without timestamps, a period
    // nobody can say.
    let mut log = w(0.8, true);
    log.log = Some("Rails".into());
    assert_eq!(log.how_often(3.0), "3 runs over the 48 minutes the Rails log covers");
    log.window_hours = 0.0;
    assert!(log.window_unknown());
    assert_eq!(log.how_often(3.0), "3 runs over the period the Rails log covers");
    // Fewer runs than days: what they are, not "0.2 runs a day".
    assert_eq!(w(146.0, true).how_often(1.0), "1 run in the last 6 days");
}

fn statement(runs: f64, first: Option<&str>, last: Option<&str>) -> QueryStat {
    let mut q: QueryStat = serde_json::from_value(serde_json::json!({
        "id": "244", "query_hash": null, "text": "SELECT AVG(value) FROM ctx.readings WHERE sensor = @s",
        "parameters": null, "object_name": null, "executions": runs, "total_time_ms": runs * 5.0,
        "mean_time_ms": 5.0, "max_time_ms": null, "stddev_time_ms": null, "cpu_time_ms": null, "rows": null,
        "logical_reads": null, "physical_reads": null, "temp_writes": null, "first_seen": first,
        "last_seen": last, "plans": [], "plan": null, "waits": []
    }))
    .unwrap();
    q.trend = None;
    q
}

/// Six days of Query Store, Oct 11 06:00 to Oct 17 06:00.
fn six_days() -> Workload {
    serde_json::from_value(serde_json::json!({
        "source": "Query Store", "kind": "query-store", "windowed": true,
        "window_start": "2026-10-11T06:00:00Z", "window_end": "2026-10-17T06:00:00Z", "window_hours": 144.0,
        "total_time_ms": 100000.0, "total_cpu_ms": null, "total_executions": 10000.0, "query_count": 1.0,
        "queries": []
    }))
    .unwrap()
}

// The fresh SQL Server demo: 750 runs in three minutes, then never again, read as "about 123 runs a day"; a single
// run as "0.2 runs a day".
#[test]
fn a_burst_and_a_few_runs_are_said_as_they_came() {
    let w = six_days();
    let burst = statement(750.0, Some("2026-10-11T15:51:56Z"), Some("2026-10-11T15:55:15Z"));
    assert_eq!(w.how_often_of(&burst), "750 runs on Oct 11, 15:51–15:55 UTC");
    assert_eq!(w.runs_per_day_of(&burst), None);
    let once = statement(1.0, Some("2026-10-11T13:58:49Z"), Some("2026-10-11T13:58:49Z"));
    assert_eq!(w.how_often_of(&once), "1 run, on Oct 11 at 13:58 UTC");
    // One run known only to the trend buckets that hold it is a point between their bounds, not a 47-minute run.
    let bucketed = statement(1.0, Some("2026-10-11T08:41:00Z"), Some("2026-10-11T09:28:00Z"));
    assert_eq!(w.how_often_of(&bucketed), "1 run, sometime between 08:41 and 09:28 UTC on Oct 11");
    let night = statement(40.0, Some("2026-10-11T22:10:00Z"), Some("2026-10-12T03:10:00Z"));
    assert_eq!(w.how_often_of(&night), "40 runs between Oct 11, 22:10 and Oct 12, 03:10 UTC");
    // Three runs over the window, dates unknown: a count.
    assert_eq!(w.how_often_of(&statement(3.0, None, None)), "3 runs in the last 6 days");
    // Runs for two of the six days, then none: a rate over those two days.
    let stopped = statement(2000.0, Some("2026-10-11T08:00:00Z"), Some("2026-10-13T08:00:00Z"));
    assert_eq!(w.how_often_of(&stopped), "about 1.0K runs a day from Oct 11 to Oct 13");
    // Since two days ago, still running.
    let recent = statement(2000.0, Some("2026-10-15T06:00:00Z"), Some("2026-10-17T05:59:00Z"));
    assert_eq!(w.how_often_of(&recent), "about 1.0K runs a day since Oct 15");
    // Steady over the window.
    let steady = statement(6000.0, Some("2026-10-11T06:01:00Z"), Some("2026-10-17T05:59:00Z"));
    assert_eq!(w.how_often_of(&steady), "about 1.0K runs a day");
    assert_eq!(w.runs_per_day_of(&steady), Some(1000.0));
}

/// The fresh MySQL demo: performance_schema reset 1.66 hours before the scan, every statement run in the first
/// minute (a temp-table spill of 35.8 s, 30K lookups in six seconds) and none since.
fn two_hours_of_counters() -> Workload {
    serde_json::from_value(serde_json::json!({
        "source": "performance_schema", "kind": "performance-schema", "windowed": false,
        "window_start": "2026-10-17T10:17:02Z", "window_end": "2026-10-17T11:56:43Z", "window_hours": 1.66,
        "total_time_ms": 45784.0, "total_cpu_ms": null, "total_executions": 40080.0, "query_count": 6.0,
        "queries": []
    }))
    .unwrap()
}

// MY-1: a 40-second burst in a window under a day was read as runs a day ("433K a day" from 30K in six seconds)
// and graded High on 623 s "a day" of spill.
#[test]
fn a_burst_in_a_window_under_a_day_is_when_it_ran_and_its_total() {
    let w = two_hours_of_counters();
    let burst = statement(30_000.0, Some("2026-10-17T10:17:02Z"), Some("2026-10-17T10:17:08Z"));
    assert_eq!(w.how_often_of(&burst), "30K runs on Oct 17 at 10:17 UTC");
    assert!(w.cadence(&burst).is_burst());
    // Graded on its total, whatever the window around it.
    assert_eq!(w.rate_per_day_of(&burst, 30_000.0), Some(30_000.0));
    assert_eq!(w.rate_per_day_of(&burst, 35_824.0), Some(35_824.0));
    // Runs from the window's start to its end are the window's: a rate over it.
    let steady = statement(30_000.0, Some("2026-10-17T10:17:02Z"), Some("2026-10-17T11:56:00Z"));
    assert!(!w.cadence(&steady).is_burst());
    assert_eq!(w.rate_per_day_of(&steady, 1.66), Some(24.0));
    // Under an hour of window there is no rate to judge: the total is all there is.
    let mut minutes = w.clone();
    minutes.window_hours = 0.03;
    assert_eq!(minutes.cadence(&burst), Cadence::Counted(30_000.0));
    assert_eq!(minutes.rate_per_day_of(&burst, 30_000.0), None);
}

/// Six days of Query Store in 2-hour buckets, Oct 11 06:00 to Oct 17 06:00.
fn six_days_in_buckets() -> Workload {
    let mut w = six_days();
    let start = crate::format::epoch_seconds("2026-10-11T06:00:00Z").unwrap();
    let starts: Vec<String> = (0..72).map(|i| crate::format::iso_from_epoch(start + i as f64 * 7200.0)).collect();
    w.timeline = Some(Timeline {
        source: TimelineSource::QueryStore,
        time_ms: vec![1000.0; starts.len()],
        executions: vec![1000.0; starts.len()],
        starts,
        end: "2026-10-17T06:00:00Z".into(),
        unknown: None,
    });
    w
}

/// A statement whose runs fell in the buckets `ran` (index, runs) of [`six_days_in_buckets`].
fn in_buckets(ran: &[(usize, f64)], first: &str, last: &str) -> QueryStat {
    let runs: f64 = ran.iter().map(|(_, r)| r).sum();
    let mut q = statement(runs, Some(first), Some(last));
    let mut executions = vec![Some(0.0); 72];
    for &(i, r) in ran {
        executions[i] = Some(r);
    }
    q.trend = Some(QueryTrend { time_ms: executions.clone(), executions });
    q
}

// MS-2 and MS-3: the planted N+1 ran 120K times in four 2-hour bursts on two of six days, from a first run on day
// one to a last on day six: read as "about 19.4K runs a day", under chatty's bar, and its neighbours as "about 388
// runs a day".
#[test]
fn runs_on_a_few_days_are_those_days_from_the_timelines_buckets() {
    let w = six_days_in_buckets();
    // Oct 11 06–08, 12–14, 14–16 and Oct 16 10–12.
    let n1 = in_buckets(
        &[(0, 30_000.0), (3, 30_000.0), (4, 30_000.0), (62, 30_000.0)],
        "2026-10-11T07:44:28Z",
        "2026-10-16T10:18:42Z",
    );
    assert_eq!(w.how_often_of(&n1), "120K runs on Oct 11 and Oct 16");
    assert_eq!(w.runs_per_day_of(&n1), None);
    // Busy at the pace it ran: 60K a day on the two days it ran, never 20K a day over six.
    assert_eq!(w.rate_per_day_of(&n1, 120_000.0), Some(60_000.0));
    // A few more days: named while they are three, counted past that.
    let three = in_buckets(&[(0, 10.0), (24, 10.0), (60, 10.0)], "2026-10-11T06:10:00Z", "2026-10-16T07:00:00Z");
    assert_eq!(w.how_often_of(&three), "30 runs on Oct 11, Oct 13 and Oct 16");
    // Every day of the window: a daily job, a rate a day.
    let nightly: Vec<(usize, f64)> = (0..6).map(|d| (d * 12, 100.0)).collect();
    let daily = in_buckets(&nightly, "2026-10-11T06:10:00Z", "2026-10-16T06:50:00Z");
    assert!(!w.cadence(&daily).is_burst(), "{:?}", w.cadence(&daily));
    // Buckets that don't hold all its runs (a timeline that began after it did) say nothing of when it ran.
    let mut partial = n1.clone();
    partial.executions = 200_000.0;
    assert!(!w.cadence(&partial).is_burst());
}

// MS-12c: "5.2 s per run" for one run of 10.5 s and one of 0.23 ms.
#[test]
fn one_run_or_two_far_apart_are_what_they_took() {
    let mut q = statement(2.0, None, None);
    q.total_time_ms = 10_490.35;
    q.max_time_ms = Some(10_490.125);
    assert_eq!(q.few_runs_took().as_deref(), Some("10.5 s and 0.23 ms"));
    q.max_time_ms = Some(5_300.0);
    assert_eq!(q.few_runs(), None, "two alike: the average says it");
    q.max_time_ms = None;
    assert_eq!(q.few_runs(), None, "no longest run kept: nothing to say apart");
    let mut once = statement(1.0, None, None);
    once.total_time_ms = 24_288.7;
    assert_eq!(once.few_runs_took().as_deref(), Some("24.3 s"));
    assert_eq!(statement(3.0, None, None).few_runs(), None);
}

/// pg_stat_statements counting since Jan 1, with 36 hours of Culprit's scans in three steps: the database ran
/// 30 s in them, and the statement `runs` and `time_ms` in each.
fn running_totals(runs: f64, time_ms: f64) -> (Workload, QueryStat) {
    let mut w: Workload = serde_json::from_value(serde_json::json!({
        "source": "pg_stat_statements", "kind": "statement-stats", "windowed": false,
        "window_start": "2026-01-01T00:00:00Z", "window_end": "2026-10-11T12:00:00Z", "window_hours": 6804.0,
        "total_time_ms": 1000000.0, "total_cpu_ms": null, "total_executions": 1000000.0, "query_count": 1.0,
        "queries": [],
        "timeline": {
            "source": "scans", "starts": ["2026-10-10T00:00:00Z", "2026-10-10T12:00:00Z", "2026-10-11T00:00:00Z"],
            "end": "2026-10-11T12:00:00Z", "time_ms": [10000.0, 10000.0, 10000.0],
            "executions": [10000.0, 10000.0, 10000.0]
        }
    }))
    .unwrap();
    w.window_hours = (crate::format::epoch_seconds("2026-10-11T12:00:00Z").unwrap()
        - crate::format::epoch_seconds("2026-01-01T00:00:00Z").unwrap())
        / 3600.0;
    // 283.5 days at 388 runs a day, a third of all the time.
    let mut q = statement(110_000.0, None, None);
    q.total_time_ms = 333_333.0;
    q.trend = Some(QueryTrend { time_ms: vec![Some(time_ms); 3], executions: vec![Some(runs); 3] });
    (w, q)
}

// The aptabase readings: "about 388 times a day" averaged over three and a half years, where Culprit's own scans
// measured 1,670 a day. Within twice the average, the scans' rate is the one said; past it, both.
#[test]
fn a_rate_on_running_totals_is_what_culprits_scans_measured() {
    let (w, q) = running_totals(800.0, 3000.0);
    let s = w.over_scans(&q).expect("36 hours of scans");
    assert_eq!((s.hours, s.runs, s.time_ms, s.workload_time_ms), (36.0, 2400.0, 9000.0, 30000.0));
    assert_eq!(s.length(), "36 hours");
    assert_eq!(
        w.how_often_of(&q),
        "about 1.6K runs a day over 36 hours of Culprit's scans (about 388 a day over the 284 days since statistics were reset)"
    );
    // About the same pace and share as since the reset: the scans' rate alone.
    let (w, q) = running_totals(200.0, 4000.0);
    assert_eq!(w.how_often_of(&q), "about 400 runs a day over 36 hours of Culprit's scans");
    // A step whose totals are unknown, or where the statement outran the database (a misread counter), is left
    // out; under a day of steps, nothing is said of them.
    let (mut w, q) = running_totals(800.0, 3000.0);
    w.timeline.as_mut().unwrap().unknown = Some(vec![false, true, false]);
    assert_eq!(w.over_scans(&q).map(|s| s.hours), Some(24.0));
    w.timeline.as_mut().unwrap().time_ms[0] = 100.0;
    assert_eq!(w.over_scans(&q), None);
    // A windowed source's own timeline is no scans of Culprit's.
    let (mut w, q) = running_totals(800.0, 3000.0);
    w.windowed = true;
    assert_eq!(w.over_scans(&q), None);
}

// The aptabase readings again: a read titled "19% of database time" and ranked below DISCARD ALL took 33% over
// Culprit's scans, and a read titled 6% took 3.1%. Over a day of scans, share, runs, average and time a day are the
// scans', in both directions; the totals since the reset are said beside them where they differ twofold.
#[test]
fn a_statement_on_running_totals_is_read_by_culprits_scans_both_ways() {
    // 9 s of the database's 30 s in the scans (30%) at 3.75 ms a run, about the totals' third at 5 ms, but four
    // times as many runs a day.
    let (w, q) = running_totals(800.0, 3000.0);
    assert!((w.statement_share(&q) - 0.3).abs() < 1e-9);
    assert_eq!(w.statement_mean_ms(&q), 3.75);
    assert_eq!(w.statement_time_per_day(&q), Some(6000.0));
    let cost = w.cost_of(&q);
    assert_eq!(cost.runs, "about 1.6K runs a day over 36 hours of Culprit's scans");
    // The share (30% against 33%) is within twice: only the runs a day differ.
    assert_eq!(cost.totals.as_deref(), Some("about 388 runs a day over the 284 days since statistics were reset"));
    // Lower now: 2.4 s of 30 s (8%), against a third of the totals.
    let (w, q) = running_totals(200.0, 800.0);
    assert!((w.statement_share(&q) - 0.08).abs() < 1e-9);
    assert_eq!(
        w.cost_of(&q).totals.as_deref(),
        Some("33% of database time over the 284 days since statistics were reset")
    );
    // A statement that didn't run in the scans is read by its totals: what it took until it stopped.
    let (w, q) = running_totals(0.0, 0.0);
    assert_eq!(w.scans_of(&q), None);
    assert!((w.statement_share(&q) - w.share_of(q.total_time_ms, &q)).abs() < 1e-9);
    assert_eq!(w.statement_mean_ms(&q), q.mean_time_ms);
    // A recent entry: most of its runs came in the scans, so its totals say nothing beside them.
    let (w, mut q) = running_totals(800.0, 3000.0);
    q.executions = 3000.0;
    assert!(w.scans_of(&q).unwrap().is_recent(&q));
    assert_eq!(w.cost_of(&q).totals, None);
    assert_eq!(w.how_often_of(&q), "about 1.6K runs a day over 36 hours of Culprit's scans");
}

#[test]
fn evictions_keep_recent_observed_calls_separate_from_complete_shares() {
    let (mut w, mut q) = running_totals(800.0, 3000.0);
    w.timeline.as_mut().unwrap().unknown = Some(vec![false, false, true]);
    let cost = w.cost_of(&q);
    assert!(cost.over.as_ref().unwrap().contains("between"));
    assert!(cost.over.as_ref().unwrap().contains("Oct"));
    assert_eq!(cost.activity.as_ref().unwrap().runs, 800.0);
    assert!(cost.activity.as_ref().unwrap().incomplete);
    assert_eq!(cost.runs_per_day, None);
    assert!(cost.runs.starts_with("at least 800 runs observed"));
    assert!((cost.share - 0.3).abs() < 1e-9);
    // A newly tracked entry has no complete interval, but its latest calls must not be diluted over years.
    q.trend.as_mut().unwrap().executions = vec![None, None, Some(825.0)];
    q.trend.as_mut().unwrap().time_ms = vec![None, None, Some(700.0)];
    let cost = w.cost_of(&q);
    assert_eq!(cost.counted, 825.0);
    assert_eq!(w.runs_per_day_of(&q), None);
    assert_eq!(cost.runs_per_day, None);
    assert!(w.how_often_of(&q).contains("825 runs observed"));
    // A missing or invalid query difference is not an observed zero.
    q.trend.as_mut().unwrap().time_ms[2] = None;
    assert!(w.observed_activity(&q).is_none());
    q.trend.as_mut().unwrap().time_ms[2] = Some(-1.0);
    assert!(w.observed_activity(&q).is_none());
}

#[test]
fn server_versions_compare_by_number() {
    assert!(!version_at_least("5.7.44", [8, 0, 18]));
    assert!(!version_at_least("8.0.17-log", [8, 0, 18]));
    assert!(version_at_least("8.0.18", [8, 0, 18]));
    assert!(version_at_least("8.4.3", [8, 0, 18]));
    assert!(version_at_least("10.5.29-MariaDB-ubu2004", [10, 5, 0]));
    assert!(!version_at_least("10.5.29-MariaDB-ubu2004", [10, 6, 0]));
    assert!(version_at_least("8.0", [8, 0, 0]));
    // Unknown builds are never refused for their age.
    assert!(version_at_least("", [8, 0, 18]));
    assert!(version_at_least("unknown", [8, 0, 18]));
}
