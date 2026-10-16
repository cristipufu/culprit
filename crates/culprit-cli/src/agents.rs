//! Commands for coding agents: the MCP server, per-connection agent access, the audit log and `agent init`.

use std::io::Write;
use std::path::PathBuf;

use clap::{Args, Subcommand, ValueEnum};
use culprit_core::{Error, Result};
use culprit_engine::{AgentAccess, AuditOutcome, Workspace};
use serde_json::json;

use crate::output::{Format, Out};

#[derive(Args)]
pub struct McpArgs {
    /// Expose only this connection (repeatable). Defaults to every connection whose agent access isn't `off`.
    #[arg(long = "connection", value_name = "NAME")]
    pub connections: Vec<String>,
}

#[derive(Args)]
pub struct AuditArgs {
    /// How many of the most recent entries to show.
    #[arg(long, default_value_t = 50)]
    pub limit: usize,
}

#[derive(Subcommand)]
pub enum AgentCommand {
    /// Teach the coding agents in a repository to use Culprit: an AGENTS.md section, a Claude Code skill and an
    /// `.mcp.json` entry. Safe to re-run; writes no findings, host names or secrets.
    Init(InitArgs),
}

#[derive(Args)]
pub struct InitArgs {
    /// The repository to write into.
    #[arg(long, default_value = ".")]
    pub dir: PathBuf,
    /// Name this connection in the instructions and limit the MCP server to it (repeatable).
    #[arg(long = "connection", value_name = "NAME")]
    pub connections: Vec<String>,
    /// Don't register the MCP server in `.mcp.json`.
    #[arg(long)]
    pub no_mcp: bool,
}

#[derive(Args)]
#[command(
    group = clap::ArgGroup::new("change").required(true).multiple(true).args(["agent", "env", "query_log", "no_query_log"])
)]
pub struct SetArgs {
    pub name: String,
    /// What coding agents may do through `culprit mcp`: off (hidden), stats (findings, statistics, schema, estimated
    /// plans; the default), query (also read-only statements, each approved in the desktop app).
    #[arg(long, value_enum)]
    pub agent: Option<AgentArg>,
    /// The connection's environment. Production refuses session kills and asks before anything that changes data.
    #[arg(long, value_enum)]
    pub env: Option<EnvironmentArg>,
    /// For a database that keeps no statement history (SQLite): the log the application's framework writes, read on
    /// every scan. Rails, Django, Entity Framework Core, Phoenix/Ecto, or JSON lines.
    #[arg(long, value_name = "FILE", conflicts_with = "no_query_log")]
    pub query_log: Option<std::path::PathBuf>,
    /// Stop reading the saved query log.
    #[arg(long)]
    pub no_query_log: bool,
}

#[derive(Clone, Copy, ValueEnum)]
pub enum EnvironmentArg {
    Production,
    Staging,
    Development,
    Local,
}

impl From<EnvironmentArg> for culprit_core::Environment {
    fn from(e: EnvironmentArg) -> culprit_core::Environment {
        match e {
            EnvironmentArg::Production => culprit_core::Environment::Production,
            EnvironmentArg::Staging => culprit_core::Environment::Staging,
            EnvironmentArg::Development => culprit_core::Environment::Development,
            EnvironmentArg::Local => culprit_core::Environment::Local,
        }
    }
}

#[derive(Clone, Copy, ValueEnum)]
pub enum AgentArg {
    Off,
    Stats,
    Query,
}

impl From<AgentArg> for AgentAccess {
    fn from(a: AgentArg) -> AgentAccess {
        match a {
            AgentArg::Off => AgentAccess::Off,
            AgentArg::Stats => AgentAccess::Stats,
            AgentArg::Query => AgentAccess::Query,
        }
    }
}

/// Serve MCP on stdin/stdout. Diagnostics go to stderr only; stdout belongs to the protocol.
pub async fn mcp(ws: Workspace, args: McpArgs) -> Result<u8> {
    for name in &args.connections {
        ws.connection(name)?;
    }
    let options = culprit_mcp::Options { connections: args.connections, ..Default::default() };
    culprit_mcp::serve_stdio(ws, options).await?;
    Ok(0)
}

pub fn set(ws: &Workspace, args: SetArgs, out: &Out) -> Result<u8> {
    if let Some(env) = args.env {
        let info = ws.set_environment(&args.name, env.into())?;
        out.done(&format!("{} is now marked {}", info.name, format!("{:?}", info.environment).to_lowercase()));
    }
    if args.query_log.is_some() || args.no_query_log {
        let info = ws.set_query_log(&args.name, args.query_log.as_deref())?;
        out.done(&match &info.query_log {
            Some(path) => format!("Scans of {} read the statements from {path}", info.name),
            None => format!("Scans of {} no longer read a query log", info.name),
        });
    }
    let Some(agent) = args.agent else { return Ok(0) };
    let info = ws.set_agent_access(&args.name, agent.into())?;
    let message = match info.agent_access {
        AgentAccess::Off => format!("Coding agents can no longer see {}", info.name),
        AgentAccess::Stats => {
            format!("Coding agents can read findings, statistics, schema and estimated plans for {}", info.name)
        }
        AgentAccess::Query => format!(
            "Coding agents can also run read-only statements on {}, each one after you approve it in the Culprit app",
            info.name
        ),
    };
    out.done(&message);
    Ok(0)
}

pub fn audit(ws: &Workspace, args: AuditArgs, out: &Out) -> Result<u8> {
    let entries = ws.audit_log(args.limit)?;
    if out.format() == Format::Json {
        out.value(&json!(entries));
        return Ok(0);
    }
    if entries.is_empty() {
        println!("No agent activity yet. Agents reach Culprit through `culprit mcp`.");
        return Ok(0);
    }
    let mut stdout = std::io::stdout().lock();
    for e in entries {
        let outcome = match e.outcome {
            AuditOutcome::Ok => "ok",
            AuditOutcome::Error => "error",
            AuditOutcome::Denied => "denied",
        };
        let sql = e.sql.as_deref().map(|s| s.split_whitespace().collect::<Vec<_>>().join(" ")).unwrap_or_default();
        let detail = if sql.is_empty() { e.message.unwrap_or_default() } else { sql };
        let _ = writeln!(
            stdout,
            "{}  {:<14} {:<16} {:<18} {:<6} {:>7}  {}",
            e.ts.get(..19).unwrap_or(&e.ts).replace('T', " "),
            e.client.chars().take(14).collect::<String>(),
            e.tool,
            e.connection.unwrap_or_default(),
            outcome,
            culprit_core::format::duration(e.duration_ms),
            detail.chars().take(100).collect::<String>()
        );
    }
    Ok(0)
}

pub fn agent(ws: &Workspace, command: AgentCommand, out: &Out) -> Result<u8> {
    match command {
        AgentCommand::Init(args) => {
            for name in &args.connections {
                ws.connection(name)?;
            }
            let dir = args.dir.canonicalize().map_err(|e| Error::not_found(format!("{}: {e}", args.dir.display())))?;
            let report =
                culprit_mcp::init(&culprit_mcp::InitOptions { dir, connections: args.connections, mcp: !args.no_mcp })?;
            if out.format() == Format::Json {
                out.value(&json!(report));
                return Ok(0);
            }
            for f in &report.files {
                println!("{:<9} {}", f.action, f.path);
            }
            if !args.no_mcp {
                println!(
                    "\nClaude Code picks up .mcp.json on its next start. For other agents, run `culprit mcp` as a stdio MCP server."
                );
            }
            Ok(0)
        }
    }
}
