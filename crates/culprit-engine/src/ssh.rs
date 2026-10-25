//! Lazy OpenSSH forwarding. Culprit owns loopback listeners continuously; each database TCP session owns an `ssh -W`
//! process tree. The last provider releases the listeners, their sessions and children. No background SSH master is
//! created, no database hostname is rewritten, and there is no direct-network fallback after an SSH failure.
mod command;
mod config;
#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::process::Stdio;
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use culprit_core::{ConnectionSpec, ConnectionTransport, Engine, Error, ErrorCode, Result, SshConfig};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::process::Command;
use tokio::sync::Semaphore;
use tokio::task::{JoinHandle, JoinSet};

use crate::process::OwnedChild;
use config::Target;
pub(crate) use config::normalize;

const MAX_ROUTES: usize = 16;
const MAX_TARGETS: usize = 16;
const MAX_ROUTE_SESSIONS: usize = 8;
const MAX_SESSIONS: usize = 32;
const MAX_STDERR: usize = 16 * 1024;
const STOP_TIMEOUT: Duration = Duration::from_secs(1);

type Factory = Arc<dyn Fn(&SshConfig, &Target) -> Result<Command> + Send + Sync>;
type Failures = Arc<Mutex<HashMap<Target, Failure>>>;

struct Failure {
    owner: Arc<()>,
    error: Option<Error>,
}

pub(crate) fn validate_spec(spec: &mut ConnectionSpec) -> Result<()> {
    if let Some(ssh) = spec.ssh.take() {
        let engine = spec.engine.ok_or_else(|| Error::invalid("Choose a database engine before configuring SSH"))?;
        let ssh = normalize(ssh, engine)?;
        spec.host = Target::new(&spec.host, spec.port.unwrap_or(1))?.host;
        spec.ssh = Some(ssh);
    }
    Ok(())
}

#[derive(PartialEq, Eq, Hash)]
struct Key {
    engine: Option<Engine>,
    host: String,
    port: Option<u16>,
    ssh: SshConfig,
}

pub(crate) struct Manager {
    routes: Mutex<HashMap<Key, Weak<Route>>>,
    sessions: Arc<Semaphore>,
    factory: Factory,
}

impl Default for Manager {
    fn default() -> Self {
        Self {
            routes: Mutex::new(HashMap::new()),
            sessions: Arc::new(Semaphore::new(MAX_SESSIONS)),
            factory: Arc::new(command::command),
        }
    }
}

impl Manager {
    pub fn attach(&self, spec: &mut ConnectionSpec) -> Result<()> {
        validate_spec(spec)?;
        let Some(ssh) = spec.ssh.clone() else { return Ok(()) };
        let target = Target::new(spec.host.trim(), spec.port.unwrap_or(1))?;
        // Fail before database credentials are requested when OpenSSH is missing. This constructs a command only.
        (self.factory)(&ssh, &target)?;
        let key = Key { engine: spec.engine, host: spec.host.clone(), port: spec.port, ssh: ssh.clone() };
        let mut routes = self.routes.lock().map_err(|_| Error::internal("Could not open the SSH route registry"))?;
        routes.retain(|_, route| route.strong_count() > 0);
        let route = if let Some(route) = routes.get(&key).and_then(Weak::upgrade) {
            route
        } else {
            if routes.len() >= MAX_ROUTES {
                return Err(busy("Too many SSH database routes are active"));
            }
            let route = Arc::new(Route {
                ssh: ssh.clone(),
                listeners: tokio::sync::Mutex::new(HashMap::new()),
                failures: Arc::new(Mutex::new(HashMap::new())),
                sessions: self.sessions.clone(),
                local_sessions: Arc::new(Semaphore::new(MAX_ROUTE_SESSIONS)),
                factory: self.factory.clone(),
            });
            routes.insert(key, Arc::downgrade(&route));
            route
        };
        spec.ssh = Some(ssh);
        spec.transport = Some(route);
        Ok(())
    }
}

struct Route {
    ssh: SshConfig,
    listeners: tokio::sync::Mutex<HashMap<Target, Forwarder>>,
    failures: Failures,
    sessions: Arc<Semaphore>,
    local_sessions: Arc<Semaphore>,
    factory: Factory,
}

struct Forwarder {
    address: SocketAddr,
    task: JoinHandle<()>,
}

impl Drop for Forwarder {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[async_trait::async_trait]
impl ConnectionTransport for Route {
    async fn endpoint(&self, host: &str, port: u16) -> Result<SocketAddr> {
        let target = Target::new(host, port)?;
        let mut listeners = self.listeners.lock().await;
        if let Some(forwarder) = listeners.get(&target)
            && !forwarder.task.is_finished()
        {
            return Ok(forwarder.address);
        }
        listeners.remove(&target);
        if listeners.len() >= MAX_TARGETS {
            return Err(busy("Too many SSH redirect endpoints are active for this connection"));
        }
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .map_err(|_| Error::new(ErrorCode::ConnectionFailed, "Could not bind the local SSH forwarding socket"))?;
        let address =
            listener.local_addr().map_err(|_| Error::internal("Could not read the SSH forwarding address"))?;
        let task = tokio::spawn(serve(
            listener,
            self.ssh.clone(),
            target.clone(),
            self.failures.clone(),
            self.sessions.clone(),
            self.local_sessions.clone(),
            self.factory.clone(),
        ));
        listeners.insert(target, Forwarder { address, task });
        Ok(address)
    }

