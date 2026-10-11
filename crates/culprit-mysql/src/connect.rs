use std::path::PathBuf;
use std::sync::{Once, OnceLock};
use std::time::Duration;

use culprit_core::{
    AuthKind, ConnectionSpec, Engine, Error, ErrorCode, Result, aws_iam_host_allowed, entra_host_allowed, is_local_host,
};
use mysql_async::prelude::Queryable;
use mysql_async::{Conn, DriverError, IoError, OptsBuilder, SslOpts};

pub const DEFAULT_PORT: u16 = 3306;
/// Marks Culprit's own statements so workload collection leaves them out of the application's history.
pub const TAG: &str = "/* culprit */";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// Every session is read-only, gives up on locks after two seconds and never runs a statement for more than ten
/// seconds. Settings a server refuses are skipped; read-only and UTC are required.
const SESSION_SETUP: [(&str, bool); 4] = [
    ("SET SESSION TRANSACTION READ ONLY", true),
    ("SET SESSION time_zone = '+00:00'", true),
    ("SET SESSION lock_wait_timeout = 2", false),
    ("SET SESSION innodb_lock_wait_timeout = 2", false),
];

/// MySQL and MariaDB share the protocol, information_schema and InnoDB, and differ in performance_schema, EXPLAIN,
/// timeouts and lock views. Every collector that cares asks the connector which one it is talking to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Flavor {
    Mysql,
    Mariadb,
}

impl Flavor {
    /// From `VERSION()`: MariaDB says so (`11.8.9-MariaDB-ubu2404`, `10.11.6-MariaDB-log`).
    pub fn of_version(version: &str) -> Flavor {
        if version.to_ascii_lowercase().contains("mariadb") { Flavor::Mariadb } else { Flavor::Mysql }
    }

