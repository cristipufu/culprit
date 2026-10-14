//! The digest: one report, compared with the previous one when there is one, in the few facts a scheduled job posts.

use culprit_core::{
    CandidateSource, CleanupKind, DeadlockPattern, Digest, DigestChanges, DigestCounts, DigestDay,
    DigestDeadlockPattern, DigestDeadlocks, DigestFinding, DigestIndexAdvice, DigestIndexCreate, DigestIndexDrop,
    DigestTopQuery, Environment, Finding, Report, Severity, SubjectKind, Workload, format,
};

/// What a report says, and what changed since `previous` (an earlier report of the same database). `top` caps every
/// list of findings, index advice and deadlock patterns; `top_queries` is how many of the heaviest queries to list.
pub fn digest(report: &Report, previous: Option<&Report>, top: usize, top_queries: usize) -> Digest {
    let baseline = previous.map(|p| {
        let findings = p.findings.iter().filter(|f| !f.inactive).map(|f| item(f, None)).collect::<Vec<_>>();
        (p.snapshot.taken_at.clone(), findings)
    });
    build(report, baseline, top, top_queries)
}

/// The same, compared with a previous digest instead of a report: what a scheduled job keeps between runs, since a
/// digest holds no statement text or parameter values and a report does.
pub fn digest_since(report: &Report, previous: &Digest, top: usize, top_queries: usize) -> Digest {
    build(report, Some((previous.taken_at.clone(), previous.findings.clone())), top, top_queries)
}

fn build(report: &Report, baseline: Option<(String, Vec<DigestFinding>)>, top: usize, top_queries: usize) -> Digest {
    let previous_at = baseline.as_ref().map(|(at, _)| at.clone());
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
            // A count since startup turns a finding High when it grew and Low when it didn't: `again` says that.
            worse: active
                .iter()
                .filter(|f| event_count(f).is_none())
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
        server_started_at: report.snapshot.server.started_at.clone(),
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
        top_queries: heaviest_queries(report, &active, top_queries),
        index_advice: Some(index_advice(report, &active, top)),
        deadlocks: Some(deadlocks(report, previous_at.as_deref(), top)),
    }
}

/// A statement this long is cut: enough to recognise it in a chat message, where the label says what it does.
const STATEMENT_CHARS: usize = 160;

/// `text` on one line, cut at `STATEMENT_CHARS` with an ellipsis.
fn one_line(text: &str) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= STATEMENT_CHARS {
        return flat;
    }
    let cut: String = flat.chars().take(STATEMENT_CHARS - 1).collect();
    format!("{}…", cut.trim_end())
}

/// The `n` application queries that used the most database time over the window, as the dashboard ranks them, each
/// with the findings about it.
fn heaviest_queries(report: &Report, active: &[&Finding], n: usize) -> Vec<DigestTopQuery> {
    let s = &report.snapshot;
    let Some(w) = &s.workload else { return Vec::new() };
    let mut queries: Vec<_> = crate::support::app_queries(s).filter(|q| q.total_time_ms > 0.0).collect();
    queries.sort_by(|a, b| b.total_time_ms.total_cmp(&a.total_time_ms));
    queries
        .into_iter()
        .take(n)
        .map(|q| DigestTopQuery {
            id: q.id.clone(),
            label: crate::support::query_handle(q),
            statement: one_line(&q.text),
            tables: culprit_core::sql::tables_in(&q.text, s.server.engine),
            object: q.object_name.clone(),
            db_time_share: (w.total_time_ms > 0.0).then(|| (q.total_time_ms / w.total_time_ms).min(1.0)),
            total_ms: q.total_time_ms,
            runs: q.executions,
            mean_ms: q.mean_time_ms,
            stopped: crate::query_stopped(s, q).is_some(),
            findings: active
                .iter()
                .filter(|f| {
                    (f.subject.kind == SubjectKind::Query && f.subject.name == q.id)
                        || f.queries.iter().any(|r| r.id == q.id)
                })
                .map(|f| f.id.clone())
                .collect(),
        })
        .collect()
}

