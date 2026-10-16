//! The `connection-churn` check: New connections per second since startup (SQL Server logins, PostgreSQL 14+ sessions, MySQL and MariaDB Connections), with how long sessions last and how many came from a pool where the engine says.

use super::*;

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

check!(ConnectionChurnCheck: connection_churn_info, connection_churn);

pub fn connection_churn(snapshot: &Snapshot) -> Vec<Finding> {
    let Some(p) = snapshot.pressure.as_ref() else { return Vec::new() };
    let Some(rate) = p.signal("new-connections").map(|s| s.value) else { return Vec::new() };
    if p.uptime_hours.is_some_and(|h| h < 1.0) {
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
        format!("The application opens {} connections a second: use a pool", format::count(rate))
    } else {
        format!("The server accepts {} new connections a second: use a pool", format::count(rate))
    };
    let detail = match engine {
        Engine::Sqlite => "SQLite has no connections to pool: each one opens the file.".to_string(),
        Engine::Postgres => {
            let length = p.signal("session-length").map(|s| s.value);
            format!(
                "Each new PostgreSQL connection starts a server process and pays for TLS and authentication: milliseconds of CPU and a few MB of memory before the first query.{}",
                length
                    .map(|s| format!(" Sessions here last {} on average.", format::duration(s * 1000.0)))
                    .unwrap_or_default()
            )
        }
        Engine::Mssql => match p.signal("pooled-reuse").map(|s| s.value) {
            Some(pct) => format!(
                "Only {:.0}% of connection uses were served from a pool (SQL Server counts a reset when a pooled connection is reused); the rest logged in from scratch.",
                pct
            ),
            None => "Every login pays for TLS, authentication and session setup.".into(),
        },
        Engine::Mysql => match p.signal("thread-cache-misses").map(|s| s.value) {
            Some(pct) => format!("{:.0}% of them needed a new thread because the thread cache was empty.", pct),
            None => "Every connection pays for TLS, authentication and a thread.".into(),
        },
    };
    let scope = match engine {
        Engine::Sqlite => "",
        Engine::Postgres => " to this database",
        Engine::Mssql => ", counting every database on the instance",
        Engine::Mysql => ", counting every schema and application on it (failed attempts left out)",
    };
    f.summary = format!(
        "{} the server accepted {} new connections a second on average{scope}. {detail} An application that connects for each request spends more time connecting than querying under load.",
        capitalize(&since_start(snapshot)),
        format::count(rate)
    );
    // A count of the whole server's connections is shared: said, with what it counts, but not graded on this
    // database (CLAUDE.md: mark shared counters and don't grade them). PostgreSQL's is this database's own.
    let shared = engine != Engine::Postgres;
    f.severity = if !shared && rate >= limit * 5.0 { Severity::Medium } else { Severity::Low };
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
    f.verify = vec![format!(
        "Scan again a day after the change: new connections should be under {} a second.",
        format::count(limit)
    )];
    f.caveats.push("A rate since startup: one busy reconnect storm counts for the whole period.".into());
    vec![f]
}
