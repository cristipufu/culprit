//! What actually happened, read from DMVs only: the last actual plan SQL Server kept for a query
//! (sys.dm_exec_query_plan_stats, 2019+) and the live progress of a running request (sys.dm_exec_query_profiles and
//! sys.dm_exec_query_statistics_xml). Nothing here runs the user's statement.

use culprit_core::{
    Engine, Error, ErrorCode, NodeProgress, NodeState, Plan, PlanKind, QueryProgress, Result, session_number,
};

use crate::conn::{PERMISSION_HINT, Pool, is_permission_error, query};
use crate::plan_xml;
use crate::value::Rec;

const SETTING_SQL: &str =
    "SELECT CAST(value AS int) AS value FROM sys.database_scoped_configurations WHERE name = 'LAST_QUERY_PLAN_STATS'";

const TURN_ON: &str = "ALTER DATABASE SCOPED CONFIGURATION SET LAST_QUERY_PLAN_STATS = ON;";

fn numeric(id: &str) -> Result<&str> {
    let id = id.trim();
    if id.is_empty() || id.len() > 19 || !id.bytes().all(|b| b.is_ascii_digit()) {
        return Err(Error::invalid("Query Store ids are numbers"));
    }
    Ok(id)
}

/// The cached plan of a Query Store query, with the runtime counters of its last execution.
pub(crate) async fn last_actual_plan(pool: &Pool, query_id: &str, plan_id: Option<&str>) -> Result<Plan> {
    let query_id = numeric(query_id)?;
    let plan_filter = match plan_id {
        Some(id) => format!(" AND p.plan_id = {}", numeric(id)?),
        None => String::new(),
    };
    let sql = format!(
        "SELECT TOP (1) CONVERT(nvarchar(max), ps.query_plan) AS query_plan, CONVERT(varchar(20), q.query_hash, 1) AS query_hash,
                CONVERT(varchar(33), DATEADD(minute, DATEDIFF(minute, GETDATE(), GETUTCDATE()), qs.last_execution_time), 126) AS last_run,
                qs.execution_count
           FROM sys.query_store_query q
           JOIN sys.query_store_plan p ON p.query_id = q.query_id{plan_filter}
           JOIN sys.dm_exec_query_stats qs ON qs.query_hash = q.query_hash AND qs.query_plan_hash = p.query_plan_hash
          CROSS APPLY sys.dm_exec_query_plan_stats(qs.plan_handle) ps
          WHERE q.query_id = {query_id}
          ORDER BY qs.last_execution_time DESC"
    );
    let rows = pool
        .with(|c| {
            let sql = sql.clone();
            Box::pin(async move {
                let setting = query(c, SETTING_SQL).await.ok().and_then(|r| r.first().and_then(|r| r.f64("value")));
                let rows = query(c, &sql).await;
                Ok((setting, rows))
            })
        })
        .await?;
    let (setting, rows) = rows;
    let rows = rows.map_err(|err| {
        if is_permission_error(&err) {
            Error::new(ErrorCode::PermissionDenied, "This login can't read the plan cache").with_hint(PERMISSION_HINT)
        } else if err.to_string().contains("dm_exec_query_plan_stats") {
            Error::new(ErrorCode::Unsupported, "Keeping the last actual plan needs SQL Server 2019 or newer")
                .with_hint("Measure the query instead: open it in the editor with real values and use Analyze.")
        } else {
            crate::conn::map_error(err)
        }
    })?;
    let off = setting == Some(0.0);
    let not_kept = || {
        Error::new(ErrorCode::Unsupported, "SQL Server isn't keeping actual plans for this database")
            .with_hint(format!("Turn on LAST_QUERY_PLAN_STATS ({TURN_ON}), let the query run once, then look again."))
    };
    let Some(row) = rows.into_iter().next() else {
        if off {
            return Err(not_kept());
        }
        return Err(Error::not_found("This query's plan isn't in the plan cache right now").with_hint(
            "SQL Server keeps the last actual plan only while the plan is cached. It comes back the next time the query runs.",
        ));
    };
    let xml = row.str("query_plan").ok_or_else(not_kept)?;
    let mut plan = plan_xml::parse_statement(&xml, row.str("query_hash").as_deref())?;
    if plan.kind != PlanKind::Actual {
        return Err(not_kept());
    }
    plan.raw = None;
    if let Some(at) = row.str("last_run") {
        plan.props.insert("LastRunAt".into(), format!("{at}Z"));
    }
    clear_unrecorded_times(&mut plan);
    if let Some(n) = row.f64("execution_count") {
        plan.props.insert("CachedExecutions".into(), n.to_string());
    }
    Ok(plan)
}

