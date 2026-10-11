//! The change timeline: `culprit mark` puts a deploy or a migration on a connection's timeline, `culprit events` lists
//! what changed around it (marks, schema, indexes, settings, restarts, throttling), `culprit cloud` shows what the cloud
//! measured and when it held the server back.

use clap::Args;
use culprit_core::changes::short_time;
use culprit_core::{ChangeEvent, CloudSignals, CloudUnit, Error, EventKind, EventSource, Result, format};
use culprit_engine::{EventLog, EventsRequest, Workspace};
use serde_json::json;

use crate::output::{Format, Out};

#[derive(Args)]
pub struct MarkArgs {
    /// A saved connection name.
    pub connection: String,
    /// What happened, e.g. `deploy v1.4.2` or `migration 0042 applied`.
    pub text: Vec<String>,
    /// When it happened (RFC 3339), if not now.
    #[arg(long)]
    pub at: Option<String>,
    /// List the connection's marks instead of adding one.
    #[arg(long, conflicts_with_all = ["text", "remove"])]
    pub list: bool,
    /// Remove the mark with this id.
    #[arg(long, conflicts_with = "text")]
    pub remove: Option<String>,
}

#[derive(Args)]
pub struct PeriodArgs {
    /// A saved connection name.
    pub connection: String,
    /// The last N hours: `6h`, `90m` or `2d` (default 7 days).
    #[arg(long, value_parser = crate::parse_hours, conflicts_with = "since")]
    pub last: Option<f64>,
    /// From this moment (RFC 3339).
    #[arg(long)]
    pub since: Option<String>,
    /// Until this moment (RFC 3339; default now).
    #[arg(long)]
    pub until: Option<String>,
    /// Skip cloud metrics (CloudWatch, Azure Monitor).
    #[arg(long)]
    pub no_cloud: bool,
    /// Fetch cloud metrics again instead of reusing the last few minutes' answer.
    #[arg(long)]
    pub refresh: bool,
}

impl PeriodArgs {
    fn request(&self) -> EventsRequest {
        EventsRequest {
            start: self.since.clone(),
            end: self.until.clone(),
            hours: self.last,
            cloud: self.no_cloud.then_some(false),
            refresh: self.refresh,
        }
    }
}

pub fn mark(ws: &Workspace, args: MarkArgs, out: &Out) -> Result<u8> {
    if args.list {
        let marks = ws.marks(&args.connection)?;
        if out.format() == Format::Json {
            out.value(&json!(marks));
        } else if marks.is_empty() {
            println!("No marks on {} yet. Add one: culprit mark {} deploy v1.4.2", args.connection, args.connection);
        } else {
            for m in &marks {
                println!("{}  {}  {}", out.dim(&m.id), short_time(&m.at), m.text);
            }
        }
        return Ok(0);
    }
    if let Some(id) = &args.remove {
        let removed = ws.unmark(&args.connection, id)?;
        if out.format() == Format::Json {
            out.value(&json!({ "removed": removed }));
        } else {
            println!("Removed \"{}\" from {}'s timeline.", removed.text, args.connection);
        }
        return Ok(0);
    }
    let text = args.text.join(" ");
    if text.trim().is_empty() {
        return Err(Error::invalid("Say what happened, e.g. `culprit mark shop deploy v1.4.2`"));
    }
    let mark = ws.mark(&args.connection, &text, args.at.as_deref(), "cli")?;
    if out.format() == Format::Json {
        out.value(&json!(mark));
    } else {
        println!(
            "Marked {} at {}: {} {}",
            args.connection,
            short_time(&mark.at),
            mark.text,
            out.dim(&format!("({})", mark.id))
        );
        println!(
            "{}",
            out.dim(&format!("Compare what ran since: culprit changes {} --since {}", args.connection, mark.at))
        );
    }
    Ok(0)
}

pub async fn events(ws: &Workspace, args: PeriodArgs, out: &Out) -> Result<u8> {
    let log = ws.events(&args.connection, &args.request()).await?;
    if out.format() == Format::Json {
        out.value(&json!(log));
        return Ok(0);
    }
    print_events(&log, out);
    Ok(0)
}

