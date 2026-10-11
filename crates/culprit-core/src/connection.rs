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
    };
    suffixes.iter().any(|suffix| host.len() > suffix.len() && host.ends_with(suffix))
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
    }
}
