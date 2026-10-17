//! Pressure signals and the "right now" wait sample: performance counters, wait totals, memory grants and the oldest open
//! transaction. Read-only and tagged; a read that fails leaves its signal out.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use culprit_core::{
    CleanupHolder, Coverage, Engine, GrantStat, OpenTransaction, Pressure, Signal, SignalScope, SignalUnit, WaitSample,
    WaitStat,
};

use crate::conn::{MsClient, query};
use crate::probe::Probe;
use crate::schema::{BENIGN_WAITS, wait_type_category};
use crate::sources;

const COUNTERS_SQL: &str = r#"
SELECT RTRIM(counter_name) AS counter, cntr_value AS value
FROM sys.dm_os_performance_counters
WHERE (object_name LIKE N'%SQL Statistics%'
       AND counter_name IN (N'Batch Requests/sec', N'SQL Compilations/sec', N'SQL Re-Compilations/sec'))
   OR (object_name LIKE N'%Buffer Manager%' AND counter_name = N'Page life expectancy')
"#;

/// Azure SQL Database's own figures for this database: 15-second samples of the share of its tier's limits it used,
/// kept for about an hour. The performance counters describe the server process behind the database, shared with every
/// database in its elastic pool, so on Azure these say whether this database is short of storage I/O or memory.
const RESOURCE_STATS_SQL: &str = "SELECT COUNT(*) AS samples, AVG(avg_data_io_percent) AS data_io,
  MAX(avg_data_io_percent) AS data_io_peak, AVG(avg_memory_usage_percent) AS memory FROM sys.dm_db_resource_stats";

/// Requests in this database waiting for a memory grant right now. The view lists the whole instance's grants; other
/// databases' requests, and Culprit's own sessions, aren't this database's queue.
const PENDING_GRANTS_SQL: &str = "SELECT COUNT(*) AS pending, MAX(g.wait_time_ms) AS longest_ms
FROM sys.dm_exec_query_memory_grants g
JOIN sys.dm_exec_sessions s ON s.session_id = g.session_id
LEFT JOIN sys.dm_exec_requests r ON r.session_id = g.session_id AND r.request_id = g.request_id
WHERE g.grant_time IS NULL AND g.session_id <> @@SPID AND ISNULL(s.program_name, N'') <> N'culprit'
  AND COALESCE(r.database_id, s.database_id) = DB_ID()";

/// The processors SQL Server schedules on, and how many NUMA nodes they are split into: what MAXDOP is sized against.
const CPUS_SQL: &str = "SELECT i.cpu_count,
  (SELECT COUNT(DISTINCT parent_node_id) FROM sys.dm_os_schedulers WHERE status = N'VISIBLE ONLINE') AS numa_nodes
FROM sys.dm_os_sys_info i";

/// Requests queued for a worker right now. System tasks (no session) wait on THREADPOOL briefly all the time, and in
/// Azure SQL Database the view shows the whole instance, so only sessions' requests waiting a measurable time count.
const THREADPOOL_NOW_SQL: &str = "SELECT COUNT(*) AS waiting FROM sys.dm_os_waiting_tasks w
JOIN sys.dm_exec_sessions s ON s.session_id = w.session_id
LEFT JOIN sys.dm_exec_requests r ON r.session_id = w.session_id
WHERE w.wait_type = N'THREADPOOL' AND w.wait_duration_ms >= 100
  AND COALESCE(r.database_id, s.database_id) = DB_ID()
  AND s.session_id <> @@SPID AND ISNULL(s.program_name, N'') <> N'culprit'";

