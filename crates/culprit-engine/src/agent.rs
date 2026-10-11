//! What coding agents may do with a saved connection, the approval queue the desktop app answers, and the audit log.
//!
//! Agents reach Culprit through `culprit mcp`. Findings, statistics, schema and estimated plans never execute application
//! SQL. Running a statement needs the connection's `query` access, a single read-only statement, and a person approving
//! that exact statement in the desktop app. Requests and decisions are private files in the data directory; a decision
//! carries a hash of the connection and the SQL, so it can't approve different text than the person saw.

use std::fs;
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use culprit_core::{Environment, Error, ErrorCode, Result};
use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::Workspace;
use crate::private_fs;

/// How long a person has to answer an approval request.
pub const APPROVAL_TIMEOUT: Duration = Duration::from_secs(60);
/// The desktop app counts as running when it looked at the approval queue this recently.
const APP_ALIVE: Duration = Duration::from_secs(5);
const POLL: Duration = Duration::from_millis(200);
/// Requests and decisions nobody picked up are removed after this long.
const STALE: Duration = Duration::from_secs(300);
const AUDIT_ROTATE_BYTES: u64 = 10 * 1024 * 1024;

#[derive(Serialize, Deserialize, TS, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
#[ts(export)]
pub enum AgentAccess {
    /// Agents don't see the connection at all.
    Off,
    /// Findings, query statistics, schema, live sessions and estimated plans. Nothing that runs application SQL.
    #[default]
    Stats,
    /// Also single read-only statements, each one approved by a person in the desktop app.
    Query,
}

impl AgentAccess {
    pub fn label(self) -> &'static str {
        match self {
            AgentAccess::Off => "off",
            AgentAccess::Stats => "stats",
            AgentAccess::Query => "query",
        }
    }
}

/// A statement an agent wants to run, waiting for a person to approve or deny it in the desktop app.
#[derive(Serialize, Deserialize, TS, Clone, Debug)]
#[ts(export)]
pub struct ApprovalRequest {
    pub id: String,
    pub connection: String,
    pub environment: Environment,
    pub tool: String,
    pub sql: String,
    /// SHA-256 of the connection name and the SQL. A decision only applies to this exact pair.
    pub hash: String,
    /// The agent's name as its MCP client reported it.
    pub client: String,
    pub requested_at: String,
    pub expires_at: String,
}

#[derive(Serialize, Deserialize, Debug)]
struct Decision {
    id: String,
    hash: String,
    approved: bool,
    decided_at: String,
}

#[derive(Serialize, Deserialize, TS, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum ApprovalOutcome {
    Approved,
    Denied,
    TimedOut,
    /// The desktop app isn't running, so nobody can approve.
    AppNotRunning,
    /// The approval was for different text than the agent sent.
    Changed,
}

impl ApprovalOutcome {
    /// Why a statement didn't run, with what to do about it.
    pub fn error(self, connection: &str) -> Error {
        let (message, hint) = match self {
            ApprovalOutcome::Approved => return Error::internal("approved"),
            ApprovalOutcome::Denied => {
                ("The statement was denied in the Culprit app", "Ask the person why before trying again")
            }
            ApprovalOutcome::TimedOut => (
                "Nobody approved the statement in the Culprit app within 60 seconds",
                "Ask the person to watch for the approval dialog in Culprit, then call run_query again",
            ),
            ApprovalOutcome::AppNotRunning => (
                "The Culprit desktop app isn't running, so nobody can approve the statement",
                "Ask the person to open Culprit, then call run_query again",
            ),
            ApprovalOutcome::Changed => (
                "The approval in the Culprit app was for different text than this statement",
                "Call run_query again with the statement you want to run",
            ),
        };
        Error::new(ErrorCode::PermissionDenied, format!("{message} ({connection})")).with_hint(hint)
    }
}

#[derive(Serialize, Deserialize, TS, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
#[ts(export)]
pub enum AuditOutcome {
    Ok,
    Error,
    Denied,
}

