use culprit_core::{Category, CheckInfo, Confidence, Engine, Finding, Fix, Metric, Severity, Snapshot, SubjectKind};

use crate::support::*;

pub fn config_info() -> CheckInfo {
    CheckInfo {
        id: "config".into(),
        title: "Configuration".into(),
        category: Category::Configuration,
        engines: vec![Engine::Mssql, Engine::Postgres, Engine::Mysql],
        requires: vec![],
        description:
            "Database and server settings that commonly hurt performance or hide the data needed to diagnose it.".into(),
    }
}

pub fn config(snapshot: &Snapshot) -> Vec<Finding> {
    match snapshot.server.engine {
        Engine::Mssql => mssql(snapshot),
        Engine::Postgres => postgres(snapshot),
        Engine::Mysql => mysql(snapshot),
    }
}

fn value<'a>(snapshot: &'a Snapshot, names: &[&str]) -> Option<&'a str> {
    names.iter().find_map(|n| snapshot.setting(n).map(|s| s.value.as_str()))
}

fn is_on(v: &str) -> bool {
    matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "on" | "yes")
}

fn setting_finding(key: &str, severity: Severity, title: String, summary: String, fix: Fix) -> Finding {
    let mut f = finding("config", key, Category::Configuration, subject(SubjectKind::Setting, key));
    f.severity = severity;
    f.impact.score = base_score(severity) * 0.8;
    f.impact.label = "Setting".into();
    f.confidence = Confidence::Medium;
    f.title = title;
    f.summary = summary;
    f.fix = Some(fix);
    f
}

fn mssql(snapshot: &Snapshot) -> Vec<Finding> {
    let mut out = Vec::new();
    let db = quote(Engine::Mssql, &snapshot.server.database);

    let qs_actual = value(snapshot, &["query_store_options.actual_state_desc"]);
    let qs_reason = value(snapshot, &["query_store_options.readonly_reason"]);
    match qs_actual.map(|v| v.to_ascii_uppercase()) {
        Some(state) if state.contains("OFF") => out.push(setting_finding(
            "query-store",
            Severity::Medium,
            "Query Store is off, so query history is lost on every restart".into(),
            "Without Query Store, Culprit (and you) only see what is still in the plan cache: no history, no plan regressions, no per-query waits. It costs little and is on by default in Azure SQL and SQL Server 2022.".into(),
            Fix {
                summary: "Turn it on with automatic capture so ad-hoc noise isn't recorded.".into(),
                sql: Some(format!("ALTER DATABASE {db} SET QUERY_STORE = ON (OPERATION_MODE = READ_WRITE, QUERY_CAPTURE_MODE = AUTO);")),
                cost: Some("Uses some space in the database (default cap 1 GB) and a little CPU.".into()),
            },
        )),
        Some(state) if state.contains("READ_ONLY") && qs_reason.is_some_and(|r| r != "0") => out.push(setting_finding(
            "query-store",
            Severity::Medium,
            "Query Store stopped recording".into(),
            format!(
                "Query Store is read-only (reason code {}), most often because it reached its size limit. New queries and plan changes aren't being captured.",
                qs_reason.unwrap_or("?")
            ),
            Fix {
                summary: "Give it more room and let it clean up old data by itself.".into(),
                sql: Some(format!("ALTER DATABASE {db} SET QUERY_STORE (MAX_STORAGE_SIZE_MB = 2048, SIZE_BASED_CLEANUP_MODE = AUTO, OPERATION_MODE = READ_WRITE);")),
                cost: None,
            },
        )),
        _ => {}
    }

    if value(snapshot, &["is_read_committed_snapshot_on"]).is_some_and(|v| !is_on(v)) {
        let mut f = setting_finding(
            "read-committed-snapshot",
            Severity::Medium,
            "Readers and writers block each other (READ_COMMITTED_SNAPSHOT is off)".into(),
            "With row versioning off, a SELECT waits for any write holding a lock on the rows it reads, and vice versa. Most OLTP applications run with it on; Azure SQL Database enables it by default.".into(),
            Fix {
                summary: "Turn on row versioning for read committed. It needs a moment with no other connections, so schedule it.".into(),
                sql: Some(format!("ALTER DATABASE {db} SET READ_COMMITTED_SNAPSHOT ON WITH ROLLBACK IMMEDIATE;")),
                cost: Some("Row versions live in tempdb (or the database with ADR), which adds some write overhead.".into()),
            },
        );
        f.caveats.push("Readers then see the last committed version instead of waiting. Code that relies on blocking reads for correctness (rare) must use locking hints.".into());
        out.push(f);
    }

    for (name, label) in
        [("is_auto_create_stats_on", "AUTO_CREATE_STATISTICS"), ("is_auto_update_stats_on", "AUTO_UPDATE_STATISTICS")]
    {
        if value(snapshot, &[name]).is_some_and(|v| !is_on(v)) {
            out.push(setting_finding(
                &label.to_ascii_lowercase(),
                Severity::High,
                format!("{label} is off"),
                "The optimizer relies on statistics to estimate row counts. With this off, estimates go stale or missing and plans degrade over time.".into(),
                Fix { summary: "Turn it back on.".into(), sql: Some(format!("ALTER DATABASE {db} SET {label} ON;")), cost: None },
            ));
        }
    }

    if let Some(level) = value(snapshot, &["compatibility_level"]).and_then(|v| v.parse::<u32>().ok()) {
        let latest = latest_compat(&snapshot.server.version);
        if level + 20 <= latest {
            let mut f = setting_finding(
                "compatibility-level",
                Severity::Low,
                format!("The database runs at compatibility level {level}"),
                format!(
                    "The server supports level {latest}, but the optimizer behaves as it did at {level}. Newer levels bring adaptive joins, memory grant feedback and parameter-sensitive plans."
                ),
                Fix {
                    summary: "Raise it after testing; Query Store makes this safe because you can force the old plans for any query that regresses.".into(),
                    sql: Some(format!("ALTER DATABASE {db} SET COMPATIBILITY_LEVEL = {latest};")),
                    cost: None,
                },
            );
            f.confidence = Confidence::Low;
            f.metrics.push(Metric::text("Current level", level.to_string()));
            out.push(f);
        }
    }

    let ctfp = value(snapshot, &["cost threshold for parallelism"]).and_then(|v| v.parse::<f64>().ok());
    if snapshot.server.cloud.is_none() && ctfp.is_some_and(|v| v <= 5.0) {
        out.push(setting_finding(
                    "cost-threshold-for-parallelism",
                    Severity::Low,
                    "Cost threshold for parallelism is still the 1990s default of 5".into(),
                    "Even cheap queries go parallel, which wastes CPU on coordination and shows up as CXPACKET/CXCONSUMER waits.".into(),
                    Fix {
                        summary: "A common starting point is 50; adjust with your workload.".into(),
                        sql: Some("EXEC sp_configure 'show advanced options', 1; RECONFIGURE;\nEXEC sp_configure 'cost threshold for parallelism', 50; RECONFIGURE;".into()),
                        cost: None,
                    },
                ));
    }
    out
}

