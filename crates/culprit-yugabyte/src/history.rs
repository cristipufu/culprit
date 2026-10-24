//! QPM's independent, evictable plan cache. These are not pg_stat_statements lifetime totals or measured operators.
use crate::{probe::Probe, sources};
use culprit_core::{Coverage, Error, Plan, PlanHistory, PlanHistoryKind, PlanKind, PlanVariant, Result, Workload};
use culprit_postgres::wire::{ObservedSession, query_bounded};
use std::collections::BTreeMap;

fn relation(probe: &Probe) -> &'static str {
    if probe.global { "pg_catalog.\"gv$yb_pg_stat_plans\"" } else { "pg_catalog.yb_pg_stat_plans" }
}
fn authorize(probe: &Probe) -> Result<()> {
    // The view materializes all entries before SQL filters apply. On verified 2026.1.2, its
    // foreign-role redaction path segfaults even for a count(*) filtered to the caller's userid.
    // pg_read_all_stats avoids that path without requiring a superuser or changing server settings.
    if probe.statement_role.is_some() {
        return Err(Error::new(
            culprit_core::ErrorCode::PermissionDenied,
            "Retained YSQL plan history requires pg_read_all_stats; the native cache cannot be read safely without it",
        )
        .with_hint("An administrator can grant pg_read_all_stats to the monitoring login. Culprit skips this source without that grant."));
    }
    Ok(())
}

pub(crate) async fn collect(
    session: &ObservedSession,
    probe: &Probe,
    workload: &mut Workload,
    coverage: &mut Coverage,
) {
    let source = format!(
        "yb_pg_stat_plans ({})",
        if probe.global {
            "configured cluster views".into()
        } else {
            format!("node {}", probe.node.as_deref().unwrap_or("unknown"))
        }
    );
    for q in &mut workload.queries {
        q.plans.clear();
        q.plan_history = Some(PlanHistory {
            source: source.clone(), kind: PlanHistoryKind::Cache,
            captured_plan_id: None, captured_plan: None,
            note: "Retained plan-cache entries have their own first/last-use dates, resets and evictions. Their calls and timings are independent of this statement's statistics window; a cache entry's first use does not prove when its plan was introduced.".into(),
        });
    }
    let ids: Vec<_> = workload.queries.iter().filter_map(|q| q.id.parse::<i64>().ok()).collect();
    if ids.is_empty() {
        return;
    }
    let ids = ids.iter().map(ToString::to_string).collect::<Vec<_>>().join(",");
    // Project no execution parameters, hints or plan bodies. Global-view filters apply before aggregation.
    let sql = format!(
        "SELECT queryid::text AS query,planid::text AS id,sum(calls::float8) AS calls,
        sum(calls::float8*avg_exec_time) AS ms,
        sum(calls::float8*avg_exec_time)/NULLIF(sum(calls::float8),0) AS mean_ms,
        max(max_exec_time)::float8 AS max_ms,{} AS first,{} AS last
        FROM {} WHERE dbid={} AND queryid IN ({ids}) AND calls>0
        GROUP BY queryid,planid ORDER BY ms DESC,queryid,planid LIMIT 1001",
        crate::probe::timestamp("min(first_used)"),
        crate::probe::timestamp("max(last_used)"),
        relation(probe),
        probe.oid
    );
    let before = session.notices();
    let result = match authorize(probe) {
        Ok(()) => query_bounded(&session.client, &sql, 1000).await,
        Err(error) => Err(error),
    };
    let truncated = result.as_ref().is_ok_and(|(_, truncated)| *truncated);
    let Some(rows) = coverage.read_rows(&sources::PLAN_HISTORY, result.map(|(rows, _)| rows)) else {
        for q in &mut workload.queries {
            if let Some(history) = &mut q.plan_history {
                history.note = "Retained plan history could not be read with this connection. See the scan's source coverage; the Plan tab can still estimate safely owned statements.".into();
            }
        }
        return;
    };
    let mut partial = truncated || session.notices() != before;
    let mut plans: BTreeMap<String, Vec<PlanVariant>> = BTreeMap::new();
    for row in rows {
        let (Some(query), Some(id), Some(calls), Some(ms), Some(mean)) = (
            row.text("query"),
            row.text("id"),
            row.number("calls").filter(|n| *n > 0.0),
            row.number("ms").filter(|n| *n >= 0.0),
            row.number("mean_ms").filter(|n| *n >= 0.0),
        ) else {
            partial = true;
            continue;
        };
        if query.parse::<i64>().is_err() || id.parse::<i64>().is_err() {
            partial = true;
            continue;
        }
        plans.entry(query).or_default().push(PlanVariant {
            id,
            executions: calls,
            total_time_ms: ms,
            mean_time_ms: mean,
            first_seen: row.text("first"),
            last_seen: row.text("last"),
            is_forced: false,
            min_time_ms: None,
            max_time_ms: row.number("max_ms").filter(|n| *n >= 0.0),
            stddev_time_ms: None,
            compiled: None,
            variant_query: None,
            cpu_time_ms: None,
            logical_reads: None,
        });
    }
    for q in &mut workload.queries {
        q.plans = plans.remove(&q.id).unwrap_or_default();
    }
    if partial {
        coverage.partial(&sources::PLAN_HISTORY,"Retained plan history is partial: a contributor warned, an entry could not be decoded, or the 1,000-entry/8 MiB retention budget was reached.");
        for q in &mut workload.queries {
            if let Some(history) = &mut q.plan_history {
                history.note.push_str(" This read was partial; no complete plan history is established.");
            }
        }
    }
}

