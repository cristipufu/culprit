//! `culprit blocking` and `culprit deadlocks`: who blocks whom right now, and recent deadlocks explained.

use std::io::Read;
use std::path::PathBuf;

use clap::Args;
use culprit_core::{BlockNode, Deadlock, DeadlockReport, Error, LiveState, Result, Session, format};
use culprit_engine::{Workspace, deadlocks};
use serde_json::{Value, json};

use crate::output::{Format, Out};

#[derive(Args)]
pub struct DeadlocksArgs {
    /// Connection to read deadlocks from. Omit it with --file.
    pub connection: Option<String>,
    /// How far back: 90m, 24h, 7d.
    #[arg(long, default_value = "24h")]
    pub since: String,
    /// Explain a saved deadlock instead: a SQL Server .xdl file, InnoDB status text or PostgreSQL log lines (`-` reads stdin).
    #[arg(long, value_name = "PATH")]
    pub file: Option<PathBuf>,
}

/// `90m`, `24h`, `7d`, or a bare number of hours.
pub fn parse_since(text: &str) -> Result<f64> {
    let t = text.trim().to_ascii_lowercase();
    let (number, unit) = t.split_at(t.find(|c: char| c.is_ascii_alphabetic()).unwrap_or(t.len()));
    let n: f64 = number
        .trim()
        .parse()
        .map_err(|_| Error::invalid(format!("Can't read `{text}` as a time span")).with_hint("Use 90m, 24h or 7d."))?;
    let hours = match unit {
        "" | "h" | "hour" | "hours" => n,
        "m" | "min" | "minutes" => n / 60.0,
        "d" | "day" | "days" => n * 24.0,
        _ => return Err(Error::invalid(format!("Unknown time unit `{unit}`")).with_hint("Use 90m, 24h or 7d.")),
    };
    if hours.is_nan() || hours <= 0.0 {
        return Err(Error::invalid("The time span must be positive"));
    }
    Ok(hours)
}

pub async fn blocking(ws: &Workspace, connection: &str, out: &Out) -> Result<u8> {
    let live = ws.live(connection).await?;
    if out.format() == Format::Json {
        out.value(&blocking_json(&live));
        return Ok(0);
    }
    let Some(blocking) = live.blocking.as_ref().filter(|b| !b.chains.is_empty()) else {
        println!(
            "Nothing is blocked: {} active sessions, none waiting on another.",
            live.sessions.iter().filter(|s| s.state != "idle").count()
        );
        return Ok(0);
    };
    for chain in &blocking.chains {
        println!("{}", out.bold(&chain.headline));
        print_node(&live, &chain.tree, out, 0);
        println!("  {}", out.dim(&chain.advice));
        println!();
    }
    Ok(0)
}

fn session<'a>(live: &'a LiveState, id: &str) -> Option<&'a Session> {
    live.sessions.iter().find(|s| s.id == id)
}

fn print_node(live: &LiveState, node: &BlockNode, out: &Out, depth: usize) {
    let s = session(live, &node.session);
    let prefix = if depth == 0 { "  ".to_string() } else { format!("{}└─ ", "   ".repeat(depth)) };
    let state = match (depth, s) {
        (0, Some(s)) => s.state.clone(),
        (_, Some(s)) => match s.wait_ms.or(s.elapsed_ms) {
            Some(ms) => format!("waits {}", format::duration(ms)),
            None => "waiting".into(),
        },
        (_, None) => "not visible".into(),
    };
    let lock = node.waits_for.clone().unwrap_or_default();
    let who = s
        .map(|s| {
            let user = s.user.clone().unwrap_or_default();
            let host = s.host.as_deref().map(|h| format!("@{h}")).unwrap_or_default();
            let app = s.application.as_deref().map(|a| format!(" · {a}")).unwrap_or_default();
            format!("{user}{host}{app}")
        })
        .unwrap_or_default();
    let sql: String = s
        .and_then(|s| s.sql.as_deref())
        .map(|q| q.split_whitespace().collect::<Vec<_>>().join(" "))
        .unwrap_or_default();
    let sql = if sql.chars().count() > 90 { format!("{}…", sql.chars().take(89).collect::<String>()) } else { sql };
    let detail = [state, lock, who].into_iter().filter(|p| !p.is_empty()).collect::<Vec<_>>().join(" · ");
    println!("{prefix}{}  {}  {}", out.bold(&node.session), detail, out.dim(&sql));
    for child in &node.children {
        print_node(live, child, out, depth + 1);
    }
}

