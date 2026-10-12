//! The digest: one report, compared with the previous one when there is one, in the few facts a scheduled job posts.

use culprit_core::{
    Digest, DigestChanges, DigestCounts, DigestDay, DigestFinding, Environment, Finding, Report, Severity, SubjectKind,
    Workload, format,
};

/// What a report says, and what changed since `previous` (an earlier report of the same database). `top` caps every
/// list of findings.
pub fn digest(report: &Report, previous: Option<&Report>, top: usize) -> Digest {
    let baseline = previous.map(|p| {
        let findings = p.findings.iter().filter(|f| !f.inactive).map(|f| item(f, None)).collect::<Vec<_>>();
        (p.snapshot.taken_at.clone(), findings)
    });
    build(report, baseline, top)
}

/// The same, compared with a previous digest instead of a report: what a scheduled job keeps between runs, since a
/// digest holds no statement text or parameter values and a report does.
pub fn digest_since(report: &Report, previous: &Digest, top: usize) -> Digest {
    build(report, Some((previous.taken_at.clone(), previous.findings.clone())), top)
}

fn build(report: &Report, baseline: Option<(String, Vec<DigestFinding>)>, top: usize) -> Digest {
    let board = crate::dashboard(report);
    let day = report.snapshot.workload.as_ref().and_then(day_over_day);
    let active: Vec<&Finding> = report.findings.iter().filter(|f| !f.inactive).collect();
    let count = |s: Severity| active.iter().filter(|f| f.severity == s).count() as u32;
    let since = baseline.map(|(taken_at, before)| {
        let was = |id: &str| before.iter().find(|f| f.id == id);
        DigestChanges {
            taken_at,
            new: active.iter().filter(|f| was(&f.id).is_none()).take(top).map(|f| item(f, None)).collect(),
            fixed: before.iter().filter(|f| !active.iter().any(|a| a.id == f.id)).take(top).cloned().collect(),
            worse: active
                .iter()
                .filter_map(|f| was(&f.id).filter(|b| f.severity > b.severity).map(|b| item(f, Some(b.severity))))
                .take(top)
                .collect(),
            again: active
                .iter()
                .filter_map(|f| {
                    let since = counted_since(event_count(f)?, was(&f.id)?.count?);
                    (since > 0.0).then(|| DigestFinding { count_since: Some(since), ..item(f, None) })
                })
                .take(top)
                .collect(),
        }
    });
    Digest {
        connection: report.snapshot.connection.clone(),
        server: board.product.clone(),
        database: board.database.clone(),
        environment: Some(environment_name(board.environment).to_string()),
        taken_at: report.snapshot.taken_at.clone(),
        window: board.coverage.clone(),
        headline: report.summary.headline.clone(),
        database_time: day.as_ref().map(day_sentence),
        day,
        counts: DigestCounts {
            active: active.len() as u32,
            high: count(Severity::High),
            medium: count(Severity::Medium),
            low: count(Severity::Low),
            stopped: report.findings.iter().filter(|f| f.inactive).count() as u32,
        },
        since,
        top: grouped(&active, top),
        findings: active.iter().map(|f| item(f, None)).collect(),
    }
}

/// The `top` most important findings, one per query, table or index: a query that is both chatty and waiting on locks
/// is one thing to look at, with the other finding's title in `also`.
fn grouped(active: &[&Finding], top: usize) -> Vec<DigestFinding> {
    let key = |f: &Finding| {
        matches!(f.subject.kind, SubjectKind::Query | SubjectKind::Table | SubjectKind::Index)
            .then(|| (f.subject.kind, f.subject.name.clone()))
    };
    let mut out: Vec<(Option<(SubjectKind, String)>, DigestFinding)> = Vec::new();
    for f in active {
        let k = key(f);
        if let Some(group) = out.iter_mut().find(|(other, _)| k.is_some() && *other == k) {
            group.1.also.push(f.title.clone());
        } else if out.len() < top {
            out.push((k, item(f, None)));
        }
    }
    out.into_iter().map(|(_, item)| item).collect()
}

