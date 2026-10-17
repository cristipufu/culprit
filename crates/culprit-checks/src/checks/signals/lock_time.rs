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

/// Whether MariaDB counted this statement's whole run as its lock time. With the slow log writing to a table
/// (`log_output` with TABLE), a statement slower than long_query_time reports a lock time of about its whole run: the
/// demo's GROUP BY over a million rows a run showed 49,963.3 ms of lock in 49,963.9 ms, and 9.6 ms in a scan with the
/// slow log off. Where long_query_time was lower for part of the window, the same shows as a statement that examines
/// thousands of rows a run in almost no time besides its lock wait. Its lock time says nothing then.
pub(crate) fn lock_time_counts_the_run(snapshot: &Snapshot, q: &QueryStat) -> bool {
    let setting = |name: &str| snapshot.setting(name).map(|s| s.value.trim().to_ascii_uppercase());
    let slow_log_to_table = setting("log_output").is_some_and(|v| v.contains("TABLE"))
        && ["slow_query_log", "log_slow_query"].iter().any(|n| setting(n).is_some_and(|v| crate::config::is_on(&v)));
    if !is_mariadb(snapshot) || !slow_log_to_table {
        return false;
    }
    let logged = setting("long_query_time")
        .and_then(|v| v.parse::<f64>().ok())
        .is_some_and(|seconds| q.max_time_ms.unwrap_or(q.mean_time_ms) >= seconds * 1_000.0);
    let examined = q.logical_reads.map(|r| per_run(r, q.executions)).unwrap_or(0.0);
    logged || (lock_ms(q) >= 0.99 * q.total_time_ms && examined >= 10_000.0)
}

pub fn lock_time(snapshot: &Snapshot) -> Vec<Finding> {
    let total_db = snapshot.workload.as_ref().map(|w| w.total_time_ms).unwrap_or(0.0);
    let mut findings: Vec<Finding> = app_queries(snapshot)
        // A statement that names no table (SELECT SLEEP(…)) isn't waiting on the application's rows.
        .filter(|q| query_handle(q) != "a query")
        // Lock wait timeouts are already the failing-query finding, with the same fix.
        .filter(|q| !times_out_on_locks(q))
        // A lock time MariaDB made of the statement's whole run isn't waiting.
        .filter(|q| !lock_time_counts_the_run(snapshot, q))
        .filter_map(|q| {
            let lock = lock_ms(q);
            let share = per_run(lock, q.total_time_ms);
            (lock >= 10_000.0 && share >= 0.25).then_some((q, lock, share.min(1.0)))
        })
        .map(|(q, lock, share)| {
            let mut f = finding("lock-time", &q.id, Category::Queries, subject(SubjectKind::Query, &q.id));
            let handle = query_handle(q);
            let each = per_run(lock, q.executions);
            // One run, or two of which one took twice the other: what happened, never an average of 5.2 s for runs of
            // 10.5 s and a fraction of a millisecond. The longer of two waited at least what the other can't have.
            let few = q.few_runs();
            let per = match few.as_deref() {
                Some([_]) => format!(": its one run waited {}", format::duration(lock)),
                Some([longest, other]) => format!(
                    ": one run waited {}{}",
                    if share >= 0.95 { "" } else { "at least " },
                    format::duration((lock - other).clamp(0.0, *longest))
                ),
                _ => format!(", {} per run", format::duration(each)),
            };
            // A statement that stopped running is told in the past.
            let stopped = crate::all_stopped(snapshot, &[q]);
            f.title = if share >= 0.95 {
                let spends = if stopped { "spent" } else { "spends" };
                format!("{} {spends} nearly all its time waiting on locks{per}", capitalize(&handle))
            } else {
                let waits = if stopped { "waited" } else { "waits" };
                format!("{} {waits} on locks for {} of its time{per}", capitalize(&handle), format::percent(share))
            };
            f.summary = format!(
                "Of its {} {}, {} went to waiting for locks other sessions held. That time belongs to the sessions holding the locks: making this statement faster won't help, shortening the transactions that hold the rows will.{}",
                format::duration(q.total_time_ms),
                ran_phrase(snapshot, q),
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
                window_rate_per_day(snapshot, q, lock),
                LOCK_HIGH_MS_PER_DAY,
                LOCK_MEDIUM_MS_PER_DAY,
            );
            f.severity = severity_for_share(db_share).min(by_minutes);
            if window_rate_per_day(snapshot, q, lock).is_none() {
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
                match few.as_deref() {
                    Some([longest, _]) => Metric::new("Longest run", *longest, Unit::Ms),
                    _ => Metric::new("Per run", each, Unit::Ms),
                },
            ];
            // A read that waits for writers' locks because reads don't use row versions here: that setting is the
            // change that removes the wait, as config:read-committed-snapshot says.
            let versioning = crate::config::read_committed_snapshot_off(snapshot) && crate::config::versioning_spares_read(q);
            // How to find the holder, as every check that names a lock wait says it: Live only for waits that come
            // often enough to catch one.
            let LockHolder { lead, sql } = find_lock_holder(snapshot, q, None);
            let holders = format!(
                "{lead} Keep the holding transactions short and commit before slow work; make UPDATE and DELETE filter on an index so they lock only the rows they change; touch rows in the same order everywhere."
            );
            f.fix = Some(Fix {
                summary: if versioning {
                    let without = match holders.strip_prefix("Find ") {
                        Some(rest) => format!("Without it, find {rest}"),
                        None => format!("Without it, it waits for the writers. {holders}"),
                    };
                    format!(
                        "This is a read, and READ_COMMITTED_SNAPSHOT is off for this database, so under read committed it waits for any write holding a lock on the rows it reads. With row versioning on (config:read-committed-snapshot has the change), it would read the last committed version of the rows instead of waiting. {without}"
                    )
                } else {
                    holders
                },
                sql,
                cost: None,
            });
            // On running totals the lock time holds every run from before the change: two scans show it.
            let now = format!("{} over {} runs", format::duration(lock), format::count(q.executions));
            f.verify = vec![match crate::queries::verify_counts_between_scans(snapshot, "lock time and runs", &now) {
                Some(compare) => format!("Between two scans, its lock time per run should drop: {compare}."),
                None => "Re-scan: lock waits should be a small part of its time.".into(),
            }];
            f
        })
        .collect();
    findings.sort_by(|a, b| b.impact.score.total_cmp(&a.impact.score));
    findings.truncate(5);
    findings
}