const GRANTS_SQL: &str = r#"
SELECT TOP (30) CONVERT(varchar(20), qs.query_hash, 1) AS query_hash,
  SUM(qs.execution_count) AS executions,
  SUM(qs.total_grant_kb) * 1.0 / NULLIF(SUM(qs.execution_count), 0) AS avg_granted_kb,
  SUM(qs.total_used_grant_kb) * 1.0 / NULLIF(SUM(qs.execution_count), 0) AS avg_used_kb,
  MAX(qs.max_grant_kb) AS max_granted_kb,
  SUM(qs.last_grant_kb) * 1.0 / COUNT(*) AS last_granted_kb,
  SUM(qs.last_used_grant_kb) * 1.0 / COUNT(*) AS last_used_kb
FROM sys.dm_exec_query_stats qs
CROSS APPLY sys.dm_exec_plan_attributes(qs.plan_handle) pa
WHERE pa.attribute = 'dbid' AND CAST(pa.value AS int) = DB_ID() AND qs.total_grant_kb > 0
GROUP BY qs.query_hash
ORDER BY SUM(qs.total_grant_kb) - SUM(qs.total_used_grant_kb) DESC
"#;

const VERSION_STORE_SQL: &str =
    "SELECT SUM(reserved_space_kb) AS kb FROM sys.dm_tran_version_store_space_usage WHERE database_id = DB_ID()";

/// The oldest transaction that holds this database's cleanup: one that changed data here (it has a first log record,
/// so the log can't be reused past it and its locks stay held) or one that reads or makes row versions (the version
/// store keeps every version since it started). A read-only transaction without versions, or one in another database,
/// holds nothing here.
const OLDEST_TRANSACTION_SQL: &str = r#"
SELECT TOP (1) CAST(s.session_id AS varchar(10)) AS session_id,
  DATEDIFF(second, at.transaction_begin_time, GETDATE()) AS seconds,
  s.program_name, s.status,
  CASE WHEN snap.transaction_id IS NULL THEN 0 ELSE 1 END AS is_snapshot,
  CASE WHEN dt.database_transaction_begin_lsn IS NULL THEN 0 ELSE 1 END AS wrote
FROM sys.dm_tran_database_transactions dt
JOIN sys.dm_tran_session_transactions st ON st.transaction_id = dt.transaction_id
JOIN sys.dm_tran_active_transactions at ON at.transaction_id = dt.transaction_id
JOIN sys.dm_exec_sessions s ON s.session_id = st.session_id
LEFT JOIN sys.dm_tran_active_snapshot_database_transactions snap ON snap.transaction_id = dt.transaction_id
WHERE dt.database_id = DB_ID() AND s.is_user_process = 1 AND s.session_id <> @@SPID
  AND ISNULL(s.program_name, N'') <> N'culprit'
  AND (dt.database_transaction_begin_lsn IS NOT NULL OR snap.transaction_id IS NOT NULL)
ORDER BY at.transaction_begin_time
"#;

fn waits_sql(azure_sql_database: bool, only: Option<&[&str]>) -> String {
    let view = if azure_sql_database { "sys.dm_db_wait_stats" } else { "sys.dm_os_wait_stats" };
    let filter = match only {
        Some(types) => {
            let list: Vec<String> = types.iter().map(|w| format!("N'{w}'")).collect();
            format!("wait_type IN ({})", list.join(", "))
        }
        None => {
            let excluded: Vec<String> = BENIGN_WAITS.iter().map(|w| format!("N'{w}'")).collect();
            format!(
                "wait_time_ms > 0 AND wait_type NOT IN ({}) AND wait_type NOT LIKE N'SLEEP[_]%'",
                excluded.join(", ")
            )
        }
    };
    format!("SELECT wait_type, wait_time_ms, signal_wait_time_ms, waiting_tasks_count FROM {view} WHERE {filter}")
}

/// Wait type → (wait ms, signal wait ms, tasks).
type WaitTotals = HashMap<String, (f64, f64, f64)>;

async fn wait_totals(client: &mut MsClient, azure: bool, only: Option<&[&str]>) -> tiberius::Result<WaitTotals> {
    Ok(query(client, &waits_sql(azure, only))
        .await?
        .iter()
        .map(|r| {
            (
                r.string("wait_type"),
                (
                    r.f64_or_zero("wait_time_ms"),
                    r.f64_or_zero("signal_wait_time_ms"),
                    r.f64_or_zero("waiting_tasks_count"),
                ),
            )
        })
        .collect())
}