// Lightweight profiling keeps row counts for past runs, not time. Withdraw its placeholder zeros in both models.
fn clear_unrecorded_times(plan: &mut Plan) {
    let facts = plan.facts_mut();
    if facts.elapsed_ms == Some(0.0) {
        facts.elapsed_ms = None;
    }
    if facts.cpu_ms == Some(0.0) {
        facts.cpu_ms = None;
    }
    for key in ["ElapsedMs", "CpuMs"] {
        if plan.props.get(key).is_some_and(|v| v == "0") {
            plan.props.remove(key);
        }
    }
}

const REQUEST_SQL: &str = r#"
SELECT r.status, r.command, r.wait_type, r.wait_time, r.total_elapsed_time AS elapsed_ms, r.cpu_time AS cpu_ms,
       r.logical_reads, r.percent_complete, CONVERT(varchar(20), r.query_hash, 1) AS query_hash,
       SUBSTRING(t.text, r.statement_start_offset / 2 + 1,
         (CASE r.statement_end_offset WHEN -1 THEN DATALENGTH(t.text) ELSE r.statement_end_offset END - r.statement_start_offset) / 2 + 1) AS statement_text,
       CONVERT(nvarchar(max), sx.query_plan) AS live_plan,
       qp.query_plan AS cached_plan
  FROM sys.dm_exec_requests r
 OUTER APPLY sys.dm_exec_sql_text(r.sql_handle) t
 OUTER APPLY sys.dm_exec_query_statistics_xml(r.session_id) sx
 OUTER APPLY sys.dm_exec_text_query_plan(r.plan_handle, r.statement_start_offset, r.statement_end_offset) qp
 WHERE r.session_id = {id}
"#;

/// Servers without sys.dm_exec_query_statistics_xml (2014 and older) or without permission to call it.
const REQUEST_BASIC_SQL: &str = r#"
SELECT r.status, r.command, r.wait_type, r.wait_time, r.total_elapsed_time AS elapsed_ms, r.cpu_time AS cpu_ms,
       r.logical_reads, r.percent_complete, CONVERT(varchar(20), r.query_hash, 1) AS query_hash,
       SUBSTRING(t.text, r.statement_start_offset / 2 + 1,
         (CASE r.statement_end_offset WHEN -1 THEN DATALENGTH(t.text) ELSE r.statement_end_offset END - r.statement_start_offset) / 2 + 1) AS statement_text,
       qp.query_plan AS cached_plan
  FROM sys.dm_exec_requests r
 OUTER APPLY sys.dm_exec_sql_text(r.sql_handle) t
 OUTER APPLY sys.dm_exec_text_query_plan(r.plan_handle, r.statement_start_offset, r.statement_end_offset) qp
 WHERE r.session_id = {id}
"#;

/// One row per operator, added up across threads. Lightweight profiling (the default) leaves the open/close timestamps
/// at zero, so state comes from rows, scans finished and re-executions instead.
const PROFILES_SQL: &str = r#"
SELECT node_id, SUM(row_count) AS row_count, MAX(estimate_row_count) AS estimate_row_count,
       MAX(elapsed_time_ms) AS elapsed_ms, SUM(cpu_time_ms) AS cpu_ms, SUM(logical_read_count) AS logical_reads,
       SUM(end_of_scan_count) AS end_of_scans, SUM(rebind_count) + SUM(rewind_count) AS replays,
       MIN(CASE WHEN close_time > 0 THEN 1 ELSE 0 END) AS closed
  FROM sys.dm_exec_query_profiles
 WHERE session_id = {id}
 GROUP BY node_id
"#;

const NO_PROFILING: &str = "Per-operator progress needs lightweight query profiling: on by default since SQL Server \
2019 (LIGHTWEIGHT_QUERY_PROFILING), trace flag 7412 on 2016 SP1 and 2017. Culprit shows the estimated plan and how \
long the statement has been running.";

