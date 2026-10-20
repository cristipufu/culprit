//! What parameter sensitivity needs beyond the workload numbers: the values each plan was compiled for, and the
//! variants SQL Server 2022's parameter-sensitive plan optimization keeps for a statement.

use std::collections::HashMap;

use culprit_core::{PlanVariant, QueryStat};

use crate::conn::{MsClient, query};
use crate::plan_xml;
use crate::workload::{id_list, utc_offset_iso};

/// Queries with several plans get every plan's compiled values; bounded so a scan stays cheap.
const MAX_QUERIES: usize = 20;
const MAX_PLANS: usize = 60;

pub(crate) async fn attach(client: &mut MsClient, window_minutes: u32, queries: &mut [QueryStat]) {
    for q in queries.iter_mut() {
        let compiled = q.plan.as_ref().and_then(|p| p.facts().compiled_parameters.clone());
        if let (Some(first), Some(compiled)) = (q.plans.first_mut(), compiled) {
            first.compiled = Some(compiled);
        }
    }
    let wanted: Vec<String> = queries
        .iter()
        .filter(|q| q.plans.len() >= 2)
        .take(MAX_QUERIES)
        .flat_map(|q| q.plans.iter().filter(|p| p.compiled.is_none()).map(|p| p.id.clone()))
        .take(MAX_PLANS)
        .collect();
    if !wanted.is_empty() {
        let sql =
            format!("SELECT plan_id, query_plan FROM sys.query_store_plan WHERE plan_id IN ({})", id_list(&wanted));
        if let Ok(rows) = query(client, &sql).await {
            let compiled: HashMap<String, String> = rows
                .into_iter()
                .filter_map(|r| {
                    let plan = plan_xml::parse(&r.str("query_plan")?).ok()?;
                    Some((r.string("plan_id"), plan.facts().compiled_parameters.clone()?))
                })
                .collect();
            for p in queries.iter_mut().flat_map(|q| q.plans.iter_mut()) {
                if p.compiled.is_none() {
                    p.compiled = compiled.get(&p.id).cloned();
                }
            }
        }
    }
    attach_variants(client, window_minutes, queries).await;
}

fn variants_sql(window_minutes: u32, ids: &str) -> String {
    format!(
        r#"
DECLARE @since datetimeoffset = DATEADD(minute, -{window_minutes}, SYSDATETIMEOFFSET());
SELECT v.parent_query_id, v.query_variant_query_id AS variant_id, p.plan_id,
  SUM(rs.count_executions) AS executions,
  SUM(rs.avg_duration * rs.count_executions) / 1000.0 AS total_ms,
  MIN(rs.min_duration) / 1000.0 AS min_ms, MAX(rs.max_duration) / 1000.0 AS max_ms,
  {first} AS first_seen, {last} AS last_seen,
  MAX(CAST(p.is_forced_plan AS int)) AS is_forced
FROM sys.query_store_query_variant v
JOIN sys.query_store_plan p ON p.query_id = v.query_variant_query_id
JOIN sys.query_store_runtime_stats rs ON rs.plan_id = p.plan_id
JOIN sys.query_store_runtime_stats_interval i ON i.runtime_stats_interval_id = rs.runtime_stats_interval_id
WHERE i.end_time >= @since AND v.parent_query_id IN ({ids})
GROUP BY v.parent_query_id, v.query_variant_query_id, p.plan_id
"#,
        first = utc_offset_iso("MIN(rs.first_execution_time)"),
        last = utc_offset_iso("MAX(rs.last_execution_time)"),
    )
}

/// SQL Server 2022 keeps one "variant" query per range of parameter values (with its own plans) under the statement
/// the application sent. Older versions don't have the view; that is not an error.
async fn attach_variants(client: &mut MsClient, window_minutes: u32, queries: &mut [QueryStat]) {
    let ids: Vec<String> = queries.iter().map(|q| q.id.clone()).collect();
    let list = id_list(&ids);
    if list == "NULL" {
        return;
    }
    let Ok(rows) = query(client, &variants_sql(window_minutes, &list)).await else { return };
    for r in rows {
        let Some(parent) = queries.iter_mut().find(|q| q.id == r.string("parent_query_id")) else { continue };
        let executions = r.f64_or_zero("executions");
        let total = r.f64_or_zero("total_ms");
        parent.plans.push(PlanVariant {
            id: r.string("plan_id"),
            executions,
            total_time_ms: total,
            mean_time_ms: if executions > 0.0 { total / executions } else { 0.0 },
            first_seen: r.str("first_seen"),
            last_seen: r.str("last_seen"),
            is_forced: r.bool("is_forced"),
            min_time_ms: r.f64("min_ms"),
            max_time_ms: r.f64("max_ms"),
            stddev_time_ms: None,
            compiled: None,
            variant_query: Some(r.string("variant_id")),
            cpu_time_ms: None,
            logical_reads: None,
        });
    }
}
