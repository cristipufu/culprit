//! `culprit dashboard`: one database at a glance. Each tile is one plain sentence, the numbers behind it, the rows to
//! open and the command to run next, the same as the app's dashboard.

use clap::Args;
use culprit_core::{Dashboard, Result, SnapshotOptions, Tile, TileKey, TileStatus};
use culprit_engine::{Workspace, reports};
use serde_json::json;

use crate::output::{Format, Out, age};

#[derive(Args)]
pub struct DashboardArgs {
    /// A saved connection name, or a saved report file.
    pub connection: String,
    /// Read the latest scan with this window (SQL Server Query Store days).
    #[arg(long)]
    pub window_days: Option<u32>,
    /// Take a fresh scan first instead of reading the latest one.
    #[arg(long)]
    pub scan: bool,
    /// Don't read sessions; the "right now" tile then says it wasn't read.
    #[arg(long)]
    pub no_live: bool,
}

pub async fn run(ws: &Workspace, args: DashboardArgs, out: &Out) -> Result<u8> {
    let mut dashboard = match crate::report_file(&args.connection) {
        Some(path) => culprit_engine::dashboard_report(&reports::load(&path)?),
        None => {
            if args.scan {
                let defaults = SnapshotOptions::default();
                let options =
                    SnapshotOptions { window_days: args.window_days.unwrap_or(defaults.window_days), ..defaults };
                out.progress(&format!("Scanning {}…", args.connection));
                ws.scan(&args.connection, &options).await?;
            }
            ws.dashboard(&args.connection, args.window_days, !args.no_live).await?
        }
    };
    let connection = dashboard.connection.clone();
    for tile in &mut dashboard.tiles {
        tile.next = next(tile, &connection);
    }
    match out.format() {
        Format::Json => out.value(&json!(dashboard)),
        Format::Md => print_markdown(&dashboard),
        Format::Text => print_text(&dashboard, out),
    }
    Ok(0)
}

/// The command that shows more of what a tile is about.
fn next(tile: &Tile, connection: &str) -> Option<String> {
    let c = shell_word(connection);
    Some(match tile.key {
        TileKey::DbTime => format!("culprit changes {c}"),
        TileKey::Findings => format!("culprit findings {c}"),
        TileKey::TopQueries => format!("culprit top {c}"),
        TileKey::Waits | TileKey::Health => format!("culprit waits {c}"),
        TileKey::Now
            if tile.status == TileStatus::Bad || tile.metric("blocked").and_then(|m| m.value).unwrap_or(0.0) > 0.0 =>
        {
            format!("culprit blocking {c}")
        }
        TileKey::Now => format!("culprit live {c}"),
        TileKey::Deadlocks => format!("culprit deadlocks {c}"),
        TileKey::Indexes => format!("culprit indexes {c}"),
        TileKey::Plans => {
            let id = tile.items.first().and_then(|i| i.query_id.as_deref())?;
            format!("culprit explain {c} --query-id {}", shell_word(id))
        }
        TileKey::Statistics => format!("culprit doctor {c}"),
        TileKey::Storage | TileKey::Connections | TileKey::Replication => format!("culprit waits {c}"),
        TileKey::Failures => format!("culprit findings {c}"),
    })
}

fn shell_word(text: &str) -> String {
    if !text.is_empty() && text.chars().all(|ch| ch.is_ascii_alphanumeric() || "-_.:/@".contains(ch)) {
        text.to_string()
    } else {
        format!("'{}'", text.replace('\'', r"'\''"))
    }
}

fn mark(out: &Out, status: TileStatus) -> String {
    match status {
        TileStatus::Bad => out.red("●"),
        TileStatus::Watch => out.yellow("●"),
        TileStatus::Good => out.green("●"),
        TileStatus::Info => out.blue("○"),
        TileStatus::Unavailable => out.dim("○"),
    }
}

fn status_word(status: TileStatus) -> &'static str {
    match status {
        TileStatus::Bad => "needs a look now",
        TileStatus::Watch => "worth a look",
        TileStatus::Good => "fine",
        TileStatus::Info => "for context",
        TileStatus::Unavailable => "not available here",
    }
}

const PAD: usize = 18;

fn print_text(d: &Dashboard, out: &Out) {
    println!("{}", out.bold(&format!("{} · {} · {}", d.connection, d.product, d.database)));
    let scanned = age(&d.taken_at).map(|a| format!("scanned {a}")).unwrap_or_default();
    let under = [d.coverage.clone().unwrap_or_default(), format!("{:?}", d.environment).to_lowercase(), scanned]
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" · ");
    println!("{}", out.dim(&under));
    println!();
    if d.attention.is_empty() {
        println!("{}", d.headline);
    } else {
        println!("{} {}", out.bold("Needs a look:"), d.headline);
    }
    let indent = " ".repeat(PAD + 2);
    for tile in &d.tiles {
        println!();
        println!("{} {}{}", mark(out, tile.status), out.bold(&format!("{:<PAD$}", tile.title)), tile.takeaway);
        if let Some(detail) = &tile.detail {
            println!("{indent}{}", out.dim(detail));
        }
        if !tile.metrics.is_empty() && tile.status != TileStatus::Unavailable {
            let line = tile.metrics.iter().map(|m| format!("{} {}", m.label, m.text)).collect::<Vec<_>>().join(" · ");
            println!("{indent}{}", out.dim(&line));
        }
        for item in tile.items.iter().take(3) {
            let value = item.value.as_deref().map(|v| format!("  {}", out.dim(v))).unwrap_or_default();
            let dot = item.status.map(|s| format!("{} ", mark(out, s))).unwrap_or_else(|| "· ".into());
            // Two reads of the same table look alike; the id tells them apart and opens with `culprit query`.
            let id =
                item.query_id.as_deref().map(|id| format!("  {}", out.dim(&format!("id {id}")))).unwrap_or_default();
            println!("{indent}{dot}{}{value}{id}", item.label);
        }
        if let Some(sql) = &tile.fix_sql {
            // Settings files and multi-line statements keep the tile's indent on every line.
            for line in sql.lines() {
                println!("{indent}{}", out.blue(line));
            }
        }
        if let Some(next) = &tile.next {
            println!("{indent}{}", out.dim(&format!("→ {next}")));
        }
    }
}

fn print_markdown(d: &Dashboard) {
    println!("## {} · {}", d.connection, d.product);
    println!();
    if let Some(coverage) = &d.coverage {
        println!("_{coverage}; scanned {}_", culprit_core::changes::dated_time(&d.taken_at));
        println!();
    }
    if d.attention.is_empty() {
        println!("{}", d.headline);
    } else {
        println!("**Needs a look:** {}", d.headline);
    }
    println!();
    for tile in &d.tiles {
        println!("- **{}** ({}): {}", tile.title, status_word(tile.status), tile.takeaway);
        if let Some(detail) = &tile.detail {
            println!("  {detail}");
        }
        for item in tile.items.iter().take(3) {
            let value = item.value.as_deref().map(|v| format!(" — {v}")).unwrap_or_default();
            println!("  - {}{value}", item.label);
        }
        if let Some(sql) = &tile.fix_sql {
            if sql.contains('\n') {
                println!();
                println!("  ```");
                for line in sql.lines() {
                    println!("  {line}");
                }
                println!("  ```");
            } else {
                println!("  `{sql}`");
            }
        }
    }
}
