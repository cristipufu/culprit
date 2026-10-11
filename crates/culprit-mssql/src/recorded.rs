//! Changes SQL Server dates itself: `sys.objects` keeps when each table, view, procedure, function and trigger was
//! created and last altered (a table's date also moves when one of its indexes is created or altered). Catalog only.

use culprit_core::changes::epoch;
use culprit_core::{ChangeEvent, EventAction, EventKind, EventSource};

use crate::conn::{MsClient, query};
use crate::workload::utc_local_iso;

/// Objects listed per scan, newest change first.
const MAX_OBJECTS: usize = 300;

pub(crate) async fn changes(client: &mut MsClient, since: &str) -> tiberius::Result<Vec<ChangeEvent>> {
    // `since` is UTC and the dates are server-local; compare on the UTC side.
    let Some(since) = culprit_core::changes::sql_timestamp(since) else { return Ok(Vec::new()) };
    let sql = format!(
        "SELECT TOP ({MAX_OBJECTS}) s.name AS schema_name, o.name, RTRIM(o.type) AS type,
            {created} AS created, {modified} AS modified,
            OBJECT_SCHEMA_NAME(o.parent_object_id) AS parent_schema, OBJECT_NAME(o.parent_object_id) AS parent
         FROM sys.objects o JOIN sys.schemas s ON s.schema_id = o.schema_id
         WHERE o.is_ms_shipped = 0 AND o.type IN ('U', 'V', 'P', 'FN', 'IF', 'TF', 'TR')
           AND DATEADD(minute, DATEDIFF(minute, GETDATE(), GETUTCDATE()), o.modify_date) > CAST('{since}' AS datetime2(0))
         ORDER BY o.modify_date DESC",
        created = utc_local_iso("o.create_date"),
        modified = utc_local_iso("o.modify_date"),
    );
    let rows = query(client, &sql).await?;
    let since = epoch(&format!("{}Z", since.replace(' ', "T"))).unwrap_or(0.0);
    let mut out = Vec::new();
    for r in rows {
        let (schema, name, kind) = (r.string("schema_name"), r.string("name"), r.string("type"));
        let qualified = format!("{schema}.{name}");
        let (created, modified) = (r.str("created"), r.str("modified"));
        let new = created.as_deref().and_then(epoch).filter(|t| *t > since);
        let altered = modified.as_deref().and_then(epoch).filter(|m| new.is_none_or(|c| *m > c + 2.0));
        let what = match kind.as_str() {
            "U" => "Table",
            "V" => "View",
            "P" => "Procedure",
            "TR" => "Trigger",
            _ => "Function",
        };
        let parent = r.str("parent").map(|p| format!("{}.{p}", r.str("parent_schema").unwrap_or_default()));
        let event = |at: &str, action: EventAction| {
            let verb = match (action, kind.as_str()) {
                (EventAction::Created, _) => "was created".to_string(),
                (_, "U") => "was altered (its columns, constraints or indexes)".to_string(),
                _ => "was altered".to_string(),
            };
            let title = match (&parent, kind.as_str()) {
                (Some(table), "TR") => format!("{what} {name} on {table} {verb}"),
                _ => format!("{what} {qualified} {verb}"),
            };
            let e = ChangeEvent::new(
                if kind == "U" { EventKind::Table } else { EventKind::Code },
                EventSource::Database,
                &qualified,
                at,
                title,
            )
            .action(action);
            match (kind.as_str(), &parent) {
                ("U", _) => e.table(qualified.clone()),
                ("TR", Some(table)) => e.table(table.clone()).object(qualified.clone()),
                _ => e.object(qualified.clone()),
            }
        };
        if new.is_some()
            && let Some(at) = &created
        {
            out.push(event(at, EventAction::Created));
        }
        if altered.is_some()
            && let Some(at) = &modified
        {
            out.push(event(at, EventAction::Changed));
        }
    }
    Ok(out)
}
