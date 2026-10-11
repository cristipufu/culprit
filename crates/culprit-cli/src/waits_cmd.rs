//! `culprit waits`: where database time goes, by wait category with the queries behind each, plus the server's pressure
//! signals; `--now` samples what it waits on right now.

use std::path::PathBuf;

use clap::Args;
use culprit_core::{
    Error, Pressure, Report, Result, Signal, SignalScope, SignalStatus, SignalUnit, Snapshot, WaitSample, WaitsReport,
    format,
};
use culprit_engine::Workspace;
use serde_json::json;

use crate::output::{Format, Out};

#[derive(Args)]
pub struct WaitsArgs {
    /// A saved connection name.
    pub connection: Option<String>,
    /// Sample what the server waits on right now instead of reading the latest scan.
    #[arg(long)]
    pub now: bool,
    /// Length of the --now sample in seconds (1 to 60).
    #[arg(long, default_value_t = 30)]
    pub seconds: u32,
    /// Read the latest scan with this window (SQL Server Query Store days).
    #[arg(long)]
    pub window_days: Option<u32>,
    /// Explain a saved snapshot or report file instead of a connection's latest scan.
    #[arg(long, conflicts_with = "now")]
    pub file: Option<PathBuf>,
}

pub async fn run(ws: &Workspace, args: WaitsArgs, out: &Out) -> Result<u8> {
    if args.now {
        let connection = args.connection.ok_or_else(|| Error::invalid("Name the connection to sample"))?;
        let seconds = args.seconds.clamp(1, 60);
        if out.format() == Format::Text {
            eprintln!("Sampling what {connection} waits on for {seconds} s…");
        }
        let sample = ws.wait_sample(&connection, seconds).await?;
        match out.format() {
            Format::Json => println!(
                "{}",
                serde_json::to_string_pretty(&json!({ "connection": connection, "sample": sample }))
                    .unwrap_or_default()
            ),
            _ => print_sample(&connection, &sample),
        }
        return Ok(0);
    }
    let (name, snapshot) = match (&args.file, &args.connection) {
        (Some(path), _) => {
            let text = std::fs::read_to_string(path).map_err(|e| Error::invalid(format!("{}: {e}", path.display())))?;
            let snapshot = serde_json::from_str::<Report>(&text)
                .map(|r| r.snapshot)
                .or_else(|_| serde_json::from_str::<Snapshot>(&text))
                .map_err(|e| Error::invalid(format!("{} is not a Culprit snapshot or report: {e}", path.display())))?;
            (snapshot.connection.clone(), snapshot)
        }
        (None, Some(connection)) => {
            let report = ws.latest_report(connection, args.window_days)?.ok_or_else(|| {
                Error::not_found(format!("No report for {connection} yet"))
                    .with_hint("Scan it first: culprit scan <connection>")
            })?;
            (connection.clone(), report.snapshot)
        }
        (None, None) => return Err(Error::invalid("Name a connection, or pass --file")),
    };
    let waits = culprit_engine::explain_waits(&snapshot);
    match out.format() {
        Format::Json => println!(
            "{}",
            serde_json::to_string_pretty(&json!({ "connection": name, "waits": waits, "pressure": snapshot.pressure }))
                .unwrap_or_default()
        ),
        _ => print_report(&name, &snapshot, waits.as_ref()),
    }
    Ok(0)
}

fn bar(share: f64) -> String {
    let filled = (share.clamp(0.0, 1.0) * 20.0).round() as usize;
    format!("{}{}", "█".repeat(filled), "·".repeat(20 - filled))
}

fn print_report(name: &str, snapshot: &Snapshot, waits: Option<&WaitsReport>) {
    match waits {
        Some(w) => {
            println!("Where {name}'s database time goes · {}", w.source);
            println!();
            for c in &w.categories {
                println!(
                    "{:>5}  {}  {:<22} {:>9}   {}",
                    format::percent(c.share),
                    bar(c.share),
                    c.label,
                    format::duration(c.wait_ms),
                    c.top_wait.as_deref().unwrap_or("")
                );
                if let Some(detail) = &c.detail {
                    println!("{:>29}{detail}", "");
                }
                for q in c.queries.iter().take(3) {
                    println!(
                        "{:>29}{:>5} of it  query {:<8} {}",
                        "",
                        format::percent(q.share_of_category),
                        q.id,
                        short(&q.text, 70)
                    );
                }
            }
            for note in &w.notes {
                println!();
                println!("Note: {note}");
            }
        }
        None => {
            println!("{name}'s latest scan recorded no waits.");
            println!("Sample what it waits on right now: culprit waits {name} --now");
        }
    }
    if let Some(p) = &snapshot.pressure {
        print_signals("Health", p, &p.signals);
    }
}

fn print_sample(name: &str, s: &WaitSample) {
    println!("What {name} waits on right now · {} s · {}", s.seconds.round(), s.method);
    println!();
    if s.categories.is_empty() {
        println!("Nothing waited during the sample.");
    }
    for c in &s.categories {
        let share = if s.total_ms > 0.0 { c.wait_ms / s.total_ms } else { 0.0 };
        let (label, _) = culprit_core::waits::category_info(culprit_core::waits::category_key(&c.category), s.engine);
        println!(
            "{:>5}  {}  {:<22} {:>9}   {}",
            format::percent(share),
            bar(share),
            label,
            format::duration(c.wait_ms),
            c.wait_type.as_deref().unwrap_or("")
        );
    }
    if let Some(a) = s.active_sessions {
        println!();
        println!("Busy sessions on average: {}", format::count(a));
    }
    if !s.signals.is_empty() {
        print_signals("During the sample", &Pressure::default(), &s.signals);
    }
    for note in &s.notes {
        println!("Note: {note}");
    }
}

fn print_signals(title: &str, p: &Pressure, signals: &[Signal]) {
    if signals.is_empty() {
        return;
    }
    println!();
    match p.uptime_hours {
        Some(h) => println!("{title} (\"since start\" covers {})", format::duration(h * 3_600_000.0)),
        None => println!("{title}"),
    }
    for s in signals {
        let mark = match s.status {
            SignalStatus::Bad => "●",
            SignalStatus::Watch => "◐",
            SignalStatus::Good => "○",
            SignalStatus::Info => " ",
        };
        let scope = match s.scope {
            SignalScope::Now => "now",
            SignalScope::SinceStartup => "since start",
            SignalScope::Sampled => "sampled",
        };
        println!("  {mark} {:<28} {:>10}  {:<11} {}", s.label, value(s), scope, s.healthy.as_deref().unwrap_or(""));
    }
}

pub fn value(s: &Signal) -> String {
    match s.unit {
        SignalUnit::Percent => format!("{:.1}%", s.value),
        SignalUnit::PerSecond => format!("{}/s", format::count(s.value)),
        SignalUnit::PerHour => format!("{}/h", format::count(s.value)),
        SignalUnit::Count => format::count(s.value),
        SignalUnit::Bytes => format::bytes(s.value),
        SignalUnit::Milliseconds => format::duration(s.value),
        SignalUnit::Seconds => format::duration(s.value * 1000.0),
    }
}

fn short(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    format!("{}…", text.chars().take(max - 1).collect::<String>())
}
