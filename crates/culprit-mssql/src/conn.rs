use std::sync::{Arc, Mutex};
use std::time::Duration;

use culprit_core::{
    AuthKind, ConnectionEndpoint, ConnectionSpec, ConnectionTransport, Engine, Error, ErrorCode, Result,
    entra_host_allowed,
};
use futures_util::future::BoxFuture;
use tiberius::{AuthMethod, Client, Config, EncryptionLevel};
use tokio::net::TcpStream;
use tokio_util::compat::{Compat, TokioAsyncWriteCompatExt};

use crate::value::Rec;

pub(crate) type MsClient = Client<Compat<TcpStream>>;

pub(crate) const DEFAULT_PORT: u16 = 1433;
const TCP_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);
const COMMAND_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_IDLE: usize = 4;

/// Applied to every statistics session: never wait long on locks, always lose a deadlock, never take shared locks.
const STATS_SESSION_SETUP: &str =
    "SET LOCK_TIMEOUT 2000; SET DEADLOCK_PRIORITY LOW; SET TRANSACTION ISOLATION LEVEL READ UNCOMMITTED;";
/// Applied to sessions that run user statements: same protections, default isolation.
const USER_SESSION_SETUP: &str = "SET LOCK_TIMEOUT 2000; SET DEADLOCK_PRIORITY LOW;";

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum SessionKind {
    Stats,
    User,
}

pub(crate) fn build_config(spec: &ConnectionSpec) -> Result<Config> {
    spec.check_transport()?;
    if spec.host.trim().is_empty() {
        return Err(Error::invalid("A SQL Server host is required"));
    }
    let mut config = Config::new();
    config.host(spec.host.trim());
    config.port(spec.port.unwrap_or(DEFAULT_PORT));
    if let Some(database) = spec.database.as_deref().filter(|d| !d.is_empty()) {
        config.database(database);
    }
    config.application_name("culprit");
    config.handshake_timeout(Some(HANDSHAKE_TIMEOUT));
    config.command_timeout(Some(COMMAND_TIMEOUT));
    config.authentication(auth_method(spec)?);
    let entra = spec.auth == AuthKind::Entra;
    let trust_any_certificate = spec.flag("trustServerCertificate").unwrap_or(false);
    if entra && trust_any_certificate {
        return Err(Error::invalid("Microsoft Entra ID authentication needs a verified server certificate").with_hint(
            "Remove TrustServerCertificate=true: the token would otherwise be readable by anyone in the middle",
        ));
    }
    let level = encryption_level(spec);
    config.encryption(if entra && level == EncryptionLevel::Off { EncryptionLevel::Required } else { level });
    if trust_any_certificate {
        config.trust_cert();
    }
    if let Some(path) = spec.option("trustServerCertificateCA").filter(|p| !p.is_empty()) {
        config.trust_cert_ca(path);
    }
    if let Some(name) = spec.option("hostNameInCertificate") {
        config.hostname_in_certificate(name);
    }
    if spec.option("applicationIntent").is_some_and(|v| v.eq_ignore_ascii_case("readonly")) {
        config.readonly(true);
    }
    Ok(config)
}

fn auth_method(spec: &ConnectionSpec) -> Result<AuthMethod> {
    match spec.auth {
        AuthKind::Password => {
            let user = spec
                .user
                .as_deref()
                .filter(|u| !u.is_empty())
                .ok_or_else(|| Error::invalid("SQL Server password authentication needs a user name"))?;
            Ok(AuthMethod::sql_server(user, spec.password.as_deref().unwrap_or("")))
        }
        AuthKind::Entra => {
            if !entra_host_allowed(Engine::Mssql, &spec.host) {
                return Err(Error::new(
                    ErrorCode::PermissionDenied,
                    "Microsoft Entra ID tokens are only sent to Azure SQL hosts",
                ));
            }
            let token = spec.token.as_deref().ok_or_else(|| {
                Error::new(ErrorCode::AuthFailed, "No Microsoft Entra ID token is available")
                    .with_hint("Sign in with `az login`, then try again.")
            })?;
            Ok(AuthMethod::aad_token(token))
        }
        AuthKind::AwsIam => Err(Error::new(
            ErrorCode::Unsupported,
            "Amazon RDS for SQL Server does not support IAM database authentication",
        )
        .with_hint("Use password authentication for this connection")),
        AuthKind::None => Err(Error::invalid("SQL Server needs either password or Microsoft Entra ID authentication")),
    }
}

fn encryption_level(spec: &ConnectionSpec) -> EncryptionLevel {
    let Some(value) = spec.option("encrypt") else { return EncryptionLevel::Required };
    match value.trim().to_ascii_lowercase().as_str() {
        "strict" => EncryptionLevel::Strict,
        "false" | "no" | "0" | "optional" => EncryptionLevel::Off,
        _ => EncryptionLevel::Required,
    }
}

