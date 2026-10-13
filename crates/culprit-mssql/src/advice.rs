//! Read-only collectors behind the index advisor and tuning checks: forwarded fetches on heaps, statements that differ
//! only in literals, single-use plans, and Query Store's own tuning state (recommendations, forced plans, hints).

use std::collections::HashMap;

use culprit_core::{
    AutomaticTuningOption, ForcedPlan, PlanCacheUse, QueryFamily, QueryHint, TuningAdvice, TuningRecommendation,
};

use crate::conn::{MsClient, query};
use crate::workload::utc_local_iso;

const FORWARDED_SQL: &str = r#"
SELECT s.name AS schema_name, t.name AS table_name, SUM(os.forwarded_fetch_count) AS forwarded
FROM sys.dm_db_index_operational_stats(DB_ID(), NULL, NULL, NULL) os
JOIN sys.tables t ON t.object_id = os.object_id
JOIN sys.schemas s ON s.schema_id = t.schema_id
WHERE os.index_id = 0
GROUP BY s.name, t.name
HAVING SUM(os.forwarded_fetch_count) > 0
"#;

/// Reads that followed a forwarding pointer, per heap.
pub(crate) async fn forwarded_fetches(client: &mut MsClient) -> tiberius::Result<HashMap<(String, String), f64>> {
    Ok(query(client, FORWARDED_SQL)
        .await?
        .into_iter()
        .map(|r| {
            (
                (r.string("schema_name").to_lowercase(), r.string("table_name").to_lowercase()),
                r.f64_or_zero("forwarded"),
            )
        })
        .collect())
}

/// Query Store queries grouped by query_hash: a family with many members is the same statement sent with different
/// literals. Members are ranked by time so the family links to its heaviest variants.
fn families_sql(window_minutes: u32) -> String {
    format!(
        r#"
WITH per_query AS (
  SELECT q.query_hash, q.query_id, q.query_text_id,
    SUM(rs.count_executions) AS executions,
    SUM(rs.avg_duration * rs.count_executions) / 1000.0 AS total_ms
  FROM sys.query_store_query q
  JOIN sys.query_store_plan p ON p.query_id = q.query_id
  JOIN sys.query_store_runtime_stats rs ON rs.plan_id = p.plan_id
  JOIN sys.query_store_runtime_stats_interval i ON i.runtime_stats_interval_id = rs.runtime_stats_interval_id
  WHERE i.end_time > DATEADD(minute, -{window_minutes}, SYSDATETIMEOFFSET())
  GROUP BY q.query_hash, q.query_id, q.query_text_id
),
families AS (
  SELECT TOP 15 query_hash, COUNT(*) AS variants, SUM(executions) AS executions, SUM(total_ms) AS total_ms
  FROM per_query
  GROUP BY query_hash
  HAVING COUNT(*) >= 10
  ORDER BY COUNT(*) DESC
),
ranked AS (
  SELECT pq.query_hash, pq.query_id, pq.query_text_id,
    ROW_NUMBER() OVER (PARTITION BY pq.query_hash ORDER BY pq.total_ms DESC) AS rn
  FROM per_query pq JOIN families f ON f.query_hash = pq.query_hash
)
SELECT CONVERT(varchar(20), f.query_hash, 1) AS query_hash, f.variants, f.executions, f.total_ms,
  CAST(r.query_id AS varchar(20)) AS query_id, r.rn,
  CASE WHEN r.rn = 1 THEN LEFT(qt.query_sql_text, 600) END AS sample
FROM families f
JOIN ranked r ON r.query_hash = f.query_hash AND r.rn <= 5
JOIN sys.query_store_query_text qt ON qt.query_text_id = r.query_text_id
ORDER BY f.variants DESC, r.rn
"#
    )
}

/// Ad-hoc plans in the plan cache grouped by query_hash: the fallback when Query Store is off, and the memory each
/// family's single-use plans hold.
const CACHE_FAMILIES_SQL: &str = r#"
SELECT TOP 15 CONVERT(varchar(20), qs.query_hash, 1) AS query_hash,
  COUNT(*) AS variants, SUM(qs.execution_count) AS executions, SUM(qs.total_elapsed_time) / 1000.0 AS total_ms,
  SUM(CAST(cp.size_in_bytes AS bigint)) AS cached_bytes,
  LEFT(MAX(CAST(st.text AS nvarchar(max))), 600) AS sample
