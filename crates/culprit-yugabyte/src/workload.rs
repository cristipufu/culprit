use crate::{
    probe::{Probe, node_id},
    sources,
};
use culprit_core::{
    CounterScope, CounterSet, Coverage, QueryStat, Spread, Workload, WorkloadKind,
    signals::{DocdbMetrics, QueryCosts},
};
use culprit_postgres::wire::{ObservedSession, Row, query, query_bounded};
use serde_json::json;
use std::collections::BTreeMap;

async fn epochs(session: &ObservedSession, probe: &Probe, coverage: &mut Coverage) -> CounterSet {
    let mut result = CounterSet {
        scope: if probe.cluster { CounterScope::Cluster } else { CounterScope::Node },
        members: BTreeMap::new(),
        complete: false,
    };
    let relation = if probe.cluster {
        let found = crate::global::configured(&session.client, "pg_stat_statements_info").await.ok().flatten();
        match found {
            Some(relation) => relation,
            None => {
                coverage.partial(&sources::EPOCHS,"Cluster statement reset epochs were not configured. Add a trusted global pg_stat_statements_info view as an administrator to establish comparisons; Culprit never creates one during a scan.");
                return result;
            }
        }
    } else {
        culprit_core::Engine::Yugabyte
            .dialect()
            .qualified(&[probe.statement_schema.as_deref().unwrap_or("pg_catalog"), "pg_stat_statements_info"])
    };
    let before = session.notices();
    let sql = format!(
        "SELECT {} AS reset{} FROM {relation}",
        crate::probe::timestamp("stats_reset"),
        if probe.cluster { ",server_uuid::text AS node" } else { "" }
    );
    if let Some(rows) = coverage.read_rows(&sources::EPOCHS, query(&session.client, &sql).await) {
        result.complete = session.notices() == before;
        for row in rows {
            if let Some(node) = if probe.cluster { node_id(&row.string("node")) } else { probe.node.clone() } {
                result.members.insert(node, row.text("reset"));
            } else {
                result.complete = false;
            }
        }
        if probe.cluster {
            match query(&session.client, "SELECT uuid::text AS node FROM yb_servers()").await {
                Ok(rows) => {
                    let expected: Option<std::collections::BTreeSet<_>> =
                        rows.iter().map(|r| node_id(&r.string("node"))).collect();
                    if expected
                        .as_ref()
                        .is_none_or(|ids| ids.is_empty() || ids != &result.members.keys().cloned().collect())
                    {
                        result.complete = false;
                    }
                }
                Err(_) => result.complete = false,
            }
        }
    }
    if !result.epochs_known() {
        coverage.partial(&sources::EPOCHS,"Not every contributing YSQL node supplied a valid reset epoch. Rates and cross-scan subtraction are unavailable for this read.");
    }
    result
}
async fn statement_rows(
    session: &ObservedSession,
    probe: &Probe,
    relation: &str,
    top: u32,
) -> culprit_core::Result<(Vec<Row>, bool)> {
    let columns: std::collections::BTreeSet<_> = query(
        &session.client,
        &format!(
            "SELECT attname FROM pg_attribute WHERE attrelid='{}'::regclass AND attnum>0 AND NOT attisdropped",
            relation.replace('\'', "''")
        ),
    )
    .await?
    .into_iter()
    .map(|row| row.string("attname"))
    .collect();
    let time_column = |modern: &str, legacy: &str| {
        if columns.contains(modern) {
            Ok(modern.to_string())
        } else if columns.contains(legacy) {
            Ok(legacy.to_string())
        } else {
            Err(culprit_core::Error::invalid("YSQL statement statistics lack execution-time columns"))
        }
    };
    let total = time_column("total_exec_time", "total_time")?;
    let min = time_column("min_exec_time", "min_time")?;
    let max = time_column("max_exec_time", "max_time")?;
    let mean = time_column("mean_exec_time", "mean_time")?;
    let stddev = time_column("stddev_exec_time", "stddev_time")?;
    let role_filter = probe.statement_role.map(|role| format!(" AND userid={role}")).unwrap_or_default();
    let extras = [
        ("docdb_read_rpcs", "read_requests"),
        ("docdb_write_rpcs", "write_requests"),
        ("docdb_read_operations", "read_operations"),
        ("docdb_write_operations", "write_operations"),
        ("docdb_rows_scanned", "rows_scanned"),
        ("docdb_rows_returned", "rows_returned"),
        ("docdb_read_time", "read_ms"),
        ("docdb_write_time", "write_ms"),
        ("docdb_wait_time", "wait_ms"),
        ("catalog_wait_time", "catalog_ms"),
        ("total_retries", "retries"),
        ("conflict_retries", "conflict_retries"),
        ("read_restart_retries", "read_restarts"),
        ("docdb_obsolete_rows_scanned", "obsolete_rows_scanned"),
        ("docdb_seeks", "seeks"),
        ("docdb_nexts", "nexts"),
        ("docdb_prevs", "prevs"),
    ]
    .iter()
    .map(|(key, alias)| {
        if columns.contains(*key) {
            format!("CASE WHEN count({key})=count(*) THEN sum({key}::float8) END AS {alias}")
        } else {
            format!("NULL::float8 AS {alias}")
        }
    })
    .collect::<Vec<_>>()
    .join(",");
    // Project only the fields used here. A whole-row JSON conversion also fetches latency histograms and every
    // unrelated column across the global FDW, before aggregating even a small top-query request.
    let sql = format!(
        "WITH grouped AS (
         SELECT queryid::text AS id,min(query) AS text,sum(calls::float8) AS calls,
         sum({total}::float8) AS ms,sum(rows::float8) AS rows,
         min({min}::float8) AS min_ms,max({max}::float8) AS max_ms,
         sum(calls::float8*(power({stddev}::float8,2)+power({mean}::float8,2))) AS moment,
         {extras} FROM {relation} WHERE dbid={} AND calls>0{role_filter}
         AND query NOT LIKE '%/* culprit */%' GROUP BY queryid)
         SELECT *,sum(ms) OVER() AS all_ms,sum(calls) OVER() AS all_calls,count(*) OVER() AS all_queries
         FROM grouped ORDER BY ms DESC,id LIMIT {}",
        probe.oid,
        top.clamp(1, 1000)
    );
    query_bounded(&session.client, &sql, top.clamp(1, 1000) as usize).await
}

