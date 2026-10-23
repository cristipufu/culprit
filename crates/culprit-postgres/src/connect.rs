use std::sync::Arc;
use std::time::Duration;

use culprit_core::{
    AuthKind, ConnectionSpec, Engine, Error, ErrorCode, Result, aws_iam_host_allowed, entra_host_allowed, is_local_host,
};
use rustls::client::WebPkiServerVerifier;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, verify_tls12_signature, verify_tls13_signature};
use rustls::{CertificateError, ClientConfig, DigitallySignedStruct, RootCertStore, SignatureScheme};
use rustls_pki_types::pem::PemObject;
use rustls_pki_types::{CertificateDer, ServerName, UnixTime};
use tokio_postgres::Client;
use tokio_postgres::config::SslMode;
use tokio_postgres_rustls::MakeRustlsConnect;

pub const DEFAULT_PORT: u16 = 5432;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Timeouts applied to a session. Collectors read bigger catalogs than interactive calls.
#[derive(Clone, Copy)]
pub struct SessionLimits {
    pub statement_timeout: Duration,
    /// Culprit's own statements only (a scan): names resolve in the catalog alone, whatever search_path the database
    /// or role sets, so no schema someone else controls can stand in for a catalog view, function or operator. The
    /// sessions that run what a person writes keep the database's search_path.
    pub catalog_path: bool,
}

pub const INTERACTIVE: SessionLimits =
    SessionLimits { statement_timeout: culprit_core::limits::OBSERVE_TIMEOUT, catalog_path: false };
pub const COLLECTOR: SessionLimits = SessionLimits { statement_timeout: Duration::from_secs(20), catalog_path: true };
pub const EXECUTION: SessionLimits =
    SessionLimits { statement_timeout: culprit_core::limits::QUERY_TIMEOUT, catalog_path: false };

#[derive(Clone)]
pub struct Connector {
    config: tokio_postgres::Config,
    tls: MakeRustlsConnect,
    pub host: String,
    pub port: u16,
}

impl Connector {
    pub fn new(spec: &ConnectionSpec) -> Result<Self> {
        if spec.host.trim().is_empty() {
            return Err(Error::invalid("A PostgreSQL connection needs a host"));
        }
        let port = spec.port.unwrap_or(DEFAULT_PORT);
        let mut config = tokio_postgres::Config::new();
        config
            .host(spec.host.trim())
            .port(port)
            .application_name("culprit")
            .connect_timeout(CONNECT_TIMEOUT)
            .keepalives(true);
        if let Some(database) = spec.database.as_deref().filter(|d| !d.is_empty()) {
            config.dbname(database);
        }
        if let Some(user) = spec.user.as_deref().filter(|u| !u.is_empty()) {
            config.user(user);
        }
        match spec.auth {
            AuthKind::Password => {
                if let Some(password) = &spec.password {
                    config.password(password.as_bytes());
                }
            }
            AuthKind::Entra => {
                if !entra_host_allowed(Engine::Postgres, &spec.host) {
                    return Err(Error::new(
                        ErrorCode::PermissionDenied,
                        "Microsoft Entra ID tokens are only sent to Azure Database for PostgreSQL hosts",
                    ));
                }
                let token = spec.token.as_ref().ok_or_else(|| {
                    Error::new(ErrorCode::AuthFailed, "No Microsoft Entra token was resolved for this connection")
                        .with_hint("Sign in with `az login`, then try again.")
                })?;
                config.password(token.as_bytes());
            }
            AuthKind::AwsIam => {
                if !aws_iam_host_allowed(&spec.host) {
                    return Err(Error::new(
                        ErrorCode::PermissionDenied,
                        "AWS IAM authentication tokens are only sent to Amazon RDS hosts",
                    ));
                }
                let token = spec.token.as_ref().ok_or_else(|| {
                    Error::new(ErrorCode::AuthFailed, "No AWS IAM token was resolved for this connection")
                        .with_hint("Sign in with `aws login`, then try again.")
                })?;
                config.password(token.as_bytes());
            }
            AuthKind::None => {}
        }
        let (ssl_mode, tls) = tls_for(spec)?;
        config.ssl_mode(ssl_mode);
        Ok(Connector { config, tls, host: spec.host.trim().to_string(), port })
    }

