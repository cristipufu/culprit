use crate::{global, probe};
use culprit_core::{Engine, Result, WaitSample, WaitStat};
use culprit_postgres::wire::{Connector, INTERACTIVE, SessionLimits, query};
use std::{collections::BTreeMap, time::Duration};
use tokio::time::{Instant, sleep_until};

/// Only the native type/event establishes a category. A missing event is unknown, not measured CPU time.
pub(crate) fn category(kind: &str, event: &str) -> &'static str {
    if event.starts_with("WAL_") || event.starts_with("Raft_WaitingFor") {
        return "Log";
    }
    if matches!(event, "LockedBatchEntry_Lock" | "ConflictResolution_WaitOnConflictingTxns") {
        return "Lock";
    }
    match kind.to_ascii_lowercase().as_str() {
        "cpu" => "CPU",
        "lock" => "Lock",
        "lwlock" | "bufferpin" => "Latch",
        "rpcwait" => "RPC",
        "diskio" | "io" => "Storage",
        "client" => "Network",
        _ => "Other",
    }
}

pub(crate) async fn sample(connector: &Connector, requested: u32) -> Result<WaitSample> {
    let session = connector.connect_observed(SessionLimits { catalog_path: true, ..INTERACTIVE }).await?;
    let p = probe::read(&session.client).await?;
    let inventory = if p.global { Some(global::nodes(&session).await) } else { None };
    let relation = if p.global { "pg_catalog.\"gv$pg_stat_activity\"" } else { "pg_catalog.pg_stat_activity" };
    let seconds = requested.clamp(1, 60);
    let started = Instant::now();
    let notices = session.notices();
    let mut counts = BTreeMap::<(String, String), f64>::new();
    let mut samples = 0.0;
    let mut partial = false;
    for tick in 0..=seconds {
        if tick > 0 && started.elapsed() >= Duration::from_secs(seconds.into()) {
            break;
        }
        if tick > 0 {
            sleep_until(started + Duration::from_secs(tick.into())).await
        }
        let rows = query(
            &session.client,
            &format!(
                "SELECT state,wait_event_type AS kind,wait_event AS event,count(*)::float8 AS n
            FROM {relation} WHERE datid={} AND (state='active' OR state IS NULL)
            AND backend_type IN ('client backend','yb-conn-mgr worker connection')
            AND COALESCE(application_name,'') <> 'culprit' GROUP BY state,wait_event_type,wait_event",
                p.oid
            ),
        )
        .await?;
        samples += 1.0;
        for row in rows {
            if row.text("state").is_none() {
                partial = true;
                continue;
            }
            let event = row.text("event").unwrap_or_else(|| "Unknown native event".into());
            let kind = category(&row.string("kind"), &event);
            if let Some(n) = row.number("n").filter(|n| *n >= 0.0) {
                *counts.entry((kind.into(), event)).or_default() += n;
            } else {
                partial = true;
            }
        }
    }
    if let Some(before) = inventory {
        let after = global::nodes(&session).await;
        partial |= before.ok().zip(after.ok()).is_none_or(|(a, b)| a != b);
    }
    partial |= session.notices() != notices;
    let elapsed = started.elapsed().as_secs_f64();
    let active: f64 = counts.values().sum::<f64>() / samples;
    let mut categories: Vec<_> = counts
        .into_iter()
        .map(|((category, event), n)| WaitStat {
            category,
            wait_type: Some(event),
            wait_ms: n / samples * elapsed * 1000.0,
            tasks: Some(n),
        })
        .collect();
    categories.sort_by(|a, b| b.wait_ms.total_cmp(&a.wait_ms));
    let mut notes = vec!["Session observations are weighted over the measured duration. They describe active YSQL backends, not CPU utilization or the time spent by internal DocDB tasks. Short operations can fall between samples.".into()];
    if partial {
        notes.push("Visibility changed, permissions hid sessions, or a node was skipped. Observed categories are partial and no complete active-session count is asserted.".into());
    }
    if !p.global {
        notes.push(format!(
            "Connected YB-TServer {} only; this is not whole-cluster activity.",
            p.node.as_deref().unwrap_or("unknown")
        ));
    }
    Ok(WaitSample {
        engine: Engine::Yugabyte,
        taken_at: p.now,
        seconds: elapsed,
        method: format!(
            "YSQL active sessions sampled at a one-second target cadence for {seconds} s ({})",
            if p.global { "configured cluster views" } else { "connected node" }
        ),
        total_ms: active * elapsed * 1000.0,
        categories,
        signals: vec![],
        active_sessions: (!partial).then_some(active),
        notes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_wait_classes_do_not_turn_unknown_events_into_cpu_or_rpc_time_into_client_io() {
        assert_eq!(category("Cpu", "OnCpu_Active"), "CPU");
        assert_eq!(category("RPCWait", "CatalogRead"), "RPC");
        assert_eq!(category("DiskIO", "RocksDB_WriteToFile"), "Storage");
        assert_eq!(category("RPCWait", "Raft_WaitingForReplication"), "Log");
        assert_eq!(category("WaitOnCondition", "LockedBatchEntry_Lock"), "Lock");
        assert_eq!(category("", ""), "Other");
    }
}