    fn failure(&self, host: &str, port: u16) -> Option<Error> {
        self.failures.lock().ok()?.get(&Target::new(host, port).ok()?)?.error.clone()
    }
}

fn busy(message: &str) -> Error {
    Error::new(ErrorCode::ConnectionFailed, message).with_hint("Finish or cancel another SSH operation, then retry")
}

fn begin(failures: &Failures, target: &Target) -> Arc<()> {
    let owner = Arc::new(());
    if let Ok(mut failures) = failures.lock() {
        failures.insert(target.clone(), Failure { owner: owner.clone(), error: None });
    }
    owner
}

fn record(failures: &Failures, target: &Target, owner: &Arc<()>, error: Error) {
    if let Ok(mut failures) = failures.lock()
        && let Some(current) = failures.get_mut(target)
        && Arc::ptr_eq(&current.owner, owner)
    {
        current.error = Some(error);
    }
}

async fn serve(
    listener: TcpListener,
    ssh: SshConfig,
    target: Target,
    failures: Failures,
    sessions: Arc<Semaphore>,
    local_sessions: Arc<Semaphore>,
    factory: Factory,
) {
    let mut jobs = JoinSet::new();
    loop {
        let accepted = tokio::select! {
            biased;
            _ = jobs.join_next(), if !jobs.is_empty() => continue,
            accepted = listener.accept() => accepted,
        };
        let Ok((mut stream, _)) = accepted else {
            let owner = begin(&failures, &target);
            record(
                &failures,
                &target,
                &owner,
                Error::new(ErrorCode::ConnectionFailed, "The local SSH forwarding listener stopped"),
            );
            break;
        };
        // No unbounded waiting tasks or late SSH launches after a queued caller has retired.
        let owner = begin(&failures, &target);
        let permits = local_sessions
            .clone()
            .try_acquire_owned()
            .and_then(|local| sessions.clone().try_acquire_owned().map(|global| (local, global)));
        let Ok(permits) = permits else {
            record(&failures, &target, &owner, busy("Too many SSH database sessions are active"));
            continue;
        };
        let (ssh, target, failures, factory) = (ssh.clone(), target.clone(), failures.clone(), factory.clone());
        jobs.spawn(async move {
            let _permits = permits;
            if let Err(error) = forward(&mut stream, &ssh, &target, &factory).await {
                // Record before dropping the TCP socket, so a driver's EOF can retain the SSH diagnosis.
                record(&failures, &target, &owner, error);
            }
        });
    }
}

async fn diagnostics(reader: impl AsyncRead + Unpin) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    reader.take((MAX_STDERR + 1) as u64).read_to_end(&mut bytes).await.map_err(|_| command::failure(&[]))?;
    if bytes.len() > MAX_STDERR {
        return Err(Error::new(ErrorCode::ConnectionFailed, "SSH diagnostics exceeded the size limit")
            .with_hint("Check the SSH configuration in a terminal, then retry"));
    }
    Ok(bytes)
}

async fn relay(
    mut input: impl AsyncRead + Unpin,
    mut output: impl AsyncWrite + Unpin,
    bytes: &mut u64,
) -> std::io::Result<()> {
    let mut buffer = [0; 8192];
    loop {
        let read = input.read(&mut buffer).await?;
        if read == 0 {
            return Ok(());
        }
        output.write_all(&buffer[..read]).await?;
        *bytes += read as u64;
    }
}

async fn forward(stream: &mut TcpStream, ssh: &SshConfig, target: &Target, factory: &Factory) -> Result<()> {
    let _ = stream.set_nodelay(true);
    let mut command = factory(ssh, target)?;
    command.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = OwnedChild::spawn(command).map_err(|error| {
        Error::new(
            if error.kind() == std::io::ErrorKind::NotFound {
                ErrorCode::NotFound
            } else {
                ErrorCode::ConnectionFailed
            },
            "Could not start OpenSSH",
        )
        .with_hint("Check that the OpenSSH client is installed and executable")
    })?;
    let stdin = child.child.stdin().take().ok_or_else(|| Error::internal("SSH stdin is unavailable"))?;
    let stdout = child.child.stdout().take().ok_or_else(|| Error::internal("SSH stdout is unavailable"))?;
    let stderr = child.child.stderr().take().ok_or_else(|| Error::internal("SSH stderr is unavailable"))?;
    let (read, write) = stream.split();
    let (mut sent, mut received) = (0, 0);
    let diagnostics = diagnostics(stderr);
    tokio::pin!(diagnostics);
    let mut stderr = None;
    let mut remote_closed = false;
    {
        let outgoing = relay(read, stdin, &mut sent);
        let incoming = relay(stdout, write, &mut received);
        tokio::pin!(outgoing, incoming);
        loop {
            tokio::select! {
                biased;
                bytes = &mut diagnostics, if stderr.is_none() => {
                    let failed = bytes.is_err();
                    stderr = Some(bytes);
                    if failed { break }
                    // Closing stderr does not close stdout: retain every pending database byte.
                },
                _ = &mut outgoing => break,
                _ = &mut incoming => { remote_closed = true; break },
            }
        }
    };
    // Kill the whole owned group/job before reaping the leader; pipes may also be held by ProxyCommand descendants.
    let _ = child.child.start_kill();
    let status = tokio::time::timeout(STOP_TIMEOUT, child.child.wait()).await;
    if matches!(status, Ok(Ok(_))) {
        child.finished = true;
    }
    let stderr = match stderr {
        Some(bytes) => bytes?,
        None => tokio::time::timeout(STOP_TIMEOUT, &mut diagnostics).await.map_err(|_| command::failure(&[]))??,
    };
    if !stderr.is_empty() && (received == 0 || command::fatal(&stderr)) {
        return Err(command::failure(&stderr));
    }
    if remote_closed && received == 0 {
        return Err(command::failure(&[]));
    }
    Ok(())
}