pub(crate) async fn read(
    session: &ObservedSession,
    probe: &Probe,
    top: u32,
    coverage: &mut Coverage,
) -> Option<Workload> {
    if !probe.statements {
        coverage.recorded(
            &sources::WORKLOAD,
            culprit_core::SourceState::Unavailable,
            Some("YSQL pg_stat_statements is not installed or visible.".into()),
        );
        coverage.lacking(
            culprit_core::Capability::PgStatStatements,
            "YSQL statement statistics are unavailable",
            Some("Enable pg_stat_statements in this database through YugabyteDB administration.".into()),
        );
        return None;
    }
    let mut counter_set = epochs(session, probe, coverage).await;
    let relation = if probe.cluster {
        "pg_catalog.\"gv$pg_stat_statements\"".into()
    } else {
        culprit_core::Engine::Yugabyte
            .dialect()
            .qualified(&[probe.statement_schema.as_deref().unwrap_or("pg_catalog"), "pg_stat_statements"])
    };
    let before = session.notices();
    let result = statement_rows(session, probe, &relation, top).await;
    let truncated = result.as_ref().is_ok_and(|(_, truncated)| *truncated);
    let rows = coverage.read_rows(&sources::WORKLOAD, result.map(|(rows, _)| rows))?;
    if truncated {
        coverage.partial(&sources::WORKLOAD, "Statement text and counters reached the shared 8 MiB retention budget. Only a contiguous prefix is retained; source totals still cover every statement when a retained row supplies them.");
        if rows.is_empty() {
            coverage.lacking(
                culprit_core::Capability::PgStatStatements,
                "No statement row fit the retention budget; complete source totals are unknown",
                None,
            );
            return None;
        }
    }
    if rows.iter().any(|row| row.text("id").is_none()) {
        coverage.recorded(&sources::WORKLOAD, culprit_core::SourceState::Unavailable, Some("Statement visibility changed during collection; hidden identities cannot establish query counts or safely owned plans.".into()));
        coverage.lacking(
            culprit_core::Capability::PgStatStatements,
            "Statement statistics visibility changed during collection",
            Some("Grant pg_read_all_stats for complete database statistics, then scan again.".into()),
        );
        return None;
    }
    let partial = session.notices() != before;
    let after = epochs(session, probe, coverage).await;
    if counter_set != after || partial {
        counter_set.complete = false;
        coverage.partial(&sources::WORKLOAD,"YSQL contributors or reset epochs changed, or a node was skipped during collection. The retained totals cannot establish complete cluster history.");
    }
    if !probe.cluster {
        coverage.note("YSQL scope",format!("Statement totals cover connected YB-TServer {} only. Cluster-wide statistics require configured, enabled global views and pg_read_all_stats; Culprit does not enable the preview automatically.",probe.node.as_deref().unwrap_or("unknown")));
    }
    if let Some(role) = probe.statement_role {
        coverage.note("YSQL role scope",format!("This login lacks pg_read_all_stats. Statement totals include current role {role} only; other roles' hidden identities are excluded before aggregation."));
    }
    let (metrics, rpc_metrics) = metric_settings(session, probe, coverage).await;
    let mut queries = Vec::new();
    for r in &rows {
        let Some(id) = r.text("id") else {
            coverage.partial(&sources::WORKLOAD, "Some statement identities are hidden by this login's permissions.");
            continue;
        };
        let (Some(calls), Some(ms)) = (r.number("calls").filter(|n| *n > 0.0), r.number("ms")) else { continue };
        let mean = ms / calls;
        let metric = |key: &str| if metrics { r.number(key).filter(|n| *n >= 0.0) } else { None };
        let costs = QueryCosts {
            docdb: Some(DocdbMetrics {
                read_requests: if rpc_metrics { r.number("read_requests").filter(|n| *n >= 0.0) } else { None },
                write_requests: if rpc_metrics { r.number("write_requests").filter(|n| *n >= 0.0) } else { None },
                read_operations: metric("read_operations"),
                write_operations: metric("write_operations"),
                rows_scanned: metric("rows_scanned"),
                rows_returned: metric("rows_returned"),
                read_ms: metric("read_ms"),
                write_ms: metric("write_ms"),
                wait_ms: metric("wait_ms"),
                catalog_ms: metric("catalog_ms"),
                retries: metric("retries"),
                conflict_retries: metric("conflict_retries"),
                read_restarts: metric("read_restarts"),
                obsolete_rows_scanned: metric("obsolete_rows_scanned"),
                seeks: metric("seeks"),
                nexts: metric("nexts"),
                prevs: metric("prevs"),
            }),
            ..Default::default()
        };
        let value = json!({"id":id,"text":r.string("text"),"executions":calls,"total_time_ms":ms,"mean_time_ms":mean,"max_time_ms":r.number("max_ms"),
            "stddev_time_ms":r.number("moment").map(|m|(m/calls-mean*mean).max(0.0).sqrt()),"rows":r.number("rows"),"plans":[],"waits":[],
            "costs":costs,"spread":Spread{min_ms:r.number("min_ms"),..Default::default()}});
        match serde_json::from_value::<QueryStat>(value) {
            Ok(q) => queries.push(q),
            Err(_) => {
                coverage.partial(&sources::WORKLOAD, "A YSQL statement row could not be normalized.");
            }
        }
    }
    let start = if counter_set.epochs_known() {
        let mut at = counter_set.members.values().flatten();
        let first = at.next().cloned();
        first.filter(|first| at.all(|s| s == first))
    } else {
        None
    };
    let hours = start
        .as_deref()
        .and_then(culprit_core::format::epoch_seconds)
        .zip(culprit_core::format::epoch_seconds(&probe.now))
        .map(|(a, b)| ((b - a) / 3600.0).max(0.0))
        .unwrap_or(0.0);
    let head = rows.first();
    coverage.delivered(culprit_core::Capability::PgStatStatements);
    Some(Workload {
        source: format!(
            "{}{}",
            if probe.cluster {
                "YSQL pg_stat_statements (cluster)".into()
            } else {
                format!("YSQL pg_stat_statements (node {})", probe.node.as_deref().unwrap_or("unknown"))
            },
            probe.statement_role.map(|role| format!("; role {role} only")).unwrap_or_default()
        ),
        kind: Some(WorkloadKind::StatementStats),
        windowed: false,
        log: None,
        window_start: start,
        window_end: Some(probe.now.clone()),
        window_hours: hours,
        total_time_ms: head.and_then(|r| r.number("all_ms")).unwrap_or(0.0),
        total_cpu_ms: None,
        total_executions: head.and_then(|r| r.number("all_calls")).unwrap_or(0.0),
        query_count: head.and_then(|r| r.number("all_queries")).unwrap_or(0.0),
        queries,
        timeline: None,
        untracked_time_ms: None,
        hourly_time_ms: None,
        min_logged_ms: None,
        measurement: None,
        history_inputs: vec![],
        counter_set: Some(counter_set),
    })
}

