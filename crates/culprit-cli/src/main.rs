mod agents;
mod dashboard_cmd;
mod locks;
mod output;
mod plan_cmd;
mod share_cmd;
mod timeline_cmd;
mod waits_cmd;

use std::io::{IsTerminal, Read};
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand, ValueEnum};
use culprit_core::{
    AuthKind, Engine, Environment, Error, ExplainMode, Report, SessionAction, Severity, SnapshotOptions,
};
use culprit_engine::entra::SignInEvent;
use culprit_engine::{ImportSource, NewConnection, Workspace, reports};
use output::{Format, Out};

#[derive(Parser)]
#[command(
    name = "culprit",
    version,
    about = "Find what is slowing your database down.",
    long_about = "Find what is slowing your database down.\n\nCulprit reads statistics views only (safe on production), ranks what it finds by share of database time, and gives each finding a stable id, a fix and a way to verify it. Output is JSON when stdout is not a terminal.",
    after_help = "Examples:\n  culprit conn add shop-staging --url 'Server=tcp:shop-staging.database.windows.net;Database=shop;Authentication=Active Directory Default' --env staging\n  culprit scan shop-staging\n  culprit show shop-staging 'missing-index:dbo.Orders(CustomerId)'\n  culprit top shop-staging --by cpu -n 10 --format json\n  culprit explain shop-staging --sql 'SELECT * FROM dbo.Orders WHERE CustomerId = 1'\n  claude mcp add culprit -- culprit mcp     # serve Culprit to coding agents"
)]
struct Cli {
    /// Output format. Defaults to text in a terminal, JSON otherwise.
    #[arg(long, global = true, value_enum)]
    format: Option<Format>,
    /// Shorthand for --format json.
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Args)]
struct ChangesArgs {
    connection: String,
    /// Compare everything since this moment (RFC 3339, e.g. a deploy) with the same length of time before it.
    #[arg(long)]
    since: Option<String>,
    /// The recent period as the last N hours: `6h`, `90m` or `2d` (default: the last hour, or since the previous scan).
    #[arg(long, value_parser = parse_hours)]
    recent: Option<f64>,
    /// Runs a query needs in each period before its averages are compared (default 10).
    #[arg(long)]
    min_runs: Option<f64>,
    /// How much slower or faster counts as a change (default 1.5).
    #[arg(long)]
    threshold: Option<f64>,
    /// Skip cloud metrics (CloudWatch, Azure Monitor) when looking for what changed around the periods.
    #[arg(long)]
    no_cloud: bool,
}

impl ChangesArgs {
    fn request(&self) -> culprit_engine::changes::ChangeRequest {
        culprit_engine::changes::ChangeRequest {
            since: self.since.clone(),
            recent_hours: self.recent,
            min_executions: self.min_runs,
            threshold: self.threshold,
            cloud: self.no_cloud.then_some(false),
            ..Default::default()
        }
    }
}

/// `6h`, `90m`, `2d`, or a bare number of hours.
pub(crate) fn parse_hours(text: &str) -> Result<f64, String> {
    let text = text.trim();
    let wrong = || format!("'{text}' isn't a duration like 6h, 90m or 2d");
    let (number, unit) = match text.chars().last() {
        Some(c) if c.is_ascii_alphabetic() => (&text[..text.len() - 1], c.to_ascii_lowercase()),
        _ => (text, 'h'),
    };
    let value: f64 = number.trim().parse().map_err(|_| wrong())?;
    let hours = match unit {
        'm' => value / 60.0,
        'h' => value,
        'd' => value * 24.0,
        _ => return Err(wrong()),
    };
    if hours > 0.0 && hours.is_finite() { Ok(hours) } else { Err("the recent period must be longer than zero".into()) }
}

