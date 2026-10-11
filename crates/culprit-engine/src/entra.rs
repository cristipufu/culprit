//! Microsoft Entra ID tokens through the Azure CLI, so no database secret is ever stored.

use culprit_core::{Engine, Error, ErrorCode, Result, entra_host_allowed};
use tokio::process::Command;

/// Token audience per engine: Azure SQL and Azure Database for PostgreSQL use different resources.
fn resource(engine: Engine) -> &'static str {
    match engine {
        Engine::Mssql => "https://database.windows.net/",
        Engine::Postgres | Engine::Mysql => "https://ossrdbms-aad.database.windows.net",
    }
}

/// A Microsoft Entra ID token for `host`, refused unless `host` is an Azure database host for that engine.
pub async fn token_for(engine: Engine, host: &str) -> Result<String> {
    if !entra_host_allowed(engine, host) {
        return Err(Error::new(
            ErrorCode::PermissionDenied,
            format!("Refusing to send a Microsoft Entra ID token to '{host}', which is not an Azure {engine} host"),
        )
        .with_hint("Entra ID authentication only works with Azure database hosts; use password authentication for other servers"));
    }
    let output = Command::new(az_binary())
        .args([
            "account",
            "get-access-token",
            "--resource",
            resource(engine),
            "--query",
            "accessToken",
            "--output",
            "tsv",
        ])
        .output()
        .await
        .map_err(|e| {
            Error::new(ErrorCode::AuthFailed, format!("Could not run the Azure CLI: {e}"))
                .with_hint("Install the Azure CLI and run `az login`, or use password authentication")
        })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(Error::new(ErrorCode::AuthFailed, format!("Azure CLI could not issue a token: {}", stderr.trim()))
            .with_hint("Run `az login` (and `az account set --subscription …` if needed), then retry"));
    }
    let token = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if token.is_empty() {
        return Err(Error::new(ErrorCode::AuthFailed, "Azure CLI returned an empty token")
            .with_hint("Run `az login` and retry"));
    }
    Ok(token)
}

fn az_binary() -> &'static str {
    if cfg!(windows) { "az.cmd" } else { "az" }
}
