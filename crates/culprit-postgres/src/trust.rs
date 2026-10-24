//! Planning runs code. To estimate a statement the planner calls the functions and operators it reaches: immutable
//! functions on constants, an operator's function on a column's most common values, an operator class's comparison
//! while proving a partial index applies. That code runs with this login's privileges, whoever wrote the statement,
//! and neither the search_path (an overload in another schema that matches the argument types better wins over
//! pg_catalog's) nor SET ROLE (the code can RESET it) keeps it out.
//!
//! So every plan Culprit asks for without running the statement is made in a read-only transaction that is rolled
//! back, with session locks released after: the code can read what this login reads, but can't write, and what it
//! changes in the session doesn't outlive the plan. And a login that can do more than read statistics
//! ([`elevated_sql`]) plans only the statements it ran itself, in its own words (a scan's statements, a report's query,
//! a session's current statement), and nothing while code another role wrote is in reach ([`foreign_code`]). A login
//! with only pg_monitor, and SELECT on the tables it plans, plans every statement.
//!
//! Culprit's own checks here run on `search_path = pg_catalog, pg_temp`, whatever the database or role sets, so no
//! schema someone else controls can stand in for the catalog or its operators; a plan is made on the database's own
//! default, as the statement's author ran it. What stays open, and is accepted: the moment between the check and the
//! plan (PostgreSQL has no lock against creating a function), and STABLE built-ins that read what this login reads
//! (`table_to_xml`) evaluated for an estimate; the ones that run arbitrary SQL (`query_to_xml`) are VOLATILE, so the
//! planner never runs them.

use culprit_core::{Error, ErrorCode, Result};
use tokio_postgres::Client;

use crate::connect::map_error;
use crate::rows::{query, tagged};

/// Culprit's own statements here: the catalog first and only, temporary objects last.
pub const CATALOG_PATH: &str = "SET LOCAL search_path = pg_catalog, pg_temp";

/// Whether `role` (`current_user` or `session_user`) can do more than read statistics: a role attribute beyond LOGIN
/// (superuser, CREATEROLE, CREATEDB, BYPASSRLS, REPLICATION); membership in any role but the monitoring ones (a cloud's
/// admin role, pg_read_all_data, an application's role); owning the database; or EXECUTE on a function neither PUBLIC
/// nor pg_monitor may run (pg_read_file granted on its own, adminpack, lo_export). Unknown counts as elevated.
fn elevated_for(role: &str) -> String {
    format!(
        "(coalesce((SELECT r.rolsuper OR r.rolcreaterole OR r.rolcreatedb OR r.rolbypassrls OR r.rolreplication
                     FROM pg_catalog.pg_roles r WHERE r.rolname = {role}), true)
          OR EXISTS (SELECT 1 FROM pg_catalog.pg_roles m
                      WHERE m.rolname <> {role}
                        AND m.rolname NOT IN ('pg_monitor', 'pg_read_all_stats', 'pg_read_all_settings', 'pg_stat_scan_tables')
                        AND pg_catalog.pg_has_role({role}, m.oid, 'MEMBER'))
          OR EXISTS (SELECT 1 FROM pg_catalog.pg_database d
                      WHERE d.datname = pg_catalog.current_database() AND pg_catalog.pg_has_role({role}, d.datdba, 'MEMBER'))
          OR EXISTS (SELECT 1 FROM pg_catalog.pg_proc p
                      WHERE p.proowner <> (SELECT r.oid FROM pg_catalog.pg_roles r WHERE r.rolname = {role})
                        AND pg_catalog.has_function_privilege({role}, p.oid, 'EXECUTE')
                        AND NOT pg_catalog.has_function_privilege('public', p.oid, 'EXECUTE')
                        AND NOT coalesce(pg_catalog.has_function_privilege(
                              (SELECT r.oid FROM pg_catalog.pg_roles r WHERE r.rolname = 'pg_monitor'), p.oid, 'EXECUTE'), false)))"
    )
}

/// Whether the login can do more than read statistics, as a boolean SQL expression: the role it acts as, or the one
/// it logged in as (a database's or role's `role` setting can make those differ, and planned code can RESET ROLE).
pub fn elevated_sql() -> String {
    format!("({} OR {})", elevated_for("current_user"), elevated_for("session_user"))
}