/// The index advisor's plan, capped at `top` per list, with Azure SQL's own recommendations after it: only those its
/// tuning check kept as findings, which it already set aside when the optimizer asks for the same index, an index
/// already serves it, or another finding already says to drop it.
fn index_advice(report: &Report, active: &[&Finding], top: usize) -> DigestIndexAdvice {
    let advice = crate::index_advice(report);
    let mut create: Vec<DigestIndexCreate> = advice
        .candidates
        .iter()
        .map(|c| DigestIndexCreate {
            table: format!("{}.{}", c.schema, c.table),
            keys: c.keys.clone(),
            include: c.include.clone(),
            source: match c.source {
                CandidateSource::Optimizer => "optimizer",
                CandidateSource::PlanScan => "plan-scan",
                CandidateSource::ForeignKey => "foreign-key",
            }
            .to_string(),
            reason: c.reason.clone(),
            db_time_share: c.db_time_share,
            sql: c.sql.clone(),
            finding: c.finding_id.clone(),
        })
        .collect();
    let mut drop: Vec<DigestIndexDrop> = advice
        .cleanups
        .iter()
        .map(|c| DigestIndexDrop {
            kind: match c.kind {
                CleanupKind::Unused => "unused",
                CleanupKind::Duplicate => "duplicate",
                CleanupKind::Prefix => "prefix",
                CleanupKind::Overlap => "overlap",
            }
            .to_string(),
            table: format!("{}.{}", c.schema, c.table),
            index: c.index.clone(),
            keys: c.keys.clone(),
            other: c.other.clone(),
            size_bytes: c.size_bytes,
            reason: c.reason.clone(),
            sql: c.sql.clone(),
            finding: c.finding_id.clone(),
        })
        .collect();
    let kept = |id: &str| active.iter().find(|f| f.id == id);
    let recommendations = report.snapshot.tuning.iter().flat_map(|t| &t.recommendations);
    for r in recommendations {
        let Some(ix) = &r.index else { continue };
        let table = format!("{}.{}", ix.schema, ix.table);
        let score = r.score.map(|s| format!(", scoring it {s:.0} out of 100")).unwrap_or_default();
        if r.kind.eq_ignore_ascii_case("CreateIndex") {
            let Some(f) = kept(&format!("tuning-advice:create-index:{table}({})", ix.columns.join(","))) else {
                continue;
            };
            create.push(DigestIndexCreate {
                table,
                keys: ix.columns.clone(),
                include: ix.include.clone(),
                source: "azure".into(),
                reason: format!("Azure SQL Database's automatic tuning recommends it{score}."),
                db_time_share: None,
                sql: r.script.clone(),
                finding: Some(f.id.clone()),
            });
        } else if r.kind.eq_ignore_ascii_case("DropIndex") {
            let Some(name) = &ix.name else { continue };
            let Some(f) = kept(&format!("tuning-advice:drop-index:{table}.{name}")) else { continue };
            let existing = report.snapshot.indexes.iter().find(|i| {
                i.schema.eq_ignore_ascii_case(&ix.schema)
                    && i.table.eq_ignore_ascii_case(&ix.table)
                    && i.name.eq_ignore_ascii_case(name)
            });
            drop.push(DigestIndexDrop {
                kind: "azure".into(),
                table,
                index: name.clone(),
                keys: existing.map(|i| i.key_names().into_iter().map(String::from).collect()).unwrap_or_default(),
                other: None,
                size_bytes: existing.and_then(|i| i.size_bytes),
                reason: format!("Azure SQL Database's automatic tuning recommends dropping it{score}."),
                sql: f.fix.as_ref().and_then(|x| x.sql.clone()),
                finding: Some(f.id.clone()),
            });
        }
    }
    let (create_total, drop_total) = (create.len() as u32, drop.len() as u32);
    create.truncate(top);
    drop.truncate(top);
    DigestIndexAdvice { create, create_total, drop, drop_total, counters: advice.counters, notes: advice.notes }
}

