//! `culprit plan`: read plans saved by other tools and compare any two plans.

use std::io::Read;
use std::path::Path;

use std::path::PathBuf;

use clap::Subcommand;
use culprit_core::{Error, Plan, Result};
use culprit_engine::export::{ExportFormat, ExportOptions};
use culprit_engine::{Workspace, export_diff, export_plan, plans};

use crate::output::Out;

#[derive(Subcommand)]
pub enum PlanCommand {
    /// Read a plan and print it as a tree: a .sqlplan, EXPLAIN (FORMAT JSON) or text output, MySQL JSON or tree output.
    Show {
        /// A file, `-` for stdin, or `<connection>:<query_id>[:<plan_id>]` for a plan of a query in the latest report.
        source: String,
    },
    /// Compare two plans: which operators changed, where the cost moved, which estimates shifted. B is compared to A.
    Diff {
        /// The baseline: a file, `-`, or `<connection>:<query_id>[:<plan_id>]`.
        a: String,
        /// The plan to compare: a file, `-`, or `<connection>:<query_id>[:<plan_id>]`.
        b: String,
    },
    /// Share a plan, or the comparison of two, as Markdown (default), a self-contained HTML page (--html) or the plan's
    /// own text (--native: .sqlplan, EXPLAIN JSON or text). Literal values are redacted unless --no-redact.
    Export {
        /// A file, `-`, or `<connection>:<query_id>[:<plan_id>]`.
        a: String,
        /// A second plan to compare A with.
        b: Option<String>,
        #[arg(long, conflicts_with = "native")]
        html: bool,
        #[arg(long)]
        native: bool,
        #[arg(long)]
        no_redact: bool,
        /// Write to this file instead of standard output.
        #[arg(long, short = 'o')]
        out: Option<PathBuf>,
    },
}

pub async fn run(ws: &Workspace, command: PlanCommand, out: &Out) -> Result<u8> {
    match command {
        PlanCommand::Show { source } => {
            let (plan, _) = load(ws, &source).await?;
            out.plan(&plan);
        }
        PlanCommand::Diff { a, b } => {
            if a == "-" && b == "-" {
                return Err(Error::invalid("Only one side can come from stdin"));
            }
            let (plan_a, name_a) = load(ws, &a).await?;
            let (plan_b, name_b) = load(ws, &b).await?;
            let (name_a, name_b) = if name_a == name_b { ("plan A".into(), "plan B".into()) } else { (name_a, name_b) };
            let diff = ws.diff_plans(&plan_a, &name_a, &plan_b, &name_b, connection_of(&a).or(connection_of(&b)));
            out.plan_diff(&diff, &name_a, &name_b);
        }
        PlanCommand::Export { a, b, html, native, no_redact, out: path } => {
            let options = ExportOptions { redact: !no_redact, notes: Vec::new() };
            let format = if html {
                ExportFormat::Html
            } else if native {
                ExportFormat::Native
            } else {
                ExportFormat::Markdown
            };
            let (plan_a, name_a) = load(ws, &a).await?;
            let file = match b {
                Some(b) => {
                    if format == ExportFormat::Native {
                        return Err(Error::invalid("--native saves one plan; compare two as Markdown or --html"));
                    }
                    let (plan_b, name_b) = load(ws, &b).await?;
                    let (name_a, name_b) =
                        if name_a == name_b { ("plan A".to_string(), "plan B".to_string()) } else { (name_a, name_b) };
                    let diff =
                        ws.diff_plans(&plan_a, &name_a, &plan_b, &name_b, connection_of(&a).or(connection_of(&b)));
                    export_diff(&plan_a, &plan_b, &diff, &name_a, &name_b, format, &options)?
                }
                None => export_plan(&plan_a, &name_a, format, &options)?,
            };
            return crate::share_cmd::write_file(&file, path.as_ref());
        }
    }
    Ok(0)
}

/// The connection named by a `<connection>:<query_id>` source, for statistics that explain plan differences.
fn connection_of(source: &str) -> Option<&str> {
    if source == "-" || std::path::Path::new(source).is_file() {
        return None;
    }
    source.split(':').next().filter(|c| !c.is_empty())
}

/// A plan and a short name for it in sentences ("Plan 815", "before.sqlplan").
async fn load(ws: &Workspace, source: &str) -> Result<(Plan, String)> {
    if source == "-" {
        let mut bytes = Vec::new();
        std::io::stdin().read_to_end(&mut bytes).map_err(|e| Error::invalid(e.to_string()))?;
        return Ok((plans::parse_plan(&plans::decode(&bytes)?)?, "stdin".into()));
    }
    let path = Path::new(source);
    if path.is_file() {
        let bytes = std::fs::read(path).map_err(|e| Error::invalid(format!("{source}: {e}")))?;
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| source.into());
        return Ok((plans::parse_plan(&plans::decode(&bytes)?)?, name));
    }
    let parts: Vec<&str> = source.split(':').collect();
    let (connection, query_id, plan_id) = match parts.as_slice() {
        [c, q] if !c.is_empty() && !q.is_empty() => (*c, *q, None),
        [c, q, p] if !c.is_empty() && !q.is_empty() && !p.is_empty() => (*c, *q, Some(*p)),
        _ => {
            return Err(Error::not_found(format!("No file '{source}'"))
                .with_hint("Pass a plan file, `-` for stdin, or <connection>:<query_id>[:<plan_id>]"));
        }
    };
    let plan = ws.query_plan(connection, query_id, plan_id).await?;
    let name = match plan_id {
        Some(id) => format!("Plan {id}"),
        None => format!("query {query_id}"),
    };
    Ok((plan, name))
}
