//! Server variables and status counters. MySQL 8 serves them from performance_schema; MariaDB has no
//! `performance_schema.global_variables`, and with performance_schema off neither has the status table, so every read
//! falls back to `SHOW GLOBAL …`, which all of them answer.

use std::collections::HashMap;

use culprit_core::Result;
use mysql_async::Conn;

use crate::rows::query;

fn names_list(names: &[&str]) -> String {
    names.iter().map(|n| format!("'{n}'")).collect::<Vec<_>>().join(", ")
}

/// `(name, value)` with lower-case names; names the server doesn't know are absent.
async fn read(conn: &mut Conn, table: &str, show: &str, names: &[&str]) -> Result<Vec<(String, String)>> {
    let list = names_list(names);
    let from_table = query(
        conn,
        &format!("SELECT VARIABLE_NAME AS name, VARIABLE_VALUE AS value FROM performance_schema.{table} WHERE VARIABLE_NAME IN ({list})"),
    )
    .await;
    let rows = match from_table {
        Ok(rows) if !rows.is_empty() => rows,
        Ok(_) | Err(_) => match query(conn, &format!("SHOW GLOBAL {show} WHERE Variable_name IN ({list})")).await {
            Ok(rows) => rows,
            Err(e) => return Err(e),
        },
    };
    Ok(rows
        .into_iter()
        .map(|r| {
            let name = r.text("name").or_else(|| r.text("variable_name")).unwrap_or_default().to_ascii_lowercase();
            let value = r.text("value").unwrap_or_default();
            (name, value)
        })
        .filter(|(name, _)| !name.is_empty())
        .collect())
}

pub async fn variables(conn: &mut Conn, names: &[&str]) -> Result<Vec<(String, String)>> {
    read(conn, "global_variables", "VARIABLES", names).await
}

/// Numeric status counters by their documented (capitalized) names.
pub async fn status(conn: &mut Conn, names: &[&str]) -> Result<HashMap<String, f64>> {
    let rows = read(conn, "global_status", "STATUS", names).await?;
    Ok(rows
        .into_iter()
        .filter_map(|(name, value)| {
            let canonical = names.iter().find(|n| n.eq_ignore_ascii_case(&name))?;
            Some((canonical.to_string(), value.trim().parse::<f64>().ok()?))
        })
        .collect())
}
