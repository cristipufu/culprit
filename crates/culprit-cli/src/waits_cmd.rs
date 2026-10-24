//! `culprit waits`: where database time goes, by wait category with the queries behind each, plus the server's pressure
//! signals; `--now` samples what it waits on right now.

use std::path::PathBuf;

use clap::Args;
use culprit_core::{
    Error, FileIo, Pressure, Replica, ReplicaRole, Result, Signal, SignalScope, SignalStatus, SignalUnit, Snapshot,
    WaitSample, WaitsReport, format,
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
    /// Read the latest scan with this window, in days, hours or minutes: 6h, 90m, 2d. Instead of --window-days.
    #[arg(long, value_parser = crate::parse_window_arg, conflicts_with = "window_days", value_name = "DURATION")]
    pub window: Option<u32>,
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
            let snapshot = ws.analyze_file(path)?.snapshot;
            (snapshot.connection.clone(), snapshot)
        }
        (None, Some(connection)) => {
            let report = ws
                .latest_report(connection, crate::window_minutes(args.window, args.window_days))?
                .ok_or_else(|| {
                    Error::not_found(format!("No report for {connection} yet"))
                        .with_hint("Scan it first: culprit scan <connection>")
                })?;
            (connection.clone(), report.snapshot)
        }
        (None, None) => return Err(Error::invalid("Name a connection, or pass --file")),
    };
    let waits = culprit_engine::explain_waits(&snapshot);
    match out.format() {
        Format::Json => {
            let mut value = json!({ "connection": name, "waits": waits, "pressure": snapshot.pressure, "operations": snapshot.operations.counters_only() });
            if let Some(resources) = &snapshot.operations.node_resources {
                value["node_resources"] = json!(resources);
            }
            if let Some(history) = &snapshot.operations.termination_history {
                value["terminations"] = json!(history);
            }
            if let Some(reason) = snapshot.source_problem(&[
                culprit_core::source_id::DOCDB_ACTIVITY_HISTORY,
                culprit_core::source_id::DOCDB_TERMINATIONS,
                culprit_core::source_id::DOCDB_NODE_RESOURCES,
            ]) {
                value["notes"] = json!([reason]);
            }
            println!("{}", serde_json::to_string_pretty(&value).unwrap_or_default());
        }
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
            println!(
                "{} · {}",
                if w.activity.is_some() {
                    format!("{name}'s retained active-task samples")
                } else {
                    format!("Where {name}'s database time goes")
                },
                w.source
            );
            println!();
            if let Some(activity) = &w.activity {
                println!(
                    "{} retained observations; {} weighted observations. {}",
                    format::count(activity.period.observations),
                    format::count(activity.period.weighted_observations),
                    if activity.period.partial { "Partial read." } else { "" }
                );
                for c in &activity.categories {
                    println!(
                        "{:>5}  {}  {:<22} weight {}",
                        format::percent(c.share),
                        bar(c.share),
                        c.label,
                        format::count(c.weighted_observations)
                    );
                    for e in c.events.iter().take(5) {
                        println!(
                            "{:>29}{} · {} · node {}{} · weight {}",
                            "",
                            e.component,
                            e.wait_event,
                            e.node.as_deref().unwrap_or("unknown"),
                            e.query_id.as_ref().map(|id| format!(" · query {id}")).unwrap_or_default(),
                            format::count(e.weighted_observations)
                        );
                    }
                    for q in c.queries.iter().take(3) {
                        println!(
                            "{:>29}{} of this category's sample weight · query {} {}",
                            "",
                            format::percent(q.share_of_category),
                            q.id,
                            q.text.as_deref().map(|s| short(s, 70)).unwrap_or_default()
                        );
                    }
                }
            }
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
            if let Some(reason) = snapshot.source_problem(&[culprit_core::source_id::DOCDB_ACTIVITY_HISTORY]) {
                println!("Recorded activity unavailable: {reason}");
            }
            println!("Sample what it waits on right now: culprit waits {name} --now");
        }
    }
    if let Some(history) = &snapshot.operations.termination_history {
        println!("\nRetained unexpected terminations · {} · {} records", history.source, history.events.len());
        println!(
            "Requested {} to {}. {}. {}",
            history.requested_start,
            history.requested_end,
            history.visibility,
            if history.partial { "Partial read." } else { "" }
        );
        for event in history.events.iter().take(12) {
            println!(
                "  {} · {} · node {} · query {}",
                event.ended_at,
                event.reason.label(),
                event.node.as_deref().unwrap_or("unknown"),
                event.query_id.as_deref().unwrap_or("unresolved")
            );
        }
        if history.events.len() > 12 {
            println!("  Showing the first 12 retained records; use --json for the full retained set.");
        }
        for note in &history.notes {
            println!("  {note}");
        }
    } else if let Some(reason) = snapshot.source_problem(&[culprit_core::source_id::DOCDB_TERMINATIONS]) {
        println!("Recorded terminations unavailable: {reason}");
    }
    if let Some(p) = &snapshot.pressure {
        print_signals("Health", p, &p.signals);
        print_storage(&p.files);
        print_replicas(&p.replicas);
    }
    if let Some(resources) = &snapshot.operations.node_resources {
        println!(
            "\nShared node resources · {} · observed {} to {}",
            resources.source, resources.observed_from, resources.observed_until
        );
        let limit = |limit: Option<culprit_core::MemoryLimit>| match limit {
            Some(culprit_core::MemoryLimit::Unlimited) => "unlimited".into(),
            Some(culprit_core::MemoryLimit::Bytes(n)) => format::bytes(n),
            None => "unknown".into(),
        };
        for node in resources
            .nodes
            .iter()
            .filter(|n| !n.available)
            .chain(resources.nodes.iter().filter(|n| n.available))
            .take(12)
        {
            if !node.available {
                println!("  {}: resource measurements unavailable", node.node);
                continue;
            }
            println!(
                "  {}: host CPU user {}, system {}; host memory available {} of {}",
                node.node,
                node.cpu_user_fraction.map_or_else(|| "unknown".into(), format::percent),
                node.cpu_system_fraction.map_or_else(|| "unknown".into(), format::percent),
                node.host_memory_available_bytes.map_or_else(|| "unknown".into(), format::bytes),
                node.host_memory_total_bytes.map_or_else(|| "unknown".into(), format::bytes)
            );
            if let Some(memory) = &node.process_memory {
                println!(
                    "    {}: tracked {}; soft limit {}; hard limit {}",
                    memory.name,
                    memory.tracked_bytes.map_or_else(|| "unknown".into(), format::bytes),
                    limit(memory.soft_limit),
                    limit(memory.hard_limit)
                );
            }
        }
        if resources.nodes.len() > 12 {
            println!("  Showing the first 12 retained nodes; use --json for the full retained set.");
        }
        if resources.partial {
            println!("  This resource read was partial.");
        }
        for note in &resources.notes {
            println!("  {note}");
        }
    } else if let Some(reason) = snapshot.source_problem(&[culprit_core::source_id::DOCDB_NODE_RESOURCES]) {
        println!("Node resources unavailable: {reason}");
    }
    let o = &snapshot.operations;
    for space in &o.spaces {
        println!(
            "{}: {} used of {} allocated ({}, when scanned); growth {}. Filesystem headroom not measured.",
            space.name,
            format::bytes(space.used_bytes),
            format::bytes(space.total_bytes),
            scope_label(space.scope),
            space.growth_enabled.map_or("unknown", |g| if g { "configured" } else { "unavailable" })
        );
        if let Some(reason) = &space.reuse_wait {
            println!("  Log reuse reason: {reason}");
        }
        for part in &space.parts {
            println!("  {}: {}", part.name, format::bytes(part.bytes));
        }
    }
    for c in &o.counters {
        println!(
            "{} · {}: {} recorded ({}, since {}). This total does not establish a current incident.",
            culprit_core::wire_name(&c.kind).replace('-', " "),
            c.subject,
            format::count(c.count),
            scope_label(c.scope),
            c.since.as_deref().unwrap_or("unknown reset")
        );
    }
    if let Some(wal) = &o.sqlite {
        println!(
            "SQLite WAL allocation when scanned: {}. Allocation does not measure uncheckpointed work.",
            wal.wal_bytes.map_or_else(|| "unknown".into(), format::bytes)
        );
    }
    for seq in &o.sequences {
        println!(
            "{}: allocation {}, bounds {} to {}, increment {}, cycling {}. Cached/skipped values prevent a remaining-request forecast.",
            seq.name, seq.current, seq.minimum, seq.maximum, seq.increment, seq.cycles
        );
    }
    for age in &o.multixacts {
        println!(
            "{}: {} MultiXact IDs old when scanned (independent of ordinary XID age).",
            age.subject,
            format::count(age.age)
        );
    }
    if let Some(cluster) = &o.cluster {
        println!(
            "Galera node when scanned: ready {}, connected {}, primary component {}; {}.",
            flag_label(cluster.ready),
            flag_label(cluster.connected),
            flag_label(cluster.primary_component),
            cluster.local_state.as_deref().unwrap_or("state unknown")
        );
    }
}

