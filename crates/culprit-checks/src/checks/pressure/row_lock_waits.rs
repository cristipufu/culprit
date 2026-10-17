//! The `row-lock-waits` check: InnoDB row-lock waits add up to real time (a minute an hour or more across the server), from long transactions or updates that scan instead of using an index.

use super::*;

pub fn row_lock_waits_info() -> CheckInfo {
    CheckInfo {
        id: "row-lock-waits".into(),
        title: "Transactions waiting for row locks".into(),
        category: Category::Queries,
        engines: vec![Engine::Mysql],
        requires: vec![],
        description: "InnoDB row-lock waits add up to real time (a minute an hour or more across the server), from long transactions or updates that scan instead of using an index.".into(),
    }
}

check!(RowLockWaitsCheck: row_lock_waits_info, row_lock_waits);

pub fn row_lock_waits(snapshot: &Snapshot) -> Vec<Finding> {
    let Some(avg_ms) = signal(snapshot, "row-lock-avg") else { return Vec::new() };
    // A rate needs at least an hour of counters.
    let Some(hours) = counter_hours(snapshot) else { return Vec::new() };
    let Some(waits) = pressure(snapshot).and_then(|p| p.signal("row-lock-waits")) else { return Vec::new() };
    let count = if waits.unit == SignalUnit::PerHour { waits.value * hours } else { waits.value };
    // The total wait time; scans before Culprit kept it are rebuilt from the count and the average.
    let total_ms = signal(snapshot, "row-lock-time").unwrap_or(count * avg_ms);
    let per_hour_ms = total_ms / hours;
    // What it costs is time, not how many waits: 61 waits of half a second an hour is half a minute.
    if per_hour_ms < MINUTE_MS {
        return Vec::new();
    }
    // Against this database's statement time an hour (the server's waits include every database's).
    let share = snapshot
        .workload
        .as_ref()
        .filter(|w| w.window_hours >= culprit_core::RATE_MIN_HOURS && w.total_time_ms > 0.0)
        .map(|w| per_hour_ms / (w.total_time_ms / w.window_hours));
    let mut f = finding(
        "row-lock-waits",
        &server_key(snapshot),
        Category::Queries,
        subject(SubjectKind::Database, snapshot.server.database.clone()),
    );
    let name = snapshot.server.name();
    let total = crate::signals::total_wait(total_ms);
    f.title = format!(
        "Transactions on this server waited {total} for row locks {}, {} each on average",
        uptime_phrase(snapshot),
        format::duration(avg_ms)
    );
    // InnoDB reads a plain SELECT from a snapshot, taking no row locks (except under SERIALIZABLE, which makes it a
    // locking read): its lock time is table and metadata locks, never these waits.
    let serializable = ["transaction_isolation", "tx_isolation"]
        .iter()
        .any(|n| snapshot.setting(n).is_some_and(|s| s.value.to_ascii_uppercase().contains("SERIALIZABLE")));
    let takes_row_locks = |q: &QueryStat| serializable || !culprit_core::sql::reads_only(&q.text, Engine::Mysql);
    // A statement whose own time is mostly lock waits is where they show; one with a little lock time isn't named
    // as their cause, nor one whose lock time MariaDB made of its whole run.
    let top = app_queries(snapshot)
        .filter(|q| takes_row_locks(q) && !crate::signals::lock_time_counts_the_run(snapshot, q))
        .filter_map(|q| {
            let lock: f64 = q.waits.iter().filter(|w| category_key(&w.category) == "lock").map(|w| w.wait_ms).sum();
            (lock > 0.0).then_some((q, lock))
        })
        .max_by(|a, b| a.1.total_cmp(&b.1));
    f.summary = format!(
        "{}, InnoDB made transactions across the whole server (it counts every database together) wait for row locks \
         {} times, {} in all and about {} each: about {} an hour on average{}; the counters don't say when.{} \
         Lock waits grow with long transactions, and with UPDATE or DELETE statements whose filter isn't indexed: they lock every row they scan.",
        capitalize(&uptime_phrase(snapshot)),
        format::count(count),
        total,
        format::duration(avg_ms),
        format::duration(per_hour_ms),
        share
            .map(|s| format!(", against {} of statement time an hour in this database", format::percent(s.min(9.99))))
            .unwrap_or_default(),
        top.map(|(q, ms)| format!(
            " In this database, {} waited longest on locks ({} in total).",
            query_handle(q),
            format::duration(ms)
        ))
        .unwrap_or_default()
    );
    if let Some((q, ms)) = top
        && q.total_time_ms > 0.0
        && ms / q.total_time_ms >= 0.5
    {
        f.queries = query_refs(&[q]);
    }
    // Totals since the start describe the past: at most Medium, and Medium only when they are a real part of the
    // work (ten minutes an hour, or a tenth of this database's statement time) and this database's own statements
    // show it (one spends most of its time waiting on locks): the counters are every database's on the server.
    let here = !f.queries.is_empty();
    f.severity = if here && (per_hour_ms >= 10.0 * MINUTE_MS || share.is_some_and(|s| s >= 0.1)) {
        Severity::Medium
    } else {
        Severity::Low
    };
    f.impact.score = base_score(f.severity);
    f.impact.label = format!("{}/h lock waits", format::duration(per_hour_ms));
    f.confidence = Confidence::Medium;
    f.metrics = vec![
        Metric::new("Lock waits since start", count, Unit::Count),
        Metric::new("Time waiting since start", total_ms, Unit::Ms),
        Metric::new("Average wait", avg_ms, Unit::Ms),
    ];
    f.caveats.push(format!(
        "{name} counts row-lock waits for the whole server, so other databases' waits are in these numbers."
    ));
    f.fix = Some(Fix {
        summary: "Keep transactions short (no user input or slow calls inside them), make UPDATE and DELETE filter on indexed columns, and touch tables in the same order everywhere. Live shows who blocks whom right now.".into(),
        sql: None,
        cost: None,
    });
    f.verify = vec!["Sample Waits → Right now during normal load: it counts row-lock waits over the sample, which should be rare and short.".into()];
    vec![f]
}