/// Run `sql` (one row) on [`CATALOG_PATH`], in a read-only transaction of its own.
async fn on_catalog(
    client: &Client,
    sql: &str,
    params: &[&(dyn tokio_postgres::types::ToSql + Sync)],
) -> Result<Option<tokio_postgres::Row>> {
    client
        .batch_execute(&format!("{}; {}", tagged("BEGIN READ ONLY"), tagged(CATALOG_PATH)))
        .await
        .map_err(map_error)?;
    let row = client.query_opt(tagged(sql).as_str(), params).await.map_err(map_error);
    let _ = client.batch_execute(&tagged("ROLLBACK")).await;
    row
}

/// The login's [`elevated_sql`], read on its own.
pub async fn elevated(client: &Client) -> Result<bool> {
    let row = on_catalog(client, &format!("SELECT {} AS elevated", elevated_sql()), &[]).await?;
    Ok(row.is_none_or(|r| r.try_get::<_, Option<bool>>("elevated").ok().flatten().unwrap_or(true)))
}

/// The roles, other than the superusers and this login (the role it authenticated as, never one it acts as, which
/// others may act as too), that wrote code the planner could run in this database:
/// functions and operators. Trigger functions are never called while planning. An extension's code is its authors'
/// only when a superuser or this login owns the extension: its owner can add a function of theirs to it. At most five
/// names. Called on [`CATALOG_PATH`].
pub async fn foreign_code(client: &Client) -> Result<Vec<String>> {
    let rows = query(
        client,
        "SELECT DISTINCT r.rolname::text AS owner
           FROM (SELECT p.proowner AS owner, p.oid AS obj, 'pg_catalog.pg_proc'::pg_catalog.regclass AS class
                   FROM pg_catalog.pg_proc p
                  WHERE p.prorettype NOT IN ('pg_catalog.trigger'::pg_catalog.regtype, 'pg_catalog.event_trigger'::pg_catalog.regtype)
                 UNION ALL
                 SELECT o.oprowner, o.oid, 'pg_catalog.pg_operator'::pg_catalog.regclass FROM pg_catalog.pg_operator o) code
           JOIN pg_catalog.pg_roles r ON r.oid = code.owner
          WHERE NOT r.rolsuper AND r.rolname <> session_user
            AND NOT EXISTS (SELECT 1 FROM pg_catalog.pg_depend d
                              JOIN pg_catalog.pg_extension e ON e.oid = d.refobjid
                              JOIN pg_catalog.pg_roles eo ON eo.oid = e.extowner
                             WHERE d.classid = code.class AND d.objid = code.obj AND d.deptype = 'e'
                               AND d.refclassid = 'pg_catalog.pg_extension'::pg_catalog.regclass
                               AND (eo.rolsuper OR eo.rolname = session_user))
          ORDER BY 1
          LIMIT 5",
    )
    .await?;
    Ok(rows.iter().filter_map(|r| r.text("owner")).collect())
}

/// Why this login doesn't plan: other roles' code is in reach (`owners`, from [`foreign_code`]).
pub fn withheld(owners: &[String]) -> Error {
    Error::new(
        ErrorCode::PermissionDenied,
        format!(
            "Not planned: functions or operators in this database belong to other roles ({}), and the planner runs the ones a statement reaches with this login's privileges, which go beyond reading statistics.",
            owners.join(", ")
        ),
    )
    .with_hint(MONITORING_LOGIN)
}

/// Why this login doesn't plan a statement someone else recorded.
pub fn not_its_own() -> Error {
    Error::new(
        ErrorCode::PermissionDenied,
        "Not planned: another role ran this statement, and planning it runs the functions it reaches with this login's privileges, which go beyond reading statistics.",
    )
    .with_hint(MONITORING_LOGIN)
}

const MONITORING_LOGIN: &str =
    "Connect with a login that has only pg_monitor, and SELECT on the tables: it plans every statement.";

