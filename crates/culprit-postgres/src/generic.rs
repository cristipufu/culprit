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

/// The generic plan of one statement (already checked to be exactly one) as EXPLAIN JSON, planned the way
/// `crate::trust` says: in a read-only transaction that is rolled back, and for an elevated login only while no other
/// role's code is in reach.
pub async fn plan_json(client: &Client, statement: &str, version_num: u32, elevated: bool) -> Result<String> {
    if version_num < MIN_VERSION {
        return Err(Error::new(
            ErrorCode::Unsupported,
            "Planning statements with $n placeholders needs PostgreSQL 12 or newer",
        )
        .with_hint("Replace the placeholders with sample values to see its plan."));
    }
    crate::trust::begin(client, elevated).await?;
    let planned = if version_num >= 160_000 {
        query(client, &format!("EXPLAIN (FORMAT JSON, VERBOSE, GENERIC_PLAN) {statement}")).await.and_then(first_plan)
    } else {
        via_prepared(client, statement).await
    };
    crate::trust::end(client).await;
    planned
}

/// Inside [`plan_json`]'s transaction: the setting goes with it. A prepared statement outlives a rollback, so it is
/// deallocated whatever EXPLAIN does, from a savepoint a failed EXPLAIN can't take with it.
async fn via_prepared(client: &Client, statement: &str) -> Result<String> {
    let run = |sql: String| async move { client.batch_execute(&tagged(&sql)).await.map_err(crate::connect::map_error) };
    run("SET LOCAL plan_cache_mode = force_generic_plan".into()).await?;
    // One left from a DEALLOCATE that failed would stop every PREPARE after it.
    if query_one(client, &format!("SELECT 1 AS left FROM pg_catalog.pg_prepared_statements WHERE name = '{NAME}'"))
        .await?
        .is_some()
    {
        run(format!("DEALLOCATE {NAME}")).await?;
    }
    run(format!("SAVEPOINT {NAME}")).await?;
    run(format!("PREPARE {NAME} AS {statement}")).await?;
    let explained = async {
        let parameters = query_one(
            client,
            &format!("SELECT coalesce(pg_catalog.array_length(parameter_types, 1), 0) AS n FROM pg_catalog.pg_prepared_statements WHERE name = '{NAME}'"),
        )
        .await?
        .and_then(|r| r.number("n"))
        .unwrap_or(0.0) as usize;
        let args = if parameters == 0 { String::new() } else { format!("({})", vec!["NULL"; parameters].join(", ")) };
        first_plan(query(client, &format!("EXPLAIN (FORMAT JSON, VERBOSE) EXECUTE {NAME}{args}")).await?)
    }
    .await;
    if explained.is_err() {
        let _ = run(format!("ROLLBACK TO SAVEPOINT {NAME}")).await;
    }
    let _ = run(format!("DEALLOCATE {NAME}")).await;
    explained
}

fn first_plan(rows: Vec<crate::rows::Row>) -> Result<String> {
    rows.first().and_then(|r| r.text("QUERY PLAN")).ok_or_else(|| Error::internal("EXPLAIN returned no plan"))
}