#[derive(Subcommand)]
enum Command {
    /// Manage saved connections. Secrets go to the OS keychain; commands refer to connections by name.
    #[command(subcommand)]
    Conn(ConnCommand),
    /// Microsoft Entra ID sign-ins made with a device code (used when the Azure CLI is missing or signed out).
    #[command(subcommand)]
    Auth(AuthCommand),
    /// One database at a glance: database time and its trend, findings, top queries, waits, health, sessions right
    /// now, deadlocks, indexes, plans and query statistics, each as one plain sentence with the numbers behind it and
    /// the command to run next. Reads the latest scan, and sessions once unless --no-live. Takes a connection or a
    /// saved report file.
    Dashboard(dashboard_cmd::DashboardArgs),
    /// Show the server, what this login can see, and which checks are locked and why.
    Doctor { connection: String },
    /// Take a snapshot, run every check, save the report and print the findings.
    Scan(ScanArgs),
    /// Findings from the latest saved report (no database access).
    Findings(FindingsArgs),
    /// One finding in full, with the agent brief.
    Show {
        connection: String,
        finding: String,
        #[arg(long)]
        report: Option<PathBuf>,
    },
    /// The heaviest queries from the latest saved report.
    Top(TopArgs),
    /// Show a statement's plan. Estimated plans never execute the statement.
    Explain(ExplainArgs),
    /// Read plans saved by other tools (.sqlplan, EXPLAIN JSON or text) and compare any two plans.
    #[command(subcommand)]
    Plan(plan_cmd::PlanCommand),
    /// Did the fix work? Re-scan and report each finding as fixed, still present or can't tell, plus new findings.
    /// Exits 2 when a finding you named is still present.
    Verify(share_cmd::VerifyArgs),
    /// A scan in a few facts, as JSON for a scheduled job to format and post: what the database looks like, what changed
    /// since the previous digest (new, fixed and worse findings, and lock escalations that happened again) and the findings
    /// worth a look. Compares the latest two
    /// scans of a connection, or the report files given (no database access).
    Digest(share_cmd::DigestArgs),
    /// A finding (--finding <id>) or the whole latest scan as Markdown for a pull request, or a self-contained HTML page
    /// (--html). Literal values are redacted unless --no-redact; connection names and hosts are never included.
    Export(share_cmd::ExportArgs),
    /// Current sessions, blocking and waits. On non-production connections, --cancel or --kill acts on one session.
    Live(LiveArgs),
    /// Who blocks whom right now: chains under each head blocker, how long, on what, and what to do.
    Blocking { connection: String },
    /// Recent deadlocks, explained: the pattern, both statements and the fix. --file explains a saved one.
    Deadlocks(locks::DeadlocksArgs),
    /// Where database time goes: waits by category with the queries behind each, and pressure signals. --now samples
    /// what the server waits on right now.
    Waits(waits_cmd::WaitsArgs),
    /// Run a statement and print up to --limit rows. Reads run in a read-only transaction; anything that changes data
    /// commits, and on production needs --yes-execute-on-production.
    Run(RunArgs),
    /// Named queries you can rerun: list, show, add, rm, run.
    #[command(subcommand)]
    Saved(SavedCommand),
    /// List every check with the engines and capabilities it needs.
    Checks,
    /// Databases, tables, views and columns as the editor's completion sees them (read live, read-only).
    Catalog { connection: String },
    /// What differs between two databases (latest scans): tables, columns, indexes, settings and version, with SQL to align
    /// missing indexes. Each side is a connection name or a saved report file.
    Drift { a: String, b: String },
    /// What changed between two periods: queries that got slower, new ones, ones that got faster or stopped, and the
    /// plan changes behind them. Query Store answers from its intervals; PostgreSQL and MySQL from Culprit's earlier scans.
    Changes(ChangesArgs),
    /// Put a moment on a connection's timeline: `culprit mark shop deploy v1.4.2` (from CI, after a migration). Charts
    /// and `changes` line regressions up with it. --list shows them, --remove <id> takes one off.
    Mark(timeline_cmd::MarkArgs),
    /// What changed around a connection: marks, tables, columns, indexes, procedures, settings, upgrades, restarts and,
    /// for Amazon RDS and Azure databases, throttling. Exact where the database dates it, otherwise between two scans.
    Events(timeline_cmd::PeriodArgs),
    /// What the cloud measured for the server (CloudWatch through `aws`, Azure Monitor through `az`; read-only, with
    /// your own sign-in): CPU, credits, IOPS against limits, latency, and when it held the server back.
    Cloud(timeline_cmd::PeriodArgs),
    /// The index advisor: indexes to add (ordered by the database time of the queries that need them), indexes to
    /// clean up, and tables whose shape costs reads or writes. Takes a connection or a saved report file.
    Indexes { connection: String },
    /// Re-run every check on a saved snapshot or report file, offline. Prints the full report.
    Analyze {
        file: PathBuf,
        /// Name to record as the connection (defaults to the one in the file).
        #[arg(long)]
        connection: Option<String>,
    },
    /// Serve Culprit to coding agents over MCP (stdio). Findings, statistics, schema and estimated plans; statements
    /// only on connections with `--agent query` and after approval in the desktop app. Every call is audited.
    Mcp(agents::McpArgs),
    /// What coding agents did through `culprit mcp`, most recent last.
    Audit(agents::AuditArgs),
    /// Set up a repository for coding agents.
    #[command(subcommand)]
    Agent(agents::AgentCommand),
}

