//! `culprit verify` (did the fix work?) and `culprit export` (share a finding or a whole scan as Markdown or HTML).

use std::io::{IsTerminal, Write};
use std::path::PathBuf;

use clap::Args;
use culprit_core::{Error, Result, VerifyStatus};
use culprit_engine::Workspace;
use culprit_engine::export::{self, ExportFile, ExportFormat, ExportOptions};

use crate::output::{Format, Out};

#[derive(Args)]
pub struct VerifyArgs {
    pub connection: String,
    /// Finding ids to check (from `culprit findings`). Omit to check every finding of the latest scan.
    pub ids: Vec<String>,
}

#[derive(Args)]
pub struct DigestArgs {
    /// The saved connection whose latest scan to digest, compared with the scan before it.
    pub connection: Option<String>,
    /// A report or snapshot file to digest instead (`culprit scan <name> --json --full > report.json`).
    #[arg(long)]
    pub report: Option<PathBuf>,
    /// What to compare with, when it isn't in this machine's history: the previous digest (what CI should keep between
    /// runs: no statement text or parameter values), or a previous report or snapshot.
    #[arg(long)]
    pub since: Option<PathBuf>,
    /// How many findings, index suggestions and deadlock patterns each list holds at most.
    #[arg(long, default_value_t = 5)]
    pub top: usize,
    /// How many of the queries that used the most database time to list; 0 leaves them out.
    #[arg(long, default_value_t = 5)]
    pub top_queries: usize,
}

#[derive(Args)]
pub struct ExportArgs {
    pub connection: String,
    /// One finding by id; omit for every finding of the latest scan.
    #[arg(long)]
    pub finding: Option<String>,
    /// A self-contained HTML page instead of Markdown.
    #[arg(long)]
    pub html: bool,
    /// Keep literal values in query text and predicates (they're replaced with ? by default).
    #[arg(long)]
    pub no_redact: bool,
    /// Write to this file instead of standard output.
    #[arg(long, short = 'o')]
    pub out: Option<PathBuf>,
}

/// Always JSON: the digest is data for a consumer (a pipeline, a bot) that decides how to show it.
pub fn digest(ws: &Workspace, args: DigestArgs, out: &Out) -> Result<u8> {
    let d = ws.digest(
        args.connection.as_deref(),
        args.report.as_deref(),
        args.since.as_deref(),
        args.top,
        args.top_queries,
    )?;
    out.value(&serde_json::to_value(&d).map_err(|e| Error::internal(e.to_string()))?);
    Ok(0)
}

/// Re-scan and report each finding as fixed, still present or can't tell, plus new findings. Exits 2 when a finding
/// you asked about is still present, so scripts and CI can gate on it.
pub async fn verify(ws: &Workspace, args: VerifyArgs, out: &Out) -> Result<u8> {
    if out.format() == Format::Text {
        eprintln!("Re-scanning {} to compare with its latest scan…", args.connection);
    }
    let v = ws.verify(&args.connection, &args.ids).await?;
    match out.format() {
        Format::Json => out.value(&serde_json::to_value(&v).map_err(|e| Error::internal(e.to_string()))?),
        Format::Md => print!("{}", export::verification_markdown(&v)),
        Format::Text => {
            let color = std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none();
            let paint =
                |code: &str, text: &str| if color { format!("\x1b[{code}m{text}\x1b[0m") } else { text.to_string() };
            println!("{}", paint("1", &v.summary));
            for f in v.findings.iter().chain(v.new_findings.iter()) {
                let (mark, code) = match f.status {
                    VerifyStatus::Fixed => ("✓ fixed  ", "32"),
                    VerifyStatus::StillPresent => ("• still  ", "33"),
                    VerifyStatus::Unknown => ("? unknown", "2"),
                    VerifyStatus::New => ("+ new    ", "36"),
                };
                let impact = match (&f.before, &f.after) {
                    (Some(b), Some(a)) if b != a => format!("  {b} → {a}"),
                    (Some(b), None) => format!("  was {b}"),
                    (_, Some(a)) => format!("  {a}"),
                    _ => String::new(),
                };
                println!("  {} {}{}", paint(code, mark), f.title, paint("2", &impact));
                if let Some(note) = &f.note {
                    println!("              {}", paint("2", note));
                }
            }
            let (before, after) = culprit_core::changes::range_times(&v.before_taken_at, &v.after_taken_at);
            println!("{}", paint("2", &format!("Scans {before} → {after}")));
        }
    }
    let still = v
        .findings
        .iter()
        .any(|f| f.status == VerifyStatus::StillPresent && (args.ids.is_empty() || args.ids.contains(&f.id)));
    Ok(if still && !args.ids.is_empty() { 2 } else { 0 })
}

/// A finding or the whole latest scan as Markdown (for a PR) or one self-contained HTML page.
pub fn export(ws: &Workspace, args: ExportArgs) -> Result<u8> {
    let options = ExportOptions { redact: !args.no_redact, notes: Vec::new() };
    let format = if args.html { ExportFormat::Html } else { ExportFormat::Markdown };
    let file = match &args.finding {
        Some(id) => ws.export_finding(&args.connection, id, format, &options)?,
        None => ws.export_report(&args.connection, format, &options)?,
    };
    write_file(&file, args.out.as_ref())
}

/// Write an export to `path`, or to standard output.
pub fn write_file(file: &ExportFile, path: Option<&PathBuf>) -> Result<u8> {
    match path {
        Some(path) => {
            std::fs::write(path, &file.contents).map_err(|e| Error::invalid(format!("{}: {e}", path.display())))?;
            eprintln!("Wrote {}", path.display());
        }
        None => {
            let mut stdout = std::io::stdout().lock();
            let _ = stdout.write_all(file.contents.as_bytes());
            if !file.contents.ends_with('\n') {
                let _ = writeln!(stdout);
            }
        }
    }
    Ok(0)
}