/// Azure SQL gateways redirect to worker hosts in the same domain; a redirect that leaves it would carry the login (password or
/// Entra token) to someone else.
fn redirect_allowed(original_addr: &str, target_host: &str) -> bool {
    let original_host = original_addr.rsplit_once(':').map(|(h, _)| h).unwrap_or(original_addr);
    if !entra_host_allowed(Engine::Mssql, original_host) {
        return true;
    }
    entra_host_allowed(Engine::Mssql, target_host)
}

async fn endpoint_for(
    config: &Config,
    transport: Option<&dyn ConnectionTransport>,
) -> Result<Option<ConnectionEndpoint>> {
    let Some(transport) = transport else { return Ok(None) };
    let addr = config.get_addr();
    let (host, port) = addr.rsplit_once(':').ok_or_else(|| Error::invalid("Invalid SQL Server endpoint"))?;
    let port = port.parse().map_err(|_| Error::invalid("Invalid SQL Server port"))?;
    transport.endpoint(host, port).await.map(Some)
}

async fn open_tcp(config: Config, endpoint: Option<&ConnectionEndpoint>) -> tiberius::Result<MsClient> {
    let addr = config.get_addr();
    let dial = endpoint.map_or_else(|| addr.clone(), |endpoint| endpoint.address.to_string());
    let tcp = tokio::time::timeout(TCP_CONNECT_TIMEOUT, TcpStream::connect(&dial)).await.map_err(|_| {
        tiberius::error::Error::Io {
            kind: std::io::ErrorKind::TimedOut,
            message: format!("timed out connecting to {addr}"),
        }
    })??;
    tcp.set_nodelay(true)?;
    Client::connect(config, tcp.compat_write()).await
}

/// Connect, following one Azure SQL gateway redirect, and prepare the session.
pub(crate) async fn connect(
    config: &Config,
    transport: Option<&dyn ConnectionTransport>,
    kind: SessionKind,
) -> Result<MsClient> {
    let mut endpoint = endpoint_for(config, transport).await?;
    let attempt = match open_tcp(config.clone(), endpoint.as_ref()).await {
        Err(tiberius::error::Error::Routing { host, port }) => {
            if !redirect_allowed(&config.get_addr(), &host) {
                return Err(Error::new(
                    ErrorCode::ConnectionFailed,
                    format!("Refusing a redirect from an Azure SQL gateway to '{host}', outside Azure SQL"),
                ));
            }
            let mut routed = config.clone();
            routed.host(&host);
            routed.port(port);
            endpoint = endpoint_for(&routed, transport).await?;
            open_tcp(routed, endpoint.as_ref()).await
        }
        other => other,
    };
    let mut client = attempt.map_err(|error| culprit_core::transport_error(endpoint.as_ref(), map_error(error)))?;
    let setup = match kind {
        SessionKind::Stats => STATS_SESSION_SETUP,
        SessionKind::User => USER_SESSION_SETUP,
    };
    execute(&mut client, setup).await.map_err(map_error)?;
    Ok(client)
}

pub(crate) async fn execute(client: &mut MsClient, sql: &str) -> tiberius::Result<()> {
    client.simple_query(sql).await?.into_results().await?;
    Ok(())
}

/// Marks Culprit's own statements so workload collection can leave them out of the application's history.
pub(crate) const OWN_QUERY_MARKER: &str = "/* culprit */";

/// Inserts the marker after each statement keyword, which puts it inside every statement text Query Store records.
fn mark(sql: &str) -> String {
    culprit_core::sql::mark_mssql_statements(sql, OWN_QUERY_MARKER)
}

pub(crate) async fn query(client: &mut MsClient, sql: &str) -> tiberius::Result<Vec<Rec>> {
    let rows = client.simple_query(mark(sql)).await?.into_first_result().await?;
    Ok(rows.iter().map(Rec::from_row).collect())
}

pub(crate) async fn query_one(client: &mut MsClient, sql: &str) -> tiberius::Result<Rec> {
    Ok(query(client, sql).await?.into_iter().next().unwrap_or_default())
}

/// Reuses statistics sessions so repeated calls (Live refreshes every couple of seconds) skip the TLS and login round-trips.
pub(crate) struct Pool {
    config: Config,
    transport: Option<Arc<dyn ConnectionTransport>>,
    idle: Mutex<Vec<MsClient>>,
}

impl Pool {
    pub(crate) fn new(config: Config, transport: Option<Arc<dyn ConnectionTransport>>) -> Self {
        Pool { config, transport, idle: Mutex::new(Vec::new()) }
    }

