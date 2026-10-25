use std::process::Stdio;

use culprit_core::{Error, ErrorCode, Result, SshConfig};
use tokio::process::Command;

use super::config::Target;
use crate::program::{self, Cli};

pub(super) fn command(ssh: &SshConfig, target: &Target) -> Result<Command> {
    Ok(configure(program::command(Cli::Ssh)?, ssh, target))
}

pub(super) fn configure(mut command: Command, ssh: &SshConfig, target: &Target) -> Command {
    command.args([
        "-T",
        "-a",
        "-x",
        "-k",
        "-oBatchMode=yes",
        "-oStrictHostKeyChecking=yes",
        "-oUpdateHostKeys=no",
        "-oClearAllForwardings=yes",
        "-oExitOnForwardFailure=yes",
        "-oPermitLocalCommand=no",
        "-oForkAfterAuthentication=no",
        "-oControlMaster=no",
        "-oControlPersist=no",
        "-oStdinNull=no",
        "-oRemoteCommand=none",
        "-oConnectionAttempts=1",
        "-oConnectTimeout=10",
        "-oServerAliveInterval=15",
        "-oServerAliveCountMax=2",
        "-oLogLevel=ERROR",
    ]);
    if let Some(port) = ssh.port {
        command.arg("-p").arg(port.to_string());
    }
    if let Some(path) = &ssh.identity_file {
        command.arg("-i").arg(path);
    }
    command.arg("-W").arg(target.forwarding()).arg("--").arg(&ssh.destination);
    command.env("SSH_ASKPASS_REQUIRE", "never").stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    command
}

pub(super) fn failure(stderr: &[u8]) -> Error {
    let message = String::from_utf8_lossy(stderr).to_ascii_lowercase();
    if message.contains("bad configuration option") || message.contains("unsupported option") {
        return Error::new(
            ErrorCode::Unsupported,
            "The installed OpenSSH client does not support the required options",
        )
        .with_hint("Update the OpenSSH client, then retry");
    }
    let (message, hint) = if message.contains("host key") || message.contains("host identification has changed") {
        (
            "SSH host key verification failed",
            "Verify the bastion's fingerprint and trust it with ssh in a terminal, then retry. Culprit requires a trusted host key and never accepts a changed key.",
        )
    } else if message.contains("permission denied")
        || message.contains("signing failed")
        || message.contains("load key")
        || message.contains("identity file")
    {
        (
            "SSH authentication failed",
            "Check the SSH user and identity file. Unlock or load encrypted keys in your OpenSSH agent, and confirm the host alias works with ssh in a terminal.",
        )
    } else if message.contains("administratively prohibited") || message.contains("forwarding disabled") {
        (
            "The SSH server refused database forwarding",
            "Ask the bastion administrator to allow TCP forwarding to this database endpoint.",
        )
    } else if message.contains("connect failed")
        || message.contains("connection refused")
        || message.contains("could not resolve")
        || message.contains("timed out")
    {
        (
            "SSH could not reach the bastion or database",
            "Check the SSH host alias, VPN and database endpoint. The database hostname must be reachable from the bastion.",
        )
    } else {
        (
            "SSH forwarding failed",
            "Check the SSH host alias and agent in a terminal. Culprit uses noninteractive OpenSSH and requires a trusted host key.",
        )
    };
    Error::new(ErrorCode::ConnectionFailed, message).with_hint(hint)
}
