//! Lazy OpenSSH forwarding. Culprit owns loopback listeners continuously; each database TCP session owns an `ssh -W`
//! process tree. The last provider releases the listeners, their sessions and children. No background SSH master is
//! created, no database hostname is rewritten, and there is no direct-network fallback after an SSH failure.
mod command;
mod config;
#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::Duration;

use culprit_core::{
    ConnectionAttempt, ConnectionEndpoint, ConnectionSpec, ConnectionTransport, Engine, Error, ErrorCode, Result,
    SshConfig,
};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::process::Command;
use tokio::sync::{Notify, Semaphore};
use tokio::task::JoinHandle;

use crate::process::OwnedChild;
use config::Target;
pub(crate) use config::normalize;

const MAX_ROUTES: usize = 16;
const MAX_ROUTE_SESSIONS: usize = 8;
const MAX_SESSIONS: usize = 32;
const MAX_STDERR: usize = 16 * 1024;
const STOP_TIMEOUT: Duration = Duration::from_secs(1);
const DRAIN_IDLE_TIMEOUT: Duration = Duration::from_secs(1);

type Factory = Arc<dyn Fn(&SshConfig, &Target) -> Result<Command> + Send + Sync>;
struct Attempt {
    error: Arc<OnceLock<Error>>,
    canceled: Arc<Notify>,
}

impl ConnectionAttempt for Attempt {
    fn failure(&self) -> Option<Error> {
        self.error.get().cloned()
    }
}

impl Drop for Attempt {
    fn drop(&mut self) {
        self.canceled.notify_one();
    }
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
                state: tokio::sync::Mutex::new(Vec::new()),
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
    state: tokio::sync::Mutex<Vec<Forwarder>>,
    sessions: Arc<Semaphore>,
    local_sessions: Arc<Semaphore>,
    factory: Factory,
}

struct Forwarder {
    task: JoinHandle<()>,
}

impl Drop for Forwarder {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[async_trait::async_trait]
impl ConnectionTransport for Route {
    async fn endpoint(&self, host: &str, port: u16) -> Result<ConnectionEndpoint> {
        let target = Target::new(host, port)?;
        let mut state = self.state.lock().await;
        state.retain(|forwarder| !forwarder.task.is_finished());
        // Reserve capacity before binding or creating a waiting task. Every endpoint accepts exactly one socket.
        let local = self
            .local_sessions
            .clone()
            .try_acquire_owned()
            .map_err(|_| busy("Too many SSH database sessions are active"))?;
        let global =
            self.sessions.clone().try_acquire_owned().map_err(|_| busy("Too many SSH database sessions are active"))?;
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .map_err(|_| Error::new(ErrorCode::ConnectionFailed, "Could not bind the local SSH forwarding socket"))?;
        let address =
            listener.local_addr().map_err(|_| Error::internal("Could not read the SSH forwarding address"))?;
        let attempt = Arc::new(Attempt { error: Arc::new(OnceLock::new()), canceled: Arc::new(Notify::new()) });
        let (error, canceled) = (attempt.error.clone(), attempt.canceled.clone());
        let (ssh, factory) = (self.ssh.clone(), self.factory.clone());
        let task = tokio::spawn(async move {
            let _permits = (local, global);
            let accepted = tokio::select! {
                biased;
                _ = canceled.notified() => return,
                accepted = accept(&listener) => accepted,
            };
            drop(listener);
            let result = match accepted {
                Ok(mut stream) => {
                    let result = forward(&mut stream, &ssh, &target, &factory).await;
                    // Publish this attempt's diagnosis before the driver sees TCP EOF.
                    if let Err(failure) = result {
                        let _ = error.set(failure);
                    }
                    return;
                }
                Err(_) => Error::new(ErrorCode::ConnectionFailed, "The local SSH forwarding listener stopped"),
            };
            let _ = error.set(result);
        });
        state.push(Forwarder { task });
        Ok(ConnectionEndpoint { address, attempt: Some(attempt) })
    }
}

fn busy(message: &str) -> Error {
    Error::new(ErrorCode::ConnectionFailed, message).with_hint("Finish or cancel another SSH operation, then retry")
}

async fn accept(listener: &TcpListener) -> std::io::Result<TcpStream> {
    for retry in 0..5 {
        match listener.accept().await {
            Ok((stream, _)) => return Ok(stream),
            Err(error) if retry == 4 => return Err(error),
            // A transient accept/resource error belongs to this pending dial, never to already running sessions.
            Err(_) => tokio::time::sleep(Duration::from_millis(20 << retry)).await,
        }
    }
    unreachable!()
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
    progress: Option<&Notify>,
) -> std::io::Result<()> {
    let mut buffer = [0; 8192];
    loop {
        let read = input.read(&mut buffer).await?;
        if read == 0 {
            return Ok(());
        }
        *bytes += read as u64;
        output.write_all(&buffer[..read]).await?;
        if let Some(progress) = progress {
            progress.notify_one();
        }
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
        let progress = Notify::new();
        let outgoing = relay(read, stdin, &mut sent, None);
        let incoming = relay(stdout, write, &mut received, Some(&progress));
        let drain = tokio::time::sleep(DRAIN_IDLE_TIMEOUT);
        tokio::pin!(drain);
        let mut half_closed = false;
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
                result = &mut outgoing, if !half_closed => {
                    if result.is_err() { break }
                    half_closed = true;
                    drain.as_mut().reset(tokio::time::Instant::now() + DRAIN_IDLE_TIMEOUT);
                },
                _ = progress.notified(), if half_closed => {
                    drain.as_mut().reset(tokio::time::Instant::now() + DRAIN_IDLE_TIMEOUT);
                },
                _ = &mut incoming => { remote_closed = true; break },
                _ = &mut drain, if half_closed => break,
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
        Some(bytes) => bytes,
        None => tokio::time::timeout(STOP_TIMEOUT, &mut diagnostics)
            .await
            .map_err(|_| command::failure(&[]))
            .and_then(|bytes| bytes),
    };
    // Once any database byte arrives, handshake/protocol diagnostics belong to the driver, even if ssh logs EOF.
    if received == 0 {
        let stderr = stderr?;
        if !stderr.is_empty() || remote_closed {
            return Err(command::failure(&stderr));
        }
    }
    Ok(())
}