    pub(crate) async fn connect(&self, kind: SessionKind) -> Result<MsClient> {
        connect(&self.config, self.transport.as_deref(), kind).await
    }

    fn take_idle(&self) -> Option<MsClient> {
        self.idle.lock().ok()?.pop()
    }

    pub(crate) async fn get(&self) -> Result<MsClient> {
        if let Some(client) = self.take_idle() {
            return Ok(client);
        }
        self.connect(SessionKind::Stats).await
    }

    pub(crate) fn put(&self, client: MsClient) {
        let Ok(mut idle) = self.idle.lock() else { return };
        if idle.len() < MAX_IDLE {
            idle.push(client);
        }
    }

    /// Run `f` on a pooled session. A session that died while idle is replaced once.
    pub(crate) async fn with<T, F>(&self, f: F) -> Result<T>
    where
        F: for<'c> Fn(&'c mut MsClient) -> BoxFuture<'c, tiberius::Result<T>>,
    {
        let mut client = self.get().await?;
        match f(&mut client).await {
            Ok(value) => {
                self.put(client);
                Ok(value)
            }
            Err(err) if is_broken_connection(&err) => {
                drop(client);
                let mut fresh = self.connect(SessionKind::Stats).await?;
                let value = f(&mut fresh).await.map_err(map_error)?;
                self.put(fresh);
                Ok(value)
            }
            Err(err) => Err(map_error(err)),
        }
    }
}

fn is_broken_connection(err: &tiberius::error::Error) -> bool {
    matches!(err, tiberius::error::Error::Io { kind, .. } if *kind != std::io::ErrorKind::TimedOut)
}

pub(crate) fn server_code(err: &tiberius::error::Error) -> Option<u32> {
    err.code()
}

pub(crate) fn is_permission_error(err: &tiberius::error::Error) -> bool {
    matches!(server_code(err), Some(229 | 230 | 262 | 297 | 300 | 15151 | 15247))
}

pub(crate) const PERMISSION_HINT: &str = "Grant the monitoring permission: GRANT VIEW DATABASE STATE TO [<user>]; on Azure SQL Database, or GRANT VIEW SERVER STATE TO [<login>]; on SQL Server.";

