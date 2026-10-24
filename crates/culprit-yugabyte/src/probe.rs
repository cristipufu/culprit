use culprit_core::{Capability, Engine, Error, Result, ServerInfo};
use culprit_postgres::wire::query_one;
use tokio_postgres::Client;

pub(crate) struct Probe {
    pub now: String,
    pub started: Option<String>,
    pub version: String,
    pub compatibility: u32,
    pub database: String,
    pub oid: u32,
    pub node: Option<String>,
    pub cluster: bool,
    pub global: bool,
    pub statements: bool,
    pub statement_schema: Option<String>,
    pub metrics: bool,
    pub rpc_metrics: bool,
    pub statement_role: Option<u32>,
}

impl Probe {
    pub fn server(&self) -> ServerInfo {
        ServerInfo {
            engine: Engine::Yugabyte,
            product: format!(
                "YugabyteDB {} (YSQL PostgreSQL {})",
                self.version,
                format_args!("{}.{}", self.compatibility / 10000, self.compatibility % 10000)
            ),
            version: self.version.clone(),
            edition: None,
            cloud: None,
            platform: None,
            database: self.database.clone(),
            database_size_bytes: None,
            started_at: self.started.clone(),
            read_only_session: true,
            capabilities: vec![Capability::TableStats, Capability::Statistics],
            missing: vec![],
        }
    }
}

pub(crate) fn node_id(value: &str) -> Option<String> {
    let value = value.replace('-', "").to_ascii_lowercase();
    (value.len() == 32 && value.bytes().all(|b| b.is_ascii_hexdigit())).then_some(value)
}

pub(crate) fn timestamp(expression: &str) -> String {
    format!("to_char(({expression}) AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"')")
}

pub(crate) async fn read(client: &Client) -> Result<Probe> {
    // The initial probe is deliberately independent of all PostgreSQL/DocDB size and heap APIs.
    let row = query_one(
        client,
        &format!(
            "SELECT version() AS version, current_setting('server_version_num') AS compatibility,
        current_database() AS database, (SELECT oid::text FROM pg_database WHERE datname=current_database()) AS oid,
        {} AS now, {} AS started, current_setting('yb_enable_global_views',true) AS global_views,
        (SELECT n.nspname FROM pg_extension e JOIN pg_namespace n ON n.oid=e.extnamespace
          WHERE e.extname='pg_stat_statements') AS statement_schema,
        to_regclass('pg_catalog.\"gv$pg_stat_statements\"') IS NOT NULL AS cluster_statements,
        current_setting('yb_enable_pg_stat_statements_docdb_metrics',true) AS metrics,
        current_setting('yb_enable_pg_stat_statements_rpc_stats',true) AS rpc_metrics,
        (SELECT oid::text FROM pg_roles WHERE rolname=current_user) AS role_oid,
        (EXISTS(SELECT 1 FROM pg_roles WHERE rolname=current_user AND rolsuper)
          OR pg_has_role(current_user,'pg_read_all_stats','MEMBER')) AS all_stats",
            timestamp("clock_timestamp()"),
            timestamp("pg_postmaster_start_time()")
        ),
    )
    .await?
    .ok_or_else(|| Error::invalid("YSQL returned no server identity"))?;
    let version = row.string("version");
    let version = version
        .split_once("-YB-")
        .map(|(_, v)| v.split_whitespace().next().unwrap_or(v))
        .ok_or_else(|| {
            Error::invalid("This server does not identify as YugabyteDB")
                .with_hint("Choose PostgreSQL for a PostgreSQL server.")
        })?
        .to_string();
    let node = query_one(client, "SELECT yb_get_local_tserver_uuid()::text AS node")
        .await
        .ok()
        .flatten()
        .and_then(|r| node_id(&r.string("node")));
    Ok(Probe {
        now: row.string("now"),
        started: row.text("started"),
        version,
        compatibility: row
            .string("compatibility")
            .parse()
            .map_err(|_| Error::invalid("YSQL returned an invalid compatibility version"))?,
        database: row.string("database"),
        oid: row.string("oid").parse().map_err(|_| Error::invalid("YSQL returned an invalid database oid"))?,
        node,
        cluster: row.flag("global_views") && row.flag("cluster_statements"),
        global: row.flag("global_views"),
        statements: row.text("statement_schema").is_some(),
        statement_schema: row.text("statement_schema"),
        metrics: row.flag("metrics"),
        rpc_metrics: row.flag("rpc_metrics"),
        statement_role: if row.flag("all_stats") {
            None
        } else {
            Some(
                row.string("role_oid")
                    .parse()
                    .map_err(|_| Error::invalid("YSQL returned no statistics role identity"))?,
            )
        },
    })
}
