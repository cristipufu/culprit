//! The `lock-time` check: Statements that spend a large part of their time waiting for locks other sessions hold: Query Store's lock waits per query, MySQL and MariaDB lock time per digest. PostgreSQL keeps no lock time per statement; Live shows blocking as it happens.

use super::*;

pub fn lock_time_info() -> CheckInfo {
    CheckInfo {
        id: "lock-time".into(),
        title: "Queries that spend their time waiting for locks".into(),
        category: Category::Queries,
        engines: vec![Engine::Mssql, Engine::Mysql],
        requires: vec![],
        description: "Statements that spend a large part of their time waiting for locks other sessions hold: Query Store's lock waits per query, MySQL and MariaDB lock time per digest. PostgreSQL keeps no lock time per statement; Live shows blocking as it happens.".into(),
    }
}

check!(LockTimeCheck: lock_time_info, lock_time);

/// Lock waits a day that make a statement's waiting High, or Medium: half an hour and five minutes.
pub(super) const LOCK_HIGH_MS_PER_DAY: f64 = 30.0 * 60_000.0;
const LOCK_MEDIUM_MS_PER_DAY: f64 = 5.0 * 60_000.0;

/// Runs that end in lock wait timeouts often enough for failing-query to report them.
pub(super) fn times_out_on_locks(q: &QueryStat) -> bool {
    q.costs.as_ref().is_some_and(|c| {
        c.top_error.as_deref().is_some_and(|e| e.starts_with("1205"))
            && c.unfinished().is_some_and(|n| n >= 5.0 && per_run(n, q.executions) >= 0.01)
    })
}

pub fn lock_time(snapshot: &Snapshot) -> Vec<Finding> {
    let total_db = snapshot.workload.as_ref().map(|w| w.total_time_ms).unwrap_or(0.0);
    let mut findings: Vec<Finding> = app_queries(snapshot)
        // A statement that names no table (SELECT SLEEP(…)) isn't waiting on the application's rows.
        .filter(|q| query_handle(q) != "a query")
        // Lock wait timeouts are already the failing-query finding, with the same fix.
        .filter(|q| !times_out_on_locks(q))
        .filter_map(|q| {
            let lock = lock_ms(q);
            let share = per_run(lock, q.total_time_ms);
            (lock >= 10_000.0 && share >= 0.25).then_some((q, lock, share.min(1.0)))
        })
        .map(|(q, lock, share)| {
            let mut f = finding("lock-time", &q.id, Category::Queries, subject(SubjectKind::Query, &q.id));
            let handle = query_handle(q);
            let each = per_run(lock, q.executions);
            f.title = if share >= 0.95 {
                format!("{} spends nearly all its time waiting on locks, {} per run", capitalize(&handle), format::duration(each))
            } else {
                format!(
                    "{} waits on locks for {} of its time, {} per run",
                    capitalize(&handle),
                    format::percent(share),
                    format::duration(each)
                )
            };
            f.summary = format!(
                "Of its {} {}, {} went to waiting for locks other sessions held. That time belongs to the sessions holding the locks: making this statement faster won't help, shortening the transactions that hold the rows will.{}",
                format::duration(q.total_time_ms),
                window_phrase(snapshot),
                format::duration(lock),
                if snapshot.server.engine == Engine::Mysql && !is_mariadb(snapshot) {
                    " MySQL counts row-lock waits in a statement's lock time from 8.0.28; earlier versions count table locks only."
                } else {
                    ""
                }
            );
            let db_share = if total_db > 0.0 { lock / total_db } else { 0.0 };
            // A share of database time alone makes one two-minute incident in a quiet database High: the waiting must
            // also add up to minutes a day, measured over an hour or more.
            let by_minutes = since_start_severity(
                window_rate_per_day(snapshot, lock),
                LOCK_HIGH_MS_PER_DAY,
                LOCK_MEDIUM_MS_PER_DAY,
            );
            f.severity = severity_for_share(db_share).min(by_minutes);
            if window_rate_per_day(snapshot, lock).is_none() {
                f.caveats.push(
                    "The statistics don't cover enough time (an hour or more from a known start) to say how many minutes of waiting a day this is, so it isn't raised on its share alone."
                        .into(),
                );
            }
            with_query(snapshot, &mut f, q);
            f.impact.score = score_for_share(db_share).max(base_score(f.severity));
            f.impact.label = format!("{} waiting on locks", format::duration(lock));
            f.confidence = Confidence::High;
            f.metrics = vec![
                Metric::new("Lock wait", lock, Unit::Ms),
                Metric::new("Share of its time", share, Unit::Ratio),
                Metric::new("Per run", each, Unit::Ms),
            ];
            f.fix = Some(Fix {
                summary: "Find the holder while it happens (Live shows who blocks whom). Keep the holding transactions short and commit before slow work; make UPDATE and DELETE filter on an index so they lock only the rows they change; touch rows in the same order everywhere.".into(),
                sql: None,
                cost: None,
            });
            f.verify = vec!["Re-scan: lock waits should be a small part of its time.".into()];
            f
        })
        .collect();
    findings.sort_by(|a, b| b.impact.score.total_cmp(&a.impact.score));
    findings.truncate(5);
    findings
}