/// One tool call by a coding agent. SQL text is recorded for tools that send SQL to the database.
#[derive(Serialize, Deserialize, TS, Clone, Debug)]
#[ts(export)]
pub struct AuditEntry {
    pub ts: String,
    pub client: String,
    pub tool: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connection: Option<String>,
    pub outcome: AuditOutcome,
    pub duration_ms: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sql: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// SHA-256 over the connection name and the SQL, hex-encoded.
pub fn approval_hash(connection: &str, sql: &str) -> String {
    let mut ctx = ring::digest::Context::new(&ring::digest::SHA256);
    ctx.update(connection.as_bytes());
    ctx.update(&[0]);
    ctx.update(sql.as_bytes());
    hex(ctx.finish().as_ref())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn new_id() -> Result<String> {
    use ring::rand::SecureRandom;
    let mut bytes = [0u8; 16];
    ring::rand::SystemRandom::new().fill(&mut bytes).map_err(|_| Error::internal("No system randomness"))?;
    Ok(hex(&bytes))
}

fn valid_id(id: &str) -> bool {
    id.len() == 32 && id.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

fn now() -> chrono::DateTime<chrono::Utc> {
    chrono::Utc::now()
}

fn rfc3339(t: chrono::DateTime<chrono::Utc>) -> String {
    t.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

fn expired(expires_at: &str) -> bool {
    chrono::DateTime::parse_from_rfc3339(expires_at).map(|t| t < now()).unwrap_or(true)
}

fn io_error(path: &Path, error: io::Error) -> Error {
    Error::internal(format!("{}: {error}", path.display()))
}

fn older_than(path: &Path, age: Duration) -> bool {
    fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| SystemTime::now().duration_since(t).ok())
        .is_some_and(|elapsed| elapsed > age)
}

struct Queue {
    dir: PathBuf,
}

impl Queue {
    fn request(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{id}.request.json"))
    }

    fn decision(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{id}.decision.json"))
    }

    fn heartbeat(&self) -> PathBuf {
        self.dir.join("app.heartbeat")
    }

    fn ensure(&self) -> Result<()> {
        private_fs::create_dir(&self.dir).map_err(|e| io_error(&self.dir, e))
    }

    fn app_running(&self) -> bool {
        self.heartbeat().exists() && !older_than(&self.heartbeat(), APP_ALIVE)
    }

    fn write<T: Serialize>(&self, path: &Path, value: &T) -> Result<()> {
        let bytes = serde_json::to_vec_pretty(value).map_err(|e| Error::internal(e.to_string()))?;
        private_fs::write_atomic(path, &bytes).map_err(|e| io_error(path, e))
    }

    fn read<T: for<'de> Deserialize<'de>>(&self, path: &Path) -> Option<T> {
        serde_json::from_slice(&fs::read(path).ok()?).ok()
    }

    fn remove(&self, id: &str) {
        let _ = fs::remove_file(self.request(id));
        let _ = fs::remove_file(self.decision(id));
    }

    /// Drop requests and decisions nobody will read any more.
    fn sweep(&self) {
        let Ok(entries) = fs::read_dir(&self.dir) else { return };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            if (name.ends_with(".request.json") || name.ends_with(".decision.json")) && older_than(&path, STALE) {
                let _ = fs::remove_file(&path);
            }
        }
    }
}

impl Workspace {
    fn queue(&self) -> Queue {
        Queue { dir: self.paths.data.join("approvals") }
    }

    fn audit_file(&self) -> PathBuf {
        self.paths.data.join("audit.jsonl")
    }

    /// What coding agents may do with a saved connection (`stats` unless set).
    pub fn agent_access(&self, name: &str) -> Result<AgentAccess> {
        Ok(self.store.get(name)?.agent.unwrap_or_default())
    }

    pub fn set_agent_access(&self, name: &str, access: AgentAccess) -> Result<crate::ConnectionInfo> {
        let mut stored = self.store.get(name)?;
        stored.agent = (access != AgentAccess::default()).then_some(access);
        self.store.put(name, stored)?;
        self.connection(name)
    }

    /// Queue `sql` for a person to approve in the desktop app and wait for the answer. Never runs anything itself.
    pub async fn request_approval(
        &self,
        connection: &str,
        tool: &str,
        sql: &str,
        client: &str,
        timeout: Duration,
    ) -> Result<ApprovalOutcome> {
        let info = self.connection(connection)?;
        let queue = self.queue();
        queue.ensure()?;
        queue.sweep();
        if !queue.app_running() {
            return Ok(ApprovalOutcome::AppNotRunning);
        }
        let id = new_id()?;
        let hash = approval_hash(connection, sql);
        let requested = now();
        let expires = requested + chrono::Duration::from_std(timeout).unwrap_or(chrono::Duration::seconds(60));
        let request = ApprovalRequest {
            id: id.clone(),
            connection: connection.to_string(),
            environment: info.environment,
            tool: tool.to_string(),
            sql: sql.to_string(),
            hash: hash.clone(),
            client: client.to_string(),
            requested_at: rfc3339(requested),
            expires_at: rfc3339(expires),
        };
        queue.write(&queue.request(&id), &request)?;
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(decision) = queue.read::<Decision>(&queue.decision(&id)) {
                queue.remove(&id);
                return Ok(match (decision.approved, decision.id == id && decision.hash == hash) {
                    (true, true) => ApprovalOutcome::Approved,
                    (true, false) => ApprovalOutcome::Changed,
                    (false, _) => ApprovalOutcome::Denied,
                });
            }
            if Instant::now() >= deadline {
                queue.remove(&id);
                return Ok(ApprovalOutcome::TimedOut);
            }
            tokio::time::sleep(POLL).await;
        }
    }

    /// Requests waiting for a person, oldest first. Calling this marks the desktop app as running.
    pub fn pending_approvals(&self) -> Result<Vec<ApprovalRequest>> {
        let queue = self.queue();
        queue.ensure()?;
        private_fs::write_atomic(&queue.heartbeat(), b"").map_err(|e| io_error(&queue.heartbeat(), e))?;
        queue.sweep();
        let mut pending = Vec::new();
        for entry in fs::read_dir(&queue.dir).map_err(|e| io_error(&queue.dir, e))?.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let Some(id) = name.strip_suffix(".request.json").filter(|id| valid_id(id)) else { continue };
            if queue.decision(id).exists() {
                continue;
            }
            let Some(mut request) = queue.read::<ApprovalRequest>(&entry.path()) else { continue };
            if request.id != id || expired(&request.expires_at) {
                continue;
            }
            // Show the hash of what is actually on screen, not whatever the file claims.
            request.hash = approval_hash(&request.connection, &request.sql);
            pending.push(request);
        }
        pending.sort_by(|a, b| a.requested_at.cmp(&b.requested_at));
        Ok(pending)
    }