pub(crate) async fn capture(
    session: &ObservedSession,
    probe: &Probe,
    workload: &mut Workload,
    requested: usize,
    budget: &mut culprit_core::limits::ResultBudget,
    coverage: &mut Coverage,
) {
    if let Err(error) = authorize(probe) {
        coverage.read::<()>(&sources::PLAN_HISTORY, Err(error));
        return;
    }
    let wanted: Vec<_> = workload
        .queries
        .iter()
        .take(requested)
        .enumerate()
        .filter_map(|(order, q)| Some((q.id.parse::<i64>().ok()?, q.plans.first()?.id.parse::<i64>().ok()?, order)))
        .collect();
    if wanted.is_empty() {
        return;
    }
    let pairs = wanted.iter().map(|(q, p, _)| format!("({q}::bigint,{p}::bigint)")).collect::<Vec<_>>().join(",");
    let priority = wanted.iter().map(|(q, _, order)| format!("WHEN {q} THEN {order}")).collect::<Vec<_>>().join(" ");
    // One scan of the native cache, rather than one full get_all_entries() scan for each requested plan.
    let sql = format!(
        "WITH entries AS (SELECT DISTINCT ON (queryid) queryid,planid,plan
        FROM {} WHERE dbid={} AND (queryid,planid) IN ({pairs})
        ORDER BY queryid,last_used DESC)
        SELECT queryid::text AS query,planid::text AS id,plan FROM entries
        ORDER BY CASE queryid {priority} END",
        relation(probe),
        probe.oid
    );
    let result = query_bounded(&session.client, &sql, requested).await;
    let Ok((rows, truncated)) = result else {
        coverage.partial(
            &sources::PLAN_HISTORY,
            "Native cache metadata was read, but retained plan bodies could not be fetched.",
        );
        return;
    };
    if truncated {
        coverage.partial(&sources::PLAN_HISTORY, "Retained plan bodies reached the 8 MiB capture budget.");
    }
    if rows.len() < wanted.len() {
        coverage.partial(
            &sources::PLAN_HISTORY,
            "Some requested cache entries were evicted, hidden or unavailable during capture.",
        );
    }
    let mut bodies = BTreeMap::new();
    for row in rows {
        let Some(raw) = row.text("plan") else {
            coverage.partial(&sources::PLAN_HISTORY, "A retained plan body was hidden or unavailable during capture.");
            continue;
        };
        if !budget.retain(raw.len()) {
            coverage.partial(&sources::PLAN_HISTORY, "Retained plan bodies reached the shared 8 MiB capture budget.");
            break;
        }
        match decode_stored(&raw) {
            Ok(plan) => {
                bodies.insert((row.string("query"), row.string("id")), plan);
            }
            Err(_) => coverage.partial(
                &sources::PLAN_HISTORY,
                "A retained plan was truncated, hidden or not complete JSON. Its recorded counts remain available.",
            ),
        }
    }
    for q in workload.queries.iter_mut().take(requested) {
        if let Some(id) = q.plans.first().map(|p| p.id.clone())
            && let (Some(history), Some(plan)) = (&mut q.plan_history, bodies.remove(&(q.id.clone(), id.clone())))
        {
            history.captured_plan_id = Some(id);
            history.captured_plan = Some(plan);
        }
    }
}

fn decode_stored(raw: &str) -> Result<Plan> {
    let mut plan = crate::plan::parse(raw,PlanKind::Estimated,None).map_err(|_| {
        Error::invalid("The retained YSQL plan is truncated, hidden, or not JSON")
            .with_hint("QPM must already retain complete JSON plans. Review yb_pg_stat_plans_plan_format; Culprit does not change it during collection.")
    })?;
    crate::plan::redacted_constants(&mut plan);
    Ok(plan)
}

pub(crate) async fn stored(session: &ObservedSession, probe: &Probe, query: &str, id: &str) -> Result<Option<Plan>> {
    authorize(probe)?;
    let query = query.parse::<i64>().map_err(|_| Error::invalid("Invalid YSQL query identifier"))?;
    let id = id.parse::<i64>().map_err(|_| Error::invalid("Invalid YSQL plan identifier"))?;
    let sql = format!(
        "SELECT plan FROM {} WHERE dbid={} AND queryid={query} AND planid={id}
        ORDER BY last_used DESC LIMIT 1",
        relation(probe),
        probe.oid
    );
    let (rows, truncated) = query_bounded(&session.client, &sql, 1).await?;
    if truncated {
        return Err(Error::invalid("The stored YSQL plan exceeds the 8 MiB retention budget"));
    }
    let Some(raw) = rows.first().and_then(|r| r.text("plan")) else {
        return Ok(None);
    };
    decode_stored(&raw).map(Some)
}