fn scope_label(scope: culprit_core::FileScope) -> &'static str {
    match scope {
        culprit_core::FileScope::Database => "current database",
        culprit_core::FileScope::Server => "whole server",
    }
}
fn flag_label(value: Option<bool>) -> &'static str {
    match value {
        Some(true) => "yes",
        Some(false) => "no",
        None => "unknown",
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
            SignalScope::Now => "when scanned",
            SignalScope::SinceStartup => "since start",
            SignalScope::Sampled => "sampled",
            SignalScope::LastHour => "last hour",
            SignalScope::QueryHistory => "retained query history",
            SignalScope::Window => "selected period",
        };
        println!("  {mark} {:<36} {:>10}  {:<11} {}", s.label, value(s), scope, s.healthy.as_deref().unwrap_or(""));
    }
}

pub fn value(s: &Signal) -> String {
    match s.unit {
        SignalUnit::Percent => format::percent_value(s.value),
        // Under one every ten seconds, a per-minute rate says more than "0.0/s".
        SignalUnit::PerSecond if s.value > 0.0 && s.value < 0.1 => format!("{}/min", format::count(s.value * 60.0)),
        SignalUnit::PerSecond => format!("{}/s", format::count(s.value)),
        SignalUnit::PerHour => format!("{}/h", format::count(s.value)),
        SignalUnit::Count => format::count(s.value),
        SignalUnit::Bytes => format::bytes(s.value),
        SignalUnit::Milliseconds => format::duration(s.value),
        SignalUnit::Seconds => format::duration(s.value * 1000.0),
    }
}

