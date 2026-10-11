//! Pressure signals and the "right now" wait sample: performance counters, wait totals, memory grants and the oldest open
//! transaction. Read-only and tagged; a read that fails leaves its signal out.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use culprit_core::{
    Engine, GrantStat, OpenTransaction, Pressure, Signal, SignalScope, SignalUnit, WaitSample, WaitStat,
};

use crate::conn::{MsClient, query};
use crate::probe::Probe;
use crate::schema::{BENIGN_WAITS, wait_type_category};

const COUNTERS_SQL: &str = r#"
SELECT RTRIM(counter_name) AS counter, cntr_value AS value
FROM sys.dm_os_performance_counters
WHERE (object_name LIKE N'%SQL Statistics%'
       AND counter_name IN (N'Batch Requests/sec', N'SQL Compilations/sec', N'SQL Re-Compilations/sec'))
   OR (object_name LIKE N'%Buffer Manager%' AND counter_name = N'Page life expectancy')
"#;

const PENDING_GRANTS_SQL: &str = "SELECT COUNT(*) AS pending, MAX(wait_time_ms) AS longest_ms FROM sys.dm_exec_query_memory_grants WHERE grant_time IS NULL";

const THREADPOOL_NOW_SQL: &str =
    "SELECT COUNT(*) AS waiting FROM sys.dm_os_waiting_tasks WHERE wait_type = N'THREADPOOL'";

const GRANTS_SQL: &str = r#"
SELECT TOP (30) CONVERT(varchar(20), qs.query_hash, 1) AS query_hash,
  SUM(qs.execution_count) AS executions,
  SUM(qs.total_grant_kb) * 1.0 / NULLIF(SUM(qs.execution_count), 0) AS avg_granted_kb,
  SUM(qs.total_used_grant_kb) * 1.0 / NULLIF(SUM(qs.execution_count), 0) AS avg_used_kb,
  MAX(qs.max_grant_kb) AS max_granted_kb
FROM sys.dm_exec_query_stats qs
CROSS APPLY sys.dm_exec_plan_attributes(qs.plan_handle) pa
WHERE pa.attribute = 'dbid' AND CAST(pa.value AS int) = DB_ID() AND qs.total_grant_kb > 0
GROUP BY qs.query_hash
ORDER BY SUM(qs.total_grant_kb) - SUM(qs.total_used_grant_kb) DESC
"#;

const VERSION_STORE_SQL: &str =
    "SELECT SUM(reserved_space_kb) AS kb FROM sys.dm_tran_version_store_space_usage WHERE database_id = DB_ID()";

const OLDEST_TRANSACTION_SQL: &str = r#"
SELECT TOP (1) CAST(s.session_id AS varchar(10)) AS session_id,
  DATEDIFF(second, at.transaction_begin_time, GETDATE()) AS seconds,
  s.program_name, s.status,
  CASE WHEN snap.transaction_id IS NULL THEN 0 ELSE 1 END AS is_snapshot
FROM sys.dm_tran_session_transactions st
JOIN sys.dm_tran_active_transactions at ON at.transaction_id = st.transaction_id
JOIN sys.dm_exec_sessions s ON s.session_id = st.session_id
LEFT JOIN sys.dm_tran_active_snapshot_database_transactions snap ON snap.transaction_id = st.transaction_id
WHERE s.is_user_process = 1 AND s.session_id <> @@SPID AND st.is_user_transaction = 1
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

