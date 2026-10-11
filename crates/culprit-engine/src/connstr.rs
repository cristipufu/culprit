//! Turns whatever a person pastes (URL, ADO, JDBC, libpq key/value, bare host) into a `ConnectionSpec`.

use culprit_core::{AuthKind, ConnectionSpec, Engine, Error, Result};

pub fn parse(input: &str) -> Result<ConnectionSpec> {
    let input = input.trim();
    if input.is_empty() {
        return Err(Error::invalid("Paste a connection string or a host name"));
    }
    let lower = input.to_ascii_lowercase();
    if let Some(rest) = strip_any(&lower, input, &["jdbc:sqlserver://"]) {
        return parse_jdbc_sqlserver(rest);
    }
    if let Some(rest) = strip_any(&lower, input, &["jdbc:postgresql://"]) {
        return parse_url(Engine::Postgres, rest);
    }
    if let Some(rest) = strip_any(&lower, input, &["postgres://", "postgresql://"]) {
        return parse_url(Engine::Postgres, rest);
    }
    if let Some(rest) = strip_any(&lower, input, &["mssql://", "sqlserver://"]) {
        return parse_url(Engine::Mssql, rest);
    }
    if input.contains(';') && input.contains('=') {
        return parse_ado(input);
    }
    if input.contains('=') {
        return parse_libpq(input);
    }
    parse_bare(input)
}

fn strip_any<'a>(lower: &str, input: &'a str, prefixes: &[&str]) -> Option<&'a str> {
    prefixes.iter().find(|p| lower.starts_with(*p)).map(|p| &input[p.len()..])
}