    /// Answer a pending request. `hash` is the one shown with the request; approving text that changed since is refused.
    pub fn decide_approval(&self, id: &str, hash: &str, approved: bool) -> Result<()> {
        if !valid_id(id) {
            return Err(Error::invalid("Not an approval request id"));
        }
        let queue = self.queue();
        let request: ApprovalRequest = queue
            .read(&queue.request(id))
            .filter(|r: &ApprovalRequest| !expired(&r.expires_at))
            .ok_or_else(|| Error::not_found("This request is no longer waiting; the agent stopped waiting for it"))?;
        let current = approval_hash(&request.connection, &request.sql);
        let approved_text = approved && current == hash;
        let decision =
            Decision { id: id.to_string(), hash: current, approved: approved_text, decided_at: rfc3339(now()) };
        queue.write(&queue.decision(id), &decision)?;
        if approved && !approved_text {
            return Err(Error::new(
                ErrorCode::PermissionDenied,
                "The statement changed after it was shown, so it was denied",
            ));
        }
        Ok(())
    }

    /// Append one agent tool call to the audit log (readable by the current user only).
    pub fn audit(&self, entry: &AuditEntry) -> Result<()> {
        let path = self.audit_file();
        private_fs::create_dir(&self.paths.data).map_err(|e| io_error(&self.paths.data, e))?;
        if fs::metadata(&path).map(|m| m.len() > AUDIT_ROTATE_BYTES).unwrap_or(false) {
            let _ = fs::rename(&path, path.with_extension("jsonl.1"));
        }
        let mut options = fs::OpenOptions::new();
        options.create(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut line = serde_json::to_vec(entry).map_err(|e| Error::internal(e.to_string()))?;
        line.push(b'\n');
        options.open(&path).and_then(|mut f| f.write_all(&line)).map_err(|e| io_error(&path, e))
    }

    /// The most recent `limit` audit entries, oldest first.
    pub fn audit_log(&self, limit: usize) -> Result<Vec<AuditEntry>> {
        let path = self.audit_file();
        let file = match fs::File::open(&path) {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(io_error(&path, e)),
        };
        let entries: Vec<AuditEntry> = io::BufReader::new(file)
            .lines()
            .map_while(|l| l.ok())
            .filter_map(|l| serde_json::from_str(&l).ok())
            .collect();
        let skip = entries.len().saturating_sub(limit);
        Ok(entries.into_iter().skip(skip).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::NewConnection;

    fn workspace(tag: &str) -> (Workspace, PathBuf) {
        let dir = std::env::temp_dir().join(format!("culprit-agent-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let ws = Workspace::open_in(&dir);
        ws.add_connection(NewConnection {
            name: "pg-demo".into(),
            input: "postgres://app@localhost:55432/shop".into(),
            ..Default::default()
        })
        .unwrap();
        (ws, dir)
    }

    #[test]
    fn access_defaults_to_stats_and_round_trips() {
        let (ws, dir) = workspace("access");
        assert_eq!(ws.agent_access("pg-demo").unwrap(), AgentAccess::Stats);
        assert_eq!(ws.set_agent_access("pg-demo", AgentAccess::Query).unwrap().agent_access, AgentAccess::Query);
        assert_eq!(ws.agent_access("pg-demo").unwrap(), AgentAccess::Query);
        ws.set_agent_access("pg-demo", AgentAccess::Stats).unwrap();
        let text = fs::read_to_string(dir.join("connections.toml")).unwrap();
        assert!(!text.contains("agent"), "the default is not written out");
        let _ = fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn approval_needs_the_app_and_binds_to_the_text() {
        let (ws, dir) = workspace("approval");
        let ws = std::sync::Arc::new(ws);
        let outcome = ws.request_approval("pg-demo", "run_query", "SELECT 1", "test", APPROVAL_TIMEOUT).await.unwrap();
        assert_eq!(outcome, ApprovalOutcome::AppNotRunning);

        // The app approves exactly what it shows.
        assert!(ws.pending_approvals().unwrap().is_empty());
        let app = {
            let ws = ws.clone();
            tokio::spawn(async move {
                loop {
                    if let Some(r) = ws.pending_approvals().unwrap().pop() {
                        assert_eq!((r.sql.as_str(), r.client.as_str()), ("SELECT 1", "test"));
                        ws.decide_approval(&r.id, &r.hash, true).unwrap();
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            })
        };
        let outcome = ws.request_approval("pg-demo", "run_query", "SELECT 1", "test", APPROVAL_TIMEOUT).await.unwrap();
        app.await.unwrap();
        assert_eq!(outcome, ApprovalOutcome::Approved);

        // Someone rewrites the queued request; the person approves what they see, which is not what the agent sent.
        let tamper = {
            let ws = ws.clone();
            tokio::spawn(async move {
                loop {
                    let queue = ws.queue();
                    if let Some(mut r) = ws.pending_approvals().unwrap().pop() {
                        r.sql = "SELECT 2".into();
                        queue.write(&queue.request(&r.id), &r).unwrap();
                        let shown = ws.pending_approvals().unwrap().pop().unwrap();
                        ws.decide_approval(&shown.id, &shown.hash, true).unwrap();
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            })
        };
        let outcome = ws.request_approval("pg-demo", "run_query", "SELECT 1", "test", APPROVAL_TIMEOUT).await.unwrap();
        tamper.await.unwrap();
        assert_eq!(outcome, ApprovalOutcome::Changed);

        // A stale hash from the UI is refused and recorded as a denial.
        let stale = {
            let ws = ws.clone();
            tokio::spawn(async move {
                loop {
                    if let Some(r) = ws.pending_approvals().unwrap().pop() {
                        assert!(ws.decide_approval(&r.id, &approval_hash("pg-demo", "SELECT 3"), true).is_err());
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            })
        };
        let outcome = ws.request_approval("pg-demo", "run_query", "SELECT 1", "test", APPROVAL_TIMEOUT).await.unwrap();
        stale.await.unwrap();
        assert_eq!(outcome, ApprovalOutcome::Denied);

        // Nobody answers.
        ws.pending_approvals().unwrap();
        let outcome =
            ws.request_approval("pg-demo", "run_query", "SELECT 1", "test", Duration::from_millis(300)).await.unwrap();
        assert_eq!(outcome, ApprovalOutcome::TimedOut);
        assert!(ws.pending_approvals().unwrap().is_empty(), "a timed-out request leaves nothing behind");

        assert!(ws.decide_approval("../../etc/passwd", "x", true).is_err());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn audit_appends_privately() {
        let (ws, dir) = workspace("audit");
        for (i, tool) in ["findings", "run_query"].into_iter().enumerate() {
            ws.audit(&AuditEntry {
                ts: rfc3339(now()),
                client: "test".into(),
                tool: tool.into(),
                connection: Some("pg-demo".into()),
                outcome: AuditOutcome::Ok,
                duration_ms: i as f64,
                sql: (tool == "run_query").then(|| "SELECT 1".into()),
                message: None,
            })
            .unwrap();
        }
        let log = ws.audit_log(10).unwrap();
        assert_eq!(log.iter().map(|e| e.tool.as_str()).collect::<Vec<_>>(), ["findings", "run_query"]);
        assert_eq!(ws.audit_log(1).unwrap()[0].sql.as_deref(), Some("SELECT 1"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(dir.join("audit.jsonl")).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        let _ = fs::remove_dir_all(dir);
    }
}
