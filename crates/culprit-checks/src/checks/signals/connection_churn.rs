//! The `connection-churn` check: New connections per second since startup (SQL Server logins, PostgreSQL 14+ sessions, MySQL and MariaDB Connections), with how long sessions last and how many came from a pool where the engine says.

use super::*;
use crate::evidence::Requirement as Evidence;

pub fn connection_churn_info() -> CheckInfo {
    CheckInfo {
        id: "connection-churn".into(),
        title: "Applications that connect for every request".into(),
        category: Category::Configuration,
        engines: vec![Engine::Mssql, Engine::Postgres, Engine::Mysql],
        requires: vec![],
        description: "New connections per second since startup (SQL Server logins, PostgreSQL 14+ sessions, MySQL and MariaDB Connections), with how long sessions last and how many came from a pool where the engine says.".into(),
    }
}

check!(
    ConnectionChurnCheck: connection_churn_info, connection_churn,
    &[Evidence::Pressure { signal: None, timed_files: false, complete: false }]
);

pub fn connection_churn(snapshot: &Snapshot) -> Vec<Finding> {
    let Some(p) = snapshot.pressure.as_ref() else { return Vec::new() };
    let Some(rate) = p.signal(culprit_core::signal_id::NEW_CONNECTIONS).map(|s| s.value) else { return Vec::new() };
    if p.uptime_hours.is_none_or(|h| h < 1.0) {
        return Vec::new();
    }
    let engine = snapshot.server.engine;
    // On Azure SQL Database the login counters belong to the server process behind the database, shared by its
    // elastic pool: another database's logins aren't this one's to fix.
    if engine == Engine::Mssql && snapshot.server.on(culprit_core::Platform::AzureSqlDatabase) {
        return Vec::new();
    }
    let limit = if engine == Engine::Postgres { 2.0 } else { 10.0 };
    if rate < limit {
        return Vec::new();
    }
    // A large pool can legitimately open many long-lived sessions. Neither a rate nor a thread-cache miss ratio
    // establishes one connection per request. Known long-lived sessions / frequent pooled resets contradict it.
    if (engine == Engine::Postgres
        && p.signal(culprit_core::signal_id::SESSION_LENGTH).is_some_and(|s| s.value >= 60.0))
        || (engine == Engine::Mssql && p.signal(culprit_core::signal_id::POOLED_REUSE).is_some_and(|s| s.value >= 90.0))
    {
        return Vec::new();
    }
    // Keyed on `server`, never the connection's name: finding ids travel into exports and agent briefs.
    let mut f = finding(
        "connection-churn",
        "server",
        Category::Configuration,
        subject(SubjectKind::Database, &snapshot.server.database),
    );
    // PostgreSQL counts sessions to this database; SQL Server's logins and MySQL's connections count the whole server,
    // every database and application on it.
    f.title = if engine == Engine::Postgres {
        format!("This database recorded {} new connections a second on average", format::count(rate))
    } else {
        format!("The whole server recorded {} new connections a second on average", format::count(rate))
    };
    let detail = match engine {
        Engine::Yugabyte => "A YSQL logical client can share a physical backend through Connection Manager; these counts alone do not identify how clients are pooled.".into(),
        Engine::Sqlite => "SQLite has no connections to pool: each one opens the file.".to_string(),
        Engine::Postgres => {
            let length = p.signal(culprit_core::signal_id::SESSION_LENGTH).map(|s| s.value);
            format!(
                "Each new PostgreSQL connection starts a server process and pays for TLS and authentication: milliseconds of CPU and a few MB of memory before the first query.{}",
                length
                    .map(|s| format!(" Sessions here last {} on average.", format::duration(s * 1000.0)))
                    .unwrap_or_default()
            )
        }
        Engine::Mssql => match p.signal(culprit_core::signal_id::POOLED_REUSE).map(|s| s.value) {
            Some(pct) => format!(
                "SQL Server recorded {:.0}% resets compared with resets plus new logins; pools can also reuse connections without a reset, so this does not identify all pool use.",
                pct
            ),
            None => "Every login pays for TLS, authentication and session setup.".into(),
        },
        Engine::Mysql => match p.signal(culprit_core::signal_id::THREAD_CACHE_MISSES).map(|s| s.value) {
            Some(pct) => format!("{:.0}% of them needed a new thread because the thread cache was empty.", pct),
            None => "Every connection pays for TLS, authentication and a thread.".into(),
        },
    };
    let scope = match engine {
        Engine::Yugabyte => ", within the measured YSQL node scope",
        Engine::Sqlite => "",
        Engine::Postgres => " to this database",
        Engine::Mssql => ", counting every database on the instance",
        Engine::Mysql => ", counting every schema and application on it (failed attempts left out)",
    };
    f.summary = format!(
        "{} the server accepted {} new connections a second on average{scope}. {detail} If the application connects for each request, pooling can avoid that setup cost; this rate alone does not prove it does so or that connection setup is slowing requests.",
        capitalize(&since_start(snapshot)),
        format::count(rate)
    );
    // A count of the whole server's connections is shared: said, with what it counts, but not graded on this
    // database (CLAUDE.md: mark shared counters and don't grade them). PostgreSQL's is this database's own.
    let shared = engine != Engine::Postgres;
    f.severity = if !shared
        && rate >= limit * 5.0
        && p.signal(culprit_core::signal_id::SESSION_LENGTH).is_some_and(|s| s.value <= 1.0)
    {
        Severity::Medium
    } else {
        Severity::Low
    };
    if shared {
        f.caveats.push(format!(
            "{} counts connections for the whole server, so other databases' applications are in this number.",
            snapshot.server.name()
        ));
    }
    severity_score(&mut f);
    f.impact.label = format!("{}/s new connections", format::count(rate));
    f.confidence = Confidence::Medium;
    f.metrics = vec![Metric::new("New connections per second", rate, Unit::Count)];
    // A larger thread cache, where the server is configured, and never smaller than it is.
    const THREAD_CACHE: f64 = 64.0;
    let thread_cache = snapshot.setting("thread_cache_size").and_then(|s| s.value.trim().parse::<f64>().ok());
    let cache_change = (engine == Engine::Mysql && thread_cache.is_none_or(|n| n < THREAD_CACHE))
        .then(|| setting_change(&snapshot.server, "thread_cache_size", &format!("{THREAD_CACHE:.0}")));
    f.fix = Some(match engine {
        Engine::Yugabyte => Fix { summary: "Reuse connections through the application pool or YSQL Connection Manager; compare connection setup latency and client/backend counts before changing pool sizes.".into(), sql: None, cost: None },
        Engine::Sqlite => Fix {
            summary: "Keep one connection open per worker instead of opening the file for each request.".into(),
            sql: None,
            cost: None,
        },
        Engine::Postgres => Fix {
            summary: "Pool connections in the application (every mainstream driver has a pool), or put PgBouncer in front in transaction mode so thousands of clients share a few dozen server connections.".into(),
            sql: None,
            cost: None,
        },
        Engine::Mssql => Fix {
            summary: "Keep connection pooling on (it is by default in ADO.NET and Microsoft.Data.SqlClient; JDBC needs a pool such as HikariCP) and close or dispose connections promptly so they go back to the pool. Every distinct connection string gets its own pool.".into(),
            sql: None,
            cost: None,
        },
        Engine::Mysql => Fix {
            summary: match (&cache_change, thread_cache) {
                (Some(change), _) => format!(
                    "Pool connections in the application. Until then, a larger thread cache lets new connections reuse threads: {}",
                    change.how
                ),
                (None, Some(n)) => format!(
                    "Pool connections in the application: the thread cache already keeps {} threads, so each connection still pays for TLS and authentication.",
                    format::count(n)
                ),
                (None, None) => "Pool connections in the application.".into(),
            },
            sql: cache_change.and_then(|c| c.sql),
            cost: None,
        },
    });
    f.verify = vec!["Compare new-connection counts over equal intervals before and after the change, together with request latency. Pooling should reduce connection setup for the same workload; there is no universal healthy connection rate.".into()];
    f.caveats.push("A rate since startup: one busy reconnect storm counts for the whole period.".into());
    vec![f]
}