pub(crate) async fn query_progress(pool: &Pool, session: &str) -> Result<QueryProgress> {
    let id = session_number(session)?;
    let taken_at = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string();
    let (request, profiles) = pool
        .with(|c| {
            Box::pin(async move {
                let request = match query(c, &REQUEST_SQL.replace("{id}", &id.to_string())).await {
                    Ok(rows) => rows,
                    Err(_) => query(c, &REQUEST_BASIC_SQL.replace("{id}", &id.to_string())).await?,
                };
                let profiles = query(c, &PROFILES_SQL.replace("{id}", &id.to_string())).await.unwrap_or_default();
                Ok((request, profiles))
            })
        })
        .await?;
    let mut progress = QueryProgress::idle(session, Engine::Mssql, taken_at);
    let Some(r) = request.into_iter().next() else {
        return Ok(progress);
    };
    progress.running = true;
    progress.state = r.str("status");
    progress.wait = r.str("wait_type");
    progress.wait_ms = r.f64("wait_time").filter(|w| *w > 0.0);
    progress.elapsed_ms = r.f64("elapsed_ms");
    progress.cpu_ms = r.f64("cpu_ms");
    progress.logical_reads = r.f64("logical_reads");
    progress.statement = r.str("statement_text").map(|s| s.trim().to_string());
    let hash = r.str("query_hash");
    let live = r.str("live_plan").and_then(|xml| plan_xml::parse_statement(&xml, hash.as_deref()).ok());
    let plan =
        live.or_else(|| r.str("cached_plan").and_then(|xml| plan_xml::parse_statement(&xml, hash.as_deref()).ok()));
    progress.plan = plan.map(|mut p| {
        p.raw = None;
        p
    });
    progress.nodes = node_progress(&profiles, progress.plan.as_ref());
    // Backups, index rebuilds, DBCC and a few other commands report their own percentage; trust it over row counts.
    match r.f64("percent_complete").filter(|p| *p > 0.0) {
        Some(pct) => {
            progress.percent = Some((pct / 100.0).clamp(0.0, 1.0));
            progress.percent_basis = Some(format!("SQL Server's own estimate for {}", r.string("command").trim()));
            if !progress.nodes.is_empty() {
                let percent = progress.percent;
                let basis = progress.percent_basis.clone();
                progress.summarize_nodes();
                progress.percent = percent;
                progress.percent_basis = basis;
            }
        }
        None => progress.summarize_nodes(),
    }
    if progress.nodes.is_empty() {
        progress.note = Some(NO_PROFILING.to_string());
    }
    Ok(progress)
}

struct Counters {
    rows: f64,
    est: f64,
    elapsed: Option<f64>,
    cpu: Option<f64>,
    reads: Option<f64>,
    end_of_scans: f64,
    replays: f64,
    closed: bool,
}

fn counters(r: &Rec) -> Counters {
    Counters {
        rows: r.f64_or_zero("row_count"),
        est: r.f64_or_zero("estimate_row_count"),
        elapsed: r.f64("elapsed_ms"),
        cpu: r.f64("cpu_ms"),
        reads: r.f64("logical_reads"),
        end_of_scans: r.f64_or_zero("end_of_scans"),
        replays: r.f64_or_zero("replays"),
        closed: r.f64("closed") == Some(1.0),
    }
}

fn producing(c: &Counters) -> bool {
    c.rows > 0.0 || c.elapsed.is_some_and(|t| t > 0.0) || c.cpu.is_some_and(|t| t > 0.0)
}

/// An operator ran to completion when every thread closed it, or it finished its scan without being re-executed, or
/// its inputs are done and it produced (about) what was estimated.
fn finished(c: &Counters, est: f64, inputs_done: bool) -> bool {
    // A re-executed operator (the inner side of a loop) is only done when its driver is; its counters can't say.
    c.closed
        || (c.replays <= 1.0
            && ((c.end_of_scans >= 1.0 && c.rows >= est * 0.99) || (inputs_done && est >= 1.0 && c.rows >= est * 0.9)))
}

