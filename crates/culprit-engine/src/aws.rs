//! Amazon RDS through the AWS CLI: IAM authentication tokens and the RDS certificate authorities, so no database secret is
//! stored and IAM tokens only travel over verified TLS.

use std::path::{Path, PathBuf};

use culprit_core::{Engine, Error, ErrorCode, Result, aws_iam_host_allowed};
use tokio::process::Command;

use crate::private_fs;

/// Amazon's public CA bundle for RDS and Aurora endpoints (truststore.pki.rds.amazonaws.com/global/global-bundle.pem).
const RDS_CA_BUNDLE: &[u8] = include_bytes!("../certs/rds-global-bundle.pem");

/// Connection option value that stands for the bundled RDS certificate authorities.
pub const RDS_CA: &str = "rds";

pub const SIGN_IN_HINT: &str = "Sign in with `aws login` (or `aws configure sso` / `aws configure`), then search again";

fn aws_binary() -> &'static str {
    if cfg!(windows) { "aws.exe" } else { "aws" }
}

/// Run the AWS CLI and return stdout, turning "not installed" and "not signed in" into actionable errors.
pub(crate) async fn run(args: &[&str]) -> Result<Vec<u8>> {
    let output = Command::new(aws_binary()).args(args).output().await.map_err(|e| {
        Error::new(ErrorCode::AuthFailed, format!("Could not run the AWS CLI: {e}"))
            .with_hint("Install the AWS CLI (`brew install awscli`), sign in with `aws login`, then try again")
    })?;
    if output.status.success() {
        return Ok(output.stdout);
    }
    Err(cli_error(&String::from_utf8_lossy(&output.stderr)))
}

fn cli_error(stderr: &str) -> Error {
    let message = stderr.trim().trim_start_matches("aws: [ERROR]:").trim().to_string();
    if is_sign_in_problem(&message) {
        return Error::new(ErrorCode::AuthFailed, format!("The AWS CLI is not signed in: {message}"))
            .with_hint(SIGN_IN_HINT);
    }
    if message.contains("AccessDenied")
        || message.contains("UnauthorizedOperation")
        || message.contains("not authorized")
    {
        return Error::new(ErrorCode::PermissionDenied, format!("AWS refused the request: {message}")).with_hint(
            "The signed-in AWS identity needs rds:Describe* (and rds-db:connect for IAM database authentication)",
        );
    }
    Error::new(ErrorCode::QueryFailed, format!("AWS CLI call failed: {message}"))
}

fn is_sign_in_problem(message: &str) -> bool {
    [
        "NoCredentials",
        "Unable to locate credentials",
        "ExpiredToken",
        "Token has expired",
        "token has expired",
        "SSO session",
        "sso session",
        "InvalidClientTokenId",
        "could not be found",
        "aws login",
    ]
    .iter()
    .any(|marker| message.contains(marker))
}

/// `shop.abc123.us-east-1.rds.amazonaws.com` → `us-east-1`.
pub fn region_from_host(host: &str) -> Option<String> {
    let host = host.trim().trim_end_matches('.').to_ascii_lowercase();
    let labels: Vec<&str> = host.split('.').collect();
    let rds = labels.iter().rposition(|l| *l == "rds")?;
    let region = labels.get(rds.checked_sub(1)?)?;
    valid_region(region).then(|| region.to_string())
}

pub fn valid_region(region: &str) -> bool {
    let parts: Vec<&str> = region.split('-').collect();
    parts.len() >= 3
        && parts.iter().all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit()))
        && parts.last().is_some_and(|p| p.chars().all(|c| c.is_ascii_digit()))
        && region.len() <= 25
}

/// `us-east-1` → `use1`, `eu-central-1` → `euc1`, `ap-southeast-2` → `apse2`, `us-gov-west-1` → `usgw1`.
pub fn region_short(region: &str) -> String {
    region
        .split('-')
        .map(|part| match part {
            "east" => "e".to_string(),
            "west" => "w".to_string(),
            "north" => "n".to_string(),
            "south" => "s".to_string(),
            "central" => "c".to_string(),
            "northeast" => "ne".to_string(),
            "northwest" => "nw".to_string(),
            "southeast" => "se".to_string(),
            "southwest" => "sw".to_string(),
            "gov" => "g".to_string(),
            other => other.to_string(),
        })
        .collect()
}

