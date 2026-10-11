//! `culprit plan`: read plans saved by other tools and compare any two plans.

use std::io::Read;
use std::path::Path;

use clap::Subcommand;
use culprit_core::{Error, Plan, Result, plan_diff_named};
use culprit_engine::{Workspace, plans};

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
            let diff = plan_diff_named(&plan_a, &name_a, &plan_b, &name_b);
            out.plan_diff(&diff, &name_a, &name_b);
        }
    }
    Ok(0)
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
