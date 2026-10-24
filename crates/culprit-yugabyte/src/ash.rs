//! Native task samples have weights, not elapsed durations. Never mix these with PGSS or exclusive wait timings.
use crate::{global, probe::Probe, sources};
use culprit_core::{
    ActivityEvent, ActivityHistory, ActivityPeriod, CounterScope, Coverage, Error, Result, SnapshotOptions,
};
use culprit_postgres::wire::{ObservedSession, query_bounded, query_one};
use std::collections::{BTreeMap, BTreeSet};

const MAX_SAMPLES: usize = 10_000;

pub(crate) async fn collect(
    session: &ObservedSession,
    probe: &Probe,
    options: &SnapshotOptions,
    coverage: &mut Coverage,
) -> Option<ActivityHistory> {
    let result =
        crate::deadline(culprit_core::limits::OBSERVE_TIMEOUT, read(session, probe, options, MAX_SAMPLES)).await;
    let history = coverage.read(&sources::ACTIVITY_HISTORY, result)?;
    coverage.completed(&sources::ACTIVITY_HISTORY, history.period.observations as usize);
    if history.period.partial {
        coverage.partial(&sources::ACTIVITY_HISTORY, "Activity observations are partial: retention reached 10,000 rows/8 MiB, a contributor warned or changed, or some samples could not be decoded.");
    }
    Some(history)
}

fn relation(probe: &Probe, start: f64, end: f64) -> String {
    if probe.global {
        return "pg_catalog.\"gv$yb_active_session_history\"".into();
    }
    // OUT parameters supply the native row type; explicit time arguments avoid the view's unbounded default call.
    format!("pg_catalog.yb_active_session_history(to_timestamp({start}),to_timestamp({end}))")
}