fn pattern_name(p: DeadlockPattern) -> &'static str {
    match p {
        DeadlockPattern::KeyLookup => "key-lookup",
        DeadlockPattern::LockConversion => "lock-conversion",
        DeadlockPattern::OppositeOrder => "opposite-order",
        DeadlockPattern::LockEscalation => "lock-escalation",
        DeadlockPattern::Parallelism => "parallelism",
        DeadlockPattern::Cycle => "cycle",
    }
}

/// The scan's deadlocks, one entry per pattern on the same tables, and how many came after `previous_at` (the
/// previous digest's scan). Without graphs to read (Postgres keeps a counter only, a permission is missing) the count
/// is `None` and the note says why, rather than reporting none.
fn deadlocks(report: &Report, previous_at: Option<&str>, top: usize) -> DigestDeadlocks {
    let s = &report.snapshot;
    let Some(d) = &s.deadlocks else {
        let why = s.notes.iter().find(|n| n.collector == "deadlocks").map(|n| n.message.clone());
        return DigestDeadlocks {
            note: Some(why.unwrap_or_else(|| {
                "This scan has no deadlocks to read: the engine doesn't expose them to Culprit.".into()
            })),
            ..DigestDeadlocks::default()
        };
    };
    let unknown = d.deadlocks.is_empty() && (d.counted.is_some() || d.note.is_some());
    let at = |t: &Option<String>| t.as_deref().and_then(format::epoch_seconds);
    let after = previous_at.and_then(format::epoch_seconds);
    let mut groups: Vec<DigestDeadlockPattern> = Vec::new();
    // Most recent first, so each group's first deadlock is its latest.
    for dl in &d.deadlocks {
        let mut tables = dl.objects.clone();
        tables.sort();
        let name = pattern_name(dl.pattern);
        if let Some(g) = groups.iter_mut().find(|g| g.pattern == name && g.tables == tables) {
            g.count += 1;
            if at(&dl.at) > at(&g.latest_at) {
                g.latest_at = dl.at.clone();
            }
            continue;
        }
        let mut statements: Vec<String> = Vec::new();
        for p in &dl.processes {
            if let Some(text) = p.statement.as_deref().map(one_line).filter(|t| !t.is_empty())
                && !statements.contains(&text)
                && statements.len() < 2
            {
                statements.push(text);
            }
        }
        let mut lock_modes: Vec<String> = Vec::new();
        for mode in dl.processes.iter().filter_map(|p| p.wait_mode.clone()) {
            if !lock_modes.contains(&mode) {
                lock_modes.push(mode);
            }
        }
        groups.push(DigestDeadlockPattern {
            pattern: name.into(),
            label: dl.pattern.label().into(),
            count: 1,
            latest_at: dl.at.clone(),
            tables,
            statements,
            lock_modes,
            headline: dl.headline.clone(),
            fix: dl.fixes.first().map(|f| f.title.clone()),
        });
    }
    groups.sort_by(|a, b| {
        b.count
            .cmp(&a.count)
            .then_with(|| at(&b.latest_at).partial_cmp(&at(&a.latest_at)).unwrap_or(std::cmp::Ordering::Equal))
    });
    groups.truncate(top);
    DigestDeadlocks {
        count: (!unknown).then_some(d.deadlocks.len() as u32),
        counted: d.counted,
        counted_since: d.counted_since.clone(),
        since_hours: Some(d.since_hours),
        source: Some(d.source.clone()).filter(|s| !s.is_empty()),
        latest_at: d.deadlocks.iter().filter_map(|x| x.at.clone()).max_by(|a, b| {
            at(&Some(a.clone())).partial_cmp(&at(&Some(b.clone()))).unwrap_or(std::cmp::Ordering::Equal)
        }),
        // Only deadlocks with a time can be placed before or after the previous scan.
        new_since: after
            .map(|after| d.deadlocks.iter().filter(|x| at(&x.at).is_some_and(|t| t > after)).count() as u32),
        patterns: groups,
        note: d.note.clone(),
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
    // A day that holds a bucket with only a lower bound (a running total that fell without a reset) has no total to
    // state or compare.
    let within = |from: f64, to: f64| -> Option<f64> {
        let mut total = 0.0;
        for (i, &start) in starts.iter().enumerate() {
            let stop = starts.get(i + 1).copied().unwrap_or(end);
            let width = stop - start;
            let overlap = (stop.min(to) - start.max(from)).max(0.0);
            if width <= 0.0 || overlap <= 0.0 {
                continue;
            }
            if t.is_unknown(i) {
                return None;
            }
            total += t.time_ms.get(i).copied().unwrap_or(0.0) * overlap / width;
        }
        Some(total)
    };
    Some(DigestDay {
        last_ms: within(end - DAY, end)?,
        before_ms: (covered >= 2.0 * DAY - SLACK).then(|| within(end - 2.0 * DAY, end - DAY)).flatten(),
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
        let d = digest(&r, None, 3, 5);
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
            unknown: None,
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
    fn a_day_holding_a_lower_bound_is_not_compared() {
        // The bucket 30 hours ago holds only a lower bound (a running total fell without a reset): the day before has
        // no total to compare; one 3 hours ago leaves the last day without one either.
        let mut w = hourly(49, |_| 100.0);
        let mark = |w: &mut Workload, ago: usize| {
            let t = w.timeline.as_mut().unwrap();
            let mut unknown = t.unknown.clone().unwrap_or_else(|| vec![false; t.starts.len()]);
            let at = t.starts.len() - ago;
            unknown[at] = true;
            t.unknown = Some(unknown);
        };
        mark(&mut w, 30);
        let day = day_over_day(&w).expect("the last day is whole");
        assert_eq!(day.before_ms, None);
        mark(&mut w, 3);
        assert_eq!(day_over_day(&w), None);
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
        let d = digest(&r, None, 3, 5);
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
        let from_report = digest(&now, Some(&before), 10, 5).since.expect("changes");
        let previous = digest(&before, None, 3, 5);
        assert_eq!(previous.findings.len(), before.findings.iter().filter(|f| !f.inactive).count());
        let from_digest = digest_since(&now, &previous, 10, 5).since.expect("changes");
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
            let d = digest(&with_escalations(report(), now), Some(&previous), 10, 5);
            assert_eq!(d.findings.iter().find(|f| f.id == "lock-escalations:dbo.orders").unwrap().count, Some(now));
            // A previous digest carries the count too.
            let from_digest = digest_since(&with_escalations(report(), now), &digest(&previous, None, 10, 5), 10, 5);
            assert_eq!(from_digest.since, d.since);
            d.since.expect("changes").again.iter().map(|f| f.count_since).collect::<Vec<_>>()
        };
        assert_eq!(again(7.0, 4.0), [Some(3.0)]);
        assert!(again(7.0, 7.0).is_empty());
        // The counters were reset after the previous scan: every escalation counted now is new.
        assert_eq!(again(2.0, 10.0), [Some(2.0)]);
        // A finding without a count since startup never shows here.
        assert!(digest(&report(), Some(&report()), 10, 5).since.expect("changes").again.is_empty());
    }

    #[test]
    fn a_digest_lists_the_heaviest_queries_first() {
        let r = report();
        let d = digest(&r, None, 3, 2);
        assert_eq!(d.top_queries.len(), 2);
        assert!(d.top_queries[0].total_ms >= d.top_queries[1].total_ms);
        let heaviest = crate::support::app_queries(&r.snapshot)
            .max_by(|a, b| a.total_time_ms.total_cmp(&b.total_time_ms))
            .expect("a query");
        let first = &d.top_queries[0];
        assert_eq!(first.id, heaviest.id);
        assert_eq!(first.label, crate::support::query_handle(heaviest));
        assert!(first.db_time_share.is_some_and(|s| s > 0.0 && s <= 1.0));
        assert!(!first.statement.contains('\n') && first.statement.chars().count() <= STATEMENT_CHARS);
        // Every finding about it, by subject or by the queries it names.
        let about: Vec<&str> = r
            .findings
            .iter()
            .filter(|f| !f.inactive)
            .filter(|f| {
                (f.subject.kind == SubjectKind::Query && f.subject.name == first.id)
                    || f.queries.iter().any(|q| q.id == first.id)
            })
            .map(|f| f.id.as_str())
            .collect();
        assert_eq!(first.findings, about);
        // `--top-queries 0` leaves them out.
        assert!(digest(&r, None, 3, 0).top_queries.is_empty());
    }

    #[test]
    fn index_advice_is_capped_but_counts_everything() {
        let r = report();
        let full = crate::index_advice(&r);
        let d = digest(&r, None, 1, 5).index_advice.expect("advice");
        assert_eq!(d.create_total as usize, full.candidates.len());
        assert_eq!(d.drop_total as usize, full.cleanups.len());
        assert!(full.candidates.len() > 1 && full.cleanups.len() > 1, "the demo needs more than one of each");
        assert_eq!((d.create.len(), d.drop.len()), (1, 1));
        assert_eq!(d.create[0].table, format!("{}.{}", full.candidates[0].schema, full.candidates[0].table));
        assert_eq!(d.create[0].sql, full.candidates[0].sql);
        assert_eq!(d.drop[0].index, full.cleanups[0].index);
    }

    #[test]
    fn azure_index_recommendations_join_the_advice() {
        let mut s = report().snapshot;
        s.tuning = Some(culprit_core::TuningAdvice {
            recommendations: vec![culprit_core::TuningRecommendation {
                name: "IR_1".into(),
                kind: "CreateIndex".into(),
                state: Some("Active".into()),
                score: Some(64.0),
                script: Some("CREATE NONCLUSTERED INDEX [nci_1] ON [shop].[orders] ([channel], [created_at])".into()),
                index: Some(culprit_core::RecommendedIndex {
                    schema: "shop".into(),
                    table: "orders".into(),
                    name: Some("nci_1".into()),
                    columns: vec!["channel".into(), "created_at".into()],
                    include: vec!["total".into()],
                }),
                ..Default::default()
            }],
            ..Default::default()
        });
        let d = digest(&crate::analyze(s), None, 10, 5).index_advice.expect("advice");
        let azure = d.create.iter().find(|c| c.source == "azure").expect("Azure's recommendation");
        assert_eq!(azure.table, "shop.orders");
        assert_eq!(azure.keys, ["channel", "created_at"]);
        assert!(azure.reason.contains("64 out of 100"), "{}", azure.reason);
        assert!(azure.sql.as_deref().is_some_and(|q| q.starts_with("CREATE NONCLUSTERED INDEX [nci_1]")));
        assert_eq!(azure.finding.as_deref(), Some("tuning-advice:create-index:shop.orders(channel,created_at)"));
    }

    fn deadlock(at: &str, pattern: &str, objects: &[&str]) -> culprit_core::Deadlock {
        serde_json::from_value(serde_json::json!({
            "id": format!("dl-{at}"), "at": at, "source": "system_health", "pattern": pattern, "objects": objects,
            "processes": [
                {"id": "p1", "victim": true, "statement": "UPDATE shop.orders SET total = ? WHERE id = ?", "wait_mode": "U"},
                {"id": "p2", "victim": false, "statement": "SELECT  total\n FROM shop.orders WHERE id = ?", "wait_mode": "S"},
            ],
            "resources": [], "headline": "Two sessions waited on each other.", "explanation": "",
            "fixes": [{"title": "Touch rows in the same order", "detail": ""}],
        }))
        .expect("deadlock")
    }

    fn with_deadlocks(deadlocks: Vec<culprit_core::Deadlock>) -> Report {
        let mut r = report();
        r.snapshot.deadlocks = Some(culprit_core::DeadlockReport {
            deadlocks,
            source: "system_health".into(),
            since_hours: 48.0,
            ..Default::default()
        });
        r
    }

    #[test]
    fn deadlocks_are_grouped_by_pattern_and_counted_since_the_previous_digest() {
        let r = with_deadlocks(vec![
            deadlock("2026-10-11T11:00:00Z", "opposite-order", &["shop.orders", "shop.customers"]),
            deadlock("2026-10-11T09:00:00Z", "opposite-order", &["shop.customers", "shop.orders"]),
            deadlock("2026-10-10T20:00:00Z", "key-lookup", &["shop.customers"]),
        ]);
        let d = digest(&r, None, 5, 5).deadlocks.expect("deadlocks");
        assert_eq!((d.count, d.since_hours, d.new_since), (Some(3), Some(48.0), None));
        assert_eq!(d.latest_at.as_deref(), Some("2026-10-11T11:00:00Z"));
        assert_eq!(d.patterns.len(), 2);
        let first = &d.patterns[0];
        assert_eq!((first.pattern.as_str(), first.count), ("opposite-order", 2));
        assert_eq!(first.tables, ["shop.customers", "shop.orders"]);
        assert_eq!(first.latest_at.as_deref(), Some("2026-10-11T11:00:00Z"));
        assert_eq!(
            first.statements,
            ["UPDATE shop.orders SET total = ? WHERE id = ?", "SELECT total FROM shop.orders WHERE id = ?"]
        );
        assert_eq!(first.lock_modes, ["U", "S"]);
        assert_eq!(first.fix.as_deref(), Some("Touch rows in the same order"));
        // Compared with a previous digest taken at 10:00: one came after it.
        let mut previous = digest(&r, None, 5, 5);
        previous.taken_at = "2026-10-11T10:00:00Z".into();
        assert_eq!(digest_since(&r, &previous, 5, 5).deadlocks.expect("deadlocks").new_since, Some(1));
        // Capped like the findings.
        assert_eq!(digest(&r, None, 1, 5).deadlocks.expect("deadlocks").patterns.len(), 1);
    }

    #[test]
    fn deadlocks_that_cant_be_read_are_not_reported_as_none() {
        // No deadlocks collected: the collector's note says why.
        let mut r = report();
        r.snapshot.deadlocks = None;
        r.snapshot.notes.push(culprit_core::CollectorNote {
            collector: "deadlocks".into(),
            message: "Reading deadlocks needs VIEW SERVER STATE.".into(),
        });
        let d = digest(&r, None, 5, 5).deadlocks.expect("deadlocks");
        assert_eq!((d.count, d.note.as_deref()), (None, Some("Reading deadlocks needs VIEW SERVER STATE.")));
        // A counter without graphs (Postgres): counted, but no count of graphs.
        let mut r = with_deadlocks(Vec::new());
        if let Some(d) = r.snapshot.deadlocks.as_mut() {
            d.counted = Some(3.0);
            d.note = Some("Postgres counts deadlocks; their details are in the server log.".into());
        }
        let d = digest(&r, None, 5, 5).deadlocks.expect("deadlocks");
        assert_eq!((d.count, d.counted), (None, Some(3.0)));
        assert!(d.note.is_some());
        // Read, and none: zero.
        assert_eq!(digest(&with_deadlocks(Vec::new()), None, 5, 5).deadlocks.expect("deadlocks").count, Some(0));
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

        let changes = digest(&now, Some(&before), 10, 5).since.expect("changes");
        assert_eq!(changes.taken_at, "2026-10-10T06:00:00Z");
        assert_eq!(changes.new.iter().map(|f| f.id.as_str()).collect::<Vec<_>>(), [gone.id.as_str()]);
        assert_eq!(changes.fixed.iter().map(|f| f.id.as_str()).collect::<Vec<_>>(), [old.id.as_str()]);
        assert_eq!(changes.worse.len(), 1);
        assert_eq!(changes.worse[0].id, rising);
        assert_eq!(changes.worse[0].was, Some(Severity::Low));
    }
}
