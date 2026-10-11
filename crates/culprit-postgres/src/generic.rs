//! Generic plans: the plan PostgreSQL uses for a statement with `$n` placeholders, without running it.
//!
//! PostgreSQL 16 has `EXPLAIN (GENERIC_PLAN)`. 12 to 15 give the same plan when the statement is prepared under
//! `plan_cache_mode = force_generic_plan` and its EXECUTE is explained with NULLs: EXPLAIN without ANALYZE never runs
//! it, and a forced generic plan doesn't look at the values.

use culprit_core::{Error, ErrorCode, Result};
use tokio_postgres::Client;

use crate::rows::{query, query_one, tagged};

/// The oldest server with `plan_cache_mode`.
pub const MIN_VERSION: u32 = 120_000;

const NAME: &str = "culprit_generic_plan";

/// The generic plan of one statement (already checked to be exactly one) as EXPLAIN JSON.
pub async fn plan_json(client: &Client, statement: &str, version_num: u32) -> Result<String> {
    let rows = if version_num >= 160_000 {
        query(client, &format!("EXPLAIN (FORMAT JSON, VERBOSE, GENERIC_PLAN) {statement}")).await?
    } else if version_num >= MIN_VERSION {
        return via_prepared(client, statement).await;
    } else {
        return Err(Error::new(
            ErrorCode::Unsupported,
            "Planning statements with $n placeholders needs PostgreSQL 12 or newer",
        )
        .with_hint("Replace the placeholders with sample values to see its plan."));
    };
    first_plan(rows)
}

async fn via_prepared(client: &Client, statement: &str) -> Result<String> {
    client
        .batch_execute(&tagged("SET plan_cache_mode = force_generic_plan"))
        .await
        .map_err(crate::connect::map_error)?;
    let result = async {
        client
            .batch_execute(&tagged(&format!("PREPARE {NAME} AS {statement}")))
            .await
            .map_err(crate::connect::map_error)?;
        let explained = async {
            let parameters = query_one(
                client,
                &format!("SELECT coalesce(array_length(parameter_types, 1), 0) AS n FROM pg_prepared_statements WHERE name = '{NAME}'"),
            )
            .await?
            .and_then(|r| r.number("n"))
            .unwrap_or(0.0) as usize;
            let args = if parameters == 0 { String::new() } else { format!("({})", vec!["NULL"; parameters].join(", ")) };
            first_plan(query(client, &format!("EXPLAIN (FORMAT JSON, VERBOSE) EXECUTE {NAME}{args}")).await?)
        }
        .await;
        let _ = client.batch_execute(&tagged(&format!("DEALLOCATE {NAME}"))).await;
        explained
    }
    .await;
    let _ = client.batch_execute(&tagged("RESET plan_cache_mode")).await;
    result
}

fn first_plan(rows: Vec<crate::rows::Row>) -> Result<String> {
    rows.first().and_then(|r| r.text("QUERY PLAN")).ok_or_else(|| Error::internal("EXPLAIN returned no plan"))
}