async fn metric_settings(session: &ObservedSession, probe: &Probe, coverage: &mut Coverage) -> (bool, bool) {
    if !probe.cluster {
        coverage.recorded(&sources::COSTS,
            if probe.metrics && probe.rpc_metrics { culprit_core::SourceState::Collected } else { culprit_core::SourceState::Partial },
            Some("Native counters are recorded only while their YSQL collection flags are enabled; disabled families remain unknown, and their totals do not reconstruct work performed before collection began.".into()));
        return (probe.metrics, probe.rpc_metrics);
    }
    let Some(relation) = crate::global::configured(&session.client, "pg_settings").await.ok().flatten() else {
        coverage.partial(&sources::COSTS, "Cluster native costs require an administrator-configured global pg_settings view. The connected node's flags cannot prove whether a remote zero was measured.");
        return (false, false);
    };
    let before = session.notices();
    let expected = crate::global::nodes(session).await;
    let result = query(&session.client, &format!("SELECT server_uuid::text AS node,
        bool_or(name='yb_enable_pg_stat_statements_docdb_metrics' AND setting='on') AS metrics,
        bool_or(name='yb_enable_pg_stat_statements_rpc_stats' AND setting='on') AS rpc_metrics
        FROM {relation} WHERE name IN ('yb_enable_pg_stat_statements_docdb_metrics','yb_enable_pg_stat_statements_rpc_stats')
        GROUP BY server_uuid")).await;
    let rows = coverage.read_rows(&sources::COSTS, result).unwrap_or_default();
    let actual: Option<std::collections::BTreeSet<_>> = rows.iter().map(|r| node_id(&r.string("node"))).collect();
    let after = crate::global::nodes(session).await;
    let complete = expected
        .ok()
        .zip(after.ok())
        .zip(actual)
        .is_some_and(|((before, after), actual)| before == after && before == actual)
        && session.notices() == before
        && !rows.is_empty();
    if !complete {
        coverage.partial(&sources::COSTS, "Native metric settings were not established for every current YSQL node. Incomplete counters remain unknown.");
        return (false, false);
    }
    let flags = (rows.iter().all(|r| r.flag("metrics")), rows.iter().all(|r| r.flag("rpc_metrics")));
    if !flags.0 || !flags.1 {
        coverage.partial(&sources::COSTS, "Some nodes are not collecting native statement costs. Only metric families enabled on every contributing node are retained.");
    }
    flags
}
