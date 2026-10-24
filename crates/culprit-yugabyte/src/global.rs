//! Optional native global views are read only when already configured. Culprit never enables the preview or creates
//! foreign views while collecting a production database.
use crate::probe::node_id;
use culprit_core::{Engine, Result};
use culprit_postgres::wire::{ObservedSession, query, query_one};
use std::collections::BTreeSet;
use tokio_postgres::Client;

pub(crate) async fn configured(client: &Client, base: &str) -> Result<Option<String>> {
    // Only internal callers select a known catalog name. Qualified names come from the catalog and are quoted.
    let base = base.replace('\'', "''");
    Ok(query_one(
        client,
        &format!(
            "SELECT n.nspname AS schema,c.relname AS name FROM pg_class c
        JOIN pg_namespace n ON n.oid=c.relnamespace JOIN pg_foreign_table f ON f.ftrelid=c.oid
        JOIN pg_foreign_server s ON s.oid=f.ftserver JOIN pg_roles r ON r.oid=c.relowner
        WHERE c.relname='gv${base}' AND s.srvname='yb_global_views_server' AND r.rolsuper
        ORDER BY n.nspname LIMIT 1"
        ),
    )
    .await?
    .map(|r| Engine::Yugabyte.dialect().qualified(&[&r.string("schema"), &r.string("name")])))
}

pub(crate) async fn nodes(session: &ObservedSession) -> Result<BTreeSet<String>> {
    let before = session.notices();
    let rows = query(&session.client, "SELECT uuid::text AS node FROM yb_servers()").await?;
    let ids: Option<BTreeSet<_>> = rows.iter().map(|r| node_id(&r.string("node"))).collect();
    ids.filter(|ids| !ids.is_empty() && session.notices() == before)
        .ok_or_else(|| culprit_core::Error::invalid("YSQL could not establish a complete node inventory"))
}
