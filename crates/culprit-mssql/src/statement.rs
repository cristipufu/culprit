use std::time::Instant;

use culprit_core::{Error, ErrorCode, Plan, Result, ResultSet};
use futures_util::TryStreamExt;
use tiberius::QueryItem;

use crate::conn::{MsClient, execute, map_error};
use crate::plan_xml;
use crate::value::to_json;

/// Column name SQL Server gives the XML plan returned by SET SHOWPLAN_XML / SET STATISTICS XML.
const SHOWPLAN_COLUMN_MARKER: &str = "Showplan";

pub(crate) async fn estimated_plan(client: &mut MsClient, sql: &str) -> Result<Plan> {
    execute(client, "SET SHOWPLAN_XML ON").await.map_err(map_error)?;
    let documents = collect_plans(client, sql).await;
    let reset = execute(client, "SET SHOWPLAN_XML OFF").await;
    let documents = documents?;
    reset.map_err(map_error)?;
    best_plan(&documents)
}

pub(crate) async fn actual_plan(client: &mut MsClient, sql: &str) -> Result<Plan> {
    execute(client, "SET STATISTICS XML ON").await.map_err(map_error)?;
    let documents = collect_plans(client, sql).await;
    let reset = execute(client, "SET STATISTICS XML OFF").await;
    let documents = documents?;
    reset.map_err(map_error)?;
    best_plan(&documents)
}

/// Every plan document the batch produced; SHOWPLAN returns one per batch, STATISTICS XML one per statement.
async fn collect_plans(client: &mut MsClient, sql: &str) -> Result<Vec<String>> {
    let mut stream = client.simple_query(sql).await.map_err(map_error)?.into_row_stream();
    let mut documents = Vec::new();
    while let Some(row) = stream.try_next().await.map_err(map_error)? {
        let is_plan = row.columns().first().is_some_and(|c| c.name().contains(SHOWPLAN_COLUMN_MARKER));
        if !is_plan {
            continue;
        }
        if let Some((_, data)) = row.cells().next()
            && let serde_json::Value::String(xml) = to_json(data)
        {
            documents.push(xml);
        }
    }
    Ok(documents)
}

fn best_plan(documents: &[String]) -> Result<Plan> {
    documents
        .iter()
        .filter_map(|xml| plan_xml::parse(xml).ok())
        .max_by(|a, b| a.total_cost.total_cmp(&b.total_cost))
        .ok_or_else(|| Error::new(ErrorCode::QueryFailed, "SQL Server returned no plan for this statement"))
}

/// Runs on a dedicated session that is discarded afterwards, so stopping early at `limit` rows is safe.
pub(crate) async fn run(client: &mut MsClient, sql: &str, limit: usize) -> Result<ResultSet> {
    let started = Instant::now();
    let mut stream = client.simple_query(sql).await.map_err(map_error)?;
    let mut columns: Vec<String> = Vec::new();
    let mut rows = Vec::new();
    let mut truncated = false;
    let mut result_sets = 0usize;
    let mut first_index: Option<usize> = None;
    while let Some(item) = stream.try_next().await.map_err(map_error)? {
        match item {
            QueryItem::Metadata(meta) => {
                result_sets += 1;
                if first_index.is_none() {
                    first_index = Some(meta.result_index());
                    columns = meta.columns().iter().map(|c| c.name().to_string()).collect();
                }
            }
            QueryItem::Row(row) => {
                if Some(row.result_index()) != first_index {
                    continue;
                }
                if rows.len() >= limit {
                    truncated = true;
                    break;
                }
                rows.push(row.cells().map(|(_, data)| to_json(data)).collect());
            }
        }
    }
    let mut messages = Vec::new();
    if result_sets == 0 {
        messages.push("The statement completed without returning rows.".to_string());
    }
    if result_sets > 1 {
        messages.push(format!("The batch returned {result_sets} result sets; showing the first."));
    }
    Ok(ResultSet { columns, rows, truncated, elapsed_ms: started.elapsed().as_secs_f64() * 1_000.0, messages })
}
