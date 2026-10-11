//! "What changed" from Query Store: every query and plan in two periods, snapped to Query Store's intervals.
//!
//! Period boundaries move to the start of the interval they fall in, so an interval belongs wholly to one period.
//! Averages are weighted by executions; the fastest and slowest run come from the intervals' own min and max.

use std::collections::BTreeMap;

use culprit_core::TimelineSource;
use culprit_core::changes::{Period, PeriodData, PeriodStats, PlanPeriods, QueryPeriods};

use crate::conn::{MsClient, OWN_QUERY_MARKER, query};
use crate::workload::{split_parameters, utc_offset_iso};

/// Queries kept per comparison: the heaviest across both periods. Enough for any real regression hunt.
const MAX_QUERIES: usize = 400;

/// A timestamp as a `datetimeoffset` literal. Only ever built from a parsed timestamp, so nothing from outside
/// reaches the SQL text.
fn literal(iso: &str) -> Option<String> {
    let t = chrono::DateTime::parse_from_rfc3339(iso).ok()?.with_timezone(&chrono::Utc);
    Some(format!("'{}'", t.format("%Y-%m-%dT%H:%M:%S+00:00")))
}

/// Period boundaries snap to the start of the Query Store interval they fall in.
fn snapped(b0: &str, r0: &str) -> String {
    format!(
        r#"
DECLARE @b0 datetimeoffset = {b0}, @r0 datetimeoffset = {r0};
SELECT @b0 = ISNULL((SELECT MAX(start_time) FROM sys.query_store_runtime_stats_interval WHERE start_time <= @b0 AND end_time > @b0), @b0);
SELECT @r0 = ISNULL((SELECT MAX(start_time) FROM sys.query_store_runtime_stats_interval WHERE start_time <= @r0 AND end_time > @r0), @r0);"#
    )
}

fn intervals_cte(b0: &str, r0: &str, r1: &str) -> String {
    format!(
        r#"{snap}
DECLARE @r1 datetimeoffset = {r1};
WITH s AS (
  SELECT p.query_id, rs.plan_id, CASE WHEN i.start_time >= @r0 THEN 1 ELSE 0 END AS recent,
         rs.count_executions AS n, rs.avg_duration AS avg_us, rs.min_duration AS min_us, rs.max_duration AS max_us,
         i.start_time, i.end_time
  FROM sys.query_store_runtime_stats rs
  JOIN sys.query_store_runtime_stats_interval i ON i.runtime_stats_interval_id = rs.runtime_stats_interval_id
  JOIN sys.query_store_plan p ON p.plan_id = rs.plan_id
  JOIN sys.query_store_query q ON q.query_id = p.query_id
  JOIN sys.query_store_query_text t ON t.query_text_id = q.query_text_id
  WHERE i.start_time >= @b0 AND i.start_time < @r1 AND t.query_sql_text NOT LIKE N'%{marker}%'
)"#,
        snap = snapped(b0, r0),
        marker = OWN_QUERY_MARKER,
    )
}

fn per_plan_sql(b0: &str, r0: &str, r1: &str) -> String {
    format!(
        r#"{cte},
agg AS (
  SELECT query_id, plan_id, recent, SUM(n) AS executions, SUM(avg_us * n) / 1000.0 AS total_ms,
         MIN(min_us) / 1000.0 AS min_ms, MAX(max_us) / 1000.0 AS max_ms, MIN(start_time) AS first_start
  FROM s GROUP BY query_id, plan_id, recent
),
top_q AS (SELECT TOP ({MAX_QUERIES}) query_id FROM agg GROUP BY query_id ORDER BY SUM(total_ms) DESC)
SELECT a.query_id, a.plan_id, a.recent, a.executions, a.total_ms, a.min_ms, a.max_ms, {first} AS first_start
FROM agg a JOIN top_q t ON t.query_id = a.query_id
"#,
        cte = intervals_cte(b0, r0, r1),
        first = utc_offset_iso("a.first_start"),
    )
}

fn totals_sql(b0: &str, r0: &str, r1: &str) -> String {
    format!(
        r#"{cte}
SELECT recent, SUM(n) AS executions, SUM(avg_us * n) / 1000.0 AS total_ms,
       {start} AS period_start, {end} AS period_end
FROM s GROUP BY recent
"#,
        cte = intervals_cte(b0, r0, r1),
        start = utc_offset_iso("MIN(start_time)"),
        end = utc_offset_iso("MAX(end_time)"),
    )
}

/// Seconds of the (snapped) baseline period that have at least one Query Store interval.
fn coverage_sql(b0: &str, r0: &str) -> String {
    format!(
        r#"{snap}
SELECT SUM(CAST(DATEDIFF(second, start_time, CASE WHEN end_time > @r0 THEN @r0 ELSE end_time END) AS float)) AS covered,
       CAST(DATEDIFF(second, @b0, @r0) AS float) AS wanted
FROM sys.query_store_runtime_stats_interval
WHERE start_time >= @b0 AND start_time < @r0
"#,
        snap = snapped(b0, r0),
    )
}