#[derive(Subcommand)]
enum ConnCommand {
    /// Save a connection. Paste a URL, ADO, JDBC or libpq string, or a host.
    Add(AddArgs),
    /// List saved connections and their last scan.
    List,
    /// Connect and identify the server.
    Test { name: String },
    /// Forget a connection and delete its keychain secret.
    Rm { name: String },
    /// Rename a connection (its alias). Keychain secret and report history move with it.
    Rename { old: String, new: String },
    /// Find databases your cloud CLI can see (Azure SQL and PostgreSQL/MySQL flexible servers via `az login`, Amazon
    /// RDS/Aurora via `aws login`) and optionally save them.
    Discover(DiscoverArgs),
    /// Store the password for a password-authenticated connection in the OS keychain. Reads it from stdin (hidden when typed).
    SetPassword { name: String },
    /// Change a connection's settings, e.g. what coding agents may do with it (`--agent off|stats|query`).
    Set(agents::SetArgs),
    /// Bring in connections saved in Azure Data Studio, VS Code (mssql), DBeaver or DataGrip. Reads names, hosts, databases
    /// and user names only, never passwords; password connections ask once with `culprit conn set-password`.
    Import(ImportArgs),
}

#[derive(Clone, Copy, ValueEnum)]
enum ImportFrom {
    Auto,
    Ads,
    Vscode,
    Dbeaver,
    Datagrip,
}

#[derive(Args)]
struct ImportArgs {
    /// Which tool to read. `auto` reads every tool found on this machine.
    #[arg(long, value_enum, default_value = "auto")]
    from: ImportFrom,
    /// Read this file instead of the tool's usual location (repeatable): settings.json, data-sources.json, dataSources.xml.
    #[arg(long = "file", value_name = "PATH")]
    files: Vec<PathBuf>,
    /// Keep connections whose alias, host or database contains this word (repeatable; all must match).
    #[arg(long = "match", value_name = "WORD")]
    terms: Vec<String>,
    /// List what would be imported without saving anything.
    #[arg(long)]
    dry_run: bool,
}

#[derive(Subcommand)]
enum AuthCommand {
    /// Forget device-code sign-ins: stored refresh tokens and cached access tokens. `az logout` signs the Azure CLI out.
    Logout,
}

#[derive(Clone, Copy, ValueEnum)]
enum CloudArg {
    Azure,
    Aws,
}

#[derive(Args)]
struct DiscoverArgs {
    /// Which cloud to search.
    #[arg(long, value_enum, default_value = "azure")]
    cloud: CloudArg,
    /// AWS region to search (repeatable). Defaults to every enabled region.
    #[arg(long = "region", value_name = "REGION")]
    regions: Vec<String>,
    /// Keep databases whose database or server name contains this word. Repeat to narrow down (all must match). Omit to list everything.
    #[arg(long = "match", value_name = "WORD")]
    terms: Vec<String>,
    /// Save every match that isn't saved yet, under its suggested alias.
    #[arg(long)]
    save: bool,
    /// Also save production databases (they are listed either way).
    #[arg(long)]
    include_production: bool,
}

#[derive(Args)]
struct AddArgs {
    name: String,
    /// Connection string or host (postgres://…, mssql://…, Server=…;Database=…, jdbc:…, host=… dbname=…).
    #[arg(long)]
    url: String,
    #[arg(long, value_enum, default_value = "development")]
    env: EnvArg,
    #[arg(long, value_enum)]
    engine: Option<EngineArg>,
    #[arg(long, value_enum)]
    auth: Option<AuthArg>,
    #[arg(long)]
    database: Option<String>,
    #[arg(long)]
    user: Option<String>,
    /// Read the password from stdin instead of the connection string.
    #[arg(long, conflicts_with = "password_env")]
    password_stdin: bool,
    /// Read the password from this environment variable every time instead of storing it (CI, agents).
    #[arg(long)]
    password_env: Option<String>,
    /// Save without testing the connection first.
    #[arg(long)]
    no_test: bool,
}

#[derive(Args)]
struct ScanArgs {
    connection: String,
    /// Days of Query Store history to read.
    #[arg(long, default_value_t = 7)]
    window_days: u32,
    /// How many of the heaviest queries to keep.
    #[arg(long, default_value_t = 60)]
    top: u32,
    /// How many plans to fetch and analyze.
    #[arg(long, default_value_t = 25)]
    plans: u32,
    /// Exit with code 3 if any finding is at least this severe (for CI).
    #[arg(long, value_enum)]
    fail_on: Option<SeverityArg>,
    /// Print the whole report, including the snapshot.
    #[arg(long)]
    full: bool,
}

