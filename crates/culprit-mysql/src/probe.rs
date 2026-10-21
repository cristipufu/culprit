//! Server identity, flavor, capabilities and source availability before collection.
use crate::{
    connect::{Connector, Flavor},
    rows::{query_one, query_with},
    vars,
};
use culprit_core::{Capability, Engine, Error, MissingCapability, Platform, Result, ServerInfo};
use mysql_async::{Conn, Value};

/// What the probe learned about the server, shared by every collector.
pub struct Probe {
    pub version: String,
    pub comment: String,
    pub database: String,
    pub size_bytes: Option<f64>,
    pub started_at: Option<String>,
    pub now: String,
    pub user: String,
    pub platform: Option<Platform>,
    pub performance_schema: bool,
    pub flavor: Flavor,
    /// Where the slow query log goes (`FILE`, `TABLE`, `FILE,TABLE`, `NONE`), whether it is on, and its threshold.
    pub log_output: String,
    pub slow_query_log: bool,
    pub long_query_time: Option<f64>,
    /// MariaDB's (and Percona's) per-table and per-index read counters; `None` where the server has no such setting.
    pub userstat: Option<bool>,
    /// `read_only` or `innodb_read_only` is on: how replicas (and Aurora's readers) are usually set up.
    pub read_only: bool,
}

impl Probe {
    pub fn product(&self) -> String {
        let short = self.version.split('-').next().unwrap_or(&self.version);
        format!("{} {short}", self.flavor.name())
    }

    pub(crate) fn grant_target(&self) -> String {
        match self.user.split_once('@') {
            Some((user, host)) => format!("'{}'@'{}'", user.replace('\'', "''"), host.replace('\'', "''")),
            None => format!("'{}'", self.user.replace('\'', "''")),
        }
    }

    /// The slow query log is written to `mysql.slow_log`, where Culprit can read it.
    pub fn slow_log_table(&self) -> bool {
        self.log_output.to_ascii_uppercase().split(',').any(|o| o.trim() == "TABLE")
    }

    /// The one statement that starts recording statements without a restart: the slow query log, into the table, at
    /// 100 ms (and, on MariaDB, per-index counters). `None` when everything it would set is already set.
    fn unlock_statement(&self) -> Option<String> {
        let mut parts: Vec<String> = Vec::new();
        if self.userstat == Some(false) {
            parts.push("GLOBAL userstat = ON".into());
        }
        if !self.slow_query_log {
            parts.push("GLOBAL slow_query_log = ON".into());
        }
        if !self.slow_log_table() {
            let current = self.log_output.trim().to_ascii_uppercase();
            let output =
                if current.is_empty() || current == "NONE" { "TABLE".to_string() } else { format!("{current},TABLE") };
            parts.push(format!("GLOBAL log_output = '{output}'"));
        }
        if self.long_query_time.is_none_or(|t| t > 1.0) {
            parts.push("GLOBAL long_query_time = 0.1".into());
        }
        (!parts.is_empty()).then(|| format!("SET {};", parts.join(", ")))
    }

    fn performance_schema_restart(&self) -> String {
        format!(
            "For statistics on every statement (not only slow ones) and waits, set performance_schema = ON in the server configuration and restart {}.",
            self.flavor.name()
        )
    }

    /// How to get statement statistics when performance_schema is off: the no-restart path first.
    fn statements_fix(&self) -> String {
        match self.unlock_statement() {
            Some(sql) => format!(
                "Run {sql} {} then logs statements slower than long_query_time to mysql.slow_log, which Culprit reads: no restart needed. \
                 Connections opened earlier keep the old threshold until they reconnect, and the settings last until the next restart unless \
                 they are also added to the server configuration. {}",
                self.flavor.name(),
                self.performance_schema_restart()
            ),
            None => self.performance_schema_restart(),
        }
    }

