use culprit_core::waits::category_key;
use culprit_core::{
    Category, CheckInfo, Confidence, Engine, Finding, Fix, Metric, ServerInfo, Severity, Snapshot, SubjectKind, format,
};

use culprit_core::Check;

use crate::support::*;

/// The checks this area registers (`crate::all()` runs every area's), in the order their findings tie.
pub(crate) const CHECKS: &[&dyn Check] = &[&ConfigCheck];

pub fn config_info() -> CheckInfo {
    CheckInfo {
        id: "config".into(),
        title: "Configuration".into(),
        category: Category::Configuration,
        engines: vec![Engine::Mssql, Engine::Postgres, Engine::Mysql, Engine::Sqlite],
        requires: vec![],
        description:
            "Database and server settings that commonly hurt performance or hide the data needed to diagnose it.".into(),
    }
}

check!(ConfigCheck: config_info, config);

pub fn config(snapshot: &Snapshot) -> Vec<Finding> {
    match snapshot.server.engine {
        Engine::Mssql => [mssql(snapshot), crate::config_health::mssql(snapshot)].concat(),
        Engine::Postgres => [postgres(snapshot), crate::config_health::postgres(snapshot)].concat(),
        Engine::Mysql => [mysql(snapshot), crate::config_health::mysql(snapshot)].concat(),
        Engine::Sqlite => crate::sqlite::settings(snapshot),
    }
}

pub(crate) fn value<'a>(snapshot: &'a Snapshot, names: &[&str]) -> Option<&'a str> {
    names.iter().find_map(|n| snapshot.setting(n).map(|s| s.value.as_str()))
}

pub(crate) fn is_on(v: &str) -> bool {
    matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "on" | "yes")
}