#[derive(Args)]
struct FindingsArgs {
    connection: String,
    /// Read a saved report file instead of the latest one.
    #[arg(long)]
    report: Option<PathBuf>,
    #[arg(long, value_enum)]
    min_severity: Option<SeverityArg>,
}

#[derive(Args)]
struct TopArgs {
    connection: String,
    #[arg(long, value_enum, default_value = "time")]
    by: TopBy,
    #[arg(short = 'n', long, default_value_t = 15)]
    limit: usize,
    #[arg(long)]
    report: Option<PathBuf>,
}

#[derive(Args)]
struct ExplainArgs {
    connection: String,
    #[arg(long, conflicts_with_all = ["file", "query_id"])]
    sql: Option<String>,
    #[arg(long, conflicts_with = "query_id")]
    file: Option<PathBuf>,
    /// Explain a query from the latest report by its id (its stored plan when the engine keeps one). PostgreSQL ids can
    /// be negative, e.g. --query-id -1973449073115120133.
    #[arg(long, allow_hyphen_values = true)]
    query_id: Option<String>,
    /// With --query-id: a specific stored plan, e.g. one of the plans Query Store kept for the query.
    #[arg(long, requires = "query_id", allow_hyphen_values = true)]
    plan_id: Option<String>,
    /// Execute the statement and report actual rows and timings.
    #[arg(long, conflicts_with = "last_actual")]
    analyze: bool,
    /// With --query-id: the actual plan of the query's last run as SQL Server kept it (2019+, LAST_QUERY_PLAN_STATS).
    /// Never runs the query.
    #[arg(long, requires = "query_id")]
    last_actual: bool,
    #[arg(long)]
    yes_execute_on_production: bool,
}

#[derive(Subcommand)]
enum SavedCommand {
    /// Saved queries, most recently run first.
    List,
    /// Print one saved query's SQL.
    Show { name: String },
    /// Save (or overwrite) a query under a name.
    Add {
        name: String,
        #[arg(long, conflicts_with = "file")]
        sql: Option<String>,
        #[arg(long)]
        file: Option<PathBuf>,
        /// The connection it runs on by default.
        #[arg(long)]
        connection: Option<String>,
        #[arg(long)]
        description: Option<String>,
    },
    /// Forget a saved query.
    Rm { name: String },
    /// Run a saved query (on --connection, or the one it was saved for).
    Run {
        name: String,
        #[arg(long)]
        connection: Option<String>,
        #[arg(long, default_value_t = 100)]
        limit: usize,
        #[arg(long)]
        yes_execute_on_production: bool,
    },
}

#[derive(Args)]
struct LiveArgs {
    connection: String,
    /// Stop the statement this session is running; the session stays connected (PostgreSQL, MySQL).
    #[arg(long, value_name = "SESSION", conflicts_with = "kill")]
    cancel: Option<String>,
    /// End this session; its open transaction rolls back.
    #[arg(long, value_name = "SESSION")]
    kill: Option<String>,
    /// How far along this session's running statement is: per-operator progress on SQL Server, the plan with elapsed
    /// time and command progress on PostgreSQL and MySQL.
    #[arg(long, value_name = "SESSION", conflicts_with_all = ["cancel", "kill"])]
    progress: Option<String>,
    /// With --progress: keep refreshing every second until the statement finishes.
    #[arg(long, requires = "progress")]
    watch: bool,
}

#[derive(Args)]
struct RunArgs {
    connection: String,
    #[arg(long)]
    sql: String,
    #[arg(long, default_value_t = 100)]
    limit: usize,
    #[arg(long)]
    yes_execute_on_production: bool,
}

#[derive(Clone, Copy, ValueEnum)]
enum EnvArg {
    Production,
    Staging,
    Development,
    Local,
}

#[derive(Clone, Copy, ValueEnum)]
enum EngineArg {
    Postgres,
    Mssql,
    Mysql,
}

#[derive(Clone, Copy, ValueEnum)]
enum AuthArg {
    Password,
    Entra,
    AwsIam,
    None,
}

#[derive(Clone, Copy, ValueEnum)]
enum SeverityArg {
    Low,
    Medium,
    High,
}

#[derive(Clone, Copy, ValueEnum)]
pub enum TopBy {
    Time,
    Cpu,
    Reads,
    Executions,
}

impl From<SeverityArg> for Severity {
    fn from(s: SeverityArg) -> Severity {
        match s {
            SeverityArg::Low => Severity::Low,
            SeverityArg::Medium => Severity::Medium,
            SeverityArg::High => Severity::High,
        }
    }
}

