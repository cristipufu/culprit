use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::model::{Engine, Environment};

#[derive(Serialize, Deserialize, TS, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
#[ts(export)]
pub enum AuthKind {
    #[default]
    Password,
    /// Microsoft Entra ID through the Azure CLI's signed-in account. No secret is stored.
    Entra,
    /// Amazon RDS IAM database authentication: a short-lived token from the AWS CLI's signed-in account. No secret is stored.
    #[serde(rename = "aws-iam")]
    AwsIam,
    None,
}

/// Everything a provider needs to open connections. Secrets are resolved by the engine at connect time and never persisted here.
#[derive(Serialize, Deserialize, TS, Clone, Default)]
#[ts(export)]
pub struct ConnectionSpec {
    pub engine: Option<Engine>,
    pub host: String,
    pub port: Option<u16>,
    pub database: Option<String>,
    pub user: Option<String>,
    pub auth: AuthKind,
    pub environment: Environment,
    /// Driver options such as `encrypt`, `trustServerCertificate`, `sslmode`.
    pub options: BTreeMap<String, String>,
    #[serde(skip)]
    #[ts(skip)]
    pub password: Option<String>,
    #[serde(skip)]
    #[ts(skip)]
    pub token: Option<String>,
}

/// Hosts that may receive a Microsoft Entra ID access token for this engine. The token is valid for every Azure database the
/// person can reach, so it must never be sent to a host outside Azure's database domains.
pub fn entra_host_allowed(engine: Engine, host: &str) -> bool {
    let host = host.trim().trim_end_matches('.').to_ascii_lowercase();
    let suffixes: &[&str] = match engine {
        Engine::Mssql => &[
            ".database.windows.net",
            ".database.usgovcloudapi.net",
            ".database.chinacloudapi.cn",
            ".sql.azuresynapse.net",
            ".datawarehouse.fabric.microsoft.com",
        ],
        Engine::Postgres => &[
            ".postgres.database.azure.com",
            ".postgres.database.usgovcloudapi.net",
            ".postgres.database.chinacloudapi.cn",
        ],
        Engine::Mysql => {
            &[".mysql.database.azure.com", ".mysql.database.usgovcloudapi.net", ".mysql.database.chinacloudapi.cn"]
        }
    };
    suffixes.iter().any(|suffix| host.len() > suffix.len() && host.ends_with(suffix))
}

/// Hosts that may receive an Amazon RDS IAM authentication token: RDS and Aurora endpoints (including RDS Proxy).
pub fn aws_iam_host_allowed(host: &str) -> bool {
    let host = host.trim().trim_end_matches('.').to_ascii_lowercase();
    [".rds.amazonaws.com", ".rds.amazonaws.com.cn"]
        .iter()
        .any(|suffix| host.len() > suffix.len() && host.ends_with(suffix))
}

/// A database on this machine (loopback or a local socket). Only these default to opportunistic TLS; every other host
/// gets a verified certificate unless the connection string explicitly asks for less.
pub fn is_local_host(host: &str) -> bool {
    let host = host.trim().trim_end_matches('.').trim_start_matches('[').trim_end_matches(']').to_ascii_lowercase();
    if host == "localhost" || host.ends_with(".localhost") || host.starts_with('/') {
        return true;
    }
    host.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

impl ConnectionSpec {
    pub fn option(&self, key: &str) -> Option<&str> {
        self.options.iter().find(|(k, _)| k.eq_ignore_ascii_case(key)).map(|(_, v)| v.as_str())
    }

    pub fn flag(&self, key: &str) -> Option<bool> {
        let value = self.option(key)?;
        Some(matches!(value.to_ascii_lowercase().as_str(), "true" | "yes" | "1" | "on" | "mandatory" | "strict"))
    }
}

impl std::fmt::Debug for ConnectionSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConnectionSpec")
            .field("engine", &self.engine)
            .field("host", &self.host)
            .field("port", &self.port)
            .field("database", &self.database)
            .field("user", &self.user)
            .field("auth", &self.auth)
            .field("environment", &self.environment)
            .field("options", &self.options)
            .field("password", &self.password.as_ref().map(|_| "<redacted>"))
            .field("token", &self.token.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entra_tokens_only_go_to_azure_database_hosts() {
        assert!(entra_host_allowed(Engine::Mssql, "shop.database.windows.net"));
        assert!(entra_host_allowed(Engine::Mssql, "tr5.westeurope1-a.worker.database.windows.net."));
        assert!(!entra_host_allowed(Engine::Mssql, "database.windows.net"));
        assert!(!entra_host_allowed(Engine::Mssql, "shop.database.windows.net.attacker.example"));
        assert!(!entra_host_allowed(Engine::Mssql, "evil-database.windows.net.example.com"));
        assert!(!entra_host_allowed(Engine::Mssql, "127.0.0.1"));
        assert!(entra_host_allowed(Engine::Postgres, "shop.postgres.database.azure.com"));
        assert!(!entra_host_allowed(Engine::Postgres, "shop.database.windows.net"));
        assert!(entra_host_allowed(Engine::Mysql, "shop.mysql.database.azure.com"));
        assert!(!entra_host_allowed(Engine::Mysql, "shop.postgres.database.azure.com"));
    }

    #[test]
    fn only_loopback_is_local() {
        for host in
            ["localhost", "127.0.0.1", "127.8.0.1", "::1", "[::1]", "api.localhost", "/var/run/mysqld/mysqld.sock"]
        {
            assert!(is_local_host(host), "{host}");
        }
        for host in ["db.example.com", "10.0.0.5", "192.168.1.10", "localhost.example.com", "0.0.0.0"] {
            assert!(!is_local_host(host), "{host}");
        }
    }

    #[test]
    fn iam_tokens_only_go_to_rds_hosts() {
        assert!(aws_iam_host_allowed("shop.abc123.us-east-1.rds.amazonaws.com"));
        assert!(aws_iam_host_allowed("shop.cluster-ro-abc123.eu-central-1.rds.amazonaws.com."));
        assert!(aws_iam_host_allowed("shop.abc123.cn-north-1.rds.amazonaws.com.cn"));
        assert!(!aws_iam_host_allowed("rds.amazonaws.com"));
        assert!(!aws_iam_host_allowed("shop.rds.amazonaws.com.attacker.example"));
        assert!(!aws_iam_host_allowed("evilrds.amazonaws.com.example"));
        assert!(!aws_iam_host_allowed("shop.database.windows.net"));
    }
}