fn node_progress(profiles: &[Rec], plan: Option<&Plan>) -> Vec<NodeProgress> {
    let raw: std::collections::HashMap<u32, Counters> =
        profiles.iter().filter_map(|r| Some((r.f64("node_id")? as u32, counters(r)))).collect();
    let progress = |id: u32, c: &Counters, est: f64, state: NodeState| NodeProgress {
        node_id: id,
        state,
        rows: c.rows,
        est_rows: est,
        elapsed_ms: c.elapsed,
        cpu_ms: c.cpu,
        logical_reads: c.reads,
    };
    let Some(plan) = plan else {
        // No plan to place operators in: judge each on its own counters.
        return raw
            .iter()
            .map(|(id, c)| {
                let state = if finished(c, c.est, true) {
                    NodeState::Done
                } else if producing(c) {
                    NodeState::Running
                } else {
                    NodeState::Pending
                };
                progress(*id, c, c.est, state)
            })
            .collect();
    };
    fn visit(
        node: &culprit_core::PlanNode,
        raw: &std::collections::HashMap<u32, Counters>,
        out: &mut Vec<(u32, NodeState, f64)>,
    ) -> NodeState {
        let inputs: Vec<NodeState> = node.children.iter().map(|c| visit(c, raw, out)).collect();
        let Some(c) = raw.get(&node.id) else {
            // Operators lightweight profiling doesn't track (Compute Scalar) follow their inputs.
            let state = if inputs.iter().all(|s| *s == NodeState::Done) && !inputs.is_empty() {
                NodeState::Done
            } else if inputs.iter().any(|s| *s != NodeState::Pending) {
                NodeState::Running
            } else {
                NodeState::Pending
            };
            return state;
        };
        let est = node.est_rows_total().max(c.est);
        let inputs_done = inputs.iter().all(|s| *s == NodeState::Done);
        let state = if finished(c, est, inputs_done) {
            NodeState::Done
        } else if producing(c) || inputs.iter().any(|s| *s != NodeState::Pending) {
            NodeState::Running
        } else {
            NodeState::Pending
        };
        out.push((node.id, state, est));
        state
    }
    let mut states = Vec::new();
    visit(&plan.root, &raw, &mut states);
    states.into_iter().filter_map(|(id, state, est)| raw.get(&id).map(|c| progress(id, c, est, state))).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lightweight_profile_zero_times_withdraw_typed_placeholders_and_keep_measured_fallbacks() {
        let snapshot: culprit_core::Snapshot =
            serde_json::from_str(include_str!("../../../fixtures/mssql-demo.json")).unwrap();
        let mut plan = snapshot.queries()[0].plan.as_ref().unwrap().clone();
        plan.kind = PlanKind::Actual;
        plan.root.children.clear();
        plan.root.act_time_ms = Some(10.0);
        plan.root.runtime = Some(culprit_core::NodeRuntime { cpu_ms: Some(2.0), ..Default::default() });
        plan.facts_mut().elapsed_ms = Some(0.0);
        plan.facts_mut().cpu_ms = Some(0.0);
        plan.props.insert("ElapsedMs".into(), "0".into());
        plan.props.insert("CpuMs".into(), "0".into());
        clear_unrecorded_times(&mut plan);
        let actual = culprit_core::actual_summary(&plan).unwrap();
        assert_eq!((actual.elapsed_ms, actual.cpu_ms), (Some(10.0), Some(2.0)));
        plan.facts_mut().elapsed_ms = Some(8.0);
        plan.facts_mut().cpu_ms = Some(3.0);
        clear_unrecorded_times(&mut plan);
        assert_eq!((plan.facts().elapsed_ms, plan.facts().cpu_ms), (Some(8.0), Some(3.0)));
    }

    fn counters(rows: f64, est: f64, end_of_scans: f64, replays: f64, elapsed: f64) -> Counters {
        Counters { rows, est, elapsed: Some(elapsed), cpu: None, reads: None, end_of_scans, replays, closed: false }
    }

    #[test]
    fn state_comes_from_counters_when_timestamps_are_zero() {
        // A finished scan, a spool replayed for every outer row, a loop still going.
        assert!(finished(&counters(1_500_000.0, 1_500_000.0, 0.0, 0.0, 69.0), 1_500_000.0, true));
        assert!(!finished(&counters(95_000_000.0, 104_000_000.0, 478.0, 478.0, 0.0), 104_000_000.0, true));
        assert!(!finished(&counters(9_000.0, 200_000.0, 0.0, 1.0, 22_000.0), 200_000.0, false));
        assert!(producing(&counters(0.0, 1.0, 0.0, 0.0, 5.0)));
        assert!(!producing(&Counters { elapsed: None, ..counters(0.0, 1.0, 0.0, 0.0, 0.0) }));
    }
}
