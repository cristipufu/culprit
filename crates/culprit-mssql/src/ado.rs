use culprit_core::{AuthKind, ConnectionSpec, Engine, Error, Result};

/// Parses an ADO.NET connection string (`Server=host,port;Database=…;User ID=…;Password=…;…`) into a spec.
/// Keys the spec has no field for are kept as driver options.
pub fn parse_ado(ado: &str) -> Result<ConnectionSpec> {
    let mut spec = ConnectionSpec { engine: Some(Engine::Mssql), ..ConnectionSpec::default() };
    for segment in ado.split(';') {
        let Some((key, value)) = segment.split_once('=') else { continue };
        let key = key.trim();
        let value = value.trim().trim_matches('"').trim_matches('\'');
        match key.to_ascii_lowercase().replace(' ', "").as_str() {
            "server" | "datasource" | "address" | "addr" | "networkaddress" => {
                let server = value.trim_start_matches("tcp:");
                let (host, port) = server.split_once(',').unwrap_or((server, ""));
                spec.host = host.trim().to_string();
                if !port.trim().is_empty() {
                    spec.port =
                        Some(port.trim().parse().map_err(|_| Error::invalid(format!("Invalid port `{port}`")))?);
                }
            }
            "database" | "initialcatalog" => spec.database = Some(value.to_string()),
            "userid" | "uid" | "user" | "username" => spec.user = Some(value.to_string()),
            "password" | "pwd" => spec.password = Some(value.to_string()),
            "authentication" => {
                if value.to_ascii_lowercase().contains("active directory") {
                    spec.auth = AuthKind::Entra;
                }
            }
            _ => {
                spec.options.insert(key.to_string(), value.to_string());
            }
        }
    }
    if spec.host.is_empty() {
        return Err(Error::invalid("The connection string has no Server"));
    }
    Ok(spec)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ado_strings() {
        let spec = parse_ado("Server=tcp:db.example.com,1444;Initial Catalog=shop;User ID=reader;Password=p;Encrypt=True;TrustServerCertificate=False").unwrap();
        assert_eq!(spec.host, "db.example.com");
        assert_eq!(spec.port, Some(1444));
        assert_eq!(spec.database.as_deref(), Some("shop"));
        assert_eq!(spec.user.as_deref(), Some("reader"));
        assert_eq!(spec.password.as_deref(), Some("p"));
        assert_eq!(spec.flag("encrypt"), Some(true));
        assert_eq!(spec.flag("trustservercertificate"), Some(false));
    }

    #[test]
    fn detects_entra() {
        let spec = parse_ado("Server=x.database.windows.net;Database=shop;Authentication=\"Active Directory Default\"")
            .unwrap();
        assert_eq!(spec.auth, AuthKind::Entra);
    }
}
