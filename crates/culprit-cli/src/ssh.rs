//! SSH arguments shared by connection creation and settings. The engine validates and normalizes the route.
use clap::Args;
use culprit_core::SshConfig;

#[derive(Args)]
pub struct SshArgs {
    /// Reach the database through this [user@]SSH host or OpenSSH configuration alias.
    #[arg(long, value_name = "HOST")]
    pub ssh: Option<String>,
    /// Override the SSH configuration's port (normally 22).
    #[arg(long, requires = "ssh", value_parser = clap::value_parser!(u16).range(1..), value_name = "PORT")]
    pub ssh_port: Option<u16>,
    /// Use this identity file. Encrypted keys must be unlocked in the OpenSSH agent.
    #[arg(long, requires = "ssh", value_name = "PATH")]
    pub ssh_key: Option<String>,
}

impl SshArgs {
    pub fn config(&self) -> Option<SshConfig> {
        self.ssh.as_ref().map(|destination| SshConfig {
            destination: destination.clone(),
            port: self.ssh_port,
            identity_file: self.ssh_key.clone(),
        })
    }
}