    /// Open a session with Culprit's safety settings applied: read-only by default, bounded statement and lock waits.
    pub async fn connect(&self, limits: SessionLimits) -> Result<Client> {
        let (client, connection) =
            self.config.connect(self.tls.clone()).await.map_err(|e| map_connect_error(e, &self.host, self.port))?;
        tokio::spawn(async move {
            let _ = connection.await;
        });
        let mut setup = vec![
            "SET default_transaction_read_only = on".to_string(),
            format!("SET statement_timeout = '{}ms'", limits.statement_timeout.as_millis()),
            "SET lock_timeout = '2s'".to_string(),
            "SET idle_in_transaction_session_timeout = '30s'".to_string(),
        ];
        if limits.catalog_path {
            setup.push("SET search_path = pg_catalog, pg_temp".to_string());
        }
        let setup = setup.iter().map(|statement| crate::rows::tagged(statement)).collect::<Vec<_>>().join("; ");
        client.batch_execute(&setup).await.map_err(map_error)?;
        Ok(client)
    }
}

fn tls_for(spec: &ConnectionSpec) -> Result<(SslMode, MakeRustlsConnect)> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    // Entra and IAM tokens are bearer credentials: never send them over unverified TLS.
    let mode = if matches!(spec.auth, AuthKind::Entra | AuthKind::AwsIam) {
        "verify-full".to_string()
    } else {
        // libpq defaults to `prefer`; Culprit only does that for a local server and verifies every other host's
        // certificate unless the connection string asks for less.
        let default = if is_local_host(&spec.host) { "prefer" } else { "verify-full" };
        spec.option("sslmode").unwrap_or(default).to_ascii_lowercase()
    };
    let root_cert = spec.option("sslrootcert").filter(|p| !p.is_empty() && *p != "system");
    let (ssl_mode, verification) = match mode.as_str() {
        "disable" => (SslMode::Disable, Verification::None),
        "allow" | "prefer" => (SslMode::Prefer, Verification::None),
        // libpq: `require` with a root certificate behaves like `verify-ca`.
        "require" if root_cert.is_some() => (SslMode::Require, Verification::Chain),
        "require" => (SslMode::Require, Verification::None),
        "verify-ca" => (SslMode::Require, Verification::Chain),
        "verify-full" => (SslMode::Require, Verification::Full),
        other => {
            return Err(Error::invalid(format!("Unknown sslmode `{other}`"))
                .with_hint("Use one of disable, prefer, require, verify-ca, verify-full."));
        }
    };
    let builder = ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .map_err(|e| Error::internal(format!("TLS setup failed: {e}")))?;
    let config = match verification {
        Verification::None => builder
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(AcceptAnyCertificate(provider)))
            .with_no_client_auth(),
        Verification::Chain => {
            let inner = WebPkiServerVerifier::builder_with_provider(Arc::new(roots(root_cert)?), provider)
                .build()
                .map_err(|e| Error::internal(format!("TLS setup failed: {e}")))?;
            builder.dangerous().with_custom_certificate_verifier(Arc::new(IgnoreHostname(inner))).with_no_client_auth()
        }
        Verification::Full => builder.with_root_certificates(roots(root_cert)?).with_no_client_auth(),
    };
    Ok((ssl_mode, MakeRustlsConnect::new(config)))
}

enum Verification {
    None,
    Chain,
    Full,
}

fn roots(extra_pem: Option<&str>) -> Result<RootCertStore> {
    let mut store = RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let Some(path) = extra_pem else { return Ok(store) };
    let certs = CertificateDer::pem_file_iter(path)
        .map_err(|e| Error::invalid(format!("Cannot read sslrootcert `{path}`: {e}")))?;
    for cert in certs {
        let cert = cert.map_err(|e| Error::invalid(format!("Invalid certificate in `{path}`: {e}")))?;
        store.add(cert).map_err(|e| Error::invalid(format!("Invalid certificate in `{path}`: {e}")))?;
    }
    Ok(store)
}

/// libpq's `prefer` / `require`: encrypt the connection without authenticating the server.
#[derive(Debug)]
struct AcceptAnyCertificate(Arc<CryptoProvider>);

