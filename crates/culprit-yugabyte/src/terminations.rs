//! Unexpected backend terminations are an evictable record set, not an error rate or a complete SQL error log.
use crate::{global, probe::Probe, sources};
use culprit_core::{
    CounterScope, Coverage, Error, QueryTermination, Result, SnapshotOptions, TerminationHistory, TerminationReason,
};
use culprit_postgres::wire::{ObservedSession, query_bounded, query_one};
use std::collections::BTreeSet;

const MAX_RECORDS: usize = 1000;

pub(crate) async fn collect(
    session: &ObservedSession,
    probe: &Probe,
    options: &SnapshotOptions,
    coverage: &mut Coverage,
) -> Option<TerminationHistory> {
    let result =
        crate::deadline(culprit_core::limits::OBSERVE_TIMEOUT, read(session, probe, options, MAX_RECORDS)).await;
    let history = coverage.read(&sources::TERMINATIONS, result)?;
    coverage.completed(&sources::TERMINATIONS, history.events.len());
    if history.partial {
        coverage.partial(&sources::TERMINATIONS, "Termination records reached the 1,000-row/8 MiB retention limit, a contributor warned or changed, or some records could not be decoded.");
    }
    Some(history)
}

pub(crate) async fn read(
    session: &ObservedSession,
    probe: &Probe,
    options: &SnapshotOptions,
    limit: usize,
) -> Result<TerminationHistory> {
    let end = culprit_core::format::epoch_seconds(&probe.now)
        .ok_or_else(|| Error::invalid("YSQL returned an invalid termination observation time"))?;
    let minutes = options
        .window_minutes
        .unwrap_or_else(|| options.window_days.saturating_mul(1440))
        .clamp(1, culprit_core::MAX_WINDOW_MINUTES);
    let start = end - f64::from(minutes) * 60.0;
    let before = session.notices();
    // Global views cannot filter an owner id for these records. Preserve native local visibility for restricted
    // logins instead of relying on a remote view's privilege mapping to establish a role boundary.
    let cluster = probe.global && probe.statement_role.is_none();
    let inventory = if cluster { Some(global::nodes(session).await) } else { None };
    let relation = if cluster { "pg_catalog.\"gv$yb_terminated_queries\"" } else { "pg_catalog.yb_terminated_queries" };
    let node = if cluster {
        "server_uuid::text".into()
    } else {
        format!("'{}'::text", probe.node.as_deref().unwrap_or("unknown"))
    };
    let privilege = format!("{}{}", if probe.statement_role.is_some() { "NOT " } else { "" }, crate::probe::ALL_STATS);
    let limit = limit.clamp(1, MAX_RECORDS);
    // Never fetch query_text, backend PIDs or arbitrary native messages. Constant filters also bound FDW traffic.
    // The native function enforces visibility for the invoking login, including its inherited role privileges.
    let sql = format!(
        "SELECT query_id::text AS query,{node} AS node,{} AS started,{} AS ended,
        CASE WHEN termination_reason='Terminated by SIGKILL' THEN 'sigkill'
        WHEN termination_reason='Terminated by SIGSEGV' THEN 'sigsegv'
        WHEN termination_reason LIKE 'temporary file size exceeds temp_file_limit%' THEN 'temp-file-limit'
        ELSE 'other' END AS reason FROM {relation}
        WHERE databasename='{}' AND query_end_time>=to_timestamp({start}) AND query_end_time<to_timestamp({end})
        AND {privilege} ORDER BY query_end_time DESC,query_start_time DESC,query_id LIMIT {}",
        crate::probe::timestamp("query_start_time"),
        crate::probe::timestamp("query_end_time"),
        probe.database.replace('\'', "''"),
        limit + 1
    );
    let (rows, truncated) = query_bounded(&session.client, &sql, limit).await?;
    let mut partial = truncated;
    let mut seen = BTreeSet::new();
    let mut events = Vec::with_capacity(rows.len());
    for row in rows {
        let Some(ended_at) = row
            .text("ended")
            .filter(|at| culprit_core::format::epoch_seconds(at).is_some_and(|t| t >= start && t < end))
        else {
            partial = true;
            continue;
        };
        let ended = culprit_core::format::epoch_seconds(&ended_at).unwrap();
        let started_at =
            row.text("started").filter(|at| culprit_core::format::epoch_seconds(at).is_some_and(|t| t <= ended));
        partial |= started_at.is_none();
        let node = row.text("node").and_then(|n| crate::probe::node_id(&n));
        if let Some(node) = &node {
            seen.insert(node.clone());
        } else {
            partial = true;
        }
        let query_id = row
            .text("query")
            .and_then(|id| id.parse::<i64>().ok())
            .filter(|id| !(0..=12).contains(id))
            .map(|id| id.to_string());
        let reason = match row.string("reason").as_str() {
            "sigkill" => TerminationReason::Sigkill,
            "sigsegv" => TerminationReason::Sigsegv,
            "temp-file-limit" => TerminationReason::TempFileLimit,
            _ => TerminationReason::Other,
        };
        events.push(QueryTermination { query_id, node, started_at, ended_at, reason });
    }
    let nodes = if let Some(inventory) = inventory {
        match inventory.ok().zip(global::nodes(session).await.ok()) {
            Some((a, b)) if a == b && seen.is_subset(&a) => a,
            _ => {
                partial = true;
                seen
            }
        }
    } else {
        partial |= probe.node.is_none();
        probe.node.iter().cloned().collect()
    };
    let window = query_one(
        &session.client,
        &format!(
            "SELECT {} AS start,{} AS end,{} AS allowed",
            crate::probe::timestamp(&format!("to_timestamp({start})")),
            crate::probe::timestamp(&format!("to_timestamp({end})")),
            privilege
        ),
    )
    .await?
    .ok_or_else(|| Error::invalid("YSQL returned no termination time bounds"))?;
    if !window.flag("allowed") {
        return Err(Error::new(
            culprit_core::ErrorCode::PermissionDenied,
            "The monitoring privilege changed while termination history was read; collect again with the current login",
        ));
    }
    partial |= session.notices() != before;
    let mut notes=vec![
        "This evictable buffer records unexpected backend terminations and temporary-file-limit failures, not all SQL errors, timeouts, cancellations or deadlocks. Empty history does not prove that no queries failed.".into(),
        "Records have their own dates and retention, independent of statement-statistics resets. They do not establish a failure rate, failed business requests, or an ongoing incident.".into(),
        "SIGKILL alone does not prove an out-of-memory kill; inspect the node's logs. Raw SQL and free-form termination messages were not collected.".into(),
    ];
    if probe.global && !cluster {
        notes.push("Global termination reads require effective pg_read_all_stats. This read retains connected-node records under native login visibility, without changing global-view configuration.".into());
    }
    Ok(TerminationHistory {
        source: format!(
            "yb_terminated_queries ({})",
            if cluster {
                "configured cluster views".into()
            } else {
                format!("node {}", probe.node.as_deref().unwrap_or("unknown"))
            }
        ),
        requested_start: window.string("start"),
        requested_end: window.string("end"),
        scope: if cluster { CounterScope::Cluster } else { CounterScope::Node },
        nodes: nodes.into_iter().collect(),
        visibility: probe.statement_role.map_or_else(
            || "Complete monitoring privilege for this database".into(),
            |id| format!("Native visibility for login role {id}; inherited privileges may expose other roles' records"),
        ),
        partial,
        notes,
        events,
    })
}