    pub fn server_info(&self) -> ServerInfo {
        let mut capabilities = vec![Capability::TableStats, Capability::Sessions, Capability::Showplan];
        let mut missing = Vec::new();
        if self.performance_schema {
            capabilities.extend([Capability::PerformanceSchema, Capability::IndexUsage, Capability::QueryPlans]);
        } else {
            let reason = match self.flavor {
                Flavor::Mariadb => {
                    "performance_schema is off (MariaDB's default), so the server keeps no statement statistics."
                }
                Flavor::Mysql => "performance_schema is off, so MySQL keeps no statement statistics.",
            };
            missing.push(MissingCapability {
                capability: Capability::PerformanceSchema,
                reason: reason.into(),
                fix: Some(self.statements_fix()),
            });
            if self.userstat == Some(true) {
                capabilities.push(Capability::IndexUsage);
            } else {
                let (reason, fix) = match self.flavor {
                    Flavor::Mariadb => (
                        "MariaDB counts index reads only with performance_schema or userstat on.".to_string(),
                        "Run SET GLOBAL userstat = ON; to start counting (no restart needed), then scan again once the application has run for a while.".to_string(),
                    ),
                    Flavor::Mysql => (
                        "performance_schema is off, so MySQL counts no index use.".to_string(),
                        "Set performance_schema = ON in the server configuration and restart MySQL.".to_string(),
                    ),
                };
                missing.push(MissingCapability { capability: Capability::IndexUsage, reason, fix: Some(fix) });
            }
            if self.slow_query_log && self.slow_log_table() {
                capabilities.push(Capability::QueryPlans);
            } else {
                missing.push(MissingCapability {
                    capability: Capability::QueryPlans,
                    reason: "Plans come from the statements the server recorded, and it records none.".into(),
                    fix: None,
                });
            }
        }
        // Logged statements are a workload of their own without performance_schema, and samples to plan with it.
        if self.slow_query_log && self.slow_log_table() {
            capabilities.push(Capability::SlowLog);
        }
        ServerInfo {
            engine: Engine::Mysql,
            product: self.product(),
            version: self.version.clone(),
            edition: Some(self.comment.clone()).filter(|c| !c.is_empty()),
            cloud: self.platform.map(|p| p.label(Engine::Mysql).to_string()),
            platform: self.platform,
            database: self.database.clone(),
            database_size_bytes: self.size_bytes,
            started_at: self.started_at.clone(),
            read_only_session: true,
            capabilities,
            missing,
        }
    }
}

fn database_of(connector: &Connector) -> Result<String> {
    connector.database.clone().ok_or_else(|| {
        Error::invalid("A MySQL or MariaDB connection needs a database (schema) to analyze")
            .with_hint("Add it to the connection string, e.g. mysql://user@host:3306/shop")
    })
}

pub async fn probe(conn: &mut Conn, connector: &Connector) -> Result<Probe> {
    let database = database_of(connector)?;
    let row = query_one(
        conn,
        "SELECT VERSION() AS version, @@version_comment AS comment, @@basedir AS basedir, CURRENT_USER() AS user_name,
                @@performance_schema AS ps, DATE_FORMAT(UTC_TIMESTAMP(), '%Y-%m-%dT%H:%i:%sZ') AS now,
                @@log_output AS log_output, @@slow_query_log AS slow_log, @@long_query_time AS long_query_time,
                @@read_only AS read_only",
    )
    .await?
    .ok_or_else(|| Error::internal("The server returned no information about itself"))?;
    let size = query_with(
        conn,
        "SELECT SUM(DATA_LENGTH + INDEX_LENGTH) AS size, COUNT(*) AS tables FROM information_schema.TABLES WHERE TABLE_SCHEMA = ?",
        vec![Value::from(database.clone())],
    )
    .await?
    .into_iter()
    .next();
    if size.as_ref().and_then(|r| r.number("tables")).unwrap_or(0.0) == 0.0 {
        let exists = query_with(
            conn,
            "SELECT 1 AS found FROM information_schema.SCHEMATA WHERE SCHEMA_NAME = ?",
            vec![Value::from(database.clone())],
        )
        .await?;
        if exists.is_empty() {
            return Err(Error::not_found(format!(
                "Database `{database}` does not exist or is not visible to this login"
            )));
        }
    }
    let uptime = vars::status(conn, &["Uptime"]).await.ok().and_then(|s| s.get("Uptime").copied());
    let started_at = match uptime {
        Some(seconds) => query_one(
            conn,
            &format!(
                "SELECT DATE_FORMAT(UTC_TIMESTAMP() - INTERVAL {} SECOND, '%Y-%m-%dT%H:%i:%sZ') AS started",
                seconds as u64
            ),
        )
        .await?
        .map(|r| r.string("started")),
        None => None,
    };
    let version = row.string("version");
    let comment = row.string("comment");
    let flavor = Flavor::of_version(&version);
    let platform = detect_platform(conn, &connector.host, &row.string("basedir"), &comment, flavor).await;
    let userstat = match flavor {
        Flavor::Mariadb => {
            query_one(conn, "SELECT @@userstat AS userstat").await.ok().flatten().map(|r| r.flag("userstat"))
        }
        Flavor::Mysql => None,
    };
    Ok(Probe {
        flavor,
        log_output: row.string("log_output"),
        slow_query_log: row.flag("slow_log"),
        long_query_time: row.number("long_query_time"),
        userstat,
        version,
        comment,
        database,
        size_bytes: size.and_then(|r| r.number("size")),
        started_at,
        now: row.string("now"),
        user: row.string("user_name"),
        platform,
        performance_schema: row.flag("ps"),
        // innodb_read_only (Aurora's readers) is read on its own: a server without it must not fail the probe.
        read_only: row.flag("read_only")
            || query_one(conn, "SELECT @@innodb_read_only AS v").await.ok().flatten().is_some_and(|r| r.flag("v")),
    })
}