const EXIT_FINDINGS: u8 = 3;

fn writes_markdown(command: &Command) -> bool {
    matches!(
        command,
        Command::Dashboard(_)
            | Command::Scan(_)
            | Command::Findings(_)
            | Command::Show { .. }
            | Command::Analyze { .. }
            | Command::Verify(_)
            | Command::Export(_)
            | Command::Plan(plan_cmd::PlanCommand::Export { .. })
    )
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let format = match (cli.json, cli.format) {
        (true, _) => Format::Json,
        (false, Some(f)) => f,
        (false, None) if std::io::stdout().is_terminal() => Format::Text,
        _ => Format::Json,
    };
    let out = Out::new(format);
    // Markdown is written by the commands whose output is meant for a pull request or a ticket; elsewhere it would be
    // plain text under a Markdown label.
    if format == Format::Md && !writes_markdown(&cli.command) {
        out.error(
            &Error::invalid("This command has no Markdown output")
                .with_hint("Markdown is available for scan, findings, show, dashboard, analyze, verify, export and plan export; use --format text or json here."),
        );
        return ExitCode::FAILURE;
    }
    let runtime = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(rt) => rt,
        Err(e) => {
            out.error(&Error::internal(e.to_string()));
            return ExitCode::FAILURE;
        }
    };
    match runtime.block_on(run(cli.command, &out)) {
        Ok(code) => ExitCode::from(code),
        Err(e) => {
            out.error(&e);
            ExitCode::FAILURE
        }
    }
}