pub(crate) fn setting_finding(key: &str, severity: Severity, title: String, summary: String, fix: Fix) -> Finding {
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

/// Several settings changed together, worded the way [`setting_change`] words one: one sentence naming them all, and
/// the statements, on a server where they run.
pub(crate) fn settings_change(server: &ServerInfo, changes: &[(&str, &str)]) -> SettingChange {
    let Some(((last, last_value), lead)) = changes.split_last() else {
        return SettingChange { how: String::new(), sql: None };
    };
    let sql: Vec<String> = changes.iter().filter_map(|(n, v)| setting_change(server, n, v).sql).collect();
    let name = match lead {
        [] => last.to_string(),
        _ => format!("{} and {last}", lead.iter().map(|(n, v)| format!("{n} to {v}")).collect::<Vec<_>>().join(", ")),
    };
    let mut change = setting_change(server, &name, last_value);
    change.sql = (!sql.is_empty()).then(|| sql.join("\n"));
    change
}

/// A SQL Server server option through [`setting_change`]: `sp_configure` with advanced options shown (it refuses
/// them otherwise) on a server you manage or a Managed Instance; the DB parameter group on RDS.
pub(crate) fn server_option(server: &ServerInfo, name: &str, value: &str) -> SettingChange {
    let mut change = setting_change(server, name, value);
    change.sql = change.sql.map(|sql| format!("EXEC sp_configure 'show advanced options', 1;\nRECONFIGURE;\n{sql}"));
    change
}

fn mssql(snapshot: &Snapshot) -> Vec<Finding> {
    let mut out = Vec::new();
    let db = quote(Engine::Mssql, &snapshot.server.database);

    let qs_actual = value(snapshot, &["query_store_options.actual_state_desc"]);
    let qs_reason =
        value(snapshot, &["query_store_options.readonly_reason"]).and_then(|r| r.trim().parse::<u64>().ok());
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
        Some(state) if state.contains("READ_ONLY") => {
            if let Some(f) = query_store_read_only(snapshot, &db, qs_reason.unwrap_or(0)) {
                out.push(f);
            }
        }
        _ => {}
    }

    if value(snapshot, &["is_read_committed_snapshot_on"]).is_some_and(|v| !is_on(v)) {
        out.push(read_committed_snapshot(snapshot, &db));
    }

    for (name, label) in
        [("is_auto_create_stats_on", "AUTO_CREATE_STATISTICS"), ("is_auto_update_stats_on", "AUTO_UPDATE_STATISTICS")]
    {
        if value(snapshot, &[name]).is_some_and(|v| !is_on(v)) {
            // Some products require statistics they create themselves: SharePoint's content databases run with
            // AUTO_CREATE_STATISTICS off by design.
            let create = label == "AUTO_CREATE_STATISTICS";
            let mut f = setting_finding(
                &label.to_ascii_lowercase(),
                if create { Severity::Medium } else { Severity::High },
                format!("{label} is off"),
                "The optimizer relies on statistics to estimate row counts. With this off, estimates go stale or missing and plans degrade over time.".into(),
                Fix { summary: "Turn it back on.".into(), sql: Some(format!("ALTER DATABASE {db} SET {label} ON;")), cost: None },
            );
            if create {
                f.caveats.push("Some products require it off and maintain statistics themselves (SharePoint content databases do): check the vendor's guidance before turning it on.".into());
            }
            out.push(f);
        }
    }

    if let Some(level) = value(snapshot, &["compatibility_level"]).and_then(|v| v.parse::<u32>().ok()) {
        let latest = latest_compat(&snapshot.server);
        if level + 20 <= latest {
            let mut f = setting_finding(
                "compatibility-level",
                Severity::Low,
                format!("The database runs at compatibility level {level}"),
                format!(
                    "{} supports level {latest}, but the optimizer behaves as it did at {level}. Newer levels bring adaptive joins, memory grant feedback and parameter-sensitive plans.",
                    snapshot.server.cloud.as_deref().filter(|_| snapshot.server.platform().is_some_and(|p| p.is_azure())).unwrap_or("The server")
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

    // Read where the server's options can be read (not Azure SQL Database). The threshold decides which plans go
    // parallel, so where nothing can (MAXDOP 1, one processor) it changes nothing.
    let ctfp = value(snapshot, &["cost threshold for parallelism"]).and_then(|v| v.parse::<f64>().ok());
    if ctfp.is_some_and(|v| v <= 5.0) && !serial_only(snapshot) {
        let change = server_option(&snapshot.server, "cost threshold for parallelism", "50");
        out.push(setting_finding(
            "cost-threshold-for-parallelism",
            Severity::Low,
            "Cost threshold for parallelism is still the 1990s default of 5".into(),
            "Even cheap queries go parallel, which wastes CPU on coordination and shows up as CXPACKET/CXCONSUMER waits.".into(),
            Fix {
                summary: format!("A common starting point is 50; adjust with your workload. {}", change.how),
                sql: change.sql,
                cost: None,
            },
        ));
    }
    out
}

/// Whether nothing in this database can run in parallel: its own MAXDOP is 1, or it defers (0) to a server MAXDOP of
/// 1, or the server has a single processor.
pub(crate) fn serial_only(snapshot: &Snapshot) -> bool {
    let number = |name: &str| value(snapshot, &[name]).and_then(|v| v.trim().parse::<f64>().ok());
    let db_maxdop = number("MAXDOP");
    let server_maxdop = number("max degree of parallelism");
    let cpus = snapshot.pressure.as_ref().and_then(|p| p.cpus);
    db_maxdop == Some(1.0) || (db_maxdop.is_none_or(|d| d == 0.0) && server_maxdop == Some(1.0)) || cpus == Some(1.0)
}

/// Query Store's `readonly_reason` codes (they add up when several apply).
const READ_ONLY_REASONS: [(u64, &str); 8] = [
    (1, "the database is read-only"),
    (2, "the database is in single-user mode"),
    (4, "the database is in emergency mode"),
    (8, "this is a secondary replica"),
    (65_536, "it reached its size limit (MAX_STORAGE_SIZE_MB)"),
    (131_072, "it holds as many different statements as its memory allows"),
    (262_144, "data waiting to be written to disk reached its memory limit"),
    (524_288, "the database is out of disk space"),
];

/// A read-only Query Store, by why. A read-only database, single-user or emergency mode, or a secondary replica keep
/// it read-only by design: nothing to fix in Query Store. The rest each have their own fix.
fn query_store_read_only(snapshot: &Snapshot, db: &str, reason: u64) -> Option<Finding> {
    const BY_DESIGN: u64 = 1 | 2 | 4 | 8;
    if reason == 0 || reason & BY_DESIGN != 0 {
        return None;
    }
    let reasons: Vec<String> =
        READ_ONLY_REASONS.iter().filter(|(code, _)| reason & code != 0).map(|(_, why)| why.to_string()).collect();
    let why = if reasons.is_empty() { format!("reason code {reason}") } else { format::list(&reasons) };
    let max = value(snapshot, &["query_store_options.max_storage_size_mb"]).and_then(|v| v.trim().parse::<f64>().ok());
    let (summary, sql, severity) = if reason & 65_536 != 0 {
        // Never less room than it has now.
        let room = max.map_or(2048.0, |m| (m * 2.0).max(2048.0));
        (
            "Give it more room and let it clean up old data by itself, then turn recording back on.",
            format!(
                "ALTER DATABASE {db} SET QUERY_STORE (MAX_STORAGE_SIZE_MB = {room:.0}, SIZE_BASED_CLEANUP_MODE = AUTO, OPERATION_MODE = READ_WRITE);"
            ),
            Severity::Medium,
        )
    } else if reason & 131_072 != 0 {
        (
            "Record only statements that matter (automatic capture), then turn recording back on.",
            format!("ALTER DATABASE {db} SET QUERY_STORE (QUERY_CAPTURE_MODE = AUTO, OPERATION_MODE = READ_WRITE);"),
            Severity::Medium,
        )
    } else if reason & 524_288 != 0 {
        (
            "Free or add space in the database's data files, then turn recording back on.",
            format!("ALTER DATABASE {db} SET QUERY_STORE (OPERATION_MODE = READ_WRITE);"),
            Severity::Medium,
        )
    } else {
        (
            "It usually returns to recording once the data is written; if it stays read-only, turn recording back on.",
            format!("ALTER DATABASE {db} SET QUERY_STORE (OPERATION_MODE = READ_WRITE);"),
            Severity::Low,
        )
    };
    Some(setting_finding(
        "query-store",
        severity,
        "Query Store stopped recording".into(),
        format!("Query Store is read-only because {why}. New queries and plan changes aren't being captured."),
        Fix { summary: summary.into(), sql: Some(sql), cost: None },
    ))
}

/// Time reads spent waiting on locks in the workload's window, and whether a deadlock in this database had a read in
/// it: what row versioning for read committed would remove.
fn readers_blocked(snapshot: &Snapshot) -> (f64, usize) {
    let lock_ms: f64 = app_queries(snapshot)
        .filter(|q| statement_handle(&q.text).starts_with("a read"))
        .flat_map(|q| q.waits.iter())
        .filter(|w| category_key(&w.category) == "lock")
        .map(|w| w.wait_ms)
        .sum();
    let deadlocks = snapshot
        .deadlocks
        .as_ref()
        .map(|r| {
            r.deadlocks
                .iter()
                .filter(|d| d.fixes.iter().any(|x| x.finding.as_deref() == Some("config:read-committed-snapshot")))
                .count()
        })
        .unwrap_or(0);
    (lock_ms, deadlocks)
}

fn read_committed_snapshot(snapshot: &Snapshot, db: &str) -> Finding {
    let (lock_ms, deadlocks) = readers_blocked(snapshot);
    let share = snapshot.workload.as_ref().map(|w| w.share(lock_ms)).unwrap_or(0.0);
    // Reads blocked for a real amount of time (a minute in the window, a few percent of it, and the amount rule every
    // time-based finding uses: about a minute a day), or a deadlock a read was part of: that's what the setting removes.
    // Without either it is advice for later.
    let real_amount = severity_for_time(snapshot, lock_ms, share) >= Severity::Medium;
    let blocking = (lock_ms >= 60_000.0 && share >= 0.03 && real_amount) || deadlocks > 0;
    let window = snapshot.workload.as_ref().map(|w| w.window_phrase()).unwrap_or_else(|| "this scan".into());
    let (severity, title, evidence) = if blocking {
        let mut parts = Vec::new();
        if lock_ms > 0.0 {
            parts.push(format!(
                "In {window}, reads waited {} on locks held by writes ({} of database time).",
                format::duration(lock_ms),
                format::percent(share)
            ));
        }
        if deadlocks > 0 {
            parts.push(format!(
                "{} deadlock{} in this database had a read in {}.",
                deadlocks,
                if deadlocks == 1 { "" } else { "s" },
                if deadlocks == 1 { "it" } else { "them" }
            ));
        }
        (
            Severity::Medium,
            "Readers and writers block each other (READ_COMMITTED_SNAPSHOT is off)".to_string(),
            format!(" {}", parts.join(" ")),
        )
    } else {
        (
            Severity::Low,
            "READ_COMMITTED_SNAPSHOT is off: reads would wait for writes on the same rows".to_string(),
            format!(
                " In {window} reads waited {} on locks, so it costs little now; it matters once reads and writes meet on the same rows.",
                format::duration(lock_ms)
            ),
        )
    };
    let snapshot_isolation =
        value(snapshot, &["snapshot_isolation_state_desc"]).is_some_and(|v| v.eq_ignore_ascii_case("ON"));
    let mut f = setting_finding(
        "read-committed-snapshot",
        severity,
        title,
        format!(
            "With row versioning off for read committed, a SELECT waits for any write holding a lock on the rows it reads, and vice versa.{evidence} Most OLTP applications run with it on; Azure SQL Database enables it by default."
        ),
        Fix {
            summary: "Turn on row versioning for read committed. It needs a moment with no other connections, so schedule it.".into(),
            sql: Some(format!("ALTER DATABASE {db} SET READ_COMMITTED_SNAPSHOT ON WITH ROLLBACK IMMEDIATE;")),
            cost: Some(if snapshot_isolation {
                "ALLOW_SNAPSHOT_ISOLATION is already on, so SQL Server already keeps row versions for this database: little extra overhead.".into()
            } else {
                "Row versions live in tempdb (or the database with ADR), which adds some write overhead.".into()
            }),
        },
    );
    if blocking && lock_ms > 0.0 {
        f.metrics.push(Metric::new("Reads waiting on locks", lock_ms, culprit_core::Unit::Ms));
    }
    f.caveats.push("Readers then see the last committed version instead of waiting. Code that relies on blocking reads for correctness (rare) must use locking hints.".into());
    if snapshot_isolation {
        f.caveats.push("ALLOW_SNAPSHOT_ISOLATION is on: code can already opt into SNAPSHOT isolation per transaction; RCSI makes every read-committed read use versions.".into());
    }
    f
}

/// The newest compatibility level the server offers. Azure SQL Database and Managed Instance report a product version
/// of 12.0 whatever they run, so their level comes from the service, not the version: 160 on both today.
fn latest_compat(server: &ServerInfo) -> u32 {
    if server.platform().is_some_and(|p| p.is_azure_sql()) {
        return 160;
    }
    let major: u32 = server.version.split('.').next().and_then(|v| v.parse().ok()).unwrap_or(16);
    (major * 10).clamp(100, 170)
}

fn postgres(snapshot: &Snapshot) -> Vec<Finding> {
    let mut out = Vec::new();
    let cloud = snapshot.server.is_managed();
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
        let change = setting_change(&snapshot.server, "track_io_timing", "on");
        let mut f = setting_finding(
            "track_io_timing",
            Severity::Low,
            "I/O timing isn't tracked".into(),
            "With track_io_timing off, pg_stat_statements and EXPLAIN (BUFFERS) can't tell how much of a query's time is spent waiting on disk.".into(),
            Fix {
                summary: format!("Turn it on; the overhead is negligible on modern systems. {}", change.how),
                sql: change.sql,
                cost: None,
            },
        );
        f.impact.score = 4.0;
        out.push(f);
    }
    out
}

/// Bytes of this database's InnoDB tables: those with a clustered primary key (InnoDB's), and those without a primary
/// key (InnoDB is the default engine). A table whose primary key isn't clustered is another engine's (MyISAM) and has
/// no pages in the buffer pool.
fn innodb_bytes(snapshot: &Snapshot) -> f64 {
    snapshot
        .tables
        .iter()
        .filter(|t| {
            !t.has_primary_key || snapshot.indexes_on(&t.schema, &t.name).any(|i| i.is_primary_key && i.is_clustered)
        })
        .filter_map(|t| t.total_bytes)
        .sum()
}

fn mysql(snapshot: &Snapshot) -> Vec<Finding> {
    let mut out = Vec::new();
    if let Some(f) = buffer_pool(snapshot) {
        out.push(f);
    }
    // With performance_schema off, its finding already leads with the slow query log.
    let ps_off = value(snapshot, &["performance_schema"]).is_some_and(|v| !is_on(v));
    if value(snapshot, &["slow_query_log"]).is_some_and(|v| !is_on(v)) && !ps_off {
        let output = value(snapshot, &["log_output"]).unwrap_or("FILE").trim().to_ascii_uppercase();
        let output = (!output.split(',').any(|o| o.trim() == "TABLE")).then(|| {
            if output.is_empty() || output == "NONE" { "TABLE".to_string() } else { format!("{output},TABLE") }
        });
        // A second is the threshold to suggest, never a raise: one already lower (0.2 s) logs more and stays.
        let lower_already =
            value(snapshot, &["long_query_time"]).and_then(|v| v.trim().parse::<f64>().ok()).is_some_and(|t| t <= 1.0);
        let mut changes = vec![("slow_query_log", "ON")];
        if !lower_already {
            changes.push(("long_query_time", "1"));
        }
        if let Some(output) = output.as_deref() {
            changes.push(("log_output", output));
        }
        let change = settings_change(&snapshot.server, &changes);
        let mut f = setting_finding(
            "slow_query_log",
            Severity::Low,
            "The slow query log is off".into(),
            "Without it there is no record of individual slow statements with their real parameter values, only the aggregated digests. Logged to the mysql.slow_log table, Culprit also uses those statements to plan digests that have no sample text.".into(),
            Fix {
                summary: format!("Turn it on with a threshold that matches your latency budget. {}", change.how),
                sql: change.sql,
                cost: None,
            },
        );
        f.impact.score = 4.0;
        out.push(f);
    }
    out
}

/// A buffer pool smaller than the data is a problem only when reads miss it: a large, mostly cold schema served
/// from memory 99.9% of the time needs nothing. Graded on the hit rate and how much it reads from disk, since the
/// server started (at least an hour of it). The server's memory isn't known, so no size is proposed.
fn buffer_pool(snapshot: &Snapshot) -> Option<Finding> {
    let pool =
        value(snapshot, &["innodb_buffer_pool_size"]).and_then(|v| v.parse::<f64>().ok()).filter(|p| *p > 0.0)?;
    let data = innodb_bytes(snapshot);
    if data < 1024.0 * 1024.0 * 1024.0 || pool >= data * 0.5 {
        return None;
    }
    counter_days_for_rate(snapshot)?;
    let pressure = snapshot.pressure.as_ref()?;
    let hit = pressure.signal("buffer-pool-hit").map(|s| s.value)?;
    if hit >= 99.0 {
        return None;
    }
    let reads = pressure.signal("buffer-pool-reads").map(|s| s.value);
    // Medium needs both a poor hit rate and real disk reads behind it (50 pages a second, about 0.8 MB/s).
    let severity = if hit < 95.0 && reads.is_some_and(|r| r >= 50.0) { Severity::Medium } else { Severity::Low };
    let name = snapshot.server.name();
    let mut f = setting_finding(
        "innodb_buffer_pool_size",
        severity,
        format!("The InnoDB buffer pool holds less than half of the data, and {} of reads miss it", format::percent((100.0 - hit) / 100.0)),
        format!(
            "innodb_buffer_pool_size is {}, and this database has {} of InnoDB tables and indexes. Since {name} started, {} of page reads were served from memory{}; the rest went to disk.",
            format::bytes(pool),
            format::bytes(data),
            format::percent(hit / 100.0),
            reads.map(|r| format!(" (about {} pages a second from disk)", format::count(r))).unwrap_or_default()
        ),
        Fix {
            summary: match snapshot.server.platform() {
                Some(platform) => format!(
                    "Give the buffer pool more of the instance's memory{} ({} sizes it from the instance's memory), or move to an instance with more memory. Leave room for connections and the operating system.",
                    platform.settings_control().map(|c| format!(" in {c}")).unwrap_or_default(),
                    platform.label(snapshot.server.engine)
                ),
                None => "Give the buffer pool most of the server's memory (often 60–75% on a dedicated host, leaving room for connections and the operating system); it can be resized online with SET GLOBAL innodb_buffer_pool_size.".into(),
            },
            sql: None,
            cost: Some("Uses that much RAM; a pool larger than the free memory makes the server swap or be killed.".into()),
        },
    );
    f.metrics.push(Metric::new("Reads served from memory", hit / 100.0, culprit_core::Unit::Ratio));
    if let Some(r) = reads {
        f.metrics.push(Metric::new("Pages read from disk per second", r, culprit_core::Unit::Count));
    }
    f.confidence = Confidence::Medium;
    Some(f)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::waits::snapshot_with;
    use culprit_core::{Pressure, Setting, Signal, SignalScope, SignalUnit, WaitStat};

    fn set(s: &mut Snapshot, name: &str, value: &str) {
        s.settings.retain(|x| !x.name.eq_ignore_ascii_case(name));
        s.settings.push(Setting {
            name: name.into(),
            value: value.into(),
            default_value: None,
            scope: "server".into(),
        });
    }

    fn found<'a>(findings: &'a [Finding], id: &str) -> Option<&'a Finding> {
        findings.iter().find(|f| f.id == id)
    }

    fn fixture(name: &str) -> Snapshot {
        let text =
            std::fs::read_to_string(format!("{}/../../fixtures/{name}", env!("CARGO_MANIFEST_DIR"))).expect("fixture");
        serde_json::from_str(&text).expect("snapshot")
    }

    // C5: the demo database has RCSI off and 15 ms of lock waits in 4.6 hours: advice for later, not Medium.
    #[test]
    fn rcsi_is_medium_only_with_reads_blocked() {
        let s = fixture("mssql-demo.json");
        let f = found(&mssql(&s), "config:read-committed-snapshot").cloned().expect("finding");
        assert_eq!(f.severity, Severity::Low);
        assert!(f.title.contains("reads would wait for writes"), "{}", f.title);

        // A read that waited minutes on locks is what RCSI removes.
        let mut s = fixture("mssql-demo.json");
        let w = s.workload.as_mut().unwrap();
        let read = w.queries.iter_mut().find(|q| q.text.starts_with("SELECT")).unwrap();
        read.waits.push(WaitStat { category: "Lock".into(), wait_type: None, wait_ms: 300_000.0, tasks: None });
        w.total_time_ms += 300_000.0;
        let f = found(&mssql(&s), "config:read-committed-snapshot").cloned().unwrap();
        assert_eq!(f.severity, Severity::Medium);
        assert!(f.summary.contains("reads waited 5 min on locks"), "{}", f.summary);
        // With snapshot isolation already allowed, versions are kept anyway.
        set(&mut s, "snapshot_isolation_state_desc", "ON");
        let f = found(&mssql(&s), "config:read-committed-snapshot").cloned().unwrap();
        assert!(f.fix.unwrap().cost.unwrap().contains("ALLOW_SNAPSHOT_ISOLATION is already on"));
    }

    // C6: the threshold is moot where nothing goes parallel; RDS words it as its parameter group.
    #[test]
    fn cost_threshold_only_where_plans_can_go_parallel() {
        let mut s = fixture("mssql-demo.json");
        let f = found(&mssql(&s), "config:cost-threshold-for-parallelism").cloned().unwrap();
        assert!(f.fix.as_ref().unwrap().sql.as_deref().unwrap().contains("show advanced options"));
        set(&mut s, "MAXDOP", "1");
        assert!(found(&mssql(&s), "config:cost-threshold-for-parallelism").is_none());
        set(&mut s, "MAXDOP", "0");
        set(&mut s, "max degree of parallelism", "1");
        assert!(found(&mssql(&s), "config:cost-threshold-for-parallelism").is_none());
        set(&mut s, "max degree of parallelism", "0");
        s.server.cloud = Some("Amazon RDS".into());
        let fix = found(&mssql(&s), "config:cost-threshold-for-parallelism").cloned().unwrap().fix.unwrap();
        assert!(fix.sql.is_none() && fix.summary.contains("DB parameter group"), "{}", fix.summary);
    }

    // C20: Azure SQL reports version 12.0 whatever it runs; the level it offers comes from the service.
    #[test]
    fn azure_compatibility_level_is_not_read_from_the_version() {
        let mut s = fixture("mssql-demo.json");
        s.server.cloud = Some("Azure SQL Database".into());
        s.server.version = "12.0.2000.8".into();
        set(&mut s, "compatibility_level", "150");
        assert!(found(&mssql(&s), "config:compatibility-level").is_none(), "150 is one step behind 160");
        set(&mut s, "compatibility_level", "130");
        let f = found(&mssql(&s), "config:compatibility-level").cloned().unwrap();
        assert!(f.fix.unwrap().sql.unwrap().contains("COMPATIBILITY_LEVEL = 160"));
        assert!(f.summary.starts_with("Azure SQL Database supports level 160"), "{}", f.summary);
    }

    // C20: some products need automatic statistics creation off.
    #[test]
    fn auto_create_statistics_off_is_medium_with_the_vendor_caveat() {
        let mut s = fixture("mssql-demo.json");
        set(&mut s, "is_auto_create_stats_on", "0");
        let f = found(&mssql(&s), "config:auto_create_statistics").cloned().unwrap();
        assert_eq!(f.severity, Severity::Medium);
        assert!(f.caveats[0].contains("SharePoint"));
    }

    // C9: read-only by design (a secondary replica) isn't a size problem; the size limit gets more room, never less.
    #[test]
    fn query_store_read_only_says_why() {
        let mut s = fixture("mssql-demo.json");
        set(&mut s, "query_store_options.actual_state_desc", "READ_ONLY");
        set(&mut s, "query_store_options.readonly_reason", "8");
        assert!(found(&mssql(&s), "config:query-store").is_none());
        set(&mut s, "query_store_options.readonly_reason", "65536");
        set(&mut s, "query_store_options.max_storage_size_mb", "4096");
        let f = found(&mssql(&s), "config:query-store").cloned().unwrap();
        assert!(f.summary.contains("reached its size limit"), "{}", f.summary);
        assert!(f.fix.unwrap().sql.unwrap().contains("MAX_STORAGE_SIZE_MB = 8192"));
        set(&mut s, "query_store_options.readonly_reason", "131072");
        let f = found(&mssql(&s), "config:query-store").cloned().unwrap();
        assert!(f.fix.unwrap().sql.unwrap().contains("QUERY_CAPTURE_MODE = AUTO"));
    }

    // C7: managed services refuse ALTER SYSTEM and SET GLOBAL; the advice names their control instead.
    #[test]
    fn settings_on_managed_services_name_the_platform_control() {
        let mut pg = snapshot_with(Engine::Postgres);
        set(&mut pg, "track_io_timing", "off");
        let fix = found(&postgres(&pg), "config:track_io_timing").cloned().unwrap().fix.unwrap();
        assert!(fix.sql.unwrap().starts_with("ALTER SYSTEM SET track_io_timing"));
        pg.server.cloud = Some("Amazon RDS".into());
        let fix = found(&postgres(&pg), "config:track_io_timing").cloned().unwrap().fix.unwrap();
        assert!(fix.sql.is_none() && fix.summary.contains("DB parameter group"), "{}", fix.summary);

        let mut my = snapshot_with(Engine::Mysql);
        set(&mut my, "slow_query_log", "OFF");
        set(&mut my, "log_output", "FILE");
        let fix = found(&mysql(&my), "config:slow_query_log").cloned().unwrap().fix.unwrap();
        assert!(fix.sql.as_deref().unwrap().contains("slow_query_log = 'ON'"), "{:?}", fix.sql);
        my.server.cloud = Some("Google Cloud SQL".into());
        let fix = found(&mysql(&my), "config:slow_query_log").cloned().unwrap().fix.unwrap();
        assert!(fix.sql.is_none());
        assert!(
            fix.summary.contains("Set slow_query_log to ON, long_query_time to 1 and log_output to FILE,TABLE in the instance's database flags"),
            "{}",
            fix.summary
        );
    }

    fn buffer_pool_snapshot(hit: f64, reads: f64) -> Snapshot {
        let mut s = snapshot_with(Engine::Mysql);
        for t in &mut s.tables {
            t.total_bytes = Some(40.0 * 1024.0 * 1024.0 * 1024.0);
        }
        set(&mut s, "innodb_buffer_pool_size", &(8u64 * 1024 * 1024 * 1024).to_string());
        s.pressure = Some(Pressure {
            uptime_hours: Some(72.0),
            signals: vec![
                Signal::new("buffer-pool-hit", "hit", hit, SignalUnit::Percent, SignalScope::SinceStartup),
                Signal::new("buffer-pool-reads", "reads", reads, SignalUnit::PerSecond, SignalScope::SinceStartup),
            ],
            ..Pressure::default()
        });
        s
    }

    // C8: a large, mostly cold schema served from memory 99.9% of the time needs no bigger pool; when one is needed,
    // no size is proposed without knowing the server's memory.
    #[test]
    fn buffer_pool_is_graded_on_misses_not_data_size() {
        assert!(buffer_pool(&buffer_pool_snapshot(99.9, 3.0)).is_none());
        let f = buffer_pool(&buffer_pool_snapshot(90.0, 400.0)).unwrap();
        assert_eq!(f.severity, Severity::Medium);
        assert!(f.fix.as_ref().unwrap().sql.is_none());
        assert_eq!(buffer_pool(&buffer_pool_snapshot(97.0, 400.0)).unwrap().severity, Severity::Low);
        // MyISAM tables (a primary key that isn't clustered) have no pages in the buffer pool.
        let mut s = buffer_pool_snapshot(90.0, 400.0);
        for i in &mut s.indexes {
            i.is_clustered = false;
        }
        for t in &mut s.tables {
            t.has_primary_key = true;
        }
        assert!(buffer_pool(&s).is_none());
    }
}