async fn detect_platform(
    conn: &mut Conn,
    host: &str,
    basedir: &str,
    comment: &str,
    flavor: Flavor,
) -> Option<Platform> {
    let host = host.to_ascii_lowercase();
    if host.ends_with(".mysql.database.azure.com") {
        return Some(Platform::AzureDatabase);
    }
    if host.ends_with(".mariadb.database.azure.com") {
        return Some(Platform::AzureDatabase);
    }
    if host.contains(".skysql.") || host.ends_with(".mariadb.com") {
        return Some(Platform::MariaDbCloud);
    }
    let aurora =
        flavor == Flavor::Mysql && query_one(conn, "SELECT AURORA_VERSION() AS v").await.ok().flatten().is_some();
    if aurora {
        return Some(Platform::AmazonAurora);
    }
    if host.contains(".rds.amazonaws.com") || basedir.contains("/rdsdbbin/") {
        return Some(Platform::AmazonRds);
    }
    if comment.to_ascii_lowercase().contains("google") || basedir.contains("/cloudsql") {
        return Some(Platform::GoogleCloudSql);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn probe(flavor: Flavor, performance_schema: bool) -> Probe {
        Probe {
            version: "10.11.6-MariaDB".into(),
            comment: String::new(),
            database: "shop".into(),
            size_bytes: None,
            started_at: None,
            now: "2026-10-11T12:00:00Z".into(),
            user: "app@%".into(),
            platform: None,
            performance_schema,
            flavor,
            log_output: "FILE".into(),
            slow_query_log: false,
            long_query_time: Some(10.0),
            userstat: (flavor == Flavor::Mariadb).then_some(false),
            read_only: false,
        }
    }

    #[test]
    fn without_performance_schema_the_no_restart_path_comes_first() {
        let p = probe(Flavor::Mariadb, false);
        assert_eq!(
            p.unlock_statement().as_deref(),
            Some(
                "SET GLOBAL userstat = ON, GLOBAL slow_query_log = ON, GLOBAL log_output = 'FILE,TABLE', GLOBAL long_query_time = 0.1;"
            )
        );
        let info = p.server_info();
        assert_eq!(info.product, "MariaDB 10.11.6");
        let ps = info.missing.iter().find(|m| m.capability == Capability::PerformanceSchema).unwrap();
        let fix = ps.fix.as_deref().unwrap();
        assert!(fix.starts_with("Run SET GLOBAL userstat = ON"), "{fix}");
        assert!(fix.contains("no restart needed") && fix.ends_with("restart MariaDB."), "{fix}");
        // Once the log goes to the table, only the restart remains.
        let logging = Probe {
            slow_query_log: true,
            log_output: "FILE,TABLE".into(),
            long_query_time: Some(0.1),
            userstat: Some(true),
            ..p
        };
        assert_eq!(logging.unlock_statement(), None);
        let info = logging.server_info();
        assert!(info.has(Capability::SlowLog) && info.has(Capability::QueryPlans) && info.has(Capability::IndexUsage));
        let mysql = probe(Flavor::Mysql, false);
        assert!(!mysql.unlock_statement().unwrap().contains("userstat"), "MySQL has no userstat");
    }
}