async fn run(command: Command, out: &Out) -> culprit_core::Result<u8> {
    let mut ws = Workspace::open()?;
    // A device-code sign-in needs a person to read the code; scripts and agents get an error with a hint instead.
    if std::io::stderr().is_terminal() {
        ws = ws.with_sign_in_handler(std::sync::Arc::new(|event| match event {
            SignInEvent::Code { prompt } => eprintln!(
                "To sign in to Microsoft Entra ID, open {} and enter the code {}  (waiting; Ctrl+C stops)",
                prompt.verification_uri, prompt.user_code
            ),
            SignInEvent::Done => eprintln!("Signed in."),
            SignInEvent::Failed { message } => eprintln!("{message}"),
        }));
    }
    match command {
        Command::Conn(c) => conn(&ws, c, out).await,
        Command::Auth(AuthCommand::Logout) => {
            let removed = ws.sign_out()?;
            out.done(&match removed {
                0 => "No device-code sign-ins were stored".to_string(),
                1 => "Forgot 1 device-code sign-in".to_string(),
                n => format!("Forgot {n} device-code sign-ins"),
            });
            Ok(0)
        }
        Command::Dashboard(args) => dashboard_cmd::run(&ws, args, out).await,
        Command::Doctor { connection } => {
            let info = ws.connection(&connection)?;
            let server = ws.probe(&connection).await?;
            let locked = ws.locked_checks(&server);
            out.doctor(&info, &server, &locked);
            Ok(0)
        }
        Command::Scan(args) => {
            let options = SnapshotOptions { window_days: args.window_days, top_queries: args.top, plans: args.plans };
            out.progress(&format!("Scanning {}…", args.connection));
            let report = ws.scan(&args.connection, &options).await?;
            out.scan(&report, args.full, &ws.paths().reports_dir(&args.connection));
            Ok(fail_code(&report, args.fail_on))
        }
        Command::Findings(args) => {
            let report = load(&ws, &args.connection, args.report.as_ref())?;
            let min: Severity = args.min_severity.map(Into::into).unwrap_or(Severity::Low);
            out.findings(&report, min);
            Ok(0)
        }
        Command::Show { connection, finding, report } => {
            let report = load(&ws, &connection, report.as_ref())?;
            let f = report.findings.iter().find(|f| f.id == finding).ok_or_else(|| {
                Error::not_found(format!("No finding '{finding}' in the latest report for {connection}"))
                    .with_hint(format!("List them with `culprit findings {connection}`"))
            })?;
            out.show(&report, f);
            Ok(0)
        }
        Command::Top(args) => {
            let report = load(&ws, &args.connection, args.report.as_ref())?;
            out.top(&report, args.by, args.limit);
            Ok(0)
        }
        Command::Explain(args) if args.last_actual => {
            let query_id = args.query_id.as_deref().unwrap_or_default();
            let plan = ws.last_actual_plan(&args.connection, query_id, args.plan_id.as_deref()).await?;
            out.plan(&plan);
            Ok(0)
        }
        Command::Explain(args) if args.query_id.is_some() && !args.analyze => {
            let query_id = args.query_id.as_deref().unwrap_or_default();
            let plan = ws.query_plan(&args.connection, query_id, args.plan_id.as_deref()).await?;
            out.plan(&plan);
            Ok(0)
        }
        Command::Explain(args) => {
            let sql = explain_sql(&ws, &args)?;
            let mode = if args.analyze { ExplainMode::Analyze } else { ExplainMode::Estimated };
            let plan = ws.explain(&args.connection, &sql, mode, args.yes_execute_on_production).await?;
            out.plan(&plan);
            Ok(0)
        }
        Command::Plan(command) => plan_cmd::run(&ws, command, out).await,
        Command::Waits(args) => waits_cmd::run(&ws, args, out).await,
        Command::Verify(args) => share_cmd::verify(&ws, args, out).await,
        Command::Digest(args) => share_cmd::digest(&ws, args, out),
        Command::Export(args) => share_cmd::export(&ws, args),
        Command::Live(args) => {
            let target = match (args.cancel, args.kill) {
                (Some(id), _) => Some((id, SessionAction::Cancel)),
                (None, Some(id)) => Some((id, SessionAction::Terminate)),
                (None, None) => None,
            };
            if let Some((id, action)) = target {
                ws.interrupt_session(&args.connection, &id, action).await?;
                out.done(&match action {
                    SessionAction::Cancel => format!("Cancelled the statement in session {id}"),
                    SessionAction::Terminate => format!("Ended session {id}"),
                });
                return Ok(0);
            }
            if let Some(session) = args.progress.as_deref() {
                let mut first = true;
                loop {
                    let progress = ws.query_progress(&args.connection, session, first || !args.watch).await?;
                    out.query_progress(&progress, !first);
                    if !args.watch || !progress.running {
                        return Ok(0);
                    }
                    first = false;
                    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                }
            }
            let live = ws.live(&args.connection).await?;
            out.live(&live);
            Ok(0)
        }
        Command::Blocking { connection } => locks::blocking(&ws, &connection, out).await,
        Command::Deadlocks(args) => locks::deadlocks(&ws, args, out).await,
        Command::Run(args) => {
            let rs = ws.run(&args.connection, &args.sql, args.limit, args.yes_execute_on_production).await?;
            out.result_set(&rs);
            Ok(0)
        }
        Command::Saved(command) => {
            match command {
                SavedCommand::List => out.saved_queries(&ws.saved_queries()?),
                SavedCommand::Show { name } => out.saved_query(&ws.saved_query(&name)?),
                SavedCommand::Add { name, sql, file, connection, description } => {
                    let sql = match (sql, file) {
                        (Some(sql), _) => sql,
                        (None, Some(path)) => std::fs::read_to_string(&path)
                            .map_err(|e| culprit_core::Error::invalid(format!("{}: {e}", path.display())))?,
                        (None, None) => return Err(culprit_core::Error::invalid("Pass the SQL with --sql or --file")),
                    };
                    let query =
                        culprit_engine::saved::SaveQuery { name, sql, connection, description, previous_name: None };
                    out.saved_query(&ws.save_query(query)?);
                }
                SavedCommand::Rm { name } => {
                    ws.delete_saved_query(&name)?;
                    out.done(&format!("Forgot '{name}'"));
                }
                SavedCommand::Run { name, connection, limit, yes_execute_on_production } => {
                    let rs = ws.run_saved(&name, connection.as_deref(), limit, yes_execute_on_production).await?;
                    out.result_set(&rs);
                }
            }
            Ok(0)
        }
        Command::Catalog { connection } => {
            out.catalog(&ws.catalog(&connection).await?);
            Ok(0)
        }
        Command::Checks => {
            out.checks(&ws.checks());
            Ok(0)
        }
        Command::Drift { a, b } => {
            let drift = match (report_file(&a), report_file(&b)) {
                (None, None) => ws.drift(&a, &b)?,
                (fa, fb) => {
                    let side = |name: &str, file: Option<PathBuf>| match file {
                        Some(path) => reports::load(&path),
                        None => load(&ws, name, None).map(|mut r| {
                            r.snapshot.connection = name.to_string();
                            r
                        }),
                    };
                    culprit_engine::drift_reports(&side(&a, fa)?, &side(&b, fb)?)?
                }
            };
            out.drift(&drift);
            Ok(0)
        }
        Command::Changes(args) => {
            out.changes(&ws.changes(&args.connection, &args.request()).await?);
            Ok(0)
        }
        Command::Mark(args) => timeline_cmd::mark(&ws, args, out),
        Command::Events(args) => timeline_cmd::events(&ws, args, out).await,
        Command::Cloud(args) => timeline_cmd::cloud(&ws, args, out).await,
        Command::Indexes { connection } => {
            let advice = match report_file(&connection) {
                Some(path) => ws.index_advice_for(reports::load(&path)?),
                None => ws.index_advice(&connection)?,
            };
            out.index_advice(&advice);
            Ok(0)
        }
        Command::Analyze { file, connection } => {
            let mut report = ws.analyze_file(&file)?;
            if let Some(name) = connection {
                report.snapshot.connection = name;
            }
            out.analyzed(&report);
            Ok(0)
        }
        Command::Mcp(args) => agents::mcp(ws, args).await,
        Command::Audit(args) => agents::audit(&ws, args, out),
        Command::Agent(command) => agents::agent(&ws, command, out),
    }
}