async fn counters(client: &mut MsClient) -> tiberius::Result<HashMap<String, f64>> {
    Ok(query(client, COUNTERS_SQL).await?.iter().map(|r| (r.string("counter"), r.f64_or_zero("value"))).collect())
}

fn uptime_hours(probe: &Probe) -> Option<f64> {
    let started = chrono::DateTime::parse_from_rfc3339(probe.started_at.as_deref()?).ok()?;
    let hours = (chrono::Utc::now() - started.with_timezone(&chrono::Utc)).num_seconds() as f64 / 3600.0;
    (hours > 0.0).then_some(hours)
}

/// What the server's performance counters say. On Azure SQL Database they describe the server process behind the
/// database, which an elastic pool shares among all its databases: shown for context, labelled as the whole server's,
/// and never graded, since another database's scans would turn this one red.
fn counter_signals(c: &HashMap<String, f64>, uptime: Option<f64>, azure: bool) -> Vec<Signal> {
    const SHARED: &str = "covers every database on the server behind this one (its elastic pool's, if it has one), so it isn't graded here";
    let label = |name: &str| if azure { format!("{name} (whole server)") } else { name.to_string() };
    let mut signals = Vec::new();
    let batches = c.get("Batch Requests/sec").copied().unwrap_or(0.0);
    let compilations = c.get("SQL Compilations/sec").copied().unwrap_or(0.0);
    // SQL Compilations/sec counts recompiles too; they have other causes (changed statistics or schema, temporary
    // tables, OPTION (RECOMPILE)) and parameters don't remove them.
    let recompiles = c.get("SQL Re-Compilations/sec").copied().unwrap_or(0.0).min(compilations);
    let compiles = compilations - recompiles;
    if let Some(hours) = uptime.filter(|_| batches > 0.0) {
        let rate = Signal::new(
            "batch-requests",
            &label("Batch requests"),
            batches / (hours * 3600.0),
            SignalUnit::PerSecond,
            SignalScope::SinceStartup,
        );
        signals.push(if azure { Signal { healthy: Some(SHARED.into()), ..rate } } else { rate });
    }
    if batches > 0.0 {
        let ratio = Signal::new(
            "compiles-per-batch",
            &label("Plans compiled"),
            compiles / batches * 100.0,
            SignalUnit::Percent,
            SignalScope::SinceStartup,
        );
        // A handful of batches compiling says nothing about plan reuse; grade the ratio only under real load.
        let busy = uptime.is_none_or(|h| batches / (h * 3600.0) >= 1.0);
        signals.push(if azure {
            Signal { healthy: Some(SHARED.into()), ..ratio }
        } else if busy {
            ratio.graded("under 10% of batches is healthy", 10.0, 25.0, true)
        } else {
            Signal { healthy: Some("too few batches to judge; under 10% is healthy under load".into()), ..ratio }
        });
        let recompiled = Signal::new(
            "recompiles-per-batch",
            &label("Plans recompiled"),
            recompiles / batches * 100.0,
            SignalUnit::Percent,
            SignalScope::SinceStartup,
        );
        signals.push(if azure { Signal { healthy: Some(SHARED.into()), ..recompiled } } else { recompiled });
    }
    if let Some(ple) = c.get("Page life expectancy") {
        let life =
            Signal::new("page-life", &label("Page life expectancy"), *ple, SignalUnit::Seconds, SignalScope::Now);
        signals.push(if azure {
            Signal { healthy: Some(SHARED.into()), ..life }
        } else {
            life.graded(
                "how long a page stays in memory once read: five minutes or more is healthy; under a minute means \
                 pages leave memory soon after they're read, often because of large scans",
                300.0,
                60.0,
                false,
            )
        });
    }
    signals
}