fn latest_compat(version: &str) -> u32 {
    let major: u32 = version.split('.').next().and_then(|v| v.parse().ok()).unwrap_or(16);
    (major * 10).clamp(100, 170)
}

fn postgres(snapshot: &Snapshot) -> Vec<Finding> {
    let mut out = Vec::new();
    let cloud = snapshot.server.cloud.is_some();
    let rpc = value(snapshot, &["random_page_cost"]).and_then(|v| v.parse::<f64>().ok());
    if let Some(rpc) = rpc.filter(|v| *v >= 4.0 && cloud) {
        {
            let mut f = setting_finding(
                "random_page_cost",
                Severity::Low,
                "random_page_cost assumes spinning disks".into(),
                format!(
                    "random_page_cost is {rpc}, which tells the planner that random reads cost four times sequential ones. On SSD-backed cloud storage they cost about the same, so the planner picks sequential scans over index scans more often than it should."
                ),
                Fix {
                    summary: "Lower it for this database.".into(),
                    sql: Some(format!(
                        "ALTER DATABASE {} SET random_page_cost = 1.1;",
                        quote(Engine::Postgres, &snapshot.server.database)
                    )),
                    cost: None,
                },
            );
            f.confidence = Confidence::Medium;
            out.push(f);
        }
    }
    if value(snapshot, &["track_io_timing"]).is_some_and(|v| !is_on(v)) {
        let mut f = setting_finding(
            "track_io_timing",
            Severity::Low,
            "I/O timing isn't tracked".into(),
            "With track_io_timing off, pg_stat_statements and EXPLAIN (BUFFERS) can't tell how much of a query's time is spent waiting on disk.".into(),
            Fix { summary: "Turn it on; the overhead is negligible on modern systems.".into(), sql: Some("ALTER SYSTEM SET track_io_timing = on;\nSELECT pg_reload_conf();".into()), cost: None },
        );
        f.impact.score = 4.0;
        out.push(f);
    }
    out
}

fn mysql(snapshot: &Snapshot) -> Vec<Finding> {
    let mut out = Vec::new();
    let pool = value(snapshot, &["innodb_buffer_pool_size"]).and_then(|v| v.parse::<f64>().ok());
    let data: f64 = snapshot.tables.iter().filter_map(|t| t.total_bytes).sum();
    if let Some(pool) = pool.filter(|p| *p > 0.0 && data >= 1024.0 * 1024.0 * 1024.0 && *p < data * 0.5) {
        let mut f = setting_finding(
            "innodb_buffer_pool_size",
            Severity::Medium,
            "The InnoDB buffer pool holds less than half of the data".into(),
            format!(
                "innodb_buffer_pool_size is {}, but this database has {} of tables and indexes. Queries that touch the rest read from disk.",
                culprit_core::format::bytes(pool),
                culprit_core::format::bytes(data)
            ),
            Fix {
                summary: "Give the buffer pool most of the server's memory (often 60–75% on a dedicated host); it can be resized online.".into(),
                sql: Some(format!("SET GLOBAL innodb_buffer_pool_size = {};", (data * 1.2).ceil() as u64)),
                cost: Some("Uses that much RAM; leave room for connections and the OS.".into()),
            },
        );
        f.confidence = Confidence::Medium;
        out.push(f);
    }
    if value(snapshot, &["slow_query_log"]).is_some_and(|v| !is_on(v)) {
        let mut f = setting_finding(
            "slow_query_log",
            Severity::Low,
            "The slow query log is off".into(),
            "Without it there is no record of individual slow statements with their real parameter values, only the aggregated digests.".into(),
            Fix {
                summary: "Turn it on with a threshold that matches your latency budget.".into(),
                sql: Some("SET GLOBAL slow_query_log = ON;\nSET GLOBAL long_query_time = 1;".into()),
                cost: None,
            },
        );
        f.impact.score = 4.0;
        out.push(f);
    }
    out
}