fn blocking_json(live: &LiveState) -> Value {
    let Some(b) = live.blocking.as_ref() else {
        return json!({ "taken_at": live.taken_at, "waiting": 0, "chains": [] });
    };
    let mut ids = Vec::new();
    fn walk(node: &BlockNode, ids: &mut Vec<String>) {
        ids.push(node.session.clone());
        for c in &node.children {
            walk(c, ids);
        }
    }
    for c in &b.chains {
        walk(&c.tree, &mut ids);
    }
    let sessions: Vec<&Session> = live.sessions.iter().filter(|s| ids.contains(&s.id)).collect();
    json!({ "taken_at": live.taken_at, "waiting": b.waiting, "chains": b.chains, "sessions": sessions })
}

pub async fn deadlocks(ws: &Workspace, args: DeadlocksArgs, out: &Out) -> Result<u8> {
    let report = match (&args.file, &args.connection) {
        (Some(path), _) => {
            let text = if path.as_os_str() == "-" {
                let mut buf = String::new();
                std::io::stdin().read_to_string(&mut buf).map_err(|e| Error::invalid(e.to_string()))?;
                buf
            } else {
                std::fs::read_to_string(path).map_err(|e| Error::invalid(format!("{}: {e}", path.display())))?
            };
            DeadlockReport { deadlocks: deadlocks::parse_deadlock(&text)?, source: "file".into(), ..Default::default() }
        }
        (None, Some(connection)) => ws.deadlocks(connection, parse_since(&args.since)?).await?,
        (None, None) => return Err(Error::invalid("Name a connection, or pass --file with a saved deadlock")),
    };
    if out.format() == Format::Json {
        out.value(&serde_json::to_value(&report).unwrap_or(Value::Null));
        return Ok(0);
    }
    let span = if report.since_hours > 0.0 {
        format!(" in the last {}", hours_phrase(report.since_hours))
    } else {
        String::new()
    };
    match report.deadlocks.len() {
        0 => println!("No deadlocks{span}{}.", source_phrase(&report)),
        1 => println!("1 deadlock{span}{}.", source_phrase(&report)),
        n => println!("{n} deadlocks{span}{}.", source_phrase(&report)),
    }
    if let Some(note) = &report.note {
        println!("{}", out.dim(note));
    }
    for d in &report.deadlocks {
        println!();
        print_deadlock(d, out);
    }
    Ok(0)
}

fn hours_phrase(hours: f64) -> String {
    if hours >= 48.0 && (hours / 24.0).fract() == 0.0 {
        format!("{:.0} days", hours / 24.0)
    } else if hours >= 1.0 {
        format!("{hours:.0} hours")
    } else {
        format!("{:.0} minutes", hours * 60.0)
    }
}

fn source_phrase(report: &DeadlockReport) -> String {
    if report.source.is_empty() { String::new() } else { format!(" ({})", report.source) }
}

fn print_deadlock(d: &Deadlock, out: &Out) {
    let when = d.at.as_deref().map(|t| format!("{} ", t.replace('T', " ").trim_end_matches('Z'))).unwrap_or_default();
    println!("{}{}", out.dim(&when), out.bold(&d.headline));
    println!("  {}", d.explanation);
    for p in &d.processes {
        let tag = if p.victim { out.red("rolled back") } else { "           ".to_string() };
        let who = [p.session.as_deref().map(|s| format!("session {s}")), p.application.clone(), p.login.clone()]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(" · ");
        let sql: String =
            p.statement.as_deref().map(|q| q.split_whitespace().collect::<Vec<_>>().join(" ")).unwrap_or_default();
        let sql =
            if sql.chars().count() > 100 { format!("{}…", sql.chars().take(99).collect::<String>()) } else { sql };
        println!("  {tag}  {who}");
        if !sql.is_empty() {
            println!("               {}", out.dim(&sql));
        }
    }
    for (i, fix) in d.fixes.iter().enumerate() {
        println!("  {}. {}: {}", i + 1, out.bold(&fix.title), fix.detail);
        if let Some(sql) = &fix.sql {
            println!("     {sql}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spans_read_like_people_write_them() {
        assert_eq!(parse_since("24h").unwrap(), 24.0);
        assert_eq!(parse_since("90m").unwrap(), 1.5);
        assert_eq!(parse_since("7d").unwrap(), 168.0);
        assert_eq!(parse_since("6").unwrap(), 6.0);
        assert!(parse_since("soon").is_err());
        assert!(parse_since("0h").is_err());
        assert_eq!(hours_phrase(24.0), "24 hours");
        assert_eq!(hours_phrase(168.0), "7 days");
    }
}