/// A 15-minute RDS IAM authentication token for `user` on `host`, refused unless `host` is an RDS endpoint.
pub async fn iam_token(engine: Engine, host: &str, port: u16, user: &str, region: &str) -> Result<String> {
    if engine == Engine::Mssql {
        return Err(Error::new(
            ErrorCode::Unsupported,
            "Amazon RDS for SQL Server does not support IAM database authentication",
        )
        .with_hint("Use password authentication for this connection"));
    }
    if !aws_iam_host_allowed(host) {
        return Err(Error::new(
            ErrorCode::PermissionDenied,
            format!("Refusing to send an AWS IAM token to '{host}', which is not an Amazon RDS endpoint"),
        )
        .with_hint("IAM database authentication only works with *.rds.amazonaws.com hosts; use password authentication for other servers"));
    }
    if !valid_region(region) {
        return Err(Error::invalid(format!("'{region}' is not an AWS region")));
    }
    let port = port.to_string();
    let stdout = run(&[
        "rds",
        "generate-db-auth-token",
        "--hostname",
        host,
        "--port",
        &port,
        "--username",
        user,
        "--region",
        region,
        "--output",
        "text",
    ])
    .await?;
    let token = String::from_utf8_lossy(&stdout).trim().to_string();
    if token.is_empty() {
        return Err(
            Error::new(ErrorCode::AuthFailed, "The AWS CLI returned an empty IAM token").with_hint(SIGN_IN_HINT)
        );
    }
    Ok(token)
}

/// Write the bundled RDS certificate authorities next to Culprit's data, once, and return the file.
pub fn rds_ca_file(data_dir: &Path) -> Result<PathBuf> {
    let dir = data_dir.join("certs");
    let path = dir.join("rds-global-bundle.pem");
    let current = std::fs::read(&path).ok();
    if current.as_deref() != Some(RDS_CA_BUNDLE) {
        private_fs::create_dir(&dir).map_err(|e| Error::internal(format!("{}: {e}", dir.display())))?;
        private_fs::write_atomic(&path, RDS_CA_BUNDLE)
            .map_err(|e| Error::internal(format!("{}: {e}", path.display())))?;
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn regions_from_rds_hosts() {
        assert_eq!(region_from_host("shop.abc123.us-east-1.rds.amazonaws.com").as_deref(), Some("us-east-1"));
        assert_eq!(
            region_from_host("shop.cluster-ro-x.eu-central-1.rds.amazonaws.com").as_deref(),
            Some("eu-central-1")
        );
        assert_eq!(region_from_host("shop.abc.cn-north-1.rds.amazonaws.com.cn").as_deref(), Some("cn-north-1"));
        assert_eq!(region_from_host("localhost"), None);
        assert!(valid_region("us-gov-west-1"));
        assert!(!valid_region("us-east-1; rm -rf"));
    }

    #[test]
    fn short_region_codes() {
        assert_eq!(region_short("us-east-1"), "use1");
        assert_eq!(region_short("eu-central-1"), "euc1");
        assert_eq!(region_short("ap-southeast-2"), "apse2");
        assert_eq!(region_short("us-gov-west-1"), "usgw1");
    }

    #[test]
    fn sign_in_errors_are_actionable() {
        let e = cli_error(
            "aws: [ERROR]: An error occurred (NoCredentials): Unable to locate credentials. You can configure credentials by running \"aws login\".",
        );
        assert_eq!(e.code, ErrorCode::AuthFailed);
        assert_eq!(e.hint.as_deref(), Some(SIGN_IN_HINT));
        let e = cli_error("An error occurred (AccessDenied) when calling the DescribeDBInstances operation");
        assert_eq!(e.code, ErrorCode::PermissionDenied);
    }

    #[tokio::test]
    async fn iam_tokens_are_refused_before_calling_aws() {
        let e = iam_token(Engine::Postgres, "collector.attacker.example", 5432, "app", "us-east-1").await.unwrap_err();
        assert_eq!(e.code, ErrorCode::PermissionDenied);
        let e = iam_token(Engine::Mssql, "shop.abc.us-east-1.rds.amazonaws.com", 1433, "app", "us-east-1")
            .await
            .unwrap_err();
        assert_eq!(e.code, ErrorCode::Unsupported);
        let e =
            iam_token(Engine::Postgres, "shop.abc.us-east-1.rds.amazonaws.com", 5432, "app", "x; y").await.unwrap_err();
        assert_eq!(e.code, ErrorCode::InvalidInput);
    }

    #[test]
    fn bundled_rds_authorities_are_pem() {
        let text = String::from_utf8_lossy(RDS_CA_BUNDLE);
        assert!(text.matches("BEGIN CERTIFICATE").count() > 10);
    }
}