    pub fn name(self) -> &'static str {
        match self {
            Flavor::Mysql => "MySQL",
            Flavor::Mariadb => "MariaDB",
        }
    }

    /// The statement timeout: `max_execution_time` (milliseconds, SELECT only) on MySQL, `max_statement_time`
    /// (seconds, every statement) on MariaDB.
    fn timeout(self) -> &'static str {
        match self {
            Flavor::Mysql => "SET SESSION max_execution_time = 10000",
            Flavor::Mariadb => "SET SESSION max_statement_time = 10",
        }
    }

    /// Lifts the timeout for a statement the person chose to run as a change: MySQL never limited it.
    pub fn no_timeout(self) -> Option<&'static str> {
        match self {
            Flavor::Mysql => None,
            Flavor::Mariadb => Some("SET SESSION max_statement_time = 0"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tls {
    Disabled,
    /// Encrypt when the server offers TLS, without verifying its certificate; fall back to plaintext otherwise.
    Preferred,
    /// Encrypt, without verifying the certificate.
    Required,
    /// Encrypt and verify the certificate chain, not the host name.
    VerifyCa,
    /// Encrypt and verify both the chain and the host name.
    VerifyIdentity,
}

impl Tls {
    fn parse(value: &str) -> Result<Tls> {
        match value.trim().to_ascii_lowercase().replace('-', "_").as_str() {
            "disabled" | "disable" | "false" | "off" => Ok(Tls::Disabled),
            "preferred" | "prefer" | "allow" => Ok(Tls::Preferred),
            "required" | "require" | "true" | "on" => Ok(Tls::Required),
            "verify_ca" => Ok(Tls::VerifyCa),
            "verify_identity" | "verify_full" => Ok(Tls::VerifyIdentity),
            other => Err(Error::invalid(format!("Unknown ssl-mode `{other}`"))
                .with_hint("Use one of DISABLED, PREFERRED, REQUIRED, VERIFY_CA, VERIFY_IDENTITY.")),
        }
    }
}

/// Which database a session starts in. Collectors start in none, so their own statements are never attributed to the
/// application's schema in performance_schema; EXPLAIN of captured statements needs the application's schema.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    Server,
    Database,
}

pub struct Connector {
    base: OptsBuilder,
    flavor: OnceLock<Flavor>,
    tls: Tls,
    root_cert: Option<PathBuf>,
    pub host: String,
    pub port: u16,
    pub database: Option<String>,
}

impl Connector {
    pub fn new(spec: &ConnectionSpec) -> Result<Connector> {
        install_crypto_provider();
        let host = spec.host.trim().to_string();
        if host.is_empty() {
            return Err(Error::invalid("A MySQL host is required"));
        }
        let port = spec.port.unwrap_or(DEFAULT_PORT);
        let user = spec.user.clone().filter(|u| !u.is_empty()).ok_or_else(|| {
            Error::invalid("MySQL needs a user name")
                .with_hint("Add one to the connection string, e.g. mysql://app@host/db")
        })?;
        let mode = ["sslmode", "ssl-mode", "ssl_mode"].iter().find_map(|k| spec.option(k));
        // Like the mysql client, a local server gets TLS when it offers it; anything else must present a certificate that
        // verifies, unless the connection string asks for less (ssl-mode=REQUIRED encrypts without verifying).
        let default = if is_local_host(&host) { Tls::Preferred } else { Tls::VerifyIdentity };
        let mut tls = mode.map(Tls::parse).transpose()?.unwrap_or(default);
        let mut base = OptsBuilder::default()
            .ip_or_hostname(host.clone())
            .tcp_port(port)
            .user(Some(user))
            .prefer_socket(false)
            .tcp_nodelay(true)
            .stmt_cache_size(0);
        match spec.auth {
            AuthKind::Password => base = base.pass(spec.password.clone()),
            AuthKind::None => {}
            AuthKind::Entra => {
                if !entra_host_allowed(Engine::Mysql, &host) {
                    return Err(Error::new(
                        ErrorCode::PermissionDenied,
                        "Microsoft Entra ID tokens are only sent to Azure Database for MySQL hosts",
                    ));
                }
                let token = spec.token.clone().ok_or_else(|| {
                    Error::new(ErrorCode::AuthFailed, "No Microsoft Entra ID token is available")
                        .with_hint("Sign in with `az login`, then try again.")
                })?;
                // The token travels as a cleartext password, so it only ever goes over fully verified TLS.
                tls = Tls::VerifyIdentity;
                base = base.pass(Some(token)).enable_cleartext_plugin(true);
            }
            AuthKind::AwsIam => {
                if !aws_iam_host_allowed(&host) {
                    return Err(Error::new(
                        ErrorCode::PermissionDenied,
                        "AWS IAM authentication tokens are only sent to Amazon RDS hosts",
                    ));
                }
                let token = spec.token.clone().ok_or_else(|| {
                    Error::new(ErrorCode::AuthFailed, "No AWS IAM authentication token is available")
                        .with_hint("Sign in with the AWS CLI (aws sso login or aws configure), then try again.")
                })?;
                // RDS IAM tokens go through the cleartext plugin; like Entra tokens, only over verified TLS.
                tls = Tls::VerifyIdentity;
                base = base.pass(Some(token)).enable_cleartext_plugin(true);
            }
        }
        let root_cert = ["sslrootcert", "ssl-ca", "ssl_ca"]
            .iter()
            .find_map(|k| spec.option(k))
            .filter(|p| !p.is_empty())
            .map(PathBuf::from);
        Ok(Connector {
            base,
            flavor: OnceLock::new(),
            tls,
            root_cert,
            host,
            port,
            database: spec.database.clone().filter(|d| !d.is_empty()),
        })
    }

    fn ssl_opts(&self, tls: Tls) -> Option<SslOpts> {
        let roots = |opts: SslOpts| match &self.root_cert {
            Some(path) => opts.with_root_certs(vec![path.clone().into()]),
            None => opts,
        };
        match tls {
            Tls::Disabled => None,
            Tls::Preferred | Tls::Required => {
                Some(SslOpts::default().with_danger_accept_invalid_certs(true).with_danger_skip_domain_validation(true))
            }
            Tls::VerifyCa => Some(roots(SslOpts::default().with_danger_skip_domain_validation(true))),
            Tls::VerifyIdentity => Some(roots(SslOpts::default())),
        }
    }

    /// Open a session in `scope`, prepared with the read-only settings. The session is set up before it enters the
    /// application's database, so the setup statements are never counted as the application's workload.
    pub async fn connect(&self, scope: Scope) -> Result<Conn> {
        let attempt = self.open(self.tls).await;
        let mut conn = match attempt {
            // PREFERRED: the server doesn't do TLS at all, so continue in plaintext like the mysql client does.
            Err(mysql_async::Error::Driver(DriverError::NoClientSslFlagFromServer)) if self.tls == Tls::Preferred => {
                self.open(Tls::Disabled).await.map_err(|e| self.connect_error(e))?
            }
            other => other.map_err(|e| self.connect_error(e))?,
        };
        let flavor = match self.flavor.get() {
            Some(flavor) => *flavor,
            None => {
                let version: Option<String> =
                    conn.query_first(format!("SELECT VERSION() {TAG}")).await.map_err(map_error)?;
                *self.flavor.get_or_init(|| Flavor::of_version(&version.unwrap_or_default()))
            }
        };
        for (statement, required) in SESSION_SETUP.into_iter().chain([(flavor.timeout(), false)]) {
            let result = conn.query_drop(format!("{statement} {TAG}")).await;
            if required {
                result.map_err(map_error)?;
            }
        }
        if let (Scope::Database, Some(db)) = (scope, &self.database) {
            conn.query_drop(format!("USE `{}` {TAG}", db.replace('`', "``"))).await.map_err(map_error)?;
        }
        Ok(conn)
    }

    /// Which server this is, once a session has been opened (MySQL until then).
    pub fn flavor(&self) -> Flavor {
        self.flavor.get().copied().unwrap_or(Flavor::Mysql)
    }

    async fn open(&self, tls: Tls) -> std::result::Result<Conn, mysql_async::Error> {
        // Sessions always open without a database; `connect` enters it once the session is set up.
        let opts = self.base.clone().db_name(None::<String>).ssl_opts(self.ssl_opts(tls));
        match tokio::time::timeout(CONNECT_TIMEOUT, Conn::new(opts)).await {
            Ok(result) => result,
            Err(_) => Err(mysql_async::Error::Io(IoError::Io(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "connection timed out",
            )))),
        }
    }

    fn connect_error(&self, error: mysql_async::Error) -> Error {
        let (host, port) = (&self.host, self.port);
        let mapped = map_error(error);
        if mapped.code != ErrorCode::ConnectionFailed && mapped.code != ErrorCode::Timeout {
            return mapped;
        }
        let lower = mapped.message.to_ascii_lowercase();
        if lower.contains("timed out") {
            return Error::new(ErrorCode::Timeout, format!("Timed out connecting to {host}:{port}"))
                .with_hint("Check the host, the port, and any firewall or VPN in between.");
        }
        if lower.contains("refused") {
            return Error::new(ErrorCode::ConnectionFailed, format!("Nothing is listening on {host}:{port}"))
                .with_hint("Check the host and port, and that the MySQL or MariaDB server is running.");
        }
        if lower.contains("failed to lookup") || lower.contains("nodename nor servname") || lower.contains("not known")
        {
            return Error::new(ErrorCode::ConnectionFailed, format!("Cannot resolve host `{host}`"))
                .with_hint("Check the host name.");
        }
        if lower.contains("certificate") || lower.contains("tls") || lower.contains("ssl") {
            return Error::new(ErrorCode::ConnectionFailed, mapped.message).with_hint(
                "TLS failed. Check `ssl-mode` (DISABLED, PREFERRED, REQUIRED, VERIFY_CA, VERIFY_IDENTITY) and `ssl-ca`.",
            );
        }
        mapped
    }
}

