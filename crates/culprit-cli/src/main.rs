mod output;

use std::io::{IsTerminal, Read};
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand, ValueEnum};
use culprit_core::{AuthKind, Engine, Environment, Error, ExplainMode, Report, Severity, SnapshotOptions};
use culprit_engine::{NewConnection, Workspace, reports};
use output::{Format, Out};

#[derive(Parser)]
#[command(
    name = "culprit",
    version,
    about = "Find what is slowing your database down.",
    long_about = "Find what is slowing your database down.\n\nCulprit reads statistics views only (safe on production), ranks what it finds by share of database time, and gives each finding a stable id, a fix and a way to verify it. Output is JSON when stdout is not a terminal.",
    after_help = "Examples:\n  culprit conn add shop-staging --url 'Server=tcp:shop-staging.database.windows.net;Database=shop;Authentication=Active Directory Default' --env staging\n  culprit scan shop-staging\n  culprit show shop-staging 'missing-index:dbo.Orders(CustomerId)'\n  culprit top shop-staging --by cpu -n 10 --format json\n  culprit explain shop-staging --sql 'SELECT * FROM dbo.Orders WHERE CustomerId = 1'"
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

#[derive(Subcommand)]
enum Command {
    /// Manage saved connections. Secrets go to the OS keychain; commands refer to connections by name.
    #[command(subcommand)]
    Conn(ConnCommand),
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
    /// Current sessions, blocking and waits.
    Live { connection: String },
    /// Run a statement and print up to --limit rows.
    Run(RunArgs),
    /// List every check with the engines and capabilities it needs.
    Checks,
    /// Re-run every check on a saved snapshot or report file, offline. Prints the full report.
    Analyze {
        file: PathBuf,
        /// Name to record as the connection (defaults to the one in the file).
        #[arg(long)]
        connection: Option<String>,
    },
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
    /// Find Azure SQL databases your `az login` can see and optionally save them (Entra ID auth, no secrets).
    Discover(DiscoverArgs),
}

#[derive(Args)]
struct DiscoverArgs {
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
    /// Explain a query from the latest report by its id.
    #[arg(long)]
    query_id: Option<String>,
    /// Execute the statement and report actual rows and timings.
    #[arg(long)]
    analyze: bool,
    #[arg(long)]
    yes_execute_on_production: bool,
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
}

#[derive(Clone, Copy, ValueEnum)]
enum AuthArg {
    Password,
    Entra,
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

fn main() -> ExitCode {
    let cli = Cli::parse();
    let format = match (cli.json, cli.format) {
        (true, _) => Format::Json,
        (false, Some(f)) => f,
        (false, None) if std::io::stdout().is_terminal() => Format::Text,
        _ => Format::Json,
    };
    let out = Out::new(format);
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
    let ws = Workspace::open()?;
    match command {
        Command::Conn(c) => conn(&ws, c, out).await,
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
        Command::Explain(args) => {
            let sql = explain_sql(&ws, &args)?;
            let mode = if args.analyze { ExplainMode::Analyze } else { ExplainMode::Estimated };
            let plan = ws.explain(&args.connection, &sql, mode, args.yes_execute_on_production).await?;
            out.plan(&plan);
            Ok(0)
        }
        Command::Live { connection } => {
            let live = ws.live(&connection).await?;
            out.live(&live);
            Ok(0)
        }
        Command::Run(args) => {
            let rs = ws.run(&args.connection, &args.sql, args.limit, args.yes_execute_on_production).await?;
            out.result_set(&rs);
            Ok(0)
        }
        Command::Checks => {
            out.checks(&ws.checks());
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
            out.connection_added(&info);
            Ok(0)
        }
        ConnCommand::Discover(args) => {
            out.progress("Searching Azure Resource Graph…");
            let found = ws.discover_azure(&args.terms).await?;
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
                }),
                database: args.database,
                user: args.user,
                password,
                password_env: args.password_env,
                auth: args.auth.map(|a| match a {
                    AuthArg::Password => AuthKind::Password,
                    AuthArg::Entra => AuthKind::Entra,
                    AuthArg::None => AuthKind::None,
                }),
            };
            if !args.no_test {
                out.progress(&format!("Testing {}…", args.name));
                ws.provider_for(&new).await?.probe().await?;
            }
            let info = ws.add_connection(new)?;
            out.connection_added(&info);
            Ok(0)
        }
    }
}