/// Azure SQL Database's own figures for this database over the last hour: storage I/O against its tier's limit,
/// graded, and memory in use, for context (a full cache is normal).
fn resource_signals(data_io: f64, data_io_peak: f64, memory: f64) -> Vec<Signal> {
    vec![
        Signal {
            healthy: Some(format!(
                "Average use of this database's I/O limit; peak {data_io_peak:.1}%. The individual resource samples establish whether pressure was sustained."
            )),
            ..Signal::new("data-io", "Storage I/O of its limit", data_io, SignalUnit::Percent, SignalScope::LastHour)
        },
        Signal {
            healthy: Some("of its memory limit; a full cache is normal, so it isn't graded".into()),
            ..Signal::new("memory-use", "Memory in use", memory, SignalUnit::Percent, SignalScope::LastHour)
        },
    ]
}

/// Signals, grants and the oldest open transaction. Each read is independent, through `sources::read`: a failure is a
/// note in the coverage returned.
pub(crate) async fn collect(client: &mut MsClient, probe: &Probe) -> (Pressure, Coverage) {
    let azure = probe.is_azure_sql_database();
    let uptime = uptime_hours(probe);
    let mut p = Pressure { uptime_hours: uptime, ..Pressure::default() };
    let mut coverage = Coverage::default();

    if let Some(c) =
        sources::read(&mut coverage, &sources::PERFORMANCE_COUNTERS, counters(client).await, || probe.grant_hint())
    {
        p.signals.extend(counter_signals(&c, uptime, azure));
    }
    if azure
        && let Some(rows) =
            sources::read(&mut coverage, &sources::RESOURCE_USE, query(client, RESOURCE_STATS_SQL).await, || {
                probe.grant_hint()
            })
        && let Some(r) = rows.first().filter(|r| r.f64_or_zero("samples") > 0.0)
    {
        p.signals.extend(resource_signals(
            r.f64_or_zero("data_io"),
            r.f64_or_zero("data_io_peak"),
            r.f64_or_zero("memory"),
        ));
    }
    if let Some(rows) =
        sources::read(&mut coverage, &sources::PENDING_GRANTS, query(client, PENDING_GRANTS_SQL).await, || {
            probe.grant_hint()
        })
    {
        let pending = rows.first().map(|r| r.f64_or_zero("pending")).unwrap_or(0.0);
        p.signals.push(
            Signal::new("memory-grants-pending", "Waiting for memory", pending, SignalUnit::Count, SignalScope::Now)
                .graded("0 is healthy", 1.0, 5.0, true),
        );
    }
    if let Some(totals) =
        sources::read(&mut coverage, &sources::WAIT_TOTALS, wait_totals(client, azure, None).await, || {
            probe.grant_hint()
        })
    {
        let (wait, signal) = totals.values().fold((0.0, 0.0), |(w, s), (tw, ts, _)| (w + tw, s + ts));
        if wait > 0.0 {
            p.signals.push(scheduling_share(signal, wait, SignalScope::SinceStartup));
        }
        let threadpool = totals.get("THREADPOOL").map(|t| t.0).unwrap_or(0.0);
        // Totals since the start: one bad hour days ago and a steady trickle average to the same rate, so checks
        // say how much and since when instead of an hourly rate.
        p.signals.push(Signal::new(
            "threadpool-wait-ms",
            if azure { "Waiting for workers since start" } else { "Waiting for workers since start (whole instance)" },
            threadpool,
            SignalUnit::Milliseconds,
            SignalScope::SinceStartup,
        ));
        // Memory grants alone; the Memory category also holds allocation waits (CMEMTHREAD) and compile memory.
        let grants = totals.get("RESOURCE_SEMAPHORE").map(|t| t.0).unwrap_or(0.0);
        p.signals.push(Signal::new(
            "grant-wait-ms",
            if azure {
                "Waiting for memory grants since start"
            } else {
                "Waiting for memory grants since start (whole instance)"
            },
            grants,
            SignalUnit::Milliseconds,
            SignalScope::SinceStartup,
        ));
        if let Some(hours) = uptime {
            p.signals.push(
                Signal::new(
                    "threadpool-waits",
                    if hours >= 1.0 { "Waiting for workers, per hour" } else { "Waiting for workers since start" },
                    // Under an hour of uptime, report the total rather than extrapolate an hourly rate.
                    threadpool / hours.max(1.0),
                    SignalUnit::Milliseconds,
                    SignalScope::SinceStartup,
                )
                .graded("under a second an hour is healthy", 1_000.0, 60_000.0, true),
            );
        }
    }
    if let Ok(rows) = query(client, THREADPOOL_NOW_SQL).await {
        let waiting = rows.first().map(|r| r.f64_or_zero("waiting")).unwrap_or(0.0);
        p.signals.push(
            Signal::new(
                "threadpool-waiting",
                "Requests in this database without a worker",
                waiting,
                SignalUnit::Count,
                SignalScope::Now,
            )
            .graded(
                "One observation; sample again to establish whether the queue persists",
                1.0,
                f64::INFINITY,
                true,
            ),
        );
    }
    if let Ok(rows) = query(client, CPUS_SQL).await
        && let Some(r) = rows.first()
    {
        p.cpus = r.f64("cpu_count").filter(|n| *n > 0.0);
        p.numa_nodes = r.f64("numa_nodes").filter(|n| *n > 0.0);
    }
    if let Some(rows) =
        sources::read(&mut coverage, &sources::GRANTS, query(client, GRANTS_SQL).await, || probe.grant_hint())
    {
        p.grants = rows
            .iter()
            .map(|r| GrantStat {
                query_hash: r.string("query_hash"),
                executions: r.f64_or_zero("executions"),
                avg_granted_kb: r.f64_or_zero("avg_granted_kb"),
                avg_used_kb: r.f64_or_zero("avg_used_kb"),
                max_granted_kb: r.f64_or_zero("max_granted_kb"),
                last_granted_kb: r.f64("last_granted_kb"),
                last_used_kb: r.f64("last_used_kb"),
            })
            .collect();
    }
    let version_store = match query(client, VERSION_STORE_SQL).await {
        Ok(rows) => rows.first().and_then(|r| r.f64("kb")).map(|kb| kb * 1024.0),
        Err(_) => None,
    };
    if let Some(bytes) = version_store {
        p.signals.push(
            Signal::new("version-store", "Version store", bytes, SignalUnit::Bytes, SignalScope::Now).graded(
                "small, and shrinking when transactions end",
                1024.0 * 1024.0 * 1024.0,
                10.0 * 1024.0 * 1024.0 * 1024.0,
                true,
            ),
        );
    }
    if let Some(rows) =
        sources::read(&mut coverage, &sources::OPEN_TRANSACTIONS, query(client, OLDEST_TRANSACTION_SQL).await, || {
            probe.grant_hint()
        })
    {
        p.oldest_transaction = rows.first().map(|r| OpenTransaction {
            session: r.string("session_id"),
            seconds: r.f64_or_zero("seconds"),
            program: r.str("program_name").filter(|s| !s.trim().is_empty()),
            state: Some(match (r.str("status").as_deref(), r.f64_or_zero("is_snapshot") > 0.0) {
                (Some("sleeping"), _) => "idle in transaction".into(),
                (_, true) => "snapshot".into(),
                _ => "running".into(),
            }),
            held_bytes: version_store.filter(|_| r.f64_or_zero("is_snapshot") > 0.0),
            held_versions: None,
            holder: Some(CleanupHolder::Session),
            xid_age: None,
            wrote: Some(r.f64_or_zero("wrote") > 0.0),
            database: None,
        });
        if let Some(t) = &p.oldest_transaction {
            p.signals.push(
                Signal::new(
                    "oldest-transaction",
                    "Oldest open transaction",
                    t.seconds,
                    SignalUnit::Seconds,
                    SignalScope::Now,
                )
                .graded("seconds or minutes, not hours", 600.0, 1800.0, true),
            );
        }
    }
    (p, coverage)
}