fn texts_sql(ids: &str) -> String {
    format!(
        "SELECT q.query_id, LEFT(t.query_sql_text, 4000) AS query_sql_text, OBJECT_NAME(q.object_id) AS object_name
         FROM sys.query_store_query q JOIN sys.query_store_query_text t ON t.query_text_id = q.query_text_id
         WHERE q.query_id IN ({ids})"
    )
}

pub(crate) async fn collect(
    client: &mut MsClient,
    baseline: &Period,
    recent: &Period,
) -> tiberius::Result<Option<PeriodData>> {
    let (Some(b0), Some(r0), Some(r1)) = (literal(&baseline.start), literal(&recent.start), literal(&recent.end))
    else {
        return Ok(None);
    };
    let mut data = PeriodData {
        source: TimelineSource::QueryStore,
        baseline: baseline.clone(),
        recent: recent.clone(),
        ..Default::default()
    };
    for r in query(client, &totals_sql(&b0, &r0, &r1)).await? {
        let stats = PeriodStats::new(r.f64_or_zero("executions"), r.f64_or_zero("total_ms"));
        let snapped =
            Period { start: r.str("period_start").unwrap_or_default(), end: r.str("period_end").unwrap_or_default() };
        if r.f64_or_zero("recent") >= 1.0 {
            data.recent_total = stats;
            data.recent = snapped;
        } else {
            data.baseline_total = stats;
            data.baseline = snapped;
        }
    }
    // Snapped periods can't run past what was asked for.
    data.recent.end = min_iso(&data.recent.end, &recent.end);
    data.baseline.end = min_iso(&data.baseline.end, &data.recent.start);
    if data.baseline.start.is_empty() {
        data.baseline = baseline.clone();
    }
    if data.recent.start.is_empty() {
        data.recent = recent.clone();
    }
    let coverage = query(client, &coverage_sql(&b0, &r0)).await?.into_iter().next();
    let covered = coverage.as_ref().and_then(|r| r.f64("covered")).unwrap_or(0.0);
    let wanted = coverage.as_ref().and_then(|r| r.f64("wanted")).unwrap_or(baseline.hours() * 3600.0);
    data.baseline_coverage = if wanted > 0.0 { (covered / wanted).clamp(0.0, 1.0) } else { 0.0 };

    let mut queries: BTreeMap<String, QueryPeriods> = BTreeMap::new();
    for r in query(client, &per_plan_sql(&b0, &r0, &r1)).await? {
        let query_id = r.string("query_id");
        let mut stats = PeriodStats::new(r.f64_or_zero("executions"), r.f64_or_zero("total_ms"));
        stats.min_ms = r.f64("min_ms");
        stats.max_ms = r.f64("max_ms");
        let is_recent = r.f64_or_zero("recent") >= 1.0;
        let q = queries.entry(query_id.clone()).or_insert_with(|| QueryPeriods { query_id, ..Default::default() });
        let side = if is_recent { &mut q.recent } else { &mut q.baseline };
        match side {
            Some(existing) => existing.add(&stats),
            None => *side = Some(stats.clone()),
        }
        let plan_id = r.string("plan_id");
        let plan = match q.plans.iter().position(|p| p.id == plan_id) {
            Some(i) => &mut q.plans[i],
            None => {
                q.plans.push(PlanPeriods { id: plan_id, ..Default::default() });
                q.plans.last_mut().expect("just pushed")
            }
        };
        if is_recent {
            plan.recent = Some(stats);
            plan.first_recent = r.str("first_start");
        } else {
            plan.baseline = Some(stats);
        }
    }

    let ids = crate::workload::id_list(&queries.keys().cloned().collect::<Vec<_>>());
    if ids != "NULL" {
        for r in query(client, &texts_sql(&ids)).await? {
            if let Some(q) = queries.get_mut(&r.string("query_id")) {
                let (_, sql) = split_parameters(&r.string("query_sql_text"));
                q.text = sql;
                q.object_name = r.str("object_name");
            }
        }
    }
    data.queries = queries.into_values().collect();
    Ok(Some(data))
}

fn min_iso(a: &str, b: &str) -> String {
    match (culprit_core::changes::epoch(a), culprit_core::changes::epoch(b)) {
        (Some(x), Some(y)) if y < x => b.to_string(),
        (Some(_), _) => a.to_string(),
        _ => b.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamps_become_literals_or_nothing() {
        assert_eq!(literal("2026-10-11T14:05:00Z").as_deref(), Some("'2026-10-11T14:05:00+00:00'"));
        assert_eq!(literal("2026-10-11T16:05:00+02:00").as_deref(), Some("'2026-10-11T14:05:00+00:00'"));
        assert!(literal("2026-10-11'; DROP TABLE x; --").is_none());
    }
}
