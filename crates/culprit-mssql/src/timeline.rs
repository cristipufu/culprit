//! Database time per bucket from Query Store's runtime-stats intervals, for the whole workload and its heaviest queries.

use std::collections::HashMap;

use culprit_core::{QueryTrend, Timeline, TimelineSource};

use crate::conn::{MsClient, OWN_QUERY_MARKER, query};

/// Enough points for a readable chart and a small report: a day in 15-minute buckets, a week in 2-hour ones.
const MAX_BUCKETS: f64 = 120.0;
const BUCKET_MINUTES: [u32; 10] = [15, 30, 60, 120, 180, 240, 360, 480, 720, 1440];

/// The smallest bucket that is at least one Query Store interval wide and keeps the window under [`MAX_BUCKETS`].
pub(crate) fn bucket_minutes(history_hours: f64, interval_minutes: u32) -> u32 {
    let minutes = history_hours.max(0.0) * 60.0;
    BUCKET_MINUTES
        .into_iter()
        .find(|&b| b >= interval_minutes && minutes / f64::from(b) <= MAX_BUCKETS)
        .unwrap_or_else(|| interval_minutes.max(1440))
}

/// Bucket number of an interval's start: whole buckets since 1970-01-01 UTC.
fn bucket_expr(minutes: u32) -> String {
    format!("DATEDIFF(minute, '19700101', CAST(SWITCHOFFSET(i.start_time, '+00:00') AS datetime2(0))) / {minutes}")
}

fn totals_sql(window_minutes: u32, minutes: u32) -> String {
    let bucket = bucket_expr(minutes);
    format!(
        r#"
DECLARE @since datetimeoffset = DATEADD(minute, -{window_minutes}, SYSDATETIMEOFFSET());
SELECT {bucket} AS bucket,
  SUM(rs.count_executions) AS executions,
  SUM(rs.avg_duration * rs.count_executions) / 1000.0 AS total_ms
FROM sys.query_store_runtime_stats rs
JOIN sys.query_store_runtime_stats_interval i ON i.runtime_stats_interval_id = rs.runtime_stats_interval_id
JOIN sys.query_store_plan p ON p.plan_id = rs.plan_id
JOIN sys.query_store_query q ON q.query_id = p.query_id
JOIN sys.query_store_query_text t ON t.query_text_id = q.query_text_id
WHERE i.end_time >= @since AND t.query_sql_text NOT LIKE N'%{marker}%'
GROUP BY {bucket}
"#,
        marker = OWN_QUERY_MARKER,
    )
}

fn per_query_sql(window_minutes: u32, minutes: u32, ids: &str) -> String {
    let bucket = bucket_expr(minutes);
    format!(
        r#"
DECLARE @since datetimeoffset = DATEADD(minute, -{window_minutes}, SYSDATETIMEOFFSET());
SELECT p.query_id, {bucket} AS bucket,
  SUM(rs.count_executions) AS executions,
  SUM(rs.avg_duration * rs.count_executions) / 1000.0 AS total_ms
FROM sys.query_store_runtime_stats rs
JOIN sys.query_store_runtime_stats_interval i ON i.runtime_stats_interval_id = rs.runtime_stats_interval_id
JOIN sys.query_store_plan p ON p.plan_id = rs.plan_id
WHERE i.end_time >= @since AND p.query_id IN ({ids})
GROUP BY p.query_id, {bucket}
"#
    )
}

const INTERVAL_SQL: &str =
    "SELECT CAST(interval_length_minutes AS int) AS minutes FROM sys.database_query_store_options";

/// The workload timeline plus a trend for each of `query_ids`, aligned to the same buckets.
pub(crate) async fn collect(
    client: &mut MsClient,
    window_minutes: u32,
    history_hours: f64,
    window_end: Option<&str>,
    query_ids: &[String],
) -> tiberius::Result<Option<(Timeline, HashMap<String, QueryTrend>)>> {
    let interval = query(client, INTERVAL_SQL)
        .await
        .ok()
        .and_then(|rows| rows.into_iter().next())
        .and_then(|r| r.f64("minutes"))
        .map(|m| m.max(1.0) as u32)
        .unwrap_or(60);
    let minutes = bucket_minutes(history_hours.min(f64::from(window_minutes) / 60.0), interval);
    let totals: Vec<(i64, f64, f64)> = query(client, &totals_sql(window_minutes, minutes))
        .await?
        .iter()
        .filter_map(|r| Some((r.f64("bucket")? as i64, r.f64_or_zero("total_ms"), r.f64_or_zero("executions"))))
        .collect();
    let (Some(first), Some(last)) = (totals.iter().map(|t| t.0).min(), totals.iter().map(|t| t.0).max()) else {
        return Ok(None);
    };
    let len = (last - first + 1) as usize;
    let mut timeline = Timeline {
        source: TimelineSource::QueryStore,
        starts: (first..=last).map(|b| iso_minutes(b * i64::from(minutes))).collect(),
        end: window_end.map(str::to_string).unwrap_or_else(|| iso_minutes((last + 1) * i64::from(minutes))),
        time_ms: vec![0.0; len],
        executions: vec![0.0; len],
        unknown: None,
    };
    for (bucket, time, runs) in totals {
        let i = (bucket - first) as usize;
        timeline.time_ms[i] = round(time);
        timeline.executions[i] = runs.round();
    }

    let ids = crate::workload::id_list(query_ids);
    let mut trends: HashMap<String, QueryTrend> = query_ids
        .iter()
        .map(|id| (id.clone(), QueryTrend { time_ms: vec![Some(0.0); len], executions: vec![Some(0.0); len] }))
        .collect();
    if ids != "NULL" {
        for r in query(client, &per_query_sql(window_minutes, minutes, &ids)).await? {
            let Some(bucket) = r.f64("bucket").map(|b| b as i64) else { continue };
            let Some(trend) = trends.get_mut(&r.string("query_id")) else { continue };
            if bucket < first || bucket > last {
                continue;
            }
            let i = (bucket - first) as usize;
            trend.time_ms[i] = Some(round(r.f64_or_zero("total_ms")));
            trend.executions[i] = Some(r.f64_or_zero("executions").round());
        }
    }
    Ok(Some((timeline, trends)))
}

/// Milliseconds to two decimals: enough for a chart, and it keeps the report small.
fn round(ms: f64) -> f64 {
    (ms * 100.0).round() / 100.0
}

fn iso_minutes(minutes_since_epoch: i64) -> String {
    chrono::DateTime::from_timestamp(minutes_since_epoch * 60, 0)
        .map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buckets_fit_the_window() {
        assert_eq!(bucket_minutes(5.0, 15), 15);
        assert_eq!(bucket_minutes(5.0, 60), 60);
        assert_eq!(bucket_minutes(24.0, 60), 60);
        assert_eq!(bucket_minutes(24.0 * 7.0, 60), 120);
        assert_eq!(bucket_minutes(24.0 * 30.0, 60), 360);
        assert_eq!(bucket_minutes(24.0 * 30.0, 1440), 1440);
        assert_eq!(bucket_minutes(24.0 * 400.0, 60), 1440);
    }

    #[test]
    fn bucket_starts_are_utc_iso() {
        assert_eq!(iso_minutes(29_000_000), "2025-02-19T21:20:00Z");
    }
}