const DAY: f64 = 24.0 * 3600.0;
/// The history starts on a bucket boundary, so a "2-day" window can begin a little after two days ago.
const SLACK: f64 = 1800.0;

/// Database time in the last 24 hours and the 24 hours before, from the timeline. Buckets that straddle a cut count in
/// proportion. `None` without a timeline or under a day of it; no day before when the history is shorter than two.
fn day_over_day(w: &Workload) -> Option<DigestDay> {
    let t = w.timeline.as_ref()?;
    let end = format::epoch_seconds(&t.end)?;
    let mut starts: Vec<f64> = t.starts.iter().map(|s| format::epoch_seconds(s)).collect::<Option<_>>()?;
    let first = *starts.first()?;
    // The window can begin inside the first bucket; time before it was never counted.
    if let Some(from) = w.window_start.as_deref().and_then(format::epoch_seconds)
        && from > first
        && starts.get(1).is_none_or(|next| from < *next)
    {
        starts[0] = from;
    }
    let covered = end - starts[0];
    if covered < DAY - SLACK {
        return None;
    }
    let within = |from: f64, to: f64| -> f64 {
        starts
            .iter()
            .enumerate()
            .map(|(i, &start)| {
                let stop = starts.get(i + 1).copied().unwrap_or(end);
                let width = stop - start;
                let overlap = (stop.min(to) - start.max(from)).max(0.0);
                if width > 0.0 { t.time_ms.get(i).copied().unwrap_or(0.0) * overlap / width } else { 0.0 }
            })
            .sum()
    };
    Some(DigestDay {
        last_ms: within(end - DAY, end),
        before_ms: (covered >= 2.0 * DAY - SLACK).then(|| within(end - 2.0 * DAY, end - DAY)),
    })
}

/// "Database time: 2.1 h in the last 24 hours, up 35% from the 24 hours before (1.6 h)."
fn day_sentence(d: &DigestDay) -> String {
    let last = format::duration(d.last_ms);
    let Some(before) = d.before_ms else {
        return format!("Database time: {last} in the last 24 hours.");
    };
    let then = format::duration(before);
    // Under a second either side, a ratio is noise.
    if before <= 0.0 || d.last_ms + before < 1_000.0 {
        return format!("Database time: {last} in the last 24 hours, {then} in the 24 hours before.");
    }
    let ratio = d.last_ms / before;
    let change = if ratio >= 2.0 {
        format!("up {ratio:.1}× from")
    } else if ratio >= 1.1 {
        format!("up {} from", format::percent(ratio - 1.0))
    } else if ratio <= 0.9 {
        format!("down {} from", format::percent(1.0 - ratio))
    } else {
        "about the same as".to_string()
    };
    format!("Database time: {last} in the last 24 hours, {change} the 24 hours before ({then}).")
}

/// As connections spell it (`--env staging`).
fn environment_name(environment: Environment) -> &'static str {
    match environment {
        Environment::Production => "production",
        Environment::Staging => "staging",
        Environment::Development => "development",
        Environment::Local => "local",
    }
}

/// What a finding counts since startup, for the findings that count events: a later digest tells from it whether
/// they happened again.
fn event_count(f: &Finding) -> Option<f64> {
    let label = match f.check.as_str() {
        "lock-escalations" => crate::signals::ESCALATIONS_METRIC,
        _ => return None,
    };
    f.metrics.iter().find(|m| m.label == label).map(|m| m.value)
}

/// How many happened between two scans of a count since startup. A count lower than before was reset after the
/// earlier scan (a restart, an index reloaded into memory), so all of it is new.
fn counted_since(now: f64, before: f64) -> f64 {
    if now >= before { now - before } else { now }
}