pub(crate) async fn read(
    session: &ObservedSession,
    probe: &Probe,
    options: &SnapshotOptions,
    limit: usize,
) -> Result<ActivityHistory> {
    let end = culprit_core::format::epoch_seconds(&probe.now)
        .ok_or_else(|| Error::invalid("YSQL returned an invalid ASH observation time"))?;
    let minutes = options
        .window_minutes
        .unwrap_or_else(|| options.window_days.saturating_mul(1440))
        .clamp(1, culprit_core::MAX_WINDOW_MINUTES);
    let start = end - f64::from(minutes) * 60.0;
    let before = session.notices();
    let inventory = if probe.global { Some(global::nodes(session).await) } else { None };
    let relation = relation(probe, start, end);
    let role = probe
        .statement_role
        .map(|id| format!(" AND ysql_userid={id}"))
        .unwrap_or_else(|| format!(" AND {}", crate::probe::ALL_STATS));
    let node = if probe.global {
        "server_uuid::text".into()
    } else {
        format!("'{}'::text", probe.node.as_deref().unwrap_or("unknown"))
    };
    let limit = limit.clamp(1, MAX_SAMPLES);
    // Constant bounds are important: global views otherwise ship each node's entire circular buffer before
    // evaluating now(). Project no client addresses, request identifiers, process memory or wait-event aux text.
    let sql = format!(
        "SELECT {} AS at,query_id::text AS query,wait_event_component AS component,
        wait_event_type AS kind,wait_event AS event,sample_weight::float8 AS weight,{node} AS node
        FROM {relation} WHERE ysql_dbid={} AND sample_time >= to_timestamp({start})
        AND sample_time < to_timestamp({end}){role}
        ORDER BY sample_time DESC,query_id,wait_event_component,wait_event LIMIT {}",
        crate::probe::timestamp("sample_time"),
        probe.oid,
        limit + 1
    );
    let (rows, truncated) = query_bounded(&session.client, &sql, limit).await?;
    let mut partial = truncated;
    let mut seen = BTreeSet::new();
    let mut events = BTreeMap::<(String, String, String, Option<String>, Option<String>), ActivityEvent>::new();
    let mut first: Option<String> = None;
    let mut last: Option<String> = None;
    let mut observations = 0.0;
    let mut weighted = 0.0;
    for row in rows {
        let (Some(at), Some(component), Some(event), Some(weight)) = (
            row.text("at"),
            row.text("component"),
            row.text("event"),
            row.number("weight").filter(|w| w.is_finite() && *w > 0.0),
        ) else {
            partial = true;
            continue;
        };
        if culprit_core::format::epoch_seconds(&at).is_none_or(|t| t < start || t >= end) {
            partial = true;
            continue;
        }
        if !(weighted + weight).is_finite() {
            partial = true;
            continue;
        }
        let node = row.text("node").and_then(|n| crate::probe::node_id(&n));
        if let Some(node) = &node {
            seen.insert(node.clone());
        } else {
            partial = true;
        }
        let query = match row.text("query").and_then(|id| id.parse::<i64>().ok()) {
            Some(id) if !(0..=12).contains(&id) => Some(id.to_string()),
            Some(_) => None, // Native reserved ids describe background work or an unresolved query, not PGSS ids.
            None => {
                partial = true;
                None
            }
        };
        first = Some(first.map_or_else(|| at.clone(), |a| a.min(at.clone())));
        last = Some(last.map_or_else(|| at.clone(), |a| a.max(at.clone())));
        let category = culprit_core::waits::category_key(crate::waits::category(&row.string("kind"), &event));
        let key = (category.to_string(), component.clone(), event.clone(), query.clone(), node.clone());
        let entry = events.entry(key).or_insert(ActivityEvent {
            category: category.into(),
            component,
            wait_event: event,
            query_id: query,
            node,
            observations: 0.0,
            weighted_observations: 0.0,
        });
        entry.observations += 1.0;
        entry.weighted_observations += weight;
        observations += 1.0;
        weighted += weight;
    }
    let nodes = if let Some(inventory) = inventory {
        let after = global::nodes(session).await;
        match inventory.ok().zip(after.ok()) {
            Some((a, b)) if a == b && seen.is_subset(&a) => a,
            _ => {
                partial = true;
                seen
            }
        }
    } else {
        if probe.node.is_none() {
            partial = true;
        }
        probe.node.iter().cloned().collect()
    };
    partial |= session.notices() != before;
    let window = query_one(
        &session.client,
        &format!(
            "SELECT {} AS start,{} AS end,{} AS all_stats",
            crate::probe::timestamp(&format!("to_timestamp({start})")),
            crate::probe::timestamp(&format!("to_timestamp({end})")),
            crate::probe::ALL_STATS
        ),
    )
    .await?
    .ok_or_else(|| Error::invalid("YSQL returned no ASH time bounds"))?;
    if probe.statement_role.is_none() && !window.flag("all_stats") {
        return Err(Error::new(
            culprit_core::ErrorCode::PermissionDenied,
            "The monitoring privilege changed while native activity history was being read",
        ));
    }
    let mut notes=vec![
        "Weights describe sampled active tasks, not query elapsed time, exclusive wait time, or CPU utilization. YSQL and DocDB components can execute concurrently.".into(),
        "The circular buffer can evict observations, short tasks can fall between samples, and sampling cadence can change. First/last samples do not prove continuous coverage of the requested period.".into(),
        "Database-scoped activity can include administrative and monitoring work. Background records without this database's identity are excluded.".into(),
    ];
    if partial {
        notes.push("This read was partial; shares describe only the retained observations.".into());
    }
    Ok(ActivityHistory {
        period: ActivityPeriod {
            source: format!(
                "yb_active_session_history ({}){}",
                if probe.global {
                    "configured cluster views".into()
                } else {
                    format!("node {}", probe.node.as_deref().unwrap_or("unknown"))
                },
                probe.statement_role.map(|id| format!(", role {id} only")).unwrap_or_default()
            ),
            requested_start: window.string("start"),
            requested_end: window.string("end"),
            first_sample: first,
            last_sample: last,
            scope: if probe.global { CounterScope::Cluster } else { CounterScope::Node },
            nodes: nodes.into_iter().collect(),
            observations,
            weighted_observations: weighted,
            partial,
            notes,
        },
        events: events.into_values().collect(),
    })
}