fn decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Some(Ok(b)) = value.get(i + 1..i + 3).map(|hex| u8::from_str_radix(hex, 16))
        {
            out.push(b);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn parse_url(engine: Engine, rest: &str) -> Result<ConnectionSpec> {
    let (main, query) = rest.split_once('?').unwrap_or((rest, ""));
    let (authority, path) = main.split_once('/').unwrap_or((main, ""));
    let (userinfo, hostport) = match authority.rsplit_once('@') {
        Some((u, h)) => (Some(u), h),
        None => (None, authority),
    };
    let mut spec = ConnectionSpec { engine: Some(engine), ..Default::default() };
    if let Some(userinfo) = userinfo {
        let (user, password) = userinfo.split_once(':').unwrap_or((userinfo, ""));
        if !user.is_empty() {
            spec.user = Some(decode(user));
        }
        if !password.is_empty() {
            spec.password = Some(decode(password));
        }
    }
    let (host, port) = split_host_port(hostport, ':')?;
    spec.host = host;
    spec.port = port;
    let database = decode(path.trim_matches('/'));
    if !database.is_empty() {
        spec.database = Some(database);
    }
    for pair in query.split('&').filter(|p| !p.is_empty()) {
        let (k, v) = pair.split_once('=').unwrap_or((pair, "true"));
        absorb_option(&mut spec, &decode(k), &decode(v));
    }
    finish(spec)
}

fn parse_ado(input: &str) -> Result<ConnectionSpec> {
    let mut spec = ConnectionSpec { engine: Some(Engine::Mssql), ..Default::default() };
    let mut looks_postgres = false;
    for segment in input.split(';') {
        let Some((k, v)) = segment.split_once('=') else {
            continue;
        };
        let key = k.trim().to_ascii_lowercase();
        let value = v.trim().trim_matches('"').trim_matches('\'').to_string();
        match key.as_str() {
            "server" | "data source" | "address" | "addr" | "network address" => {
                let raw = value.trim_start_matches("tcp:");
                let (host, port) = split_host_port(raw, ',')?;
                spec.host = host;
                spec.port = port;
            }
            "host" => {
                looks_postgres = true;
                spec.host = value;
            }
            "port" => spec.port = value.parse().ok(),
            "database" | "initial catalog" => spec.database = Some(value),
            "user id" | "uid" | "user" | "username" => spec.user = Some(value),
            "password" | "pwd" => spec.password = Some(value),
            _ => absorb_option(&mut spec, k.trim(), &value),
        }
    }
    if looks_postgres {
        spec.engine = Some(Engine::Postgres);
    }
    finish(spec)
}

fn parse_jdbc_sqlserver(rest: &str) -> Result<ConnectionSpec> {
    let (hostport, props) = rest.split_once(';').unwrap_or((rest, ""));
    let (host, port) = split_host_port(hostport, ':')?;
    let mut spec = ConnectionSpec { engine: Some(Engine::Mssql), host, port, ..Default::default() };
    for segment in props.split(';') {
        let Some((k, v)) = segment.split_once('=') else {
            continue;
        };
        match k.trim().to_ascii_lowercase().as_str() {
            "databasename" | "database" => spec.database = Some(v.trim().to_string()),
            "user" | "username" => spec.user = Some(v.trim().to_string()),
            "password" => spec.password = Some(v.trim().to_string()),
            _ => absorb_option(&mut spec, k.trim(), v.trim()),
        }
    }
    finish(spec)
}

fn parse_libpq(input: &str) -> Result<ConnectionSpec> {
    let mut spec = ConnectionSpec { engine: Some(Engine::Postgres), ..Default::default() };
    for pair in input.split_whitespace() {
        let Some((k, v)) = pair.split_once('=') else {
            continue;
        };
        let value = v.trim_matches('\'').to_string();
        match k {
            "host" | "hostaddr" => spec.host = value,
            "port" => spec.port = value.parse().ok(),
            "dbname" => spec.database = Some(value),
            "user" => spec.user = Some(value),
            "password" => spec.password = Some(value),
            _ => absorb_option(&mut spec, k, &value),
        }
    }
    finish(spec)
}

fn parse_bare(input: &str) -> Result<ConnectionSpec> {
    let (hostport, database) = input.split_once('/').unwrap_or((input, ""));
    let (host, port) = split_host_port(hostport, if hostport.contains(',') { ',' } else { ':' })?;
    let engine = guess_engine(&host, port);
    let spec = ConnectionSpec {
        engine,
        host,
        port,
        database: (!database.is_empty()).then(|| database.to_string()),
        ..Default::default()
    };
    finish(spec)
}

fn guess_engine(host: &str, port: Option<u16>) -> Option<Engine> {
    let host = host.to_ascii_lowercase();
    if host.ends_with(".database.windows.net") || port == Some(1433) {
        return Some(Engine::Mssql);
    }
    if host.contains(".postgres.") || host.contains("postgres") || port == Some(5432) {
        return Some(Engine::Postgres);
    }
    None
}

fn split_host_port(raw: &str, separator: char) -> Result<(String, Option<u16>)> {
    let raw = raw.trim();
    if let Some(stripped) = raw.strip_prefix('[') {
        let (host, rest) = stripped.split_once(']').ok_or_else(|| Error::invalid("Unclosed IPv6 address"))?;
        let port = rest.trim_start_matches(separator).parse().ok();
        return Ok((host.to_string(), port));
    }
    match raw.rsplit_once(separator) {
        Some((host, port)) => {
            let port: u16 = port.trim().parse().map_err(|_| Error::invalid(format!("'{port}' is not a valid port")))?;
            Ok((host.trim().to_string(), Some(port)))
        }
        None => Ok((raw.to_string(), None)),
    }
}

fn absorb_option(spec: &mut ConnectionSpec, key: &str, value: &str) {
    let normalized = key.to_ascii_lowercase().replace([' ', '_'], "");
    match normalized.as_str() {
        "authentication" => {
            let v = value.to_ascii_lowercase();
            if v.contains("activedirectory") || v.contains("active directory") || v.contains("entra") {
                spec.auth = AuthKind::Entra;
            }
        }
        "encrypt" => {
            spec.options.insert("encrypt".into(), value.to_string());
        }
        "trustservercertificate" => {
            spec.options.insert("trustServerCertificate".into(), value.to_string());
        }
        "sslmode" => {
            spec.options.insert("sslmode".into(), value.to_string());
        }
        "applicationintent" => {
            spec.options.insert("applicationIntent".into(), value.to_string());
        }
        "connecttimeout" | "connectiontimeout" | "logintimeout" => {
            spec.options.insert("connectTimeout".into(), value.to_string());
        }
        _ => {}
    }
}

fn finish(mut spec: ConnectionSpec) -> Result<ConnectionSpec> {
    if spec.host.is_empty() {
        return Err(Error::invalid("The connection string has no host"));
    }
    if spec.engine.is_none() {
        spec.engine = guess_engine(&spec.host, spec.port);
    }
    if spec.host.to_ascii_lowercase().ends_with(".database.windows.net") {
        spec.options.entry("encrypt".into()).or_insert_with(|| "true".into());
    }
    if spec.auth == AuthKind::Password && spec.user.is_none() && spec.password.is_none() && is_azure_sql(&spec.host) {
        spec.auth = AuthKind::Entra;
    }
    Ok(spec)
}

fn is_azure_sql(host: &str) -> bool {
    host.to_ascii_lowercase().ends_with(".database.windows.net")
}

/// A display form with no secrets, used in lists and error messages.
pub fn describe(spec: &ConnectionSpec) -> String {
    let mut out = String::new();
    if let Some(user) = &spec.user {
        out.push_str(user);
        out.push('@');
    }
    out.push_str(&spec.host);
    if let Some(port) = spec.port {
        out.push(':');
        out.push_str(&port.to_string());
    }
    if let Some(db) = &spec.database {
        out.push('/');
        out.push_str(db);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn postgres_url() {
        let s = parse("postgres://app:p%40ss@db.example.com:6432/orders?sslmode=require").unwrap();
        assert_eq!(s.engine, Some(Engine::Postgres));
        assert_eq!(s.host, "db.example.com");
        assert_eq!(s.port, Some(6432));
        assert_eq!(s.user.as_deref(), Some("app"));
        assert_eq!(s.password.as_deref(), Some("p@ss"));
        assert_eq!(s.database.as_deref(), Some("orders"));
        assert_eq!(s.option("sslmode"), Some("require"));
    }

    #[test]
    fn ado_azure_with_entra() {
        let s = parse("Server=tcp:shop-staging.database.windows.net,1433;Initial Catalog=shop;Authentication=Active Directory Default;Encrypt=True").unwrap();
        assert_eq!(s.engine, Some(Engine::Mssql));
        assert_eq!(s.host, "shop-staging.database.windows.net");
        assert_eq!(s.port, Some(1433));
        assert_eq!(s.database.as_deref(), Some("shop"));
        assert_eq!(s.auth, AuthKind::Entra);
        assert_eq!(s.flag("encrypt"), Some(true));
    }

    #[test]
    fn mssql_url_and_jdbc() {
        let s = parse("mssql://sa:secret@127.0.0.1:14333/shop?encrypt=false&trustServerCertificate=true").unwrap();
        assert_eq!(s.engine, Some(Engine::Mssql));
        assert_eq!(s.flag("trustServerCertificate"), Some(true));
        assert_eq!(s.flag("encrypt"), Some(false));
        let j = parse("jdbc:sqlserver://host:1433;databaseName=app;encrypt=true").unwrap();
        assert_eq!(j.database.as_deref(), Some("app"));
        assert_eq!(j.port, Some(1433));
    }

    #[test]
    fn libpq_and_bare() {
        let s = parse("host=localhost port=5432 dbname=app user=me sslmode=disable").unwrap();
        assert_eq!(s.engine, Some(Engine::Postgres));
        assert_eq!(s.database.as_deref(), Some("app"));
        let b = parse("myserver.database.windows.net/shop").unwrap();
        assert_eq!(b.engine, Some(Engine::Mssql));
        assert_eq!(b.auth, AuthKind::Entra);
        assert!(parse("").is_err());
    }

    #[test]
    fn percent_signs_before_multibyte_characters_never_panic() {
        let s = parse("postgres://app:p%aé%20x@h:5432/db").unwrap();
        assert_eq!(s.password.as_deref(), Some("p%aé x"));
    }

    #[test]
    fn describe_hides_password() {
        let s = parse("postgres://app:secret@h:5432/db").unwrap();
        assert_eq!(describe(&s), "app@h:5432/db");
    }
}
