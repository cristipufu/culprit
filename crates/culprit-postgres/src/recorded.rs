//! Changes PostgreSQL dates itself: when the configuration was last reloaded (settings changed in postgresql.conf, ALTER
//! SYSTEM or an RDS parameter group take effect then) and, with `track_commit_timestamp` on, when each index was created
//! or rebuilt. Other schema changes carry no date in Postgres; Culprit's scans find those.

use culprit_core::changes::epoch;
use culprit_core::{ChangeEvent, EventAction, EventKind, EventSource, Result};
use tokio_postgres::Client;

use crate::rows::{iso, query, query_one};

const MAX_INDEXES: usize = 200;

pub async fn changes(client: &Client, since: &str) -> Result<Vec<ChangeEvent>> {
    let Some(since_t) = epoch(since) else { return Ok(Vec::new()) };
    let mut out = Vec::new();
    let row = query_one(
        client,
        &format!(
            "SELECT {} AS reloaded, {} AS started, current_setting('track_commit_timestamp') AS commit_ts",
            iso("pg_conf_load_time()"),
            iso("pg_postmaster_start_time()")
        ),
    )
    .await?;
    let Some(row) = row else { return Ok(out) };
    if let (Some(reloaded), Some(started)) = (row.text("reloaded"), row.text("started").as_deref().and_then(epoch))
        && let Some(t) = epoch(&reloaded)
        && t > since_t
        && t > started + 60.0
    {
        out.push(
            ChangeEvent::new(
                EventKind::Setting,
                EventSource::Database,
                "reload",
                &reloaded,
                "The server's configuration was reloaded",
            )
            .action(EventAction::Changed)
            .detail("Settings changed in postgresql.conf, with ALTER SYSTEM or in a cloud parameter group take effect at a reload."),
        );
    }
    if row.flag("commit_ts") {
        let Some(since_utc) = culprit_core::changes::sql_timestamp(since).map(|t| format!("{t}+00")) else {
            return Ok(out);
        };
        let rows = query(
            client,
            &format!(
                "SELECT n.nspname AS schema, t.relname AS table_name, i.relname AS index_name, {} AS at
                 FROM pg_class i
                 JOIN pg_index x ON x.indexrelid = i.oid
                 JOIN pg_class t ON t.oid = x.indrelid
                 JOIN pg_namespace n ON n.oid = t.relnamespace
                 WHERE i.relkind = 'i' AND n.nspname NOT IN ('pg_catalog', 'information_schema') AND n.nspname NOT LIKE 'pg_toast%'
                   AND pg_xact_commit_timestamp(i.xmin) > '{since_utc}'::timestamptz
                 ORDER BY pg_xact_commit_timestamp(i.xmin) DESC
                 LIMIT {MAX_INDEXES}",
                iso("pg_xact_commit_timestamp(i.xmin)")
            ),
        )
        .await
        // Commit timestamps from before the setting was turned on raise an error on some versions; nothing to date then.
        .unwrap_or_default();
        for r in rows {
            let (Some(at), table, index) =
                (r.text("at"), format!("{}.{}", r.string("schema"), r.string("table_name")), r.string("index_name"))
            else {
                continue;
            };
            out.push(
                ChangeEvent::new(
                    EventKind::Index,
                    EventSource::Database,
                    &format!("{table}.{index}"),
                    &at,
                    format!("Index {index} on {table} was created or rebuilt"),
                )
                .action(EventAction::Created)
                .table(table)
                .object(index),
            );
        }
    }
    Ok(out)
}