impl ServerCertVerifier for AcceptAnyCertificate {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> std::result::Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(message, cert, dss, &self.0.signature_verification_algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(message, cert, dss, &self.0.signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

/// libpq's `verify-ca`: the chain must lead to a trusted root, the host name is not checked.
#[derive(Debug)]
struct IgnoreHostname(Arc<WebPkiServerVerifier>);

impl ServerCertVerifier for IgnoreHostname {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> std::result::Result<ServerCertVerified, rustls::Error> {
        match self.0.verify_server_cert(end_entity, intermediates, server_name, ocsp_response, now) {
            Err(rustls::Error::InvalidCertificate(
                CertificateError::NotValidForName | CertificateError::NotValidForNameContext { .. },
            )) => Ok(ServerCertVerified::assertion()),
            other => other,
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        self.0.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        self.0.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.supported_verify_schemes()
    }
}

fn io_kind(error: &(dyn std::error::Error + 'static)) -> Option<std::io::ErrorKind> {
    let mut current: Option<&(dyn std::error::Error + 'static)> = Some(error);
    while let Some(e) = current {
        if let Some(io) = e.downcast_ref::<std::io::Error>() {
            return Some(io.kind());
        }
        current = e.source();
    }
    None
}

fn full_message(error: &tokio_postgres::Error) -> String {
    let mut message = error.to_string();
    let mut source = std::error::Error::source(error);
    while let Some(e) = source {
        message.push_str(": ");
        message.push_str(&e.to_string());
        source = e.source();
    }
    message
}

fn map_connect_error(error: tokio_postgres::Error, host: &str, port: u16) -> Error {
    if error.as_db_error().is_some() {
        return map_error(error);
    }
    let message = full_message(&error);
    let lower = message.to_ascii_lowercase();
    match io_kind(&error) {
        Some(std::io::ErrorKind::ConnectionRefused) => {
            return Error::new(ErrorCode::ConnectionFailed, format!("Connection refused by {host}:{port}"))
                .with_hint("Check that PostgreSQL is running and reachable on that host and port.");
        }
        Some(std::io::ErrorKind::TimedOut) => {
            return Error::new(ErrorCode::Timeout, format!("Timed out connecting to {host}:{port}"))
                .with_hint("Check the host, the port, and any firewall or VPN in between.");
        }
        _ => {}
    }
    if lower.contains("timeout") {
        return Error::new(ErrorCode::Timeout, format!("Timed out connecting to {host}:{port}"))
            .with_hint("Check the host, the port, and any firewall or VPN in between.");
    }
    if lower.contains("invalid peer certificate") {
        return Error::new(ErrorCode::ConnectionFailed, message).with_hint(
            "The server's certificate is not trusted. Point `sslrootcert` at its CA bundle, or use sslmode=require to encrypt without verifying.",
        );
    }
    if lower.contains("tls") || lower.contains("certificate") || lower.contains("ssl") {
        return Error::new(ErrorCode::ConnectionFailed, message)
            .with_hint("TLS negotiation failed. Check `sslmode` (disable, prefer, require, verify-ca, verify-full).");
    }
    if lower.contains("failed to lookup")
        || lower.contains("nodename nor servname")
        || lower.contains("name or service not known")
    {
        return Error::new(ErrorCode::ConnectionFailed, format!("Cannot resolve host `{host}`"))
            .with_hint("Check the host name.");
    }
    if lower.contains("password missing") {
        return Error::new(
            ErrorCode::AuthFailed,
            "The server asks for a password and none is saved for this connection",
        )
        .with_hint(
            "Enter it when Culprit asks, or run `culprit conn set-password <name>`. It's kept in the OS keychain.",
        );
    }
    if lower.contains("authentication") {
        return Error::new(ErrorCode::AuthFailed, message).with_hint(
            "The server rejected the user name or password; set it again with `culprit conn set-password <name>`.",
        );
    }
    Error::new(ErrorCode::ConnectionFailed, message)
}

pub fn map_error(error: tokio_postgres::Error) -> Error {
    let Some(db) = error.as_db_error() else {
        let message = full_message(&error);
        if error.is_closed() {
            return Error::new(ErrorCode::ConnectionFailed, "The connection to PostgreSQL was closed");
        }
        return Error::new(ErrorCode::QueryFailed, message);
    };
    let mut message = db.message().to_string();
    if let Some(detail) = db.detail() {
        message.push_str(&format!(" ({detail})"));
    }
    let code = db.code().code();
    match code {
        "28P01" | "28000" => Error::new(ErrorCode::AuthFailed, message)
            .with_hint("Check the user name and password (or token) for this connection."),
        "42501" => Error::new(ErrorCode::PermissionDenied, message).with_hint(
            "Grant the monitoring role: GRANT pg_read_all_stats TO <user>; (or GRANT pg_monitor TO <user>;)",
        ),
        "57014" => Error::new(ErrorCode::Timeout, message)
            .with_hint("The statement hit Culprit's statement timeout and was cancelled."),
        "55P03" => Error::new(ErrorCode::Timeout, message)
            .with_hint("Another session holds a conflicting lock; Culprit gives up after 2 s instead of waiting."),
        "3D000" => Error::new(ErrorCode::NotFound, message).with_hint("Check the database name."),
        "53300" => Error::new(ErrorCode::ConnectionFailed, message)
            .with_hint("The server has no free connection slots right now."),
        "25006" => Error::new(ErrorCode::PermissionDenied, message)
            .with_hint("Culprit runs statements in a read-only transaction."),
        "42601" | "42P01" | "42703" | "42883" | "42P02" => {
            let hint = db.hint().map(str::to_string);
            let e = Error::new(ErrorCode::InvalidInput, message);
            match hint {
                Some(h) => e.with_hint(h),
                None => e,
            }
        }
        _ => {
            let hint = db.hint().map(str::to_string);
            let e = Error::new(ErrorCode::QueryFailed, format!("{message} [{code}]"));
            match hint {
                Some(h) => e.with_hint(h),
                None => e,
            }
        }
    }
}

pub fn sql_state(error: &tokio_postgres::Error) -> Option<&str> {
    error.as_db_error().map(|db| db.code().code())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(sslmode: Option<&str>) -> ConnectionSpec {
        let mut spec = ConnectionSpec { host: "db.example.com".into(), ..Default::default() };
        if let Some(mode) = sslmode {
            spec.options.insert("sslmode".into(), mode.into());
        }
        spec
    }

    #[test]
    fn sslmodes_follow_libpq() {
        // No sslmode: a remote host must present a verifying certificate; a local one stays opportunistic.
        assert!(matches!(tls_for(&spec(None)).unwrap().0, SslMode::Require));
        let local = ConnectionSpec { host: "localhost".into(), ..Default::default() };
        assert!(matches!(tls_for(&local).unwrap().0, SslMode::Prefer));
        assert!(matches!(tls_for(&spec(Some("disable"))).unwrap().0, SslMode::Disable));
        assert!(matches!(tls_for(&spec(Some("require"))).unwrap().0, SslMode::Require));
        assert!(matches!(tls_for(&spec(Some("verify-full"))).unwrap().0, SslMode::Require));
        assert!(tls_for(&spec(Some("bogus"))).is_err());
    }

    #[test]
    fn entra_without_token_is_an_auth_error() {
        let mut s = spec(None);
        s.auth = AuthKind::Entra;
        s.host = "shop.postgres.database.azure.com".into();
        let error = Connector::new(&s).err().unwrap();
        assert_eq!(error.code, ErrorCode::AuthFailed);
    }

    #[test]
    fn iam_tokens_stay_on_verified_rds_hosts() {
        let mut s = spec(Some("disable"));
        s.auth = AuthKind::AwsIam;
        s.token = Some("t".into());
        s.host = "collector.attacker.example".into();
        assert_eq!(Connector::new(&s).err().unwrap().code, ErrorCode::PermissionDenied);
        s.host = "shop.abc123.us-east-1.rds.amazonaws.com".into();
        assert!(matches!(tls_for(&s).unwrap().0, SslMode::Require));
        s.token = None;
        assert_eq!(Connector::new(&s).err().unwrap().code, ErrorCode::AuthFailed);
    }

    #[test]
    fn entra_tokens_stay_on_verified_azure_hosts() {
        let mut s = spec(Some("disable"));
        s.auth = AuthKind::Entra;
        s.token = Some("t".into());
        s.host = "collector.attacker.example".into();
        assert_eq!(Connector::new(&s).err().unwrap().code, ErrorCode::PermissionDenied);
        s.host = "shop.postgres.database.azure.com".into();
        assert!(matches!(tls_for(&s).unwrap().0, SslMode::Require));
    }
}