pub(crate) fn map_error(err: tiberius::error::Error) -> Error {
    use tiberius::error::Error as T;
    match &err {
        T::Server(token) => {
            let message = token.message().to_string();
            match token.code() {
                18456 | 18452 | 18470 | 18486 | 18487 | 18488 => {
                    let hint = if message.contains("token-identified principal") {
                        "This Microsoft Entra identity has no user in the database. Ask an admin to run CREATE USER [you@company.com] FROM EXTERNAL PROVIDER;"
                    } else {
                        "Check the user name and password, and that SQL authentication is enabled on the server."
                    };
                    Error::new(ErrorCode::AuthFailed, message).with_hint(hint)
                }
                4060 => Error::new(ErrorCode::NotFound, message)
                    .with_hint("Check the database name and that this login has a user in it."),
                40615 | 40914 => Error::new(ErrorCode::ConnectionFailed, message)
                    .with_hint("Your IP address is not allowed by the server firewall. Add it under the server's networking settings."),
                40613 | 40501 | 40197 | 49918 | 49919 | 49920 => Error::new(ErrorCode::ConnectionFailed, message)
                    .with_hint("The database is temporarily unavailable or throttled. Try again in a few seconds."),
                1222 => Error::new(ErrorCode::Timeout, message)
                    .with_hint("A lock wait exceeded Culprit's 2 second limit; the server is busy or something is blocking."),
                229 | 230 | 262 | 297 | 300 | 15151 | 15247 => {
                    Error::new(ErrorCode::PermissionDenied, message).with_hint(PERMISSION_HINT)
                }
                _ => Error::new(ErrorCode::QueryFailed, message),
            }
        }
        T::Io { kind, message } if *kind == std::io::ErrorKind::TimedOut => {
            Error::new(ErrorCode::Timeout, format!("SQL Server did not respond in time: {message}"))
                .with_hint("Check the host, port and firewall, or whether the server is under heavy load.")
        }
        T::Io { message, .. } => {
            Error::new(ErrorCode::ConnectionFailed, format!("Could not reach SQL Server: {message}"))
                .with_hint("Check the host and port, and that the server accepts TCP connections from this machine.")
        }
        T::Tls(message) => Error::new(ErrorCode::ConnectionFailed, format!("TLS handshake failed: {message}"))
            .with_hint(
                "If this is a development server with a self-signed certificate, set TrustServerCertificate=true.",
            ),
        T::Routing { host, port } => {
            Error::new(ErrorCode::ConnectionFailed, format!("SQL Server redirected twice (last to {host}:{port})"))
        }
        _ => Error::new(ErrorCode::Internal, err.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Route(Mutex<Vec<(String, u16)>>);
    #[async_trait::async_trait]
    impl ConnectionTransport for Route {
        async fn endpoint(&self, host: &str, port: u16) -> Result<ConnectionEndpoint> {
            self.0.lock().unwrap().push((host.into(), port));
            Ok(ConnectionEndpoint::new("127.0.0.1:11433".parse().unwrap()))
        }
    }

    #[tokio::test]
    async fn gateway_redirects_use_the_route_without_changing_tls_identity_to_loopback() {
        let spec = ConnectionSpec { host: "shop.database.windows.net".into(), ..spec() };
        let config = build_config(&spec).unwrap();
        let route = Route(Mutex::new(Vec::new()));
        assert_eq!(endpoint_for(&config, Some(&route)).await.unwrap().unwrap().address.port(), 11433);
        assert_eq!(config.get_addr(), "shop.database.windows.net:1433");
        let mut routed = config.clone();
        routed.host("worker.database.windows.net");
        routed.port(11001);
        assert!(redirect_allowed(&config.get_addr(), "worker.database.windows.net"));
        assert!(!redirect_allowed(&config.get_addr(), "attacker.example"));
        assert_eq!(endpoint_for(&routed, Some(&route)).await.unwrap().unwrap().address.port(), 11433);
        assert_eq!(routed.get_addr(), "worker.database.windows.net:11001");
        assert_eq!(
            *route.0.lock().unwrap(),
            vec![("shop.database.windows.net".into(), 1433), ("worker.database.windows.net".into(), 11001)]
        );
    }

    fn spec() -> ConnectionSpec {
        ConnectionSpec {
            host: "db.example.com".into(),
            user: Some("reader".into()),
            password: Some("secret".into()),
            ..Default::default()
        }
    }

    #[test]
    fn marks_every_select() {
        assert_eq!(mark("DECLARE @x int = 1;\nSELECT a FROM t"), "DECLARE @x int = 1;\nSELECT /* culprit */ a FROM t");
        assert_eq!(
            mark("WITH s AS (SELECT 1 AS a) SELECT a FROM s"),
            "WITH s AS (SELECT /* culprit */ 1 AS a) SELECT /* culprit */ a FROM s"
        );
        assert_eq!(mark("SELECTED"), "SELECTED");
        assert_eq!(
            mark("SELECT 1 FROM t WHERE name LIKE N'%select%'"),
            "SELECT /* culprit */ 1 FROM t WHERE name LIKE N'%select%'"
        );
    }

    #[test]
    fn encryption_defaults_to_required() {
        assert!(matches!(encryption_level(&spec()), EncryptionLevel::Required));
        let mut s = spec();
        s.options.insert("Encrypt".into(), "false".into());
        assert!(matches!(encryption_level(&s), EncryptionLevel::Off));
        s.options.insert("Encrypt".into(), "strict".into());
        assert!(matches!(encryption_level(&s), EncryptionLevel::Strict));
    }

    #[test]
    fn password_auth_needs_a_user() {
        let mut s = spec();
        s.user = None;
        assert_eq!(build_config(&s).unwrap_err().code, ErrorCode::InvalidInput);
    }

    #[test]
    fn gateway_redirects_stay_inside_azure_sql() {
        assert!(redirect_allowed("shop.database.windows.net:1433", "tr5.westeurope1-a.worker.database.windows.net"));
        assert!(!redirect_allowed("shop.database.windows.net:1433", "collector.attacker.example"));
        assert!(redirect_allowed("sql01.corp.local:1433", "sql02.corp.local"));
    }

    #[test]
    fn entra_refuses_unverified_certificates() {
        let mut s = spec();
        s.auth = AuthKind::Entra;
        s.host = "shop.database.windows.net".into();
        s.token = Some("t".into());
        s.options.insert("trustServerCertificate".into(), "true".into());
        assert!(build_config(&s).is_err());
    }

    #[test]
    fn entra_tokens_are_refused_for_non_azure_hosts() {
        let mut s = spec();
        s.auth = AuthKind::Entra;
        s.host = "collector.attacker.example".into();
        s.token = Some("t".into());
        assert!(build_config(&s).is_err());
    }

    #[test]
    fn entra_needs_a_token() {
        let mut s = spec();
        s.auth = AuthKind::Entra;
        s.host = "shop.database.windows.net".into();
        assert_eq!(build_config(&s).unwrap_err().code, ErrorCode::AuthFailed);
        s.token = Some("t".into());
        assert!(build_config(&s).is_ok());
    }
}