/// Latency per file, slowest first: what reads and writes took on average since startup.
fn print_storage(files: &[FileIo]) {
    if files.is_empty() {
        return;
    }
    println!();
    println!("Storage (average per operation, since start)");
    let avg = |ms: Option<f64>, n: f64| match ms {
        Some(ms) if n > 0.0 => format!("{:>9} × {:<6}", latency(ms / n), format::count(n)),
        _ if n > 0.0 => format!("{:>9} × {:<6}", "untimed", format::count(n)),
        _ => format!("{:>18}", ""),
    };
    for f in files.iter().take(12) {
        let name = match &f.database {
            Some(db) if !f.name.starts_with(db.as_str()) => format!("{db} · {}", f.name),
            _ => f.name.clone(),
        };
        println!("  {:<34} reads {}  writes {}", short(&name, 34), avg(f.read_ms, f.reads), avg(f.write_ms, f.writes));
    }
}

/// Storage latency reads best in microseconds below a tenth of a millisecond (cache hits, fast SSDs).
fn latency(ms: f64) -> String {
    if ms < 0.1 { format!("{:.0} µs", ms * 1000.0) } else { format::duration(ms) }
}

fn print_replicas(replicas: &[Replica]) {
    if replicas.is_empty() {
        return;
    }
    println!();
    println!("Replication");
    for r in replicas {
        let role = match r.role {
            ReplicaRole::Replica => "replica",
            ReplicaRole::ThisServer => "this server",
            ReplicaRole::Slot => "slot",
        };
        let mut parts: Vec<String> = Vec::new();
        parts.extend(r.state.clone());
        parts.extend(r.sync.clone());
        if let Some(s) = r.lag_seconds {
            parts.push(format!("{} behind", format::duration(s * 1000.0)));
        }
        if let Some(b) = r.lag_bytes {
            parts.push(format!(
                "{} {}",
                format::bytes(b),
                if r.role == ReplicaRole::Slot { "kept" } else { "not applied" }
            ));
        }
        parts.extend(r.error.clone());
        println!("  {:<11} {:<28} {}", role, short(&r.name, 28), parts.join(" · "));
    }
}

fn short(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    format!("{}…", text.chars().take(max - 1).collect::<String>())
}
