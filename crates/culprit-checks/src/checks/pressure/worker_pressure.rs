//! The `worker-pressure` check: SQL Server sessions waiting for a worker thread (THREADPOOL), or PostgreSQL/MySQL connections close to max_connections.

use super::*;
use crate::evidence::Requirement as Evidence;

pub fn worker_pressure_info() -> CheckInfo {
    CheckInfo {
        id: "worker-pressure".into(),
        title: "Running out of workers or connections".into(),
        category: Category::Configuration,
        engines: vec![Engine::Mssql, Engine::Postgres, Engine::Mysql],
        requires: vec![],
        description: "SQL Server sessions waiting for a worker thread (THREADPOOL), or PostgreSQL/MySQL connections close to max_connections.".into(),
    }
}

check!(
    WorkerPressureCheck: worker_pressure_info, worker_pressure,
    &[Evidence::Pressure { signal: Some("connections"), timed_files: false, complete: true }]
);

pub fn worker_pressure(snapshot: &Snapshot) -> Vec<Finding> {
    let key = server_key(snapshot);
    match snapshot.server.engine {
        Engine::Sqlite => Vec::new(),
        Engine::Mssql => {
            let waiting = pressure(snapshot)
                .and_then(|p| p.signal("threadpool-waiting"))
                .filter(|s| s.label.contains("in this database"))
                .map(|s| s.value)
                .unwrap_or(0.0);
            let hours = counter_hours(snapshot);
            // The total since the start; scans before Culprit kept it stored an hourly average instead.
            let total = signal(snapshot, "threadpool-wait-ms").or_else(|| {
                let p = pressure(snapshot)?;
                let s = p.signal("threadpool-waits")?;
                Some(s.value * p.uptime_hours.unwrap_or(1.0).max(1.0))
            });
            // An average since the start says how much, not when: one bad hour days ago averages the same as a
            // steady trickle. Only over at least an hour of counters.
            let per_hour = total.zip(hours).map(|(t, h)| t / h);
            let database_history = snapshot.server.on(culprit_core::Platform::AzureSqlDatabase)
                || app_queries(snapshot).any(|q| {
                    q.waits.iter().any(|w| w.category.eq_ignore_ascii_case("worker thread") && w.wait_ms >= MINUTE_MS)
                });
            let history = database_history && per_hour.is_some_and(|p| p >= 10_000.0);
            if waiting <= 0.0 && !history {
                return Vec::new();
            }
            let mut f = finding("worker-pressure", &key, Category::Configuration, subject(SubjectKind::Database, &key));
            f.title = if waiting > 0.0 {
                format!(
                    "{} waiting for a worker thread when the scan ran (THREADPOOL)",
                    if waiting == 1.0 {
                        "1 request was".to_string()
                    } else {
                        format!("{} requests were", format::count(waiting))
                    }
                )
            } else {
                format!(
                    "Requests waited {} for a worker thread {} (THREADPOOL)",
                    format::duration(total.unwrap_or(0.0)),
                    uptime_phrase(snapshot)
                )
            };
            let mut parts = Vec::new();
            if history {
                parts.push(format!(
                    "{}, requests on this instance waited {} in total for a worker thread, about {} an hour on average; \
                     the counters don't say when.",
                    capitalize(&uptime_phrase(snapshot)),
                    format::duration(total.unwrap_or(0.0)),
                    format::duration(per_hour.unwrap_or(0.0))
                ));
            }
            if waiting > 0.0 {
                parts.push(format!(
                    "When the scan ran, {} in this database waiting for one.",
                    if waiting == 1.0 {
                        "1 request was".to_string()
                    } else {
                        format!("{} requests were", format::count(waiting))
                    }
                ));
            }
            parts.push(
                "A request that can't get a worker can't even begin, so the app sees timeouts while the server looks idle. \
                 It is almost always a symptom: a blocking chain holding many workers, or many parallel queries each taking several."
                    .into(),
            );
            f.summary = parts.join(" ");
            // A queue at the moment of the scan is starvation now; totals since the start describe the past (at most
            // Medium); a task or two seen once, with nothing on record, is a moment worth watching.
            f.severity = if waiting >= 5.0 && history {
                Severity::High
            } else if waiting >= 5.0 || (history && per_hour.is_some_and(|p| p >= MINUTE_MS)) {
                Severity::Medium
            } else {
                Severity::Low
            };
            f.impact.score = base_score(f.severity);
            f.impact.label = "THREADPOOL waits".into();
            f.confidence = match (history, waiting > 0.0) {
                (true, true) => Confidence::High,
                (true, false) => Confidence::Medium,
                _ => Confidence::Low,
            };
            if !history {
                f.caveats.push(SEEN_ONCE.into());
            }
            if let Some(t) = total.filter(|t| *t > 0.0) {
                f.metrics.push(Metric::new("Waited since start", t, Unit::Ms));
            }
            if waiting > 0.0 {
                f.metrics.push(Metric::new("Waiting now", waiting, Unit::Count));
            }
            f.fix = Some(Fix {
                summary: "Find the head blocker in Live and end the transaction behind the chain; then lower parallelism so each query takes fewer workers (cost threshold for parallelism, MAXDOP). Raising max worker threads only hides it.".into(),
                sql: None,
                cost: None,
            });
            f.verify =
                vec!["Re-scan: THREADPOOL waits should stop growing and nothing should wait for a worker.".into()];
            vec![f]
        }
        Engine::Postgres | Engine::Mysql => {
            let Some(used) = signal(snapshot, "connections") else { return Vec::new() };
            let peak = signal(snapshot, "max-used-connections");
            let hit_limit = peak.is_some_and(|v| v >= 100.0);
            if used < 85.0 && !hit_limit {
                return Vec::new();
            }
            // When the busiest moment was (MySQL 5.7.5+ keeps it); without it, only that it happened since the start.
            let peak_ago = signal(snapshot, "max-used-connections-ago");
            let when = match peak_ago {
                Some(s) => format!("{} ago", format::duration(s * 1000.0)),
                None => uptime_phrase(snapshot),
            };
            let mut f = finding(
                "worker-pressure",
                &key,
                Category::Configuration,
                subject(SubjectKind::Setting, "max_connections"),
            );
            f.title = if used >= 85.0 {
                format!("The whole server's connections were at {:.0}% of max_connections when scanned", used)
            } else {
                format!("{} reached its connection limit once, {when}", snapshot.server.name())
            };
            let peak_note = match peak.filter(|_| used < 85.0) {
                Some(p) if p > 100.0 => format!(
                    " At its busiest it had {:.0}% of today's max_connections open: the limit was lowered after that.",
                    p
                ),
                Some(_) => format!(
                    " That was the busiest moment since {} started; it isn't near the limit now ({:.0}% in use).",
                    snapshot.server.name(),
                    used
                ),
                None => String::new(),
            };
            f.summary = format!(
                "This count covers every database on the server, not only this database's application. When every connection slot is taken, new connections are refused and the application errors out.{peak_note} {} \
                 Each connection costs memory, so the fix is fewer, busier connections rather than a higher limit.",
                if snapshot.server.engine == Engine::Postgres {
                    "Idle connections still hold a backend process each."
                } else {
                    "Idle connections still hold a thread and buffers each."
                }
            );
            // Now is measured; a peak since the start is one moment in the past: at most Medium, and Low unless it
            // was within the last day.
            // The server's capacity is relevant context, but this count does not identify this database as its cause.
            f.severity = Severity::Low;
            f.impact.score = base_score(f.severity);
            f.impact.label = format!("{:.0}% of connections", used);
            f.confidence = Confidence::High;
            f.metrics = vec![Metric::new("Connections in use", used / 100.0, Unit::Ratio)];
            f.fix = Some(Fix {
                summary: if snapshot.server.engine == Engine::Postgres {
                    "Put a connection pooler in front (PgBouncer, or the application's pool with a smaller maximum) and close idle sessions. Raise max_connections only with memory to spare (it needs a restart).".into()
                } else {
                    "Size the application's connection pools to what the server can run (a few per core), close idle sessions, and use ProxySQL or the cloud proxy if many clients connect. Raise max_connections only with memory to spare.".into()
                },
                sql: None,
                cost: None,
            });
            f.verify = vec!["Re-scan: connections in use should stay under 80% of max_connections.".into()];
            vec![f]
        }
    }
}
