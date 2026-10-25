# SSH connections

Culprit can reach a private server database through an SSH jump host. The database connection keeps its real hostname and port: certificate verification, AWS IAM token signing, Entra ID and cloud metrics continue to use the database endpoint.

## Setup

Install a current OpenSSH client and make sure `ssh` works from a terminal. On Windows, enable the optional [OpenSSH Client](https://learn.microsoft.com/en-us/windows-server/administration/openssh/openssh_install_firstuse). Culprit also checks its standard installation location when launched without a terminal's PATH.

Connect to the bastion with OpenSSH first. Verify its host-key fingerprint with your administrator before trusting it in `known_hosts`. Culprit requires an already trusted host key, refuses changed keys and never opens a terminal prompt.

In **New connection**, keep the real database address in the connection string and enable **SSH jump host**. Enter `reader@bastion.example.com` or a host alias from your SSH configuration. **Identity file & port** optionally overrides the key path or SSH port. Leaving them empty uses OpenSSH's configuration and agent.

For an existing connection, choose **SSH** on its card. This also works for databases saved through Azure or AWS discovery. **Test connection** uses the draft route with the saved database credentials; **Save settings** changes only the route. Disable the checkbox and save to return to a direct connection.

A loopback database address through SSH belongs to the remote machine. Culprit does not label it local; an unclassified new SSH connection defaults to production confirmations. Choose its environment explicitly when it is staging or development.

The CLI uses the same settings:

```sh
culprit conn add shop --env staging \
  --url 'postgres://app@shop.internal.example:5432/shop?sslmode=verify-full' \
  --ssh prod-bastion

culprit conn set shop --ssh reader@bastion.example.com --ssh-port 2222 \
  --ssh-key ~/.ssh/id_ed25519

culprit conn set shop --no-ssh
```

An SSH destination can include its port (`reader@bastion:2222`); use brackets for an IPv6 address with a port (`reader@[2001:db8::1]:2222`). The separate port field or `--ssh-port` must agree with an embedded port.

Identity-file paths are literal paths. Paths containing `%` expansion tokens must be configured through OpenSSH's `IdentityFile` setting instead.

## Authentication and configuration

Culprit delegates SSH authentication to [OpenSSH](https://man.openbsd.org/ssh_config). Your host aliases, `ProxyJump`, `ProxyCommand`, `IdentityAgent`, certificates and hardware-backed keys remain available through its configuration. Required proxy tools must be installed, configured and able to run without interactive terminal input.

Use an SSH agent or an unencrypted identity file. Unlock encrypted keys in the agent before connecting. Culprit stores only the destination, optional port and optional identity-file path in its connection metadata. It does not store an SSH password or key passphrase, display a passphrase prompt or read private-key contents into the UI. A key-file picker selects the path on your machine.

The database's authentication remains separate: its password stays in the keychain, and cloud tokens are requested for the original database hostname. TLS still follows the connection's existing verification settings; the tunnel does not relax certificate checks. The bastion must be able to resolve and reach that hostname and port. SQL Server gateway redirects are forwarded through the bastion too.

## Lifetime and limits

Each connection attempt owns a loopback listener and opens an [`ssh -W`](https://man.openbsd.org/ssh#W) channel for each database TCP session. It creates no persistent background SSH master. An existing control connection configured by you can be reused; Culprit does not close that user-owned master.

Dropping a pending connection cancels its listener. Closing a database session closes its SSH process tree, including proxy children; a half-close drains final response bytes with a one-second idle grace period. The last provider releases the listener and remaining sessions. Cancellation and normal operation deadlines still apply. SSH work is bounded to 16 active routes, 8 sessions per route and 32 sessions per workspace; pending connection attempts count toward those limits. Streams use fixed-size buffers and bounded diagnostic capture. Errors describe the failure without exposing raw proxy output.

SQLite reads a local file and does not offer SSH settings.