fn item(f: &Finding, was: Option<Severity>) -> DigestFinding {
    DigestFinding {
        id: f.id.clone(),
        title: f.title.clone(),
        severity: f.severity,
        db_time_share: f.impact.db_time_share,
        impact: f.impact.label.clone(),
        was,
        also: Vec::new(),
        detail: None,
        count: event_count(f),
        count_since: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report() -> Report {
        let text = std::fs::read_to_string(format!("{}/../../fixtures/mssql-demo.json", env!("CARGO_MANIFEST_DIR")))
            .expect("fixture");
        crate::analyze(serde_json::from_str(&text).expect("snapshot"))
    }

    #[test]
    fn a_first_digest_counts_and_ranks_without_changes() {
        let r = report();
        let d = digest(&r, None, 3);
        let active = r.findings.iter().filter(|f| !f.inactive).count() as u32;
        assert_eq!(d.counts.active, active);
        assert_eq!(d.counts.high + d.counts.medium + d.counts.low, active);
        assert_eq!(d.top.len(), 3.min(active as usize));
        assert_eq!(d.top[0].id, r.findings.iter().find(|f| !f.inactive).unwrap().id);
        assert!(d.since.is_none());
        assert_eq!(d.headline, r.summary.headline);
        // The demo's history covers under a day: no day-over-day comparison.
        assert!(d.database_time.is_none() && d.day.is_none());
    }

    /// `hours` hourly buckets ending at 2026-10-12T12:00Z, the bucket `i` hours before the end holding `ms(i)`.
    fn hourly(hours: usize, ms: impl Fn(usize) -> f64) -> Workload {
        let at = |h: usize| format!("2026-10-{:02}T{:02}:00:00Z", 10 + h / 24, h % 24);
        let first = 60 - hours;
        let mut w = report().snapshot.workload.expect("workload");
        w.window_start = Some(at(first));
        w.timeline = Some(culprit_core::Timeline {
            source: Default::default(),
            starts: (first..60).map(at).collect(),
            end: at(60),
            time_ms: (first..60).map(|h| ms(60 - h)).collect(),
            executions: vec![0.0; hours],
        });
        w
    }

    #[test]
    fn database_time_compares_the_last_24_hours_with_the_24_before() {
        // 49 hours of history: 200 ms an hour in the last day, 100 in the day before, an hour older than that.
        let w = hourly(49, |ago| if ago <= 24 { 200.0 } else { 100.0 });
        let day = day_over_day(&w).expect("a day");
        assert_eq!(day, DigestDay { last_ms: 4_800.0, before_ms: Some(2_400.0) });
        assert_eq!(
            day_sentence(&day),
            "Database time: 4.8 s in the last 24 hours, up 2.0× from the 24 hours before (2.4 s)."
        );
        // A bucket that straddles a cut counts in proportion: the window starts half way into the first bucket.
        let mut w = hourly(48, |_| 100.0);
        if let Some(t) = w.timeline.as_mut() {
            t.starts[0] = "2026-10-10T11:30:00Z".into();
        }
        let day = day_over_day(&w).expect("a day");
        assert_eq!(day.last_ms, 2_400.0);
        assert!((day.before_ms.expect("day before") - 2_400.0).abs() < 1e-6);
    }

    #[test]
    fn database_time_without_two_days_of_history() {
        let day = day_over_day(&hourly(30, |_| 100.0)).expect("a day");
        assert_eq!(day.before_ms, None);
        assert_eq!(day_sentence(&day), "Database time: 2.4 s in the last 24 hours.");
        assert_eq!(day_over_day(&hourly(10, |_| 100.0)), None);
    }

    #[test]
    fn database_time_says_how_much_it_moved() {
        let say = |last_ms, before_ms| day_sentence(&DigestDay { last_ms, before_ms: Some(before_ms) });
        assert!(
            say(13_500.0, 10_000.0).contains("up 35% from the 24 hours before (10.0 s)"),
            "{}",
            say(13_500.0, 10_000.0)
        );
        assert!(say(6_000.0, 10_000.0).contains("down 40% from"), "{}", say(6_000.0, 10_000.0));
        assert!(say(10_400.0, 10_000.0).contains("about the same as the 24 hours before"));
    }

    #[test]
    fn findings_about_the_same_query_are_one_item() {
        let mut r = report();
        let active: Vec<usize> = (0..r.findings.len()).filter(|&i| !r.findings[i].inactive).collect();
        let (a, b) = (active[0], active[2]);
        for i in [a, b] {
            r.findings[i].subject = culprit_core::Subject { kind: SubjectKind::Query, name: "42".into() };
        }
        let d = digest(&r, None, 3);
        assert_eq!(d.top.len(), 3);
        assert_eq!(d.top[0].id, r.findings[a].id);
        assert_eq!(d.top[0].also, [r.findings[b].title.clone()]);
        assert!(d.top.iter().all(|f| f.id != r.findings[b].id));
        // Every finding is still kept for the next comparison.
        assert!(d.findings.iter().any(|f| f.id == r.findings[b].id));
    }

    #[test]
    fn a_previous_digest_is_as_good_a_baseline_as_its_report() {
        let now = report();
        let mut before = now.clone();
        before.snapshot.taken_at = "2026-10-10T06:00:00Z".into();
        let gone = before.findings.remove(0);
        let from_report = digest(&now, Some(&before), 10).since.expect("changes");
        let previous = digest(&before, None, 3);
        assert_eq!(previous.findings.len(), before.findings.iter().filter(|f| !f.inactive).count());
        let from_digest = digest_since(&now, &previous, 10).since.expect("changes");
        assert_eq!(from_digest, from_report);
        assert_eq!(from_digest.new[0].id, gone.id);
    }

    fn with_escalations(mut r: Report, count: f64) -> Report {
        let mut f = crate::support::finding(
            "lock-escalations",
            "dbo.orders",
            culprit_core::Category::Indexes,
            crate::support::subject(SubjectKind::Table, "dbo.orders"),
        );
        f.title = "Row locks on orders escalated to a lock on the whole table".into();
        f.metrics =
            vec![culprit_core::Metric::new(crate::signals::ESCALATIONS_METRIC, count, culprit_core::Unit::Count)];
        r.findings.push(f);
        r
    }

    #[test]
    fn a_digest_says_when_a_count_since_startup_grew() {
        let again = |now: f64, before: f64| {
            let mut previous = with_escalations(report(), before);
            previous.snapshot.taken_at = "2026-10-10T06:00:00Z".into();
            let d = digest(&with_escalations(report(), now), Some(&previous), 10);
            assert_eq!(d.findings.iter().find(|f| f.id == "lock-escalations:dbo.orders").unwrap().count, Some(now));
            // A previous digest carries the count too.
            let from_digest = digest_since(&with_escalations(report(), now), &digest(&previous, None, 10), 10);
            assert_eq!(from_digest.since, d.since);
            d.since.expect("changes").again.iter().map(|f| f.count_since).collect::<Vec<_>>()
        };
        assert_eq!(again(7.0, 4.0), [Some(3.0)]);
        assert!(again(7.0, 7.0).is_empty());
        // The counters were reset after the previous scan: every escalation counted now is new.
        assert_eq!(again(2.0, 10.0), [Some(2.0)]);
        // A finding without a count since startup never shows here.
        assert!(digest(&report(), Some(&report()), 10).since.expect("changes").again.is_empty());
    }

    #[test]
    fn a_digest_reports_what_is_new_fixed_and_worse_since_the_previous_one() {
        let now = report();
        let mut before = now.clone();
        before.snapshot.taken_at = "2026-10-10T06:00:00Z".into();
        let gone = before.findings.remove(0);
        let mut old = now.findings[1].clone();
        old.id = "unused-index:shop.old_table".into();
        before.findings.push(old.clone());
        // A finding that is medium or high now was low before.
        let rising = before.findings.iter().position(|f| !f.inactive && f.severity > Severity::Low).expect("one");
        before.findings[rising].severity = Severity::Low;
        let rising = before.findings[rising].id.clone();

        let changes = digest(&now, Some(&before), 10).since.expect("changes");
        assert_eq!(changes.taken_at, "2026-10-10T06:00:00Z");
        assert_eq!(changes.new.iter().map(|f| f.id.as_str()).collect::<Vec<_>>(), [gone.id.as_str()]);
        assert_eq!(changes.fixed.iter().map(|f| f.id.as_str()).collect::<Vec<_>>(), [old.id.as_str()]);
        assert_eq!(changes.worse.len(), 1);
        assert_eq!(changes.worse[0].id, rising);
        assert_eq!(changes.worse[0].was, Some(Severity::Low));
    }
}
