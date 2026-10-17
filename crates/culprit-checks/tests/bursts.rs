//! Runs that came in bursts are said by when they ran, graded on their total and judged "busy" at the rate they ran
//! while they ran, never extrapolated to a day: the planted N+1 of the SQL Server demo (120K lookups in four 2-hour
//! stretches on two of six days), a minute-long burst in two hours of counters, and reads that waited on locks one
//! afternoon and stopped.

use culprit_checks::analyze;
use culprit_core::format::{epoch_seconds, iso_from_epoch};
use culprit_core::{QueryTrend, Severity, Snapshot, Timeline, TimelineSource, WaitStat};

fn fixture(name: &str) -> Snapshot {
    let text =
        std::fs::read_to_string(format!("{}/../../fixtures/{name}", env!("CARGO_MANIFEST_DIR"))).expect("fixture");
    serde_json::from_str(&text).expect("snapshot")
}

const END: &str = "2026-10-17T11:57:15Z";

/// The SQL Server demo read as six days of Query Store in 2-hour buckets, every statement but `burst` running steadily
/// over them, and `burst` running only in the buckets `ran` (index, runs).
fn six_days(burst: &str, ran: &[(usize, f64)]) -> Snapshot {
    let mut s = fixture("mssql-demo.json");
    s.taken_at = END.into();
    let end = epoch_seconds(END).expect("time");
    let buckets = 74;
    let start = end - buckets as f64 * 7200.0;
    let w = s.workload.as_mut().expect("workload");
    w.window_end = Some(END.into());
    w.window_hours = buckets as f64 * 2.0;
    w.window_start = Some(iso_from_epoch(start));
    let starts: Vec<String> = (0..buckets).map(|i| iso_from_epoch(start + i as f64 * 7200.0)).collect();
    let total: f64 = w.queries.iter().map(|q| q.total_time_ms).sum();
    let runs: f64 = w.queries.iter().map(|q| q.executions).sum();
    w.timeline = Some(Timeline {
        source: TimelineSource::QueryStore,
        time_ms: vec![total / buckets as f64; buckets],
        executions: vec![runs / buckets as f64; buckets],
        starts,
        end: END.into(),
        unknown: None,
    });
    for q in &mut w.queries {
        q.trend = None;
        q.first_seen = Some(iso_from_epoch(start + 60.0));
        q.last_seen = Some(iso_from_epoch(end - 60.0));
        if q.id != burst {
            continue;
        }
        let runs: f64 = ran.iter().map(|(_, r)| r).sum();
        let mean = q.mean_time_ms;
        let mut executions = vec![Some(0.0); buckets];
        for &(i, r) in ran {
            executions[i] = Some(r);
        }
        q.trend = Some(QueryTrend { time_ms: executions.iter().map(|e| e.map(|e| e * mean)).collect(), executions });
        q.executions = runs;
        q.total_time_ms = runs * mean;
        let (first, last) = (ran.first().expect("runs").0, ran.last().expect("runs").0);
        q.first_seen = Some(iso_from_epoch(start + first as f64 * 7200.0 + 600.0));
        q.last_seen = Some(iso_from_epoch(start + last as f64 * 7200.0 + 1200.0));
    }
    w.total_executions = w.queries.iter().map(|q| q.executions).sum();
    w.total_time_ms = w.queries.iter().map(|q| q.total_time_ms).sum();
    s
}

// MS-2: 120K runs averaged over six days are 19.4K a day, under chatty's 20K bar: the N+1 went unreported.
#[test]
fn the_n_plus_one_in_four_two_hour_bursts_is_busy_at_the_pace_it_ran() {
    // Buckets 0, 3 and 4 (Oct 11) and 72 (Oct 17).
    let s = six_days("6", &[(0, 30_000.0), (3, 30_000.0), (4, 30_000.0), (72, 30_000.0)]);
    let report = analyze(s);
    let f = report.findings.iter().find(|f| f.id == "chatty-query:6").expect("the N+1 is found");
    assert!(f.title.contains("ran 120K times on Oct 11 and Oct 17"), "{}", f.title);
    // 60K round trips a day on the two days it ran: the minute a day Medium asks for.
    assert_eq!(f.severity, Severity::Medium, "{}", f.title);
    assert!(!f.metrics.iter().any(|m| m.label == "Runs per day"), "{:?}", f.metrics);
    let brief = culprit_checks::agent_brief(&report, f);
    let line = brief.lines().find(|l| l.starts_with("Query `6`")).expect("its line");
    assert!(line.contains(" of DB time, 120K runs on Oct 11 and Oct 17, "), "{line}");
    // The same count spread over every bucket is 19.4K a day: not busy.
    let steady: Vec<(usize, f64)> = (0..74).map(|i| (i, 120_000.0 / 74.0)).collect();
    let report = analyze(six_days("6", &steady));
    assert!(!report.findings.iter().any(|f| f.id == "chatty-query:6"), "steady 19.4K a day is under the bar");
}