fn fail_code(report: &Report, fail_on: Option<SeverityArg>) -> u8 {
    let Some(threshold) = fail_on.map(Severity::from) else { return 0 };
    if report.findings.iter().any(|f| f.severity >= threshold) { EXIT_FINDINGS } else { 0 }
}

fn load(ws: &Workspace, connection: &str, path: Option<&PathBuf>) -> culprit_core::Result<Report> {
    if let Some(path) = path {
        return reports::load(path);
    }
    ws.latest_report(connection, None)?.ok_or_else(|| {
        Error::not_found(format!("No saved report for '{connection}'"))
            .with_hint(format!("Run `culprit scan {connection}` first"))
    })
}

/// `arg` names a saved report file rather than a connection.
fn report_file(arg: &str) -> Option<PathBuf> {
    let path = PathBuf::from(arg);
    (arg.ends_with(".json") && path.is_file()).then_some(path)
}

fn explain_sql(ws: &Workspace, args: &ExplainArgs) -> culprit_core::Result<String> {
    if let Some(sql) = &args.sql {
        return Ok(sql.clone());
    }
    if let Some(file) = &args.file {
        return std::fs::read_to_string(file).map_err(|e| Error::invalid(format!("{}: {e}", file.display())));
    }
    if let Some(id) = &args.query_id {
        let report = load(ws, &args.connection, None)?;
        let q = report
            .snapshot
            .workload
            .as_ref()
            .and_then(|w| w.query(id))
            .ok_or_else(|| Error::not_found(format!("No query '{id}' in the latest report")))?;
        return Ok(q.text.clone());
    }
    Err(Error::invalid("Pass --sql, --file or --query-id"))
}

