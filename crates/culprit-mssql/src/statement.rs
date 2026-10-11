use std::time::Instant;

use culprit_core::{Error, ErrorCode, Plan, Result, ResultSet};
use futures_util::TryStreamExt;
use tiberius::QueryItem;

use crate::conn::{MsClient, execute, map_error};
use crate::plan_xml;
use crate::value::to_json;

/// Column name SQL Server gives the XML plan returned by SET SHOWPLAN_XML / SET STATISTICS XML.
const SHOWPLAN_COLUMN_MARKER: &str = "Showplan";

/// SHOWPLAN compiles the text as it is: an application's parameters (`@P1`) aren't declared there, and SQL Server
/// can't plan a statement whose values it can't see the type of.
pub(crate) fn parameter_error(e: culprit_core::Error) -> culprit_core::Error {
    // Query Store keeps an application's text with its parameter list in front: `(@P1 int)SELECT …`.
    if e.message.contains("only allowed for parameterized queries") {
        return culprit_core::Error::invalid(
            "This text starts with a parameter list ((@P1 int)…), which SQL Server only accepts from an application",
        )
        .with_hint(
            "Delete the leading parameter list and replace each parameter with a sample value of its type. Queries \
             Culprit captured open from Queries with the plan SQL Server actually used.",
        );
    }
    if !e.message.contains("Must declare the scalar variable") {
        return e;
    }
    let name = e.message.split('"').nth(1).unwrap_or("parameters").to_string();
    culprit_core::Error::invalid(format!("This statement uses {name}, which SQL Server needs a value for")).with_hint(
        "Replace each parameter with a sample value of its type (a quoted string for text, a number for an id) and \
             try again. Queries Culprit captured open from Queries with the plan SQL Server actually used.",
    )
}

pub(crate) async fn estimated_plan(client: &mut MsClient, sql: &str) -> Result<Plan> {
    execute(client, "SET SHOWPLAN_XML ON").await.map_err(map_error)?;
    let documents = collect_plans(client, sql).await;
    let reset = execute(client, "SET SHOWPLAN_XML OFF").await;
    let documents = documents?;
    reset.map_err(map_error)?;
    best_plan(&documents)
}

/// The server's own reading of a batch, without running it: SHOWPLAN compiles every statement and executes none.
pub(crate) async fn statement_types(client: &mut MsClient, sql: &str) -> Result<Vec<String>> {
    execute(client, "SET SHOWPLAN_XML ON").await.map_err(map_error)?;
    let documents = collect_plans(client, sql).await;
    let reset = execute(client, "SET SHOWPLAN_XML OFF").await;
    let documents = documents?;
    reset.map_err(map_error)?;
    let mut types = Vec::new();
    for xml in &documents {
        types.extend(plan_xml::statement_types(xml)?);
    }
    Ok(types)
}

/// Runs the statement to measure it. SQL Server has no read-only transactions, so it always runs inside one that is
/// rolled back: a write measured on a non-production connection changes nothing (the Workspace never lets a write
/// reach here on production).
pub(crate) async fn actual_plan(client: &mut MsClient, sql: &str) -> Result<Plan> {
    execute(client, "SET STATISTICS XML ON").await.map_err(map_error)?;
    let documents = collect_plans(client, &measured_batch(sql)).await;
    let _ = execute(client, "IF @@TRANCOUNT > 0 ROLLBACK TRANSACTION").await;
    let reset = execute(client, "SET STATISTICS XML OFF").await;
    let documents = documents?;
    reset.map_err(map_error)?;
    let own: Vec<String> = documents.into_iter().filter(|xml| !is_wrapper_plan(xml)).collect();
    best_plan(&own)
}

const WRAPPER_START: &str = "SET XACT_ABORT ON; BEGIN TRANSACTION;";
const WRAPPER_END: &str = "IF @@TRANCOUNT > 0 ROLLBACK TRANSACTION;";

/// The statement inside a transaction that is always rolled back, so measuring a write changes nothing.
pub(crate) fn measured_batch(sql: &str) -> String {
    format!("{WRAPPER_START}\n{sql}\n;{WRAPPER_END}")
}

