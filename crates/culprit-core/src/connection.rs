use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::model::{Engine, Environment};
use crate::{Error, ErrorCode, Result};

/// System OpenSSH routing. The destination is `[user@]host` or an alias in the user's SSH configuration.
/// An omitted port uses that configuration (normally 22); an omitted identity file uses its keys and agent.
/// Passphrases are handled by the SSH agent, never stored here.
#[derive(Serialize, Deserialize, TS, Clone, Debug, PartialEq, Eq, Hash)]
#[ts(export)]
pub struct SshConfig {
    pub destination: String,
    pub port: Option<u16>,
    pub identity_file: Option<String>,
}

/// A runtime route to a database. The logical hostname remains the identity for TLS, tokens and cloud metadata.
/// Implementations own their forwarding resources until the last provider releases this handle. A SQL Server
/// gateway may request another endpoint after a validated redirect; that connection must follow the same route.
#[async_trait::async_trait]
pub trait ConnectionTransport: Send + Sync {
    async fn endpoint(&self, host: &str, port: u16) -> Result<ConnectionEndpoint>;
}

/// Diagnostics and cancellation for one physical connection attempt, never shared by hostname or another dial.
/// Dropping the last handle may cancel a listener that has not accepted its database socket yet.
pub trait ConnectionAttempt: Send + Sync {
    fn failure(&self) -> Option<Error>;
}

/// A dial address and its attempt owner. Retain this until the driver finishes connecting, including TLS and login.
pub struct ConnectionEndpoint {
    pub address: SocketAddr,
    pub attempt: Option<Arc<dyn ConnectionAttempt>>,
}

impl ConnectionEndpoint {
    pub fn new(address: SocketAddr) -> Self {
        Self { address, attempt: None }
    }

    pub fn failure(&self) -> Option<Error> {
        self.attempt.as_ref().and_then(|attempt| attempt.failure())
    }
}

pub fn transport_error(endpoint: Option<&ConnectionEndpoint>, error: Error) -> Error {
    if matches!(error.code, ErrorCode::ConnectionFailed | ErrorCode::Timeout)
        && let Some(failure) = endpoint.and_then(ConnectionEndpoint::failure)
    {
        return failure;
    }
    error
}

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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub ssh: Option<SshConfig>,
    /// Runtime-only; serialized input cannot redirect a connection or supply a forwarding owner.
    #[serde(skip)]
    #[ts(skip)]
    pub transport: Option<Arc<dyn ConnectionTransport>>,
    #[serde(skip)]
    #[ts(skip)]
    pub password: Option<String>,
    #[serde(skip)]
    #[ts(skip)]
    pub token: Option<String>,
}

/// A file path as a person writes it, with `~/` meaning their home folder (a SQLite connection's file).
pub fn expand_home(path: &str) -> std::path::PathBuf {
    match (path.strip_prefix("~/"), std::env::var_os("HOME")) {
        (Some(rest), Some(home)) => std::path::PathBuf::from(home).join(rest),
        _ => std::path::PathBuf::from(path),
    }
}

/// Hosts that may receive a Microsoft Entra ID access token for this engine. The token is valid for every Azure database the
/// person can reach, so it must never be sent to a host outside Azure's database domains.
pub fn entra_host_allowed(engine: Engine, host: &str) -> bool {
    let host = host.trim().trim_end_matches('.').to_ascii_lowercase();
    let suffixes: &[&str] = match engine {
        // A file: no server to sign in to.
        Engine::Sqlite | Engine::Yugabyte => &[],
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
    pub fn check_transport(&self) -> Result<()> {
        if self.ssh.is_some() && self.transport.is_none() {
            return Err(Error::invalid("SSH forwarding has not been initialized")
                .with_hint("Open this connection through Culprit's Workspace so its SSH route is managed"));
        }
        Ok(())
    }

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
            .field("ssh", &self.ssh)
            .field("transport", &self.transport.as_ref().map(|_| "<managed>"))
            .field("password", &self.password.as_ref().map(|_| "<redacted>"))
            .field("token", &self.token.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Route;
    #[async_trait::async_trait]
    impl ConnectionTransport for Route {
        async fn endpoint(&self, _: &str, _: u16) -> Result<ConnectionEndpoint> {
            Ok(ConnectionEndpoint::new("127.0.0.1:15432".parse().unwrap()))
        }
    }
    impl ConnectionAttempt for Route {
        fn failure(&self) -> Option<Error> {
            Some(Error::new(ErrorCode::ConnectionFailed, "SSH host key is not trusted"))
        }
    }

    #[test]
    fn forwarding_owners_and_secrets_cannot_cross_serialization() {
        let spec = ConnectionSpec {
            host: "db.example.com".into(),
            transport: Some(Arc::new(Route)),
            password: Some("private-password".into()),
            token: Some("private-token".into()),
            ..Default::default()
        };
        let mut serialized = serde_json::to_value(&spec).unwrap();
        assert!(!serialized.to_string().contains("private-"));
        assert!(serialized.get("transport").is_none());
        serialized["transport"] = serde_json::json!({"address": "attacker.example:1234"});
        let loaded: ConnectionSpec = serde_json::from_value(serialized).unwrap();
        assert!(loaded.transport.is_none() && loaded.password.is_none() && loaded.token.is_none());
    }

    #[test]
    fn an_unresolved_ssh_route_cannot_fall_back_to_a_direct_connection() {
        let spec = ConnectionSpec {
            ssh: Some(SshConfig { destination: "bastion".into(), port: None, identity_file: None }),
            ..Default::default()
        };
        assert!(spec.check_transport().is_err());
        assert!(ConnectionSpec { transport: Some(Arc::new(Route)), ..spec }.check_transport().is_ok());
    }

    #[test]
    fn ssh_error_does_not_replace_a_database_authentication_rejection() {
        let endpoint =
            ConnectionEndpoint { address: "127.0.0.1:15432".parse().unwrap(), attempt: Some(Arc::new(Route)) };
        let auth = Error::new(ErrorCode::AuthFailed, "Database password rejected");
        assert_eq!(transport_error(Some(&endpoint), auth).code, ErrorCode::AuthFailed);
        let closed = Error::new(ErrorCode::ConnectionFailed, "Connection closed");
        assert_eq!(transport_error(Some(&endpoint), closed).message, "SSH host key is not trusted");
    }

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
