mod diff;
#[cfg(test)]
mod tests;
mod views;

use std::io::{IsTerminal, Write};
use std::path::Path;

use clap::ValueEnum;
use culprit_core::{
    AuthKind, CheckInfo, Engine, Error, Finding, LockedCheck, QueryStat, Report, ServerInfo, Severity, format,
};
use culprit_engine::{ConnectionInfo, DiscoveredDatabase, ImportCandidate};
use serde_json::{Value, json};

use crate::TopBy;

/// "just now", "7 min ago", "2 h 5 min ago": how long ago a scan was taken, as the app says it.
pub(crate) fn age(taken_at: &str) -> Option<String> {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).ok()?.as_secs_f64();
    let seconds = (now - format::epoch_seconds(taken_at)?).max(0.0);
    Some(if seconds < 60.0 { "just now".into() } else { format!("{} ago", format::duration(seconds * 1000.0)) })
}

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Format {
    Text,
    Json,
    #[value(alias = "markdown")]
    Md,
}

pub struct Out {
    pub(super) format: Format,
    color: bool,
}

pub(super) struct Style(&'static str);

pub(super) const BOLD: Style = Style("1");
pub(super) const DIM: Style = Style("2");
pub(super) const RED: Style = Style("31");
pub(super) const YELLOW: Style = Style("33");
pub(super) const BLUE: Style = Style("34");
pub(super) const GREEN: Style = Style("32");

impl Out {
    pub fn new(format: Format) -> Out {
        let color = format == Format::Text && std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none();
        Out { format, color }
    }

    pub(super) fn paint(&self, style: Style, text: &str) -> String {
        if !self.color {
            return text.to_string();
        }
        format!("\x1b[{}m{text}\x1b[0m", style.0)
    }

    pub fn format(&self) -> Format {
        self.format
    }

    /// Print a value as JSON (for commands whose output is already the right shape).
    pub fn value(&self, value: &Value) {
        self.json(value)
    }

    /// Text styled for a terminal (plain when piped or NO_COLOR is set).
    pub fn bold(&self, text: &str) -> String {
        self.paint(BOLD, text)
    }

    pub fn dim(&self, text: &str) -> String {
        self.paint(DIM, text)
    }

    pub fn red(&self, text: &str) -> String {
        self.paint(RED, text)
    }

    pub fn yellow(&self, text: &str) -> String {
        self.paint(YELLOW, text)
    }

    pub fn green(&self, text: &str) -> String {
        self.paint(GREEN, text)
    }

    pub fn blue(&self, text: &str) -> String {
        self.paint(BLUE, text)
    }

    pub(super) fn json(&self, value: &Value) {
        let mut stdout = std::io::stdout().lock();
        let _ = serde_json::to_writer_pretty(&mut stdout, value);
        let _ = writeln!(stdout);
    }

    pub fn progress(&self, message: &str) {
        if self.format == Format::Text && std::io::stderr().is_terminal() {
            eprintln!("{}", self.paint(DIM, message));
        }
    }

    pub fn done(&self, message: &str) {
        match self.format {
            Format::Json => self.json(&json!({ "ok": true, "message": message })),
            _ => println!("{message}"),
        }
    }

    pub fn error(&self, error: &Error) {
        if self.format == Format::Json {
            let body = json!({ "error": error });
            eprintln!("{}", serde_json::to_string_pretty(&body).unwrap_or_default());
            return;
        }
        eprintln!("{} {}", self.paint(RED, "error:"), error.message);
        if let Some(hint) = &error.hint {
            eprintln!("{} {hint}", self.paint(DIM, "hint:"));
        }
    }

    pub(super) fn severity_mark(&self, s: Severity) -> String {
        match s {
            Severity::High => self.paint(RED, "●"),
            Severity::Medium => self.paint(YELLOW, "●"),
            Severity::Low => self.paint(BLUE, "●"),
        }
    }

    pub fn connections(&self, list: &[ConnectionInfo]) {
        if self.format == Format::Json {
            return self.json(&json!(list));
        }
        if list.is_empty() {
            println!(
                "No saved connections. Add one with:\n  culprit conn add <name> --url '<connection string>' --env development"
            );
            return;
        }
        for c in list {
            let env = format!("{:?}", c.environment).to_lowercase();
            let last = c
                .last_report
                .as_ref()
                .map(|r| {
                    let when = age(&r.taken_at).unwrap_or_else(|| r.taken_at.clone());
                    format!("{} opportunities · scanned {when}", r.opportunities)
                })
                .unwrap_or_else(|| "never scanned".into());
            println!(
                "{}  {:<11}  {}  {}",
                self.paint(BOLD, &format!("{:<20}", c.name)),
                engine_name(c),
                self.paint(DIM, &format!("{:<12}", env)),
                self.paint(DIM, &format!("{}  ·  {last}", c.display))
            );
        }
    }

    /// `server`: what the connection test found, so a MariaDB server is named as itself.
    pub fn connection_added(&self, info: &ConnectionInfo, server: Option<&ServerInfo>) {
        if self.format == Format::Json {
            return self.json(&json!(info));
        }
        let engine = server.map(|s| s.name()).unwrap_or(engine_name(info));
        println!("Saved {} ({engine}, {}).", self.paint(BOLD, &info.name), info.display);
        match (&info.query_log, info.engine.has_server()) {
            (Some(log), _) => println!(
                "Statements: read from {log} on every scan (change it with `culprit conn set {} --query-log <file>`, or --no-query-log).",
                info.name
            ),
            (None, false) => println!(
                "No query log found beside the database. SQLite keeps no history of its statements: to read them from the \
                 log your framework writes, `culprit conn set {} --query-log <file>`.",
                info.name
            ),
            (None, true) => {}
        }
        println!("Next: culprit scan {}", info.name);
    }

    pub fn connection_renamed(&self, old: &str, info: &ConnectionInfo) {
        if self.format == Format::Json {
            return self.json(&json!(info));
        }
        println!(
            "Renamed {old} to {}; its scan history and saved password moved with it.",
            self.paint(BOLD, &info.name)
        );
    }

    pub fn discovered(&self, found: &[DiscoveredDatabase], saved: &[ConnectionInfo]) {
        if self.format == Format::Json {
            return self.json(&json!({ "databases": found, "saved": saved }));
        }
        if found.is_empty() {
            println!("No databases match. Try a different --match (or --region for AWS).");
            return;
        }
        for d in found {
            let env = format!("{:?}", d.environment).to_lowercase();
            let auth = match d.auth {
                AuthKind::Entra => "Entra ID",
                AuthKind::AwsIam => "AWS IAM",
                AuthKind::Password => "password",
                AuthKind::None => "no auth",
            };
            let status = match (&d.saved_as, saved.iter().any(|s| s.name == d.alias)) {
                (Some(name), _) => self.paint(DIM, &format!("saved as {name}")),
                (None, true) => self.paint(GREEN, "saved"),
                (None, false) => String::new(),
            };
            println!(
                "{}  {}  {:<10}  {}/{} ({}, {})  {}",
                self.paint(BOLD, &format!("{:<28}", d.alias)),
                self.paint(DIM, &format!("{:<11}", env)),
                d.engine.label(),
                d.server,
                if d.database.is_empty() { "—" } else { &d.database },
                d.location,
                auth,
                status
            );
        }
        if saved.is_empty() && found.iter().any(|d| d.saved_as.is_none()) {
            println!();
            println!(
                "{}",
                self.paint(DIM, "Add --save to save the unsaved ones (production only with --include-production).")
            );
        }
    }

    pub fn imported(&self, found: &[ImportCandidate], saved: &[ConnectionInfo], dry_run: bool) {
        if self.format == Format::Json {
            return self.json(&json!({ "connections": found, "saved": saved, "dry_run": dry_run }));
        }
        if found.is_empty() {
            println!(
                "No connections found in Azure Data Studio, VS Code, DBeaver or DataGrip. Point --file at a settings file."
            );
            return;
        }
        for c in found {
            let env = format!("{:?}", c.environment).to_lowercase();
            let status = match (&c.saved_as, saved.iter().any(|s| s.name == c.alias)) {
                (Some(name), _) => self.paint(DIM, &format!("already saved as {name}")),
                (None, true) => self.paint(GREEN, "imported"),
                (None, false) => String::new(),
            };
            let target = format!(
                "{}{}{}",
                c.host,
                c.port.map(|p| format!(":{p}")).unwrap_or_default(),
                c.database.as_deref().map(|d| format!("/{d}")).unwrap_or_default()
            );
            println!(
                "{}  {}  {:<10}  {}  {}  {}",
                self.paint(BOLD, &format!("{:<28}", c.alias)),
                self.paint(DIM, &format!("{:<11}", env)),
                c.engine.label(),
                target,
                self.paint(DIM, &format!("({})", c.source.label())),
                status
            );
            if let Some(note) = &c.note {
                println!("    {}", self.paint(YELLOW, note));
            }
        }
        if dry_run && found.iter().any(|c| c.saved_as.is_none()) {
            println!();
            println!("{}", self.paint(DIM, "Nothing saved (--dry-run). Run again without it to import."));
        } else if saved.iter().any(|s| s.auth == AuthKind::Password) {
            println!();
            println!(
                "{}",
                self.paint(DIM, "Passwords were not imported; set each once with `culprit conn set-password <name>`.")
            );
        }
    }

    pub fn server(&self, server: &ServerInfo) {
        if self.format == Format::Json {
            return self.json(&json!(server));
        }
        println!(
            "{} {} · database {}{}",
            self.paint(GREEN, "✓"),
            server.product,
            server.database,
            server.cloud.as_deref().map(|c| format!(" · {c}")).unwrap_or_default()
        );
    }

    pub fn doctor(&self, info: &ConnectionInfo, server: &ServerInfo, locked: &[LockedCheck]) {
        if self.format == Format::Json {
            return self.json(&json!({ "connection": info, "server": server, "locked": locked }));
        }
        self.server(server);
        println!(
            "  version {}{}",
            server.version,
            server.edition.as_deref().map(|e| format!(" · {e}")).unwrap_or_default()
        );
        if let Some(started) = &server.started_at {
            println!("  counters since {started}");
        }
        println!();
        for cap in &server.capabilities {
            println!("  {} {}", self.paint(GREEN, "✓"), cap.label_for(server.engine));
        }
        for m in &server.missing {
            println!("  {} {} — {}", self.paint(RED, "✗"), m.capability.label_for(server.engine), m.reason);
            if let Some(fix) = &m.fix {
                println!("    {}", self.paint(DIM, &fix.replace('\n', "\n    ")));
            }
        }
        if !locked.is_empty() {
            println!();
            println!("{}", self.paint(BOLD, "Locked checks"));
            for l in locked {
                let needs: Vec<String> = l.missing.iter().map(|c| c.label_for(server.engine).to_string()).collect();
                println!("  {} needs {}", l.check.title, format::list(&needs));
            }
        }
    }

    pub fn scan(&self, report: &Report, full: bool, dir: &Path) {
        let path = dir.join(format!("{}.json", report.snapshot.id));
        if self.format == Format::Json {
            if full {
                return self.json(&json!(culprit_engine::reports::display(report)));
            }
            return self.json(&compact_report(report, &path));
        }
        if self.format == Format::Md {
            return print!("{}", markdown_report(report));
        }
        let s = &report.snapshot;
        println!(
            "{} · {} · {} · scanned in {}",
            self.paint(BOLD, &s.connection),
            s.server.product,
            s.server.database,
            format::duration(s.duration_ms)
        );
        if let Some(w) = &s.workload {
            // Under 20 hours of history a per-day figure would be an extrapolation: say what was measured, and over what.
            let time = if w.measurement_hours() < 20.0 {
                format!("{} of database time over {}", format::duration(w.total_time_ms), w.window_phrase())
            } else {
                format!("{} of database time per day", format::duration(w.per_day(w.total_time_ms)))
            };
            println!(
                "{}",
                self.paint(DIM, &format!("{} · {} queries · {time}", w.source, format::count(w.query_count)))
            );
        }
        println!();
        self.findings(report, Severity::Low);
        if !s.notes.is_empty() {
            println!();
            for n in &s.notes {
                println!("{} {}: {}", self.paint(YELLOW, "!"), n.collector, n.message);
            }
        }
        println!();
        println!("{}", self.paint(DIM, &format!("Saved {}", path.display())));
    }

    pub fn analyzed(&self, report: &Report) {
        if self.format == Format::Json {
            return self.json(&json!(culprit_engine::reports::display(report)));
        }
        self.findings(report, Severity::Low);
    }

    pub fn findings(&self, report: &Report, min: Severity) {
        let list: Vec<&Finding> = report.findings.iter().filter(|f| f.severity >= min).collect();
        if self.format == Format::Json {
            return self.json(&json!({ "summary": report.summary, "findings": list, "locked": report.locked, "assessments": report.assessments }));
        }
        if self.format == Format::Md {
            return print!("{}", markdown_report(report));
        }
        println!("{}", self.paint(BOLD, &report.summary.headline));
        println!();
        // Findings whose queries stopped running come last, under their own heading: likely fixed or changed by a deploy.
        let (active, stopped): (Vec<&Finding>, Vec<&Finding>) = list.iter().partition(|f| f.is_actionable());
        for (i, f) in active.iter().enumerate() {
            println!(
                "{:>3}  {}  {}  {}",
                i + 1,
                self.severity_mark(f.severity),
                f.title,
                self.paint(DIM, &format!("· {}", f.impact.label))
            );
            println!("       {}", self.paint(DIM, &f.id));
        }
        if !stopped.is_empty() {
            println!();
            println!("{}", self.paint(BOLD, &format!("Information and history ({})", stopped.len())));
            for (i, f) in stopped.iter().enumerate() {
                println!("{:>3}  {}  {}", active.len() + i + 1, self.paint(DIM, "○"), self.paint(DIM, &f.title));
                println!("       {}", self.paint(DIM, &f.id));
            }
        }
        let list: Vec<&Finding> = active.iter().chain(stopped.iter()).copied().collect();
        if !report.assessments.is_empty() {
            let assessed =
                report.assessments.iter().filter(|a| a.status == culprit_core::CheckStatus::Assessed).count();
            let limited = report
                .assessments
                .iter()
                .filter(|a| {
                    matches!(
                        a.status,
                        culprit_core::CheckStatus::Unavailable | culprit_core::CheckStatus::InsufficientEvidence
                    ) || (a.status == culprit_core::CheckStatus::Findings && a.reason.is_some())
                })
                .count();
            println!("\n{}", self.paint(DIM, &format!("Check coverage: {assessed} assessed without a finding; {limited} limited by missing or incomplete evidence. JSON includes each check's reason.")));
        }
        if !report.locked.is_empty() {
            println!();
            let names: Vec<String> = report.locked.iter().map(|l| l.check.title.clone()).collect();
            println!(
                "{} {} locked ({}). See `culprit doctor {}`.",
                self.paint(YELLOW, "!"),
                if names.len() == 1 { "1 check is".to_string() } else { format!("{} checks are", names.len()) },
                format::list(&names),
                report.snapshot.connection
            );
        }
        if let Some(first) = list.first() {
            println!();
            println!(
                "{}",
                self.paint(DIM, &format!("Next: culprit show {} '{}'", report.snapshot.connection, first.id))
            );
        }
    }

    pub fn show(&self, report: &Report, finding: &Finding) {
        let brief = culprit_engine::agent_brief(report, finding);
        if self.format == Format::Json {
            let queries: Vec<Value> = finding
                .queries
                .iter()
                .filter_map(|r| report.snapshot.workload.as_ref()?.query(&r.id))
                .map(|q| query_json(report, q))
                .collect();
            return self.json(&json!({ "finding": finding, "queries": queries, "brief": brief }));
        }
        print!("{brief}");
    }

    pub fn top(&self, report: &Report, by: TopBy, limit: usize) {
        let Some(w) = &report.snapshot.workload else {
            let why = report.summary.no_workload.clone().unwrap_or_else(|| "This scan has no query history.".into());
            if self.format == Format::Json {
                return self.json(&json!({ "queries": [], "note": why }));
            }
            println!("{why}");
            return;
        };
        let mut queries: Vec<&QueryStat> = w.queries.iter().collect();
        let key = |q: &QueryStat| match by {
            // Its share as every surface states it: over Culprit's scans where they measured it.
            TopBy::Time => w.statement_share(q),
            TopBy::Cpu => q.cpu_time_ms.unwrap_or(0.0),
            TopBy::Reads => q.logical_reads.unwrap_or(0.0),
            TopBy::Executions => q.executions,
        };
        queries.sort_by(|a, b| key(b).total_cmp(&key(a)));
        queries.truncate(limit);
        if self.format == Format::Json {
            let list: Vec<Value> = queries.iter().map(|q| query_json(report, q)).collect();
            return self.json(&json!({ "source": w.source, "window_hours": w.measurement_hours(), "queries": list }));
        }
        // Under 20 hours of history, runs per day would be an extrapolation: show the runs counted, and over what.
        let short_window = w.measurement_hours() < 20.0;
        let span = format::duration(w.measurement_hours() * 3_600_000.0);
        // On a source of running totals, what Culprit's scans measured of the statements they did, as every surface
        // states it; the source's totals for the rest.
        let scans = queries.iter().filter_map(|q| w.scans_of(q)).max_by(|a, b| a.hours.total_cmp(&b.hours));
        let header = match &scans {
            Some(s) => {
                format!("{} · {} (its totals for the rest) · share of DB time · runs/day · avg", w.source, s.phrase())
            }
            None if short_window => format!("{} · {} · share of DB time · runs · avg", w.source, w.window_phrase()),
            None => format!("{} · {span} · share of DB time · runs/day · avg", w.source),
        };
        println!("{}", self.paint(DIM, &header));
        for q in queries {
            let text = q.text.split_whitespace().collect::<Vec<_>>().join(" ");
            let short: String = text.chars().take(90).collect();
            // A burst, or fewer runs than days, has no rate a day: when it ran is said after the statement.
            let burst = w.cadence(q).is_burst();
            let runs = match w.scans_of(q).filter(|_| !burst) {
                Some(s) => Some(s.per_day(s.runs)),
                None if short_window && !burst => Some(q.executions),
                None => w.runs_per_day_of(q),
            };
            let ran =
                if runs.is_none() { self.paint(DIM, &format!("  ({})", w.how_often_of(q))) } else { String::new() };
            println!(
                "{:>5}  {:>8}  {:>9}  {}  {}{ran}",
                format::percent(w.statement_share(q)),
                runs.map_or_else(|| "—".to_string(), format::count),
                format::duration(w.statement_mean_ms(q)),
                self.paint(DIM, &format!("{:<8}", q.id)),
                short
            );
        }
    }

    pub fn sources(&self, engines: &[(Engine, &[culprit_core::Source])]) {
        if self.format == Format::Json {
            let by_engine: serde_json::Map<String, Value> =
                engines.iter().map(|(e, s)| (format!("{e:?}").to_ascii_lowercase(), json!(s))).collect();
            return self.json(&Value::Object(by_engine));
        }
        for (engine, sources) in engines.iter().filter(|(_, s)| !s.is_empty()) {
            println!("{}", self.paint(BOLD, engine.label()));
            for s in *sources {
                let delivers: Vec<&str> = s.delivers.iter().map(|c| c.label_for(*engine)).collect();
                println!(
                    "  {:<26}{}",
                    s.name,
                    self.paint(
                        DIM,
                        &if delivers.is_empty() {
                            format!("{} · adds detail", s.reads)
                        } else {
                            format!("{} · {}", s.reads, delivers.join(", "))
                        }
                    )
                );
            }
        }
    }

    pub fn checks(&self, checks: &[CheckInfo]) {
        if self.format == Format::Json {
            return self.json(&json!(checks));
        }
        for c in checks {
            // The MySQL provider serves MariaDB too, and every MySQL check runs on both.
            let engines: Vec<&str> =
                c.engines.iter().map(|e| if *e == Engine::Mysql { "MySQL/MariaDB" } else { e.label() }).collect();
            println!("{}  {}", self.paint(BOLD, &format!("{:<16}", c.id)), c.title);
            println!("{}", self.paint(DIM, &format!("{:<18}{} · {}", "", engines.join(", "), c.description)));
        }
    }
}

pub(super) fn query_json(report: &Report, q: &QueryStat) -> Value {
    let w = report.snapshot.workload.as_ref();
    let hot = q.plan.as_ref().and_then(|p| {
        // Where the plan's work is: none in a plan too cheap for any of its steps to matter, nor where the plan can't
        // state the step's share (beside a step it puts no cost on, or on rows that rest on a default guess).
        let n = culprit_core::insights::working_step(p)?;
        let share = p.step_share(n)?;
        Some(json!({
            "op": n.op,
            "object": n.object,
            "index": n.index,
            "predicate": n.predicate,
            "cost_share": share,
            "warnings": n.warnings,
        }))
    });
    json!({
        "id": q.id,
        "text": q.text,
        "object_name": q.object_name,
        // Share, runs a day and average as every surface states them: over Culprit's scans where they measured it
        // (`measured_over`), with the source's totals beside them where those differ twofold (`totals`).
        "db_time_share": w.map(|w| w.statement_share(q)),
        // Per day only from a day's worth of history (20 hours); `runs` and `window_hours` are what was measured.
        // Null for a burst or fewer runs than days, said in `how_often` by when it ran.
        "runs_per_day": w.and_then(|w| match w.scans_of(q).filter(|_| !w.cadence(q).is_burst()) {
            Some(s) => Some(s.per_day(s.runs)),
            None => w.runs_per_day_of(q),
        }),
        "how_often": w.map(|w| w.cost_of(q).runs),
        "window_hours": w.map(|w| w.measurement_hours()),
        "measured_over": w.and_then(|w| w.scans_of(q)).map(|s| s.phrase()),
        "totals": w.and_then(|w| w.cost_of(q).totals),
        "mean_time_ms": w.map_or(q.mean_time_ms, |w| w.statement_mean_ms(q)),
        "max_time_ms": q.max_time_ms,
        "cpu_time_ms": q.cpu_time_ms,
        "rows_per_run": q.rows_per_execution(),
        "reads_per_run": q.reads_per_execution(),
        "runs": q.executions,
        "plans": q.plans.len(),
        // Fastest run and, where the engine keeps them, percentiles and a run-time histogram.
        "spread": q.spread,
        // Failed and cancelled runs, planning and JIT time, log bytes, estimated row width: null where not kept.
        "costs": q.costs,
        // Per plan, when there are several: how each one's run times spread and what values it was compiled for.
        "plan_spread": (q.plans.len() > 1).then(|| {
            q.plans
                .iter()
                .map(|p| {
                    json!({
                        "plan_id": p.id,
                        "runs": p.executions,
                        "mean_ms": p.mean_time_ms,
                        "min_ms": p.min_time_ms,
                        "max_ms": p.max_time_ms,
                        "stddev_ms": p.stddev_time_ms,
                        "compiled": p.compiled,
                        "variant_query": p.variant_query,
                    })
                })
                .collect::<Vec<_>>()
        }),
        "hottest_operator": hot,
        // Average duration per run in each timeline bucket (null where it didn't run or the source can't tell).
        "trend_mean_ms": q.trend.as_ref().map(|t| {
            (0..t.time_ms.len()).map(|i| t.mean_ms(i).map(|m| (m * 100.0).round() / 100.0)).collect::<Vec<_>>()
        }),
    })
}

pub(super) fn compact_report(report: &Report, path: &Path) -> Value {
    let s = &report.snapshot;
    json!({
        "connection": s.connection,
        "report_id": s.id,
        "report_path": path,
        "taken_at": s.taken_at,
        "duration_ms": s.duration_ms,
        "server": {
            "engine": s.server.engine,
            "product": s.server.product,
            "version": s.server.version,
            "edition": s.server.edition,
            "cloud": s.server.cloud,
            "database": s.server.database,
        },
        "workload": s.workload.as_ref().map(|w| json!({
            "source": w.source,
            "window_hours": w.measurement_hours(),
            "total_time_ms": w.total_time_ms,
            "query_count": w.query_count,
            "total_executions": w.total_executions,
        })),
        "summary": report.summary,
        "findings": report.findings,
        "locked": report.locked,
        "assessments": report.assessments,
        "engine_limitations": s.server.engine.spec().limitations(),
        "notes": s.notes,
        "next": report.findings.iter().take(3).map(|f| format!("culprit show {} '{}'", s.connection, f.id)).collect::<Vec<_>>(),
    })
}

pub(super) fn markdown_report(report: &Report) -> String {
    let s = &report.snapshot;
    let mut out = format!(
        "# Culprit report: {}\n\n{} · database `{}` · {}\n\n**{}**\n\n",
        s.connection,
        s.server.product,
        s.server.database,
        culprit_core::changes::dated_time(&s.taken_at),
        report.summary.headline
    );
    let (active, stopped): (Vec<&Finding>, Vec<&Finding>) = report.findings.iter().partition(|f| f.is_actionable());
    let table = |out: &mut String, list: &[&Finding], from: usize| {
        out.push_str("| # | Status | Finding | Impact | Id |\n|---|---|---|---|---|\n");
        for (i, f) in list.iter().enumerate() {
            out.push_str(&format!(
                "| {} | {} | {} | {} | `{}` |\n",
                from + i + 1,
                if f.inactive { "Stopped running" } else { f.actionability.label() },
                md_cell(&f.title),
                md_cell(&f.impact.label),
                f.id.replace('`', "'")
            ));
        }
    };
    table(&mut out, &active, 0);
    if !stopped.is_empty() {
        out.push_str(&format!("\n## Information and history ({})\n\n", stopped.len()));
        table(&mut out, &stopped, active.len());
    }
    out
}

/// The engine as people know it: a MySQL connection whose last scan met MariaDB is MariaDB.
pub(super) fn engine_name(c: &ConnectionInfo) -> &'static str {
    let mariadb = c.last_report.as_ref().and_then(|r| r.product.as_deref()).is_some_and(|p| p.contains("MariaDB"));
    if c.engine == Engine::Mysql && mariadb { "MariaDB" } else { c.engine.label() }
}

/// Database text in a Markdown table cell: pipes would split the row, and HTML would render where the report is pasted.
pub(super) fn md_cell(text: &str) -> String {
    text.replace('|', "\\|").replace('<', "&lt;").replace('>', "&gt;").replace('\n', " ")
}

pub(super) fn kb(value: f64) -> String {
    if value >= 1024.0 * 1024.0 {
        format!("{:.1} GB", value / 1024.0 / 1024.0)
    } else if value >= 1024.0 {
        format!("{:.1} MB", value / 1024.0)
    } else {
        format!("{value:.0} KB")
    }
}

pub(super) fn bar(fraction: f64, width: usize) -> String {
    let filled = ((fraction.clamp(0.0, 1.0) * width as f64).round() as usize).min(width);
    format!("{}{}", "█".repeat(filled), "░".repeat(width - filled))
}