/// Plan documents STATISTICS XML returns for Culprit's own transaction handling rather than the user's statement.
fn is_wrapper_plan(xml: &str) -> bool {
    let statements: Vec<&str> =
        xml.split("StatementText=\"").skip(1).filter_map(|rest| rest.split('"').next()).map(str::trim).collect();
    !statements.is_empty()
        && statements.iter().all(|s| {
            let upper = s.to_ascii_uppercase();
            upper.starts_with("IF @@TRANCOUNT")
                || upper.starts_with("SET XACT_ABORT")
                || upper.starts_with("BEGIN TRAN")
        })
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
        .ok_or_else(|| {
            Error::new(ErrorCode::Unsupported, "SQL Server returned no plan for this statement").with_hint(
                "It builds no plan for a statement that reads no table (SELECT 1, a variable assignment) or only \
                 declares or sets something. Explain a query that reads a table.",
            )
        })
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

/// A statement that changes data and returns no rows (INSERT/UPDATE/DELETE/MERGE without OUTPUT, DDL): run it through
/// the RPC path, which reports how many rows each statement touched.
pub(crate) async fn run_changes(client: &mut MsClient, sql: &str) -> Result<ResultSet> {
    let started = Instant::now();
    let done = client.execute(sql, &[]).await.map_err(map_error)?;
    let affected: u64 = done.rows_affected().iter().sum();
    Ok(ResultSet {
        columns: Vec::new(),
        rows: Vec::new(),
        truncated: false,
        elapsed_ms: started.elapsed().as_secs_f64() * 1_000.0,
        messages: vec![format!("{affected} row{} changed", if affected == 1 { "" } else { "s" })],
    })
}

/// Whether a statement can return rows, so the results grid is worth reading (SELECT, OUTPUT clauses, procedures).
pub(crate) fn returns_rows(sql: &str) -> bool {
    let verb = culprit_core::sql::check(sql, culprit_core::Engine::Mssql).verb;
    let output = sql.split(|c: char| !c.is_ascii_alphanumeric() && c != '_').any(|w| w.eq_ignore_ascii_case("OUTPUT"));
    output
        || !matches!(verb.as_str(), "INSERT" | "UPDATE" | "DELETE" | "MERGE" | "TRUNCATE" | "CREATE" | "ALTER" | "DROP")
}

#[cfg(test)]
mod changes_tests {
    use super::returns_rows;

    #[test]
    fn writes_without_output_take_the_count_path() {
        assert!(!returns_rows("UPDATE shop.orders SET status = 'x' WHERE id = 1"));
        assert!(!returns_rows("delete from t where id = 1"));
        assert!(returns_rows("UPDATE t SET a = 1 OUTPUT inserted.id WHERE id = 1"));
        assert!(returns_rows("SELECT 1"));
        assert!(returns_rows("EXEC dbo.report"));
        assert!(returns_rows("DECLARE @x int = 1; SELECT @x"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn measuring_always_rolls_back() {
        let batch = measured_batch("UPDATE shop.orders SET note = '' WHERE id = 1");
        assert!(batch.starts_with("SET XACT_ABORT ON; BEGIN TRANSACTION;"));
        assert!(batch.contains("\nUPDATE shop.orders SET note = '' WHERE id = 1\n"));
        assert!(batch.ends_with("IF @@TRANCOUNT > 0 ROLLBACK TRANSACTION;"));
    }

    #[test]
    fn only_the_users_statement_plan_is_kept() {
        let wrapper = r#"<ShowPlanXML><BatchSequence><Batch><Statements><StmtCond StatementText="IF @@TRANCOUNT &gt; 0" /></Statements></Batch></BatchSequence></ShowPlanXML>"#;
        let users = r#"<ShowPlanXML><BatchSequence><Batch><Statements><StmtSimple StatementText="UPDATE shop.orders SET note = ''" /></Statements></Batch></BatchSequence></ShowPlanXML>"#;
        assert!(is_wrapper_plan(wrapper));
        assert!(!is_wrapper_plan(users));
        assert!(!is_wrapper_plan("<ShowPlanXML/>"), "no statement text: keep it and let parsing decide");
    }
}

#[cfg(test)]
mod parameter_tests {
    use super::parameter_error;
    use culprit_core::{Error, ErrorCode};

    #[test]
    fn undeclared_parameters_get_a_plain_hint() {
        let raw = Error::new(ErrorCode::QueryFailed, "Must declare the scalar variable \"@P1\".");
        let mapped = parameter_error(raw);
        assert_eq!(mapped.message, "This statement uses @P1, which SQL Server needs a value for");
        assert!(mapped.hint.as_deref().is_some_and(|h| h.contains("sample value")));
        let prefixed = parameter_error(Error::new(
            ErrorCode::QueryFailed,
            "The batch could not be analyzed because of compile errors. This syntax is only allowed for parameterized queries.",
        ));
        assert!(prefixed.message.starts_with("This text starts with a parameter list"), "{}", prefixed.message);
        let other = parameter_error(Error::new(ErrorCode::QueryFailed, "Invalid object name 'x'."));
        assert_eq!(other.message, "Invalid object name 'x'.");
    }
}
