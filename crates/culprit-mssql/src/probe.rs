use culprit_core::{Capability, Engine, MissingCapability, ServerInfo};

use crate::conn::{MsClient, query, query_one};

/// A principal name as a bracketed T-SQL identifier. Names come from the server and end up in fixes an administrator runs.
fn bracket(name: &str) -> String {
    format!("[{}]", name.replace(']', "]]"))
}
use crate::value::Rec;

/// SERVERPROPERTY('EngineEdition') values that change what the server exposes.
pub(crate) const ENGINE_EDITION_AZURE_SQL_DATABASE: i64 = 5;
pub(crate) const ENGINE_EDITION_MANAGED_INSTANCE: i64 = 8;

const PROBE_SQL: &str = r#"
SELECT
  CAST(SERVERPROPERTY('ProductVersion') AS nvarchar(128)) AS version,
  CAST(SERVERPROPERTY('ProductMajorVersion') AS nvarchar(16)) AS major,
  CAST(SERVERPROPERTY('Edition') AS nvarchar(128)) AS edition,
  CAST(SERVERPROPERTY('EngineEdition') AS int) AS engine_edition,
  DB_NAME() AS database_name,
  (SELECT CAST(SUM(CAST(size AS bigint)) * 8192 AS bigint) FROM sys.database_files WHERE type = 0) AS data_bytes,
  HAS_PERMS_BY_NAME(NULL, 'DATABASE', 'VIEW DATABASE STATE') AS can_view_database_state,
  HAS_PERMS_BY_NAME(NULL, NULL, 'VIEW SERVER STATE') AS can_view_server_state,
  HAS_PERMS_BY_NAME(NULL, 'DATABASE', 'SHOWPLAN') AS can_showplan,
  SUSER_SNAME() AS login_name,
  USER_NAME() AS user_name
"#;

const START_TIME_SQL: &str = "SELECT CONVERT(varchar(19), DATEADD(minute, DATEDIFF(minute, GETDATE(), GETUTCDATE()), sqlserver_start_time), 126) + 'Z' AS started_at FROM sys.dm_os_sys_info";

const QUERY_STORE_SQL: &str = "SELECT * FROM sys.database_query_store_options";

/// Query Store states in which its views can be read.
const QUERY_STORE_READABLE: &[&str] = &["READ_WRITE", "READ_ONLY"];

#[derive(Clone, Debug, Default)]
pub(crate) struct QueryStoreState {
    pub(crate) actual: String,
    /// Every column of sys.database_query_store_options, surfaced as settings.
    pub(crate) options: Rec,
}

impl QueryStoreState {
    pub(crate) fn readable(&self) -> bool {
        QUERY_STORE_READABLE.contains(&self.actual.as_str())
    }
}

#[derive(Clone, Debug)]
pub(crate) struct Probe {
    pub(crate) version: String,
    pub(crate) major: u32,
    pub(crate) edition: String,
    pub(crate) engine_edition: i64,
    pub(crate) database: String,
    pub(crate) data_bytes: Option<f64>,
    pub(crate) can_view_database_state: bool,
    pub(crate) can_view_server_state: bool,
    pub(crate) can_showplan: bool,
    pub(crate) login: String,
    pub(crate) user: String,
    pub(crate) started_at: Option<String>,
    pub(crate) query_store: Option<QueryStoreState>,
}

impl Probe {
    pub(crate) fn is_azure_sql_database(&self) -> bool {
        self.engine_edition == ENGINE_EDITION_AZURE_SQL_DATABASE
    }

    pub(crate) fn cloud(&self) -> Option<String> {
        match self.engine_edition {
            ENGINE_EDITION_AZURE_SQL_DATABASE => Some("Azure SQL Database".into()),
            ENGINE_EDITION_MANAGED_INSTANCE => Some("Azure SQL Managed Instance".into()),
            _ => None,
        }
    }

    pub(crate) fn product(&self) -> String {
        if self.is_azure_sql_database() {
            return "Azure SQL Database".into();
        }
        if self.engine_edition == ENGINE_EDITION_MANAGED_INSTANCE {
            return "Azure SQL Managed Instance".into();
        }
        match self.major {
            17 => "SQL Server 2025".into(),
            16 => "SQL Server 2022".into(),
            15 => "SQL Server 2019".into(),
            14 => "SQL Server 2017".into(),
            13 => "SQL Server 2016".into(),
            12 => "SQL Server 2014".into(),
            0 => "SQL Server".into(),
            n => format!("SQL Server (version {n})"),
        }
    }

    /// Whether this login can read the database-scoped DMVs Culprit relies on.
    pub(crate) fn can_read_dmvs(&self) -> bool {
        self.can_view_database_state || self.can_view_server_state
    }

