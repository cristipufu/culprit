//! Changes MySQL and MariaDB date themselves: a table's CREATE_TIME moves whenever ALTER TABLE rebuilds or changes it
//! (adding an index or a column included), routines and triggers keep when they were created or altered, and MySQL 8
//! records when each global setting was last SET. Catalog and performance_schema only.

use culprit_core::changes::epoch;
use culprit_core::{ChangeEvent, EventAction, EventKind, EventSource, Result};
use mysql_async::{Conn, Value};

use crate::rows::{query, query_with};

const MAX_ROWS: usize = 300;

fn utc(column: &str) -> String {
    format!("DATE_FORMAT({column}, '%Y-%m-%dT%H:%i:%sZ')")
}

/// Changes to `database` since `since`. The session has no default database, so these reads never count as the
/// application's workload where the server keeps no statement text to tell them apart (MySQL 5.7, MariaDB).
pub async fn changes(conn: &mut Conn, database: &str, since: &str) -> Result<Vec<ChangeEvent>> {
    // The session runs in UTC, so TIMESTAMP columns compare and print in UTC.
    let Some(since) = culprit_core::changes::sql_timestamp(since) else { return Ok(Vec::new()) };
    let schema = || vec![Value::from(database)];
    let mut out = Vec::new();
    let tables = query_with(
        conn,
        &format!(
            "SELECT TABLE_SCHEMA AS s, TABLE_NAME AS n, TABLE_TYPE AS kind, {} AS at FROM information_schema.TABLES
             WHERE TABLE_SCHEMA = ? AND CREATE_TIME > '{since}' ORDER BY CREATE_TIME DESC LIMIT {MAX_ROWS}",
            utc("CREATE_TIME")
        ),
        schema(),
    )
    .await?;
    for r in tables {
        let (Some(at), table) = (r.text("at"), format!("{}.{}", r.string("s"), r.string("n"))) else { continue };
        let view = r.string("kind").eq_ignore_ascii_case("VIEW");
        let event = if view {
            ChangeEvent::new(
                EventKind::Code,
                EventSource::Database,
                &table,
                &at,
                format!("View {table} was created or altered"),
            )
            .object(table)
        } else {
            ChangeEvent::new(
                EventKind::Table,
                EventSource::Database,
                &table,
                &at,
                format!("Table {table} was created or altered (its columns or indexes)"),
            )
            .table(table)
        };
        out.push(event.action(EventAction::Changed));
    }
    let routines = query_with(
        conn,
        &format!(
            "SELECT ROUTINE_SCHEMA AS s, ROUTINE_NAME AS n, ROUTINE_TYPE AS kind, {} AS created, {} AS altered
             FROM information_schema.ROUTINES WHERE ROUTINE_SCHEMA = ? AND LAST_ALTERED > '{since}'
             ORDER BY LAST_ALTERED DESC LIMIT {MAX_ROWS}",
            utc("CREATED"),
            utc("LAST_ALTERED")
        ),
        schema(),
    )
    .await
    .unwrap_or_default();
    for r in routines {
        let name = format!("{}.{}", r.string("s"), r.string("n"));
        let what = if r.string("kind").eq_ignore_ascii_case("PROCEDURE") { "Procedure" } else { "Function" };
        let (created, altered) = (r.text("created"), r.text("altered"));
        let new = created
            .as_deref()
            .and_then(epoch)
            .zip(altered.as_deref().and_then(epoch))
            .is_some_and(|(c, a)| a - c < 2.0);
        let (Some(at), action, verb) = (
            altered.or(created),
            if new { EventAction::Created } else { EventAction::Changed },
            if new { "was created" } else { "was altered" },
        ) else {
            continue;
        };
        out.push(
            ChangeEvent::new(EventKind::Code, EventSource::Database, &name, &at, format!("{what} {name} {verb}"))
                .action(action)
                .object(name),
        );
    }
    let triggers = query_with(
        conn,
        &format!(
            "SELECT TRIGGER_NAME AS n, EVENT_OBJECT_SCHEMA AS s, EVENT_OBJECT_TABLE AS t, {} AS at
             FROM information_schema.TRIGGERS WHERE TRIGGER_SCHEMA = ? AND CREATED > '{since}' LIMIT {MAX_ROWS}",
            utc("CREATED")
        ),
        schema(),
    )
    .await
    .unwrap_or_default();
    for r in triggers {
        let (Some(at), table, name) = (r.text("at"), format!("{}.{}", r.string("s"), r.string("t")), r.string("n"))
        else {
            continue;
        };
        out.push(
            ChangeEvent::new(
                EventKind::Code,
                EventSource::Database,
                &name,
                &at,
                format!("Trigger {name} on {table} was created"),
            )
            .action(EventAction::Created)
            .table(table)
            .object(name),
        );
    }
    // MySQL 8 dates every runtime SET; MariaDB has no such table. These only date setting changes the scans saw.
    let settings = query(
        conn,
        &format!(
            "SELECT VARIABLE_NAME AS n, {} AS at FROM performance_schema.variables_info
             WHERE VARIABLE_SOURCE IN ('DYNAMIC', 'PERSISTED') AND SET_TIME > '{since}' LIMIT {MAX_ROWS}",
            utc("SET_TIME")
        ),
    )
    .await
    .unwrap_or_default();
    for r in settings {
        let (Some(at), name) = (r.text("at"), r.string("n")) else { continue };
        out.push(
            ChangeEvent::new(EventKind::Setting, EventSource::Database, &name, &at, format!("{name} was set"))
                .action(EventAction::Changed)
                .object(name),
        );
    }
    Ok(out)
}