/// Two readings of wait totals and counters `seconds` apart; the difference is what happened in between.
pub(crate) async fn sample(client: &mut MsClient, azure: bool, seconds: u32) -> tiberius::Result<WaitSample> {
    let started = Instant::now();
    let taken_at = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let first = wait_totals(client, azure, None).await?;
    let counters_before = counters(client).await.unwrap_or_default();
    tokio::time::sleep(Duration::from_secs(u64::from(seconds))).await;
    let second = wait_totals(client, azure, None).await?;
    let counters_after = counters(client).await.unwrap_or_default();
    let elapsed = started.elapsed().as_secs_f64().max(1.0);

    let mut by_category: HashMap<&'static str, (f64, f64, String, f64)> = HashMap::new();
    let (mut wait_total, mut signal_total) = (0.0, 0.0);
    for (wait_type, (ms, signal, tasks)) in &second {
        let (ms0, signal0, tasks0) = first.get(wait_type).copied().unwrap_or((0.0, 0.0, 0.0));
        let delta = ms - ms0;
        if delta <= 0.0 {
            continue;
        }
        wait_total += delta;
        signal_total += (signal - signal0).max(0.0);
        let entry = by_category.entry(wait_type_category(wait_type)).or_default();
        entry.0 += delta;
        entry.1 += (tasks - tasks0).max(0.0);
        if delta > entry.3 {
            entry.2 = wait_type.clone();
            entry.3 = delta;
        }
    }
    let mut categories: Vec<WaitStat> = by_category
        .into_iter()
        .map(|(category, (ms, tasks, top, _))| WaitStat {
            category: category.into(),
            wait_type: Some(top),
            wait_ms: ms,
            tasks: Some(tasks),
        })
        .collect();
    categories.sort_by(|a, b| b.wait_ms.total_cmp(&a.wait_ms));

    let rate = |name: &str| {
        let (a, b) = (counters_before.get(name)?, counters_after.get(name)?);
        (b >= a).then_some((b - a) / elapsed)
    };
    let mut signals = Vec::new();
    if let Some(batches) = rate("Batch Requests/sec") {
        signals.push(Signal::new(
            "batch-requests",
            "Batch requests",
            batches,
            SignalUnit::PerSecond,
            SignalScope::Sampled,
        ));
        // Recompiles are counted in compilations too; plan reuse is about the first compile.
        let recompiles = rate("SQL Re-Compilations/sec").unwrap_or(0.0);
        if let Some(compiles) =
            rate("SQL Compilations/sec").map(|c| (c - recompiles).max(0.0)).filter(|_| batches > 0.0)
        {
            signals.push(Signal::new(
                "compiles-per-batch",
                "Plans compiled (whole instance)",
                compiles / batches * 100.0,
                SignalUnit::Percent,
                SignalScope::Sampled,
            ));
        }
    }
    if let Some(recompiles) = rate("SQL Re-Compilations/sec") {
        signals.push(Signal::new("recompiles", "Recompiles", recompiles, SignalUnit::PerSecond, SignalScope::Sampled));
    }
    if wait_total > 0.0 {
        signals.push(scheduling_share(signal_total, wait_total, SignalScope::Sampled));
    }
    if azure {
        signals.retain(|s| !matches!(s.key.as_str(), "batch-requests" | "compiles-per-batch" | "recompiles"));
    }
    let view = if azure { "sys.dm_db_wait_stats" } else { "sys.dm_os_wait_stats" };
    Ok(WaitSample {
        engine: Engine::Mssql,
        taken_at,
        seconds: elapsed,
        method: format!("Difference of two readings of {view} {seconds} s apart"),
        total_ms: wait_total,
        categories,
        signals,
        active_sessions: None,
        notes: if azure {
            Vec::new()
        } else {
            vec!["These wait counters cover the whole SQL Server instance, including other databases; they do not establish pressure in the selected database.".into()]
        },
    })
}