/// Signals, grants and the oldest open transaction. Each read is independent; failures are returned as notes.
pub(crate) async fn collect(client: &mut MsClient, probe: &Probe) -> (Pressure, Vec<String>) {
    let azure = probe.is_azure_sql_database();
    let uptime = uptime_hours(probe);
    let mut p = Pressure { uptime_hours: uptime, ..Pressure::default() };
    let mut notes = Vec::new();

    match counters(client).await {
        Ok(c) => {
            let batches = c.get("Batch Requests/sec").copied().unwrap_or(0.0);
            let compiles = c.get("SQL Compilations/sec").copied().unwrap_or(0.0);
            if let Some(hours) = uptime.filter(|_| batches > 0.0) {
                p.signals.push(Signal::new(
                    "batch-requests",
                    "Batch requests",
                    batches / (hours * 3600.0),
                    SignalUnit::PerSecond,
                    SignalScope::SinceStartup,
                ));
            }
            if batches > 0.0 {
                let ratio = Signal::new(
                    "compiles-per-batch",
                    "Plans compiled",
                    compiles / batches * 100.0,
                    SignalUnit::Percent,
                    SignalScope::SinceStartup,
                );
                // A handful of batches compiling says nothing about plan reuse; grade the ratio only under real load.
                let busy = uptime.is_none_or(|h| batches / (h * 3600.0) >= 1.0);
                p.signals.push(if busy {
                    ratio.graded("under 10% of batches is healthy", 10.0, 25.0, true)
                } else {
                    Signal {
                        healthy: Some("too few batches to judge; under 10% is healthy under load".into()),
                        ..ratio
                    }
                });
            }
            if let Some(ple) = c.get("Page life expectancy") {
                p.signals.push(
                    Signal::new("page-life", "Page life expectancy", *ple, SignalUnit::Seconds, SignalScope::Now)
                        .graded(
                            "minutes, not seconds: a drop means a big scan pushed data out of memory",
                            300.0,
                            60.0,
                            false,
                        ),
                );
            }
        }
        Err(e) => notes.push(format!("Performance counters: {e}")),
    }
    match query(client, PENDING_GRANTS_SQL).await {
        Ok(rows) => {
            let pending = rows.first().map(|r| r.f64_or_zero("pending")).unwrap_or(0.0);
            p.signals.push(
                Signal::new(
                    "memory-grants-pending",
                    "Waiting for memory",
                    pending,
                    SignalUnit::Count,
                    SignalScope::Now,
                )
                .graded("0 is healthy", 1.0, 5.0, true),
            );
        }
        Err(e) => notes.push(format!("Memory grants: {e}")),
    }
    match wait_totals(client, azure, None).await {
        Ok(totals) => {
            let (wait, signal) = totals.values().fold((0.0, 0.0), |(w, s), (tw, ts, _)| (w + tw, s + ts));
            if wait > 0.0 {
                p.signals.push(
                    Signal::new(
                        "signal-wait",
                        "Waiting for CPU",
                        signal / wait * 100.0,
                        SignalUnit::Percent,
                        SignalScope::SinceStartup,
                    )
                    .graded(
                        "under 20% of waiting is healthy; more means queries queue for a processor",
                        20.0,
                        35.0,
                        true,
                    ),
                );
            }
            let threadpool = totals.get("THREADPOOL").map(|t| t.0).unwrap_or(0.0);
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
        Err(e) => notes.push(format!("Wait totals: {e}")),
    }
    if let Ok(rows) = query(client, THREADPOOL_NOW_SQL).await {
        let waiting = rows.first().map(|r| r.f64_or_zero("waiting")).unwrap_or(0.0);
        p.signals.push(
            Signal::new("threadpool-waiting", "Tasks without a worker", waiting, SignalUnit::Count, SignalScope::Now)
                .graded("0 is healthy", 1.0, 1.0, true),
        );
    }
    match query(client, GRANTS_SQL).await {
        Ok(rows) => {
            p.grants = rows
                .iter()
                .map(|r| GrantStat {
                    query_hash: r.string("query_hash"),
                    executions: r.f64_or_zero("executions"),
                    avg_granted_kb: r.f64_or_zero("avg_granted_kb"),
                    avg_used_kb: r.f64_or_zero("avg_used_kb"),
                    max_granted_kb: r.f64_or_zero("max_granted_kb"),
                })
                .collect();
        }
        Err(e) => notes.push(format!("Memory grants per query: {e}")),
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
    match query(client, OLDEST_TRANSACTION_SQL).await {
        Ok(rows) => {
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
        Err(e) => notes.push(format!("Open transactions: {e}")),
    }
    (p, notes)
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
        Some(((b - a) / elapsed).max(0.0))
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
        if let Some(compiles) = rate("SQL Compilations/sec").filter(|_| batches > 0.0) {
            signals.push(
                Signal::new(
                    "compiles-per-batch",
                    "Plans compiled",
                    compiles / batches * 100.0,
                    SignalUnit::Percent,
                    SignalScope::Sampled,
                )
                .graded("under 10% of batches is healthy", 10.0, 25.0, true),
            );
        }
    }
    if let Some(recompiles) = rate("SQL Re-Compilations/sec") {
        signals.push(Signal::new("recompiles", "Recompiles", recompiles, SignalUnit::PerSecond, SignalScope::Sampled));
    }
    if wait_total > 0.0 {
        signals.push(
            Signal::new(
                "signal-wait",
                "Waiting for CPU",
                signal_total / wait_total * 100.0,
                SignalUnit::Percent,
                SignalScope::Sampled,
            )
            .graded(
                "under 20% of waiting is healthy; more means queries queue for a processor",
                20.0,
                35.0,
                true,
            ),
        );
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
        notes: Vec::new(),
    })
}