fn install_crypto_provider() {
    static ONCE: Once = Once::new();
    // mysql_async builds its TLS config from the process-wide provider; pin it to ring so it is never ambiguous.
    ONCE.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

pub fn map_error(error: mysql_async::Error) -> Error {
    match error {
        mysql_async::Error::Server(server) => {
            let message = server.message.clone();
            match server.code {
                1045 => Error::new(ErrorCode::AuthFailed, message)
                    .with_hint("Check the user name and password (or token) for this connection."),
                1044 | 1142 | 1143 | 1227 | 1370 => Error::new(ErrorCode::PermissionDenied, message).with_hint(
                    "Grant the monitoring privileges: GRANT PROCESS ON *.* TO <user>; GRANT SELECT ON performance_schema.* TO <user>;",
                ),
                1049 => Error::new(ErrorCode::NotFound, message).with_hint("Check the database name."),
                1792 => Error::new(ErrorCode::PermissionDenied, message)
                    .with_hint("Culprit's sessions are read-only; this statement would write."),
                1205 | 3024 | 1969 => Error::new(ErrorCode::Timeout, message)
                    .with_hint("The statement hit Culprit's lock or execution timeout and was stopped."),
                1064 => Error::new(ErrorCode::QueryFailed, message).with_hint("Check the statement's syntax."),
                _ => Error::new(ErrorCode::QueryFailed, format!("{message} (error {})", server.code)),
            }
        }
        mysql_async::Error::Io(io) => {
            let message = io.to_string();
            let lower = message.to_ascii_lowercase();
            if lower.contains("timed out") {
                return Error::new(ErrorCode::Timeout, message);
            }
            if lower.contains("certificate") {
                return Error::new(ErrorCode::ConnectionFailed, format!("The server's TLS certificate didn't verify: {message}"))
                    .with_hint("Point ssl-ca at the server's CA bundle (ssl-ca=rds for Amazon RDS), or add ssl-mode=REQUIRED to encrypt without verifying the certificate.");
            }
            Error::new(ErrorCode::ConnectionFailed, message)
        }
        mysql_async::Error::Driver(DriverError::ConnectionClosed) => {
            Error::new(ErrorCode::ConnectionFailed, "The server closed the connection")
        }
        mysql_async::Error::Driver(DriverError::NoClientSslFlagFromServer) => {
            Error::new(ErrorCode::ConnectionFailed, "The server does not support TLS")
                .with_hint("Use ssl-mode=PREFERRED or DISABLED for this server, or enable TLS on it.")
        }
        other => Error::new(ErrorCode::QueryFailed, other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> ConnectionSpec {
        ConnectionSpec {
            engine: Some(Engine::Mysql),
            host: "db.example.com".into(),
            user: Some("reader".into()),
            password: Some("secret".into()),
            ..Default::default()
        }
    }

    #[test]
    fn ssl_modes_follow_mysql_names() {
        assert_eq!(Tls::parse("DISABLED").unwrap(), Tls::Disabled);
        assert_eq!(Tls::parse("verify-identity").unwrap(), Tls::VerifyIdentity);
        assert_eq!(Tls::parse("verify_ca").unwrap(), Tls::VerifyCa);
        assert_eq!(Tls::parse("require").unwrap(), Tls::Required);
        assert!(Tls::parse("bogus").is_err());
        // Remote hosts verify the certificate unless the connection string asks for less; local ones stay opportunistic.
        assert_eq!(Connector::new(&spec()).unwrap().tls, Tls::VerifyIdentity);
        let local = ConnectionSpec { host: "127.0.0.1".into(), ..spec() };
        assert_eq!(Connector::new(&local).unwrap().tls, Tls::Preferred);
        let mut relaxed = spec();
        relaxed.options.insert("ssl-mode".into(), "REQUIRED".into());
        assert_eq!(Connector::new(&relaxed).unwrap().tls, Tls::Required);
    }

    #[test]
    fn entra_stays_on_verified_azure_hosts() {
        let mut s = spec();
        s.auth = AuthKind::Entra;
        s.token = Some("t".into());
        assert_eq!(Connector::new(&s).err().unwrap().code, ErrorCode::PermissionDenied);
        s.host = "shop.mysql.database.azure.com".into();
        s.options.insert("sslmode".into(), "disabled".into());
        assert_eq!(Connector::new(&s).unwrap().tls, Tls::VerifyIdentity);
        s.token = None;
        assert_eq!(Connector::new(&s).err().unwrap().code, ErrorCode::AuthFailed);
    }

    #[test]
    fn aws_iam_tokens_stay_on_verified_rds_hosts() {
        let mut s = spec();
        s.auth = AuthKind::AwsIam;
        s.token = Some("t".into());
        assert_eq!(Connector::new(&s).err().unwrap().code, ErrorCode::PermissionDenied);
        s.host = "shop.abc123.eu-central-1.rds.amazonaws.com".into();
        s.options.insert("ssl-mode".into(), "PREFERRED".into());
        s.options.insert("ssl-ca".into(), "/tmp/rds-bundle.pem".into());
        let connector = Connector::new(&s).unwrap();
        assert_eq!(connector.tls, Tls::VerifyIdentity);
        assert_eq!(connector.root_cert.as_deref(), Some(std::path::Path::new("/tmp/rds-bundle.pem")));
        s.token = None;
        assert_eq!(Connector::new(&s).err().unwrap().code, ErrorCode::AuthFailed);
    }

    #[test]
    fn flavor_comes_from_the_version() {
        assert_eq!(Flavor::of_version("11.8.9-MariaDB-ubu2404"), Flavor::Mariadb);
        assert_eq!(Flavor::of_version("5.5.5-10.11.6-MariaDB-log"), Flavor::Mariadb);
        assert_eq!(Flavor::of_version("8.4.3"), Flavor::Mysql);
        assert_eq!(Flavor::Mariadb.no_timeout(), Some("SET SESSION max_statement_time = 0"));
        assert_eq!(Flavor::Mysql.no_timeout(), None);
    }

    #[test]
    fn a_user_is_required() {
        let mut s = spec();
        s.user = None;
        assert_eq!(Connector::new(&s).err().unwrap().code, ErrorCode::InvalidInput);
    }

    #[test]
    fn server_errors_map_to_codes() {
        let server = |code| {
            mysql_async::Error::Server(mysql_async::ServerError { code, message: "m".into(), state: "HY000".into() })
        };
        assert_eq!(map_error(server(1045)).code, ErrorCode::AuthFailed);
        assert_eq!(map_error(server(1142)).code, ErrorCode::PermissionDenied);
        assert_eq!(map_error(server(3024)).code, ErrorCode::Timeout);
        assert_eq!(map_error(server(1049)).code, ErrorCode::NotFound);
    }
}