FROM sys.dm_exec_query_stats qs
JOIN sys.dm_exec_cached_plans cp ON cp.plan_handle = qs.plan_handle
CROSS APPLY sys.dm_exec_sql_text(qs.sql_handle) st
WHERE cp.objtype IN ('Adhoc', 'Prepared') AND (st.dbid = DB_ID() OR st.dbid IS NULL)
GROUP BY qs.query_hash
HAVING COUNT(*) >= 10
ORDER BY COUNT(*) DESC
"#;

pub(crate) async fn query_families(
    client: &mut MsClient,
    window_minutes: u32,
    query_store: bool,
) -> tiberius::Result<Vec<QueryFamily>> {
    let mut families: Vec<QueryFamily> = Vec::new();
    if query_store {
        for r in query(client, &families_sql(window_minutes.max(1))).await? {
            let key = r.string("query_hash");
            let id = r.string("query_id");
            match families.iter_mut().find(|f| f.key == key) {
                Some(f) => f.query_ids.push(id),
                None => families.push(QueryFamily {
                    key,
                    sample: r.string("sample"),
                    variants: r.f64_or_zero("variants"),
                    executions: r.f64_or_zero("executions"),
                    total_time_ms: r.f64_or_zero("total_ms"),
                    cached_bytes: None,
                    query_ids: vec![id],
                }),
            }
        }
    }
    // The plan cache adds memory per family, and stands in for Query Store when it is off.
    if let Ok(rows) = query(client, CACHE_FAMILIES_SQL).await {
        for r in rows {
            let key = r.string("query_hash");
            match families.iter_mut().find(|f| f.key == key) {
                Some(f) => f.cached_bytes = r.f64("cached_bytes"),
                None if !query_store => families.push(QueryFamily {
                    key,
                    sample: r.string("sample"),
                    variants: r.f64_or_zero("variants"),
                    executions: r.f64_or_zero("executions"),
                    total_time_ms: r.f64_or_zero("total_ms"),
                    cached_bytes: r.f64("cached_bytes"),
                    query_ids: Vec::new(),
                }),
                None => {}
            }
        }
    }
    Ok(families)
}

const PLAN_CACHE_SQL: &str = r#"
SELECT COUNT(*) AS total_plans, SUM(CAST(size_in_bytes AS bigint)) AS total_bytes,
  SUM(CASE WHEN usecounts = 1 AND objtype IN ('Adhoc', 'Prepared') THEN 1 ELSE 0 END) AS single_use_plans,
  SUM(CASE WHEN usecounts = 1 AND objtype IN ('Adhoc', 'Prepared') THEN CAST(size_in_bytes AS bigint) ELSE 0 END) AS single_use_bytes
FROM sys.dm_exec_cached_plans
"#;

pub(crate) async fn plan_cache(client: &mut MsClient) -> tiberius::Result<Option<PlanCacheUse>> {
    let row = query(client, PLAN_CACHE_SQL).await?.into_iter().next();
    Ok(row.map(|r| PlanCacheUse {
        single_use_plans: r.f64_or_zero("single_use_plans"),
        single_use_bytes: r.f64_or_zero("single_use_bytes"),
        total_plans: r.f64_or_zero("total_plans"),
        total_bytes: r.f64_or_zero("total_bytes"),
    }))
}

const AUTOMATIC_TUNING_SQL: &str =
    "SELECT name, desired_state_desc, actual_state_desc, reason_desc FROM sys.database_automatic_tuning_options";

fn recommendations_sql() -> String {
    format!(
        r#"
SELECT name, type, reason, {valid_since} AS valid_since,
  JSON_VALUE(state, '$.currentValue') AS state,
  JSON_VALUE(details, '$.planForceDetails.queryId') AS query_id,
  JSON_VALUE(details, '$.planForceDetails.regressedPlanId') AS regressed_plan_id,
  JSON_VALUE(details, '$.planForceDetails.recommendedPlanId') AS recommended_plan_id,
  TRY_CAST(JSON_VALUE(details, '$.planForceDetails.regressedPlanCpuTimeAverage') AS float) AS regressed_cpu,
  TRY_CAST(JSON_VALUE(details, '$.planForceDetails.recommendedPlanCpuTimeAverage') AS float) AS recommended_cpu,
  TRY_CAST(JSON_VALUE(details, '$.planForceDetails.regressedPlanErrorCount') AS int) AS regressed_errors,
  TRY_CAST(JSON_VALUE(details, '$.planForceDetails.recommendedPlanErrorCount') AS int) AS recommended_errors,
  JSON_VALUE(details, '$.implementationDetails.script') AS script,
  (SELECT {last} FROM sys.query_store_plan p
   WHERE p.query_id = TRY_CAST(JSON_VALUE(details, '$.planForceDetails.queryId') AS bigint)) AS last_execution
FROM sys.dm_db_tuning_recommendations
"#,
        valid_since = utc_local_iso("valid_since"),
        last = utc_local_iso("CAST(MAX(p.last_execution_time) AS datetime2)")
    )
}