/// The MySQL demo read as two hours of counters (performance_schema reset 1.66 hours before the scan) whose
/// statements all ran in its first minute.
fn a_minute_in_two_hours() -> Snapshot {
    let mut s = fixture("mysql-demo.json");
    let w = s.workload.as_mut().expect("workload");
    let end = epoch_seconds(w.window_end.as_deref().expect("end")).expect("time");
    w.window_hours = 1.66;
    w.window_start = Some(iso_from_epoch(end - 1.66 * 3600.0));
    w.timeline = None;
    for q in &mut w.queries {
        q.trend = None;
        q.first_seen = Some(iso_from_epoch(end - 1.66 * 3600.0 + 1.0));
        q.last_seen = Some(iso_from_epoch(end - 1.66 * 3600.0 + 40.0));
    }
    s
}

// MY-1: a spill of 35.8 s in a 40-second burst graded on "8.6 min a day"; 30K lookups in six seconds Medium on "433K
// round trips a day"; 4,000 in six seconds "busy" at 57.8K a day.
#[test]
fn a_burst_in_two_hours_of_counters_is_graded_on_its_total() {
    let report = analyze(a_minute_in_two_hours());
    let by_check = |check: &'static str| report.findings.iter().filter(move |f| f.check == check);
    let spills: Vec<_> = by_check("temp-spill").collect();
    assert!(!spills.is_empty(), "the demo's spill");
    for f in &spills {
        assert_eq!(f.severity, Severity::Low, "{}: {}", f.id, f.title);
    }
    let chatty: Vec<_> = by_check("chatty-query").collect();
    assert_eq!(
        chatty.len(),
        1,
        "30K lookups are busy, 4,000 aren't: {:?}",
        chatty.iter().map(|f| &f.title).collect::<Vec<_>>()
    );
    assert_eq!(chatty[0].severity, Severity::Low, "{}", chatty[0].title);
    assert!(chatty[0].title.contains("ran 30K times on "), "{}", chatty[0].title);
    for f in &report.findings {
        assert!(
            !f.title.contains(" a day") && !f.summary.contains("times a day"),
            "{}: {} {}",
            f.id,
            f.title,
            f.summary
        );
    }
}

// MS-3: "reads waited 1 min … but only about 11.6 s a day" for three runs one afternoon, by a read that stopped.
#[test]
fn reads_that_waited_one_afternoon_and_stopped_are_said_by_when() {
    // Query 7, a read of customers, ran three times on Oct 11 13:56–14:19 (buckets 2 and 3) and waited 72 s on locks.
    let mut s = six_days("7", &[(2, 1.0), (3, 2.0)]);
    let w = s.workload.as_mut().expect("workload");
    let q = w.queries.iter_mut().find(|q| q.id == "7").expect("query 7");
    q.first_seen = Some("2026-10-11T13:56:51Z".into());
    q.last_seen = Some("2026-10-11T14:19:10Z".into());
    q.total_time_ms = 71_698.8;
    q.mean_time_ms = q.total_time_ms / 3.0;
    q.waits = vec![WaitStat { category: "Lock".into(), wait_type: None, wait_ms: 71_600.0, tasks: None }];
    w.total_time_ms = w.queries.iter().map(|q| q.total_time_ms).sum();
    let report = analyze(s);
    let f = report.findings.iter().find(|f| f.id == "config:read-committed-snapshot").expect("RCSI");
    assert_eq!(f.severity, Severity::Low, "{}", f.summary);
    assert!(
        f.summary.contains("all of it on Oct 11, 13:56–14:19 UTC, and that read hasn't run since"),
        "{}",
        f.summary
    );
    assert!(!f.summary.contains(" a day"), "{}", f.summary);
}