/// A wait composition, never CPU utilization or a CPU-pressure verdict, even in a short live sample.
fn scheduling_share(signal: f64, wait: f64, scope: SignalScope) -> Signal {
    Signal {
        healthy: Some("Share of recorded waiting spent ready to run before being scheduled. This is not CPU utilization; the ratio alone does not establish CPU pressure.".into()),
        ..Signal::new("signal-wait", "Scheduling share of recorded waits", signal / wait * 100.0, SignalUnit::Percent, scope)
    }
}

#[cfg(test)]
mod tests {
    use culprit_core::SignalStatus;

    use super::*;

    fn counters(ple: f64) -> HashMap<String, f64> {
        HashMap::from([
            ("Batch Requests/sec".to_string(), 7_200_000.0),
            ("SQL Compilations/sec".to_string(), 3_600_000.0),
            ("Page life expectancy".to_string(), ple),
        ])
    }

    fn signal<'a>(signals: &'a [Signal], key: &str) -> &'a Signal {
        signals.iter().find(|s| s.key == key).expect(key)
    }

    // On a server of its own, a short page life is this server's, and graded.
    #[test]
    fn a_short_page_life_is_graded_on_a_server_of_its_own() {
        let signals = counter_signals(&counters(44.0), Some(100.0), false);
        let life = signal(&signals, "page-life");
        assert_eq!((life.label.as_str(), life.status), ("Page life expectancy", SignalStatus::Bad));
        assert!(life.healthy.as_deref().unwrap().starts_with("how long a page stays in memory once read"));
        assert_eq!(signal(&signals, "compiles-per-batch").status, SignalStatus::Bad);
    }

    // On Azure SQL Database the counters are the server process's, shared by the elastic pool: context, never graded.
    #[test]
    fn azure_counters_are_the_whole_servers_and_never_graded() {
        let signals = counter_signals(&counters(44.0), Some(100.0), true);
        for key in ["page-life", "compiles-per-batch", "batch-requests"] {
            let s = signal(&signals, key);
            assert_eq!(s.status, SignalStatus::Info, "{key}");
            assert!(s.label.ends_with("(whole server)"), "{}", s.label);
            assert!(s.healthy.as_deref().unwrap().contains("every database on the server"), "{key}");
        }
    }

    // SQL Compilations/sec counts recompiles too: plan reuse is judged on first compiles only.
    #[test]
    fn recompiles_are_not_counted_as_new_plans() {
        let mut c = counters(400.0);
        c.insert("SQL Re-Compilations/sec".into(), 1_800_000.0);
        let signals = counter_signals(&c, Some(100.0), false);
        assert_eq!(signal(&signals, "compiles-per-batch").value, 25.0);
        assert_eq!(signal(&signals, "recompiles-per-batch").value, 25.0);
    }

    #[test]
    fn aggregate_resource_counters_need_individual_samples_before_a_pressure_verdict() {
        let calm = resource_signals(0.0, 3.0, 0.4);
        assert_eq!(signal(&calm, "data-io").status, SignalStatus::Info);
        assert!(signal(&calm, "data-io").healthy.as_deref().unwrap().contains("peak 3.0%"));
        assert_eq!(signal(&calm, "memory-use").status, SignalStatus::Info);
        assert_eq!(signal(&resource_signals(85.0, 100.0, 90.0), "data-io").status, SignalStatus::Info);
    }

    #[test]
    fn scheduling_share_alone_never_claims_cpu_pressure() {
        for scope in [SignalScope::SinceStartup, SignalScope::Sampled] {
            assert_eq!(scheduling_share(99.0, 100.0, scope).status, SignalStatus::Info);
        }
    }
}
