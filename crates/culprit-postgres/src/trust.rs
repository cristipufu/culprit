//! Planning runs code. To estimate a statement the planner calls the functions and operators it reaches: immutable
//! functions on constants, an operator's function on a column's most common values, an operator class's comparison
//! while proving a partial index applies. That code runs with this login's privileges, whoever wrote the statement,
//! and neither the search_path (an overload in another schema that matches the argument types better wins over
//! pg_catalog's) nor SET ROLE (the code can RESET it) keeps it out.
//!
//! So every plan Culprit asks for without running the statement is made in a read-only transaction that is rolled
//! back, with session locks released after: the code can read what this login reads, but can't write, and what it
//! changes in the session doesn't outlive the plan. And a login that can do more than read statistics ([`ELEVATED`])
//! doesn't plan while code another role wrote is in reach ([`foreign_code`]). A login with only pg_monitor, and
//! SELECT on the tables it plans, plans every statement.

use culprit_core::{Error, ErrorCode, Result};
use tokio_postgres::Client;

use crate::connect::map_error;
use crate::rows::{query, query_one, tagged};

/// Whether the login can do more than read statistics: a role attribute beyond LOGIN (superuser, CREATEROLE,
/// CREATEDB, BYPASSRLS, REPLICATION), or membership in any role but the monitoring ones (a cloud's admin role, the
/// database's owner, pg_read_all_data, an application's role). A boolean SQL expression; unknown counts as elevated.
pub const ELEVATED: &str = "(coalesce((SELECT rolsuper OR rolcreaterole OR rolcreatedb OR rolbypassrls OR rolreplication
                              FROM pg_roles WHERE rolname = current_user), true)
       OR EXISTS (SELECT 1 FROM pg_roles r
                   WHERE r.rolname <> current_user
                     AND r.rolname NOT IN ('pg_monitor', 'pg_read_all_stats', 'pg_read_all_settings', 'pg_stat_scan_tables')
                     AND pg_has_role(current_user, r.oid, 'MEMBER')))";

/// The login's [`ELEVATED`], read on its own.
pub async fn elevated(client: &Client) -> Result<bool> {
    Ok(query_one(client, &format!("SELECT {ELEVATED} AS elevated")).await?.is_none_or(|r| r.flag("elevated")))
}

/// The roles, other than the superusers and this login, that wrote code the planner could run in this database:
/// functions and operators. Trigger functions are never called while planning, and an extension's code is its
/// authors'. At most five names.
pub async fn foreign_code(client: &Client) -> Result<Vec<String>> {
    let rows = query(
        client,
        "SELECT DISTINCT r.rolname::text AS owner
           FROM (SELECT p.proowner AS owner FROM pg_proc p
                  WHERE p.prorettype NOT IN ('trigger'::regtype, 'event_trigger'::regtype)
                    AND NOT EXISTS (SELECT 1 FROM pg_depend d
                                     WHERE d.classid = 'pg_proc'::regclass AND d.objid = p.oid AND d.deptype = 'e')
                 UNION ALL
                 SELECT o.oprowner FROM pg_operator o
                  WHERE NOT EXISTS (SELECT 1 FROM pg_depend d
                                     WHERE d.classid = 'pg_operator'::regclass AND d.objid = o.oid AND d.deptype = 'e')) code
           JOIN pg_roles r ON r.oid = code.owner
          WHERE NOT r.rolsuper AND r.rolname <> current_user
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
    .with_hint("Connect with a login that has only pg_monitor, and SELECT on the tables: it plans every statement.")
}

/// Start planning: a read-only transaction and, for an [`ELEVATED`] login, no other role's code in reach. Whatever
/// planning does next, [`end`] follows.
pub async fn begin(client: &Client, elevated: bool) -> Result<()> {
    client.batch_execute(&tagged("BEGIN READ ONLY")).await.map_err(map_error)?;
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
    Ok(())
}

/// End planning: roll back, and release session-level advisory locks the planner's code may have taken (a rollback
/// keeps those).
pub async fn end(client: &Client) {
    let _ = client.batch_execute(&tagged("ROLLBACK")).await;
    let _ = client.batch_execute(&tagged("SELECT pg_advisory_unlock_all()")).await;
}