async fn conn(ws: &Workspace, command: ConnCommand, out: &Out) -> culprit_core::Result<u8> {
    match command {
        ConnCommand::List => {
            out.connections(&ws.connections()?);
            Ok(0)
        }
        ConnCommand::Test { name } => {
            let server = ws.probe(&name).await?;
            out.server(&server);
            Ok(0)
        }
        ConnCommand::Rename { old, new } => {
            let info = ws.rename_connection(&old, &new)?;
            out.connection_renamed(&old, &info);
            Ok(0)
        }
        ConnCommand::Discover(args) => {
            let found = match args.cloud {
                CloudArg::Azure => {
                    if !args.regions.is_empty() {
                        return Err(Error::invalid("--region only applies to --cloud aws"));
                    }
                    out.progress("Searching Azure Resource Graph…");
                    ws.discover_azure(&args.terms).await?
                }
                CloudArg::Aws => {
                    let message = if args.regions.is_empty() {
                        "Searching Amazon RDS in every enabled region…".to_string()
                    } else {
                        format!("Searching Amazon RDS in {}…", args.regions.join(", "))
                    };
                    out.progress(&message);
                    ws.discover_aws(&args.regions, &args.terms).await?
                }
            };
            let mut saved = Vec::new();
            if args.save {
                for d in found.iter().filter(|d| d.saved_as.is_none()) {
                    if d.environment.is_production() && !args.include_production {
                        continue;
                    }
                    saved.push(ws.add_discovered(d)?);
                }
            }
            out.discovered(&found, &saved);
            Ok(0)
        }
        ConnCommand::Import(args) => {
            let source = match args.from {
                ImportFrom::Auto => None,
                ImportFrom::Ads => Some(ImportSource::Ads),
                ImportFrom::Vscode => Some(ImportSource::Vscode),
                ImportFrom::Dbeaver => Some(ImportSource::Dbeaver),
                ImportFrom::Datagrip => Some(ImportSource::Datagrip),
            };
            let mut found = if args.files.is_empty() {
                ws.import_candidates(source)?
            } else {
                let mut all = Vec::new();
                for file in &args.files {
                    all.extend(ws.import_file(file, source)?);
                }
                all
            };
            let terms: Vec<String> = args.terms.iter().map(|t| t.to_ascii_lowercase()).collect();
            found.retain(|c| {
                let text =
                    format!("{} {} {}", c.alias, c.host, c.database.as_deref().unwrap_or("")).to_ascii_lowercase();
                terms.iter().all(|t| text.contains(t.as_str()))
            });
            let new: Vec<_> = found.iter().filter(|c| c.saved_as.is_none()).cloned().collect();
            let saved = if args.dry_run || new.is_empty() { Vec::new() } else { ws.import_connections(&new)? };
            out.imported(&found, &saved, args.dry_run);
            Ok(0)
        }
        ConnCommand::SetPassword { name } => {
            let password = read_password(&format!("Password for {name}: "))?;
            let info = ws.set_password(&name, &password)?;
            out.done(&format!("Stored the password for {} in the OS keychain", info.name));
            Ok(0)
        }
        ConnCommand::Set(args) => agents::set(ws, args, out),
        ConnCommand::Rm { name } => {
            ws.remove_connection(&name)?;
            out.done(&format!("Removed {name}"));
            Ok(0)
        }
        ConnCommand::Add(args) => {
            let password = if args.password_stdin {
                let mut buf = String::new();
                std::io::stdin().read_to_string(&mut buf).map_err(|e| Error::invalid(e.to_string()))?;
                Some(buf.trim_end_matches(['\r', '\n']).to_string())
            } else {
                None
            };
            let new = NewConnection {
                name: args.name.clone(),
                input: args.url,
                environment: match args.env {
                    EnvArg::Production => Environment::Production,
                    EnvArg::Staging => Environment::Staging,
                    EnvArg::Development => Environment::Development,
                    EnvArg::Local => Environment::Local,
                },
                engine: args.engine.map(|e| match e {
                    EngineArg::Postgres => Engine::Postgres,
                    EngineArg::Mssql => Engine::Mssql,
                    EngineArg::Mysql => Engine::Mysql,
                }),
                database: args.database,
                user: args.user,
                password,
                password_env: args.password_env,
                auth: args.auth.map(|a| match a {
                    AuthArg::Password => AuthKind::Password,
                    AuthArg::Entra => AuthKind::Entra,
                    AuthArg::AwsIam => AuthKind::AwsIam,
                    AuthArg::None => AuthKind::None,
                }),
            };
            let server = if args.no_test {
                None
            } else {
                out.progress(&format!("Testing {}…", args.name));
                Some(ws.provider_for(&new).await?.probe().await?)
            };
            let info = ws.add_connection(new)?;
            out.connection_added(&info, server.as_ref());
            Ok(0)
        }
    }
}

/// One line from stdin. When stdin is a terminal the typed characters are hidden and a prompt goes to stderr.
fn read_password(prompt: &str) -> culprit_core::Result<String> {
    use std::io::{BufRead, Write};
    let interactive = std::io::stdin().is_terminal();
    let hide = |on: bool| {
        if interactive {
            echo_input(!on);
        }
    };
    if interactive {
        eprint!("{prompt}");
        let _ = std::io::stderr().flush();
    }
    hide(true);
    let mut line = String::new();
    let read = std::io::stdin().lock().read_line(&mut line);
    hide(false);
    if interactive {
        eprintln!();
    }
    read.map_err(|e| Error::invalid(e.to_string()))?;
    Ok(line.trim_end_matches(['\r', '\n']).to_string())
}

/// Turn the terminal's echo of typed characters on or off.
#[cfg(unix)]
fn echo_input(on: bool) {
    let _ = std::process::Command::new("stty").arg(if on { "echo" } else { "-echo" }).status();
}

/// Turn the console's echo of typed characters on or off.
#[cfg(windows)]
fn echo_input(on: bool) {
    use windows_sys::Win32::System::Console::{
        CONSOLE_MODE, ENABLE_ECHO_INPUT, GetConsoleMode, GetStdHandle, STD_INPUT_HANDLE, SetConsoleMode,
    };
    // SAFETY: GetStdHandle returns this process's console input handle (or an invalid one, which GetConsoleMode then
    // rejects), and the mode is only written back to the same handle.
    unsafe {
        let input = GetStdHandle(STD_INPUT_HANDLE);
        let mut mode: CONSOLE_MODE = 0;
        if GetConsoleMode(input, &mut mode) != 0 {
            SetConsoleMode(input, if on { mode | ENABLE_ECHO_INPUT } else { mode & !ENABLE_ECHO_INPUT });
        }
    }
}

#[cfg(not(any(unix, windows)))]
fn echo_input(_on: bool) {}