fn print_events(log: &EventLog, out: &Out) {
    let (from, to) = culprit_core::changes::range_times(&log.start, &log.end);
    println!("{}", out.bold(&format!("What changed around {} · {from} → {to}", log.connection)));
    if log.events.is_empty() {
        println!("Nothing: no marks, schema, index or settings changes, restarts or throttling in this period.");
    }
    for e in log.events.iter().filter(|e| !e.minor) {
        print_event(e, out);
    }
    let minor = log.events.iter().filter(|e| e.minor).count();
    if minor > 0 {
        println!("{}", out.dim(&format!("+ {minor} statistics updates (--json lists them)")));
    }
    for note in &log.notes {
        println!("{}", out.dim(note));
    }
}

fn label(kind: EventKind) -> &'static str {
    match kind {
        EventKind::Mark => "mark",
        EventKind::Index => "index",
        EventKind::Table => "table",
        EventKind::Code => "code",
        EventKind::Setting => "setting",
        EventKind::Version => "version",
        EventKind::Restart => "restart",
        EventKind::StatsReset => "reset",
        EventKind::Statistics => "stats",
        EventKind::Throttle => "cloud",
    }
}

fn print_event(e: &ChangeEvent, out: &Out) {
    let tag = format!("{:<8}", label(e.kind));
    let tag = if e.kind == EventKind::Throttle { out.red(&tag) } else { out.dim(&tag) };
    let source = match (e.source, &e.by) {
        (EventSource::Mark, Some(by)) => format!(" · by {by}"),
        (EventSource::Scans, _) => " · noticed between scans".to_string(),
        _ => String::new(),
    };
    println!("{tag} {}  {}", e.title, out.dim(&format!("{}{source}", e.when())));
    if let Some(detail) = &e.detail {
        println!("         {}", out.dim(detail));
    }
}

pub async fn cloud(ws: &Workspace, args: PeriodArgs, out: &Out) -> Result<u8> {
    let mut request = args.request();
    request.cloud = Some(true);
    if request.start.is_none() && request.hours.is_none() {
        request.hours = Some(24.0);
    }
    let signals = ws.cloud(&args.connection, &request).await?;
    if out.format() == Format::Json {
        out.value(&json!(signals));
        return Ok(0);
    }
    print_cloud(&signals, out);
    Ok(0)
}

fn print_cloud(s: &CloudSignals, out: &Out) {
    let tier = s.tier.as_deref().map(|t| format!(" · {t}")).unwrap_or_default();
    println!("{}", out.bold(&format!("{} · {}{tier}", s.service, s.resource)));
    println!("{}\n", s.summary);
    for e in &s.episodes {
        let until = if e.ongoing { "now".to_string() } else { short_time(&e.end) };
        println!("{} {}  {}", out.red("●"), e.title, out.dim(&format!("{} → {until}", short_time(&e.start))));
        println!("  {}", out.dim(&e.detail));
    }
    if !s.episodes.is_empty() {
        println!();
    }
    for series in &s.series {
        let values: Vec<f64> = series.points.iter().map(|p| p.value).collect();
        if values.is_empty() {
            continue;
        }
        let show = |v: f64| match series.unit {
            CloudUnit::Percent => format!("{v:.0}%"),
            CloudUnit::Ms => format::duration(v),
            CloudUnit::Bytes => format::bytes(v),
            CloudUnit::Credits => format!("{v:.1}"),
            CloudUnit::Iops | CloudUnit::Sessions if v < 1.0 => format!("{v:.2}"),
            CloudUnit::Iops | CloudUnit::Sessions if v < 10.0 => format!("{v:.1}"),
            CloudUnit::Iops | CloudUnit::Sessions => format::count(v),
        };
        let mean = values.iter().sum::<f64>() / values.len() as f64;
        let (worst_word, worst) = if series.floor {
            ("min", values.iter().copied().fold(f64::INFINITY, f64::min))
        } else {
            ("max", values.iter().copied().fold(f64::NEG_INFINITY, f64::max))
        };
        let limit = series.limit_label.as_deref().map(|l| format!(" · {l}")).unwrap_or_default();
        println!("  {:<24} avg {:<10} {worst_word} {}{}", series.label, show(mean), show(worst), out.dim(&limit));
    }
    for note in &s.notes {
        println!("{}", out.dim(note));
    }
}