    /// The exact statement an administrator should run to unlock the monitoring views.
    pub(crate) fn grant_hint(&self) -> String {
        if self.is_azure_sql_database() {
            return format!("GRANT VIEW DATABASE STATE TO {};", bracket(&self.user));
        }
        if self.major >= 16 {
            return format!("GRANT VIEW SERVER PERFORMANCE STATE TO {};", bracket(&self.login));
        }
        format!("GRANT VIEW SERVER STATE TO {};", bracket(&self.login))
    }

    pub(crate) fn query_store_readable(&self) -> bool {
        self.query_store.as_ref().is_some_and(QueryStoreState::readable)
    }
}

pub(crate) async fn run(client: &mut MsClient) -> tiberius::Result<Probe> {
    let r = query_one(client, PROBE_SQL).await?;
    let started_at = query(client, START_TIME_SQL)
        .await
        .ok()
        .and_then(|rows| rows.into_iter().next())
        .and_then(|r| r.str("started_at"));
    let query_store = query(client, QUERY_STORE_SQL)
        .await
        .ok()
        .and_then(|rows| rows.into_iter().next())
        .map(|o| QueryStoreState { actual: o.string("actual_state_desc"), options: o });
    Ok(Probe {
        version: r.string("version"),
        major: r.str("major").and_then(|m| m.parse().ok()).unwrap_or(0),
        edition: r.string("edition"),
        engine_edition: r.f64("engine_edition").unwrap_or(0.0) as i64,
        database: r.string("database_name"),
        data_bytes: r.f64("data_bytes"),
        can_view_database_state: r.bool("can_view_database_state"),
        can_view_server_state: r.bool("can_view_server_state"),
        can_showplan: r.bool("can_showplan"),
        login: r.string("login_name"),
        user: r.string("user_name"),
        started_at,
        query_store,
    })
}

/// What a login can be expected to read, before collecting anything.
pub(crate) fn expected_capabilities(p: &Probe) -> (Vec<Capability>, Vec<MissingCapability>) {
    let mut have = Vec::new();
    let mut missing = Vec::new();
    let grant = p.grant_hint();
    let dmv = p.can_read_dmvs();

    match &p.query_store {
        Some(qs) if qs.readable() && dmv => have.push(Capability::QueryStore),
        Some(qs) if qs.readable() => missing.push(MissingCapability {
            capability: Capability::QueryStore,
            reason: "This login cannot read Query Store views.".into(),
            fix: Some(grant.clone()),
        }),
        Some(qs) => missing.push(MissingCapability {
            capability: Capability::QueryStore,
            reason: format!("Query Store is {} on this database.", qs.actual.to_ascii_lowercase().replace('_', " ")),
            fix: Some("ALTER DATABASE CURRENT SET QUERY_STORE = ON (OPERATION_MODE = READ_WRITE);".into()),
        }),
        None => missing.push(MissingCapability {
            capability: Capability::QueryStore,
            reason: "Query Store options could not be read.".into(),
            fix: Some(grant.clone()),
        }),
    }
    if !p.query_store_readable() && dmv {
        have.push(Capability::PlanCache);
    }

    let dmv_backed = [
        Capability::TableStats,
        Capability::IndexUsage,
        Capability::MissingIndexes,
        Capability::WaitStats,
        Capability::Sessions,
    ];
    for capability in dmv_backed {
        if dmv {
            have.push(capability);
            continue;
        }
        missing.push(MissingCapability {
            capability,
            reason: "This login lacks VIEW DATABASE STATE / VIEW SERVER STATE.".into(),
            fix: Some(grant.clone()),
        });
    }
    have.push(Capability::Statistics);
    if dmv {
        have.push(Capability::QueryPlans);
    }
    if p.is_azure_sql_database() && dmv {
        have.push(Capability::ResourceUsage);
    }
    if p.can_showplan {
        have.push(Capability::Showplan);
    } else {
        missing.push(MissingCapability {
            capability: Capability::Showplan,
            reason: "This login cannot request estimated plans.".into(),
            fix: Some(format!("GRANT SHOWPLAN TO {};", bracket(&p.user))),
        });
    }
    (have, missing)
}

pub(crate) fn server_info(p: &Probe, capabilities: Vec<Capability>, missing: Vec<MissingCapability>) -> ServerInfo {
    ServerInfo {
        engine: Engine::Mssql,
        product: p.product(),
        version: p.version.clone(),
        edition: Some(p.edition.clone()).filter(|e| !e.is_empty()),
        cloud: p.cloud(),
        database: p.database.clone(),
        database_size_bytes: p.data_bytes,
        started_at: p.started_at.clone(),
        read_only_session: false,
        capabilities,
        missing,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn principal_names_cannot_break_out_of_the_grant() {
        assert_eq!(bracket("app]; DROP LOGIN sa; --"), "[app]]; DROP LOGIN sa; --]");
        assert_eq!(format!("GRANT SHOWPLAN TO {};", bracket("reader")), "GRANT SHOWPLAN TO [reader];");
    }
}