/// This login's own text of the statement pg_stat_statements records as `query_id`, when it ran it. "This login" is
/// the role it authenticated as (`session_user`), and only while it acts as itself: one acting as a shared role (a
/// `role` setting) can't tell its statements from anyone else's acting as that role. The id is bound, never spliced;
/// the view's schema is quoted.
pub async fn own_text(client: &Client, query_id: &str) -> Result<Option<String>> {
    let Ok(id) = query_id.trim().parse::<i64>() else { return Ok(None) };
    let schema = on_catalog(
        client,
        "SELECT n.nspname::text AS schema FROM pg_catalog.pg_extension e
           JOIN pg_catalog.pg_namespace n ON n.oid = e.extnamespace WHERE e.extname = 'pg_stat_statements'",
        &[],
    )
    .await?
    .and_then(|r| r.try_get::<_, String>("schema").ok());
    let Some(schema) = schema else { return Ok(None) };
    let sql = format!(
        "SELECT query FROM {}.pg_stat_statements
          WHERE queryid = $1 AND dbid = (SELECT oid FROM pg_catalog.pg_database WHERE datname = pg_catalog.current_database())
            AND pg_catalog.pg_get_userbyid(userid) = session_user AND current_user = session_user LIMIT 1",
        crate::rows::quote_ident(&schema)
    );
    Ok(on_catalog(client, &sql, &[&id]).await?.and_then(|r| r.try_get::<_, String>("query").ok()))
}

/// YSQL's global statistics can prove ownership of this login's own copy on another node. Never plan the
/// representative text picked while aggregating other roles; retrieve the authenticated role's copy instead.
pub async fn own_ysql_text(client: &Client, query_id: &str, cluster: bool) -> Result<Option<String>> {
    if !cluster {
        return own_text(client, query_id).await;
    }
    let Ok(id) = query_id.trim().parse::<i64>() else { return Ok(None) };
    let relation = "pg_catalog.\"gv$pg_stat_statements\"";
    let sql = format!("SELECT query FROM {relation}
        WHERE queryid = $1 AND dbid = (SELECT oid FROM pg_catalog.pg_database WHERE datname = pg_catalog.current_database())
          AND pg_catalog.pg_get_userbyid(userid) = session_user AND current_user = session_user LIMIT 1");
    Ok(on_catalog(client, &sql, &[&id]).await?.and_then(|r| r.try_get::<_, String>("query").ok()))
}

/// Whether the session `pid` logged in as this login (`pg_stat_activity.usesysid` is a session's own login).
pub async fn session_is_own(client: &Client, pid: i32) -> Result<bool> {
    let row = on_catalog(
        client,
        "SELECT pg_catalog.pg_get_userbyid(usesysid) = session_user AS own FROM pg_catalog.pg_stat_activity WHERE pid = $1",
        &[&pid],
    )
    .await?;
    Ok(row.is_some_and(|r| r.try_get::<_, Option<bool>>("own").ok().flatten().unwrap_or(false)))
}

/// Start planning: a read-only transaction; for an elevated login, no other role's code in reach (checked on
/// [`CATALOG_PATH`]); then the database's own search_path for the statement. Whatever planning does next, [`end`]
/// follows.
pub async fn begin(client: &Client, elevated: bool) -> Result<()> {
    client
        .batch_execute(&format!("{}; {}", tagged("BEGIN READ ONLY"), tagged(CATALOG_PATH)))
        .await
        .map_err(map_error)?;
    if elevated {
        let refused = match foreign_code(client).await {
            Ok(owners) if owners.is_empty() => None,
            Ok(owners) => Some(withheld(&owners)),
            Err(e) => Some(e),
        };
        if let Some(e) = refused {
            end(client).await;
            return Err(e);
        }
    }
    if let Err(e) = client.batch_execute(&tagged("SET LOCAL search_path TO DEFAULT")).await {
        end(client).await;
        return Err(map_error(e));
    }
    Ok(())
}

/// End planning: roll back, and release session-level advisory locks the planner's code may have taken (a rollback
/// keeps those).
pub async fn end(client: &Client) {
    let _ = client.batch_execute(&tagged("ROLLBACK")).await;
    let _ = client.batch_execute(&tagged("SELECT pg_catalog.pg_advisory_unlock_all()")).await;
}