fn forced_plans_sql() -> String {
    format!(
        "SELECT CAST(query_id AS varchar(20)) AS query_id, CAST(plan_id AS varchar(20)) AS plan_id,
           force_failure_count, last_force_failure_reason_desc, {last} AS last_execution
         FROM sys.query_store_plan WHERE is_forced_plan = 1",
        last = utc_local_iso("CAST(last_execution_time AS datetime2)")
    )
}

const HINTS_SQL: &str = "SELECT CAST(query_id AS varchar(20)) AS query_id, query_hint_text, query_hint_failure_count,
  last_query_hint_failure_reason_desc FROM sys.query_store_query_hints";

/// Errors that only mean "this version doesn't have it" (SQL Server 2016 has no tuning DMV, hints arrived in 2022).
fn absent(err: &tiberius::error::Error) -> bool {
    let text = err.to_string().to_ascii_lowercase();
    text.contains("invalid object name") || text.contains("invalid column name")
}

/// Query Store's tuning state. Each part is optional: older versions lack some views, and a missing view is not an error.
pub(crate) async fn tuning(client: &mut MsClient, query_store: bool) -> tiberius::Result<TuningAdvice> {
    let mut advice = TuningAdvice::default();
    match query(client, AUTOMATIC_TUNING_SQL).await {
        Ok(rows) => {
            advice.automatic_tuning = rows
                .iter()
                .map(|r| AutomaticTuningOption {
                    name: r.string("name"),
                    desired: r.string("desired_state_desc"),
                    actual: r.string("actual_state_desc"),
                    reason: r.str("reason_desc").filter(|s| !s.is_empty()),
                })
                .collect()
        }
        Err(e) if absent(&e) => {}
        Err(e) => return Err(e),
    }
    match query(client, &recommendations_sql()).await {
        Ok(rows) => {
            advice.recommendations = rows
                .iter()
                .map(|r| {
                    let gain = match (r.f64("regressed_cpu"), r.f64("recommended_cpu")) {
                        (Some(bad), Some(good)) if bad > 0.0 => Some(((bad - good) / bad).clamp(0.0, 1.0)),
                        _ => None,
                    };
                    TuningRecommendation {
                        name: r.string("name"),
                        kind: r.string("type"),
                        reason: r.str("reason"),
                        state: r.str("state"),
                        query_id: r.str("query_id"),
                        regressed_plan_id: r.str("regressed_plan_id"),
                        recommended_plan_id: r.str("recommended_plan_id"),
                        estimated_gain: gain,
                        error_prone: r.f64_or_zero("regressed_errors") > r.f64_or_zero("recommended_errors"),
                        script: r.str("script"),
                        valid_since: r.str("valid_since"),
                        last_execution: r.str("last_execution"),
                    }
                })
                .collect()
        }
        Err(e) if absent(&e) => {}
        Err(e) => return Err(e),
    }
    if query_store {
        advice.forced_plans = query(client, &forced_plans_sql())
            .await?
            .iter()
            .map(|r| ForcedPlan {
                query_id: r.string("query_id"),
                plan_id: r.string("plan_id"),
                failures: r.f64_or_zero("force_failure_count"),
                last_failure_reason: r.str("last_force_failure_reason_desc").filter(|s| s != "NONE" && !s.is_empty()),
                last_execution: r.str("last_execution"),
            })
            .collect();
        match query(client, HINTS_SQL).await {
            Ok(rows) => {
                advice.hints = rows
                    .iter()
                    .map(|r| QueryHint {
                        query_id: r.string("query_id"),
                        hint: r.string("query_hint_text"),
                        failures: r.f64_or_zero("query_hint_failure_count"),
                        last_failure_reason: r
                            .str("last_query_hint_failure_reason_desc")
                            .filter(|s| s != "NONE" && !s.is_empty()),
                    })
                    .collect()
            }
            Err(e) if absent(&e) => {}
            Err(e) => return Err(e),
        }
    }
    Ok(advice)
}
