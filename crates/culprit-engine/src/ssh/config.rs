use culprit_core::{Engine, Error, Result, SshConfig};

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) struct Target {
    pub host: String,
    pub port: u16,
}

fn host_name(host: &str) -> Option<&str> {
    if let Some(raw) = host.strip_prefix('[').and_then(|h| h.strip_suffix(']')) {
        return raw.parse::<std::net::Ipv6Addr>().is_ok().then_some(raw);
    }
    (host.parse::<std::net::IpAddr>().is_ok()
        || (!host.is_empty()
            && host.len() <= 253
            && !host.starts_with('-')
            && host.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))))
    .then_some(host)
}

impl Target {
    pub fn new(host: &str, port: u16) -> Result<Self> {
        let host = host_name(host.trim()).filter(|_| port != 0 && !host.chars().any(char::is_control));
        let Some(host) = host else {
            return Err(Error::invalid(
                "SSH forwarding needs a database hostname or IP address and a nonzero TCP port",
            ));
        };
        Ok(Self { host: host.into(), port })
    }

    pub fn forwarding(&self) -> String {
        if self.host.contains(':') {
            format!("[{}]:{}", self.host, self.port)
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }
}

/// Validate before persisting or resolving database credentials. Never let SSH turn an input into shell syntax,
/// including through `%h` / `%r` expansion in a trusted ProxyCommand configuration.
pub(crate) fn normalize(mut ssh: SshConfig, engine: Engine) -> Result<SshConfig> {
    if !engine.has_server() {
        return Err(Error::invalid("SSH forwarding is available for server databases; SQLite opens a local file"));
    }
    let input = ssh.destination.trim().strip_prefix("ssh://").unwrap_or(ssh.destination.trim());
    let (user, host) = input.rsplit_once('@').map_or((None, input), |(user, host)| (Some(user), host));
    if let Some(user) = user
        && (user.is_empty()
            || user.len() > 256
            || user.starts_with('-')
            || !user.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '@' | '\\')))
    {
        return Err(Error::invalid("The SSH user name contains unsupported characters"));
    }
    let (host, embedded_port) = if host.starts_with('[') {
        let end = host.find(']').ok_or_else(|| Error::invalid("The SSH IPv6 address needs a closing bracket"))?;
        let rest = &host[end + 1..];
        (&host[..=end], (!rest.is_empty()).then_some(rest))
    } else if host.matches(':').count() == 1 {
        let separator = host.find(':').unwrap();
        (&host[..separator], Some(&host[separator..]))
    } else {
        (host, None)
    };
    let host = host_name(host).ok_or_else(|| {
        Error::invalid("Enter an SSH host alias or [user@]hostname")
            .with_hint("Brackets require an IPv6 address. SSH options, commands and passwords do not belong here.")
    })?;
    if let Some(port) = embedded_port {
        let port: u16 = port
            .strip_prefix(':')
            .and_then(|p| p.parse().ok())
            .filter(|p| *p > 0)
            .ok_or_else(|| Error::invalid("The SSH port must be between 1 and 65535"))?;
        if ssh.port.is_some_and(|explicit| explicit != port) {
            return Err(Error::invalid("The SSH host and port field specify different ports"));
        }
        ssh.port = Some(port);
    }
    if ssh.port == Some(0) {
        return Err(Error::invalid("The SSH port must be between 1 and 65535"));
    }
    ssh.destination = match user {
        Some(user) => format!("{user}@{host}"),
        None => host.into(),
    };
    ssh.identity_file = ssh
        .identity_file
        .filter(|key| !key.trim().is_empty())
        .map(|key| {
            if key.len() > 4096 || key.chars().any(char::is_control) {
                return Err(Error::invalid("The SSH identity file path contains unsupported characters"));
            }
            let path = match key.trim().strip_prefix("~/").or_else(|| key.trim().strip_prefix("~\\")) {
                Some(rest) => dirs::home_dir()
                    .ok_or_else(|| Error::not_found("Cannot find the home folder for the SSH key"))?
                    .join(rest),
                None => std::path::PathBuf::from(key.trim()),
            };
            let path = std::path::absolute(path)
                .map_err(|_| Error::invalid("Cannot resolve the SSH identity file path"))?
                .to_string_lossy()
                .into_owned();
            // Home/cwd expansion can introduce tokens even when the entered path contains none.
            if path.contains('%') {
                return Err(Error::invalid("The SSH identity file path cannot contain '%' characters")
                    .with_hint("Choose a file without OpenSSH expansion tokens, or configure IdentityFile in your SSH configuration"));
            }
            Ok(path)
        })
        .transpose()?;
    Ok(ssh)
}
