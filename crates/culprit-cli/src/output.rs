use std::io::{IsTerminal, Write};
use std::path::Path;

use clap::ValueEnum;
use culprit_core::changes::{Period, short_time};
use culprit_core::{
    AuthKind, ChangeKind, ChangeStatus, Changes, CheckInfo, Drift, DriftKind, DriftPresence, Engine, Error, Finding,
    LiveState, LockedCheck, Plan, PlanDiff, PlanNode, QueryStat, Report, ResultSet, ServerInfo, Severity, format,
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
    Md,
}

pub struct Out {
    format: Format,
    color: bool,
}

struct Style(&'static str);

const BOLD: Style = Style("1");
const DIM: Style = Style("2");
const RED: Style = Style("31");
const YELLOW: Style = Style("33");
const BLUE: Style = Style("34");
const GREEN: Style = Style("32");

impl Out {
    pub fn new(format: Format) -> Out {
        let color = format == Format::Text && std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none();
        Out { format, color }
    }

    fn paint(&self, style: Style, text: &str) -> String {
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

    fn json(&self, value: &Value) {
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

    fn severity_mark(&self, s: Severity) -> String {
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
                return self.json(&json!(report));
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
            let time = if w.window_hours < 20.0 {
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
            return self.json(&json!(report));
        }
        self.findings(report, Severity::Low);
    }

    pub fn findings(&self, report: &Report, min: Severity) {
        let list: Vec<&Finding> = report.findings.iter().filter(|f| f.severity >= min).collect();
        if self.format == Format::Json {
            return self.json(&json!({ "summary": report.summary, "findings": list, "locked": report.locked }));
        }
        if self.format == Format::Md {
            return print!("{}", markdown_report(report));
        }
        println!("{}", self.paint(BOLD, &report.summary.headline));
        println!();
        // Findings whose queries stopped running come last, under their own heading: likely fixed or changed by a deploy.
        let (active, stopped): (Vec<&Finding>, Vec<&Finding>) = list.iter().partition(|f| !f.inactive);
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
            println!(
                "{}",
                self.paint(
                    BOLD,
                    &format!(
                        "Stopped running ({}): their queries no longer run; a deploy may have changed them",
                        stopped.len()
                    )
                )
            );
            for (i, f) in stopped.iter().enumerate() {
                println!("{:>3}  {}  {}", active.len() + i + 1, self.paint(DIM, "○"), self.paint(DIM, &f.title));
                println!("       {}", self.paint(DIM, &f.id));
            }
        }
        let list: Vec<&Finding> = active.iter().chain(stopped.iter()).copied().collect();
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
            TopBy::Time => q.total_time_ms,
            TopBy::Cpu => q.cpu_time_ms.unwrap_or(0.0),
            TopBy::Reads => q.logical_reads.unwrap_or(0.0),
            TopBy::Executions => q.executions,
        };
        queries.sort_by(|a, b| key(b).total_cmp(&key(a)));
        queries.truncate(limit);
        if self.format == Format::Json {
            let list: Vec<Value> = queries.iter().map(|q| query_json(report, q)).collect();
            return self.json(&json!({ "source": w.source, "window_hours": w.window_hours, "queries": list }));
        }
        // Under 20 hours of history, runs per day would be an extrapolation: show the runs counted, and over what.
        let short_window = w.window_hours < 20.0;
        let span = format::duration(w.window_hours * 3_600_000.0);
        let header = if short_window {
            format!("{} · {} · share of DB time · runs · avg", w.source, w.window_phrase())
        } else {
            format!("{} · {span} · share of DB time · runs/day · avg", w.source)
        };
        println!("{}", self.paint(DIM, &header));
        for q in queries {
            let text = q.text.split_whitespace().collect::<Vec<_>>().join(" ");
            let short: String = text.chars().take(90).collect();
            println!(
                "{:>5}  {:>8}  {:>9}  {}  {}",
                format::percent(w.share(q.total_time_ms)),
                format::count(if short_window { q.executions } else { w.per_day(q.executions) }),
                format::duration(q.mean_time_ms),
                self.paint(DIM, &format!("{:<8}", q.id)),
                short
            );
        }
    }

    pub fn plan(&self, plan: &Plan) {
        let found = plan.insights.clone().unwrap_or_else(|| culprit_core::insights::PlanInsights {
            items: culprit_core::insights::insights(plan),
            note: culprit_core::insights::coverage_note(plan),
        });
        let summary = culprit_core::actual_summary(plan);
        if self.format == Format::Json {
            let mut value = json!(plan);
            value["raw"] = Value::Null;
            value["insights"] = json!(found);
            if let Some(summary) = &summary {
                value["actual"] = json!(summary);
            }
            return self.json(&value);
        }
        let own = culprit_core::self_times(plan);
        match (&summary, &own) {
            (Some(s), Some(_)) => {
                let mut parts = vec!["Actual plan".to_string()];
                if let Some(ms) = s.elapsed_ms {
                    parts.push(format!("{} elapsed", format::duration(ms)));
                }
                if let Some(ms) = s.cpu_ms {
                    parts.push(format!("{} CPU", format::duration(ms)));
                }
                if let Some(reads) = s.logical_reads {
                    parts.push(format!("{} reads", format::count(reads)));
                }
                parts.push("% = share of the time".into());
                println!("{}", self.paint(DIM, &parts.join(" · ")));
                if let (Some(granted), Some(used)) = (s.memory_granted_kb.filter(|g| *g > 0.0), s.memory_used_kb) {
                    println!("{}", self.paint(DIM, &format!("Memory: used {} of {} granted", kb(used), kb(granted))));
                }
                if s.spilled {
                    println!("{}", self.paint(YELLOW, "⚠ Ran out of memory and spilled to disk"));
                }
            }
            (Some(_), None) => {
                let when = plan
                    .props
                    .get("LastRunAt")
                    .map(|at| format!(" · last ran {}", at.replace('T', " ")))
                    .unwrap_or_default();
                println!(
                    "{}",
                    self.paint(
                        DIM,
                        &format!("Actual rows from the last run{when} · % = share of the estimated cost (no per-operator time was kept)")
                    )
                );
            }
            _ if !plan.weighed() => println!(
                "{}",
                self.paint(DIM, &format!("{:?} plan · the access path only: no row or cost estimates", plan.kind))
            ),
            _ => println!("{}", self.paint(DIM, &format!("{:?} plan · total cost {:.2}", plan.kind, plan.total_cost))),
        }
        let total_time = own.as_ref().map(|t| t.values().sum::<f64>()).filter(|t| *t > 0.0);
        let flagged: Vec<u32> = found.items.iter().filter_map(|i| i.nodes.first().copied()).collect();
        self.plan_node(plan, &plan.root, 0, own.as_ref().zip(total_time), &flagged);
        // Shares of the estimated cost: why a step shows none ("—").
        if total_time.is_none()
            && let Some(note) = culprit_engine::export::shares_note(plan)
        {
            println!("{}", self.paint(DIM, &format!("   —  {note}")));
        }
        self.plan_insights(&found, plan.weighed());
    }

    /// Insights under the tree: what's wrong, how to fix it, how to check, most important first.
    fn plan_insights(&self, found: &culprit_core::insights::PlanInsights, weighed: bool) {
        println!();
        if found.items.is_empty() {
            println!("{}", self.paint(BOLD, "No problems found in this plan."));
        } else {
            println!("{}", self.paint(BOLD, &format!("Insights ({})", found.items.len())));
        }
        for i in &found.items {
            let badge = match i.severity {
                Severity::High => self.paint(RED, "high  "),
                Severity::Medium => self.paint(YELLOW, "medium"),
                Severity::Low => self.paint(DIM, "low   "),
            };
            println!(
                "\n{badge}  {}  {}",
                self.paint(BOLD, &i.title),
                self.paint(
                    DIM,
                    &format!(
                        "· {}{} · operator {}",
                        // A rule's own weight isn't a share of the work, and a plan without costs has none.
                        if i.fixed_weight || !weighed {
                            String::new()
                        } else {
                            format!("{} of the work · ", format::percent(i.impact))
                        },
                        i.rule,
                        i.nodes.first().copied().unwrap_or(0)
                    )
                )
            );
            println!("        {}", i.detail);
            println!("        {} {}", self.paint(BOLD, "Fix:"), i.fix);
            if let Some(sql) = &i.fix_sql {
                for line in sql.lines() {
                    println!("            {}", self.paint(DIM, line));
                }
            }
            println!("        {} {}", self.paint(BOLD, "Verify:"), i.verify);
        }
        if let Some(note) = &found.note {
            println!("\n{}", self.paint(DIM, note));
        }
    }

    fn plan_node(
        &self,
        plan: &Plan,
        node: &PlanNode,
        depth: usize,
        time: Option<(&std::collections::HashMap<u32, f64>, f64)>,
        flagged: &[u32],
    ) {
        let own_ms = time.and_then(|(t, _)| t.get(&node.id).copied());
        // The share of the measured time, else of the estimated cost where the plan can state one (none beside a step it
        // puts no cost on, nor on rows that rest on a default guess).
        let pct = match time {
            Some((t, total)) => Some(t.get(&node.id).copied().unwrap_or(0.0) / total),
            None => plan.step_share(node),
        };
        // A plan without costs or times (SQLite) has no shares or estimates to print.
        let weighed = time.is_some() || plan.weighed();
        let rows = match node.act_rows {
            // The estimate for the runs that happened, so an inner seek estimated right per run isn't shown as off.
            Some(a) => format!("rows {} → {}", format::count(node.est_rows_for_runs()), format::count(a)),
            None if weighed => format!("rows {}", format::count(node.est_rows_total())),
            None => String::new(),
        };
        let label = format!(
            "{}{}{}{}",
            if flagged.contains(&node.id) { "◆ " } else { "" },
            node.op,
            node.object.as_deref().map(|o| format!(" {o}")).unwrap_or_default(),
            node.index.as_deref().map(|i| format!(" ({i})")).unwrap_or_default()
        );
        let pct_text = match pct {
            Some(p) if weighed => format!("{:>4}", format::percent(p)),
            None if weighed => "   —".to_string(),
            _ => "   ·".to_string(),
        };
        let pct_text = if weighed && pct.is_some_and(|p| p >= 0.3) { self.paint(RED, &pct_text) } else { pct_text };
        let warnings: Vec<&str> = node.warnings.iter().map(|w| w.label()).collect();
        let timing =
            own_ms.filter(|ms| *ms >= 0.5).map(|ms| format!(" · {}", format::duration(ms))).unwrap_or_default();
        println!(
            "{pct_text}  {}{}  {}{}",
            "  ".repeat(depth),
            label,
            self.paint(DIM, &format!("{rows}{timing}")),
            if warnings.is_empty() {
                String::new()
            } else {
                self.paint(YELLOW, &format!("  ⚠ {}", warnings.join(", ")))
            }
        );
        for child in &node.children {
            self.plan_node(plan, child, depth + 1, time, flagged);
        }
    }

    /// A running statement. With `again`, a refresh under `--watch` (text mode reprints compactly).
    pub fn query_progress(&self, progress: &culprit_core::QueryProgress, again: bool) {
        if self.format == Format::Json {
            let mut value = json!(progress);
            if again {
                value["plan"] = Value::Null;
            }
            return self.json(&value);
        }
        if !progress.running {
            println!(
                "{}",
                self.paint(DIM, &format!("Session {} isn't running a statement right now.", progress.session))
            );
            return;
        }
        let mut head = vec![format!("Session {}", progress.session)];
        if let Some(ms) = progress.elapsed_ms {
            head.push(format!("running {}", format::duration(ms)));
        }
        if let Some(ms) = progress.cpu_ms {
            head.push(format!("CPU {}", format::duration(ms)));
        }
        if let Some(reads) = progress.logical_reads {
            head.push(format!("{} reads", format::count(reads)));
        }
        if let Some(rows) = progress.rows_examined {
            head.push(format!("{} rows examined", format::count(rows)));
        }
        if let Some(wait) = &progress.wait {
            head.push(format!("waiting on {wait}"));
        }
        println!("{}", self.paint(BOLD, &head.join(" · ")));
        if let Some(phase) = &progress.phase {
            let pct = progress.phase_percent.map(|p| format!(" · {}", format::percent(p))).unwrap_or_default();
            println!("{phase}{pct}");
        }
        if let Some(pct) = progress.percent {
            let basis = progress.percent_basis.as_deref().unwrap_or("");
            println!("{} {}  {}", bar(pct, 24), format::percent(pct), self.paint(DIM, basis));
        }
        let Some(plan) = &progress.plan else {
            if let Some(note) = &progress.note {
                println!("{}", self.paint(DIM, note));
            }
            return;
        };
        let by_id: std::collections::HashMap<u32, &culprit_core::NodeProgress> =
            progress.nodes.iter().map(|n| (n.node_id, n)).collect();
        if let Some(hot) = progress.hot_node.and_then(|id| plan.nodes().into_iter().find(|n| n.id == id)) {
            let label = format!("{}{}", hot.op, hot.object.as_deref().map(|o| format!(" {o}")).unwrap_or_default());
            println!("{}", self.paint(YELLOW, &format!("Busiest step right now: {label}")));
        }
        fn visit(
            out: &Out,
            node: &PlanNode,
            depth: usize,
            by_id: &std::collections::HashMap<u32, &culprit_core::NodeProgress>,
        ) {
            let label = format!("{}{}", node.op, node.object.as_deref().map(|o| format!(" {o}")).unwrap_or_default());
            let line = match by_id.get(&node.id) {
                Some(p) => {
                    let state = match p.state {
                        culprit_core::NodeState::Pending => out.paint(DIM, "waiting"),
                        culprit_core::NodeState::Running => out.paint(GREEN, "running"),
                        culprit_core::NodeState::Done => out.paint(DIM, "done"),
                    };
                    let fraction = p.fraction().unwrap_or(0.0);
                    let over = if p.est_rows >= 1.0 && p.rows > p.est_rows * 1.5 {
                        out.paint(YELLOW, &format!(" ({}× the estimate)", format::count(p.rows / p.est_rows)))
                    } else {
                        String::new()
                    };
                    format!(
                        "{} {:>4}  {}{}  {} / {} rows{}  {}",
                        bar(fraction, 10),
                        format::percent(fraction),
                        "  ".repeat(depth),
                        label,
                        format::count(p.rows),
                        format::count(p.est_rows),
                        over,
                        state
                    )
                }
                None => format!(
                    "{}  {}{}  {} rows est.",
                    " ".repeat(16),
                    "  ".repeat(depth),
                    label,
                    format::count(node.est_rows_total())
                ),
            };
            println!("{line}");
            for child in &node.children {
                visit(out, child, depth + 1, by_id);
            }
        }
        visit(self, &plan.root, 0, &by_id);
        if let Some(note) = &progress.note {
            println!("{}", self.paint(DIM, note));
        }
    }

    pub fn plan_diff(&self, diff: &PlanDiff, name_a: &str, name_b: &str) {
        if self.format == Format::Json {
            let mut value = json!(diff);
            value["a"] = json!(name_a);
            value["b"] = json!(name_b);
            return self.json(&value);
        }
        for headline in &diff.headlines {
            println!("{}", self.paint(BOLD, headline));
        }
        let cost = |c: f64| if c < 10.0 { format!("{c:.3}") } else { format::count(c) };
        println!(
            "{}",
            self.paint(
                DIM,
                &format!("A = {name_a} · cost {}   B = {name_b} · cost {}", cost(diff.cost_a), cost(diff.cost_b))
            )
        );
        if diff.changes.is_empty() {
            return;
        }
        println!();
        for change in &diff.changes {
            let mark = match change.kind {
                ChangeKind::Access | ChangeKind::Operator => self.paint(YELLOW, "~"),
                ChangeKind::Added => self.paint(GREEN, "+"),
                ChangeKind::Removed => self.paint(RED, "-"),
                ChangeKind::Cost => self.paint(BLUE, "$"),
                ChangeKind::Rows => self.paint(BLUE, "#"),
                ChangeKind::Warning => self.paint(YELLOW, "!"),
            };
            let detail = change.detail.as_deref().map(|d| self.paint(DIM, &format!("  {d}"))).unwrap_or_default();
            println!("{mark} {}{detail}", change.title);
        }
    }

    pub fn live(&self, live: &LiveState) {
        if self.format == Format::Json {
            return self.json(&json!(live));
        }
        let blocked: Vec<_> = live.sessions.iter().filter(|s| s.blocked_by.is_some()).collect();
        if blocked.is_empty() {
            println!("{} Nothing is blocked.", self.paint(GREEN, "✓"));
        } else {
            println!(
                "{} {} blocked",
                self.paint(RED, "!"),
                if blocked.len() == 1 { "1 session".to_string() } else { format!("{} sessions", blocked.len()) }
            );
        }
        for s in &live.sessions {
            println!(
                "{:>6}  {:<10} {:>9}  {}{}  {}",
                s.id,
                s.state,
                s.elapsed_ms.map(format::duration).unwrap_or_default(),
                s.wait.as_deref().unwrap_or(""),
                s.blocked_by.as_deref().map(|b| format!(" blocked by {b}")).unwrap_or_default(),
                s.sql
                    .as_deref()
                    .map(|q| q.split_whitespace().collect::<Vec<_>>().join(" "))
                    .unwrap_or_default()
                    .chars()
                    .take(80)
                    .collect::<String>()
            );
        }
    }

    pub fn result_set(&self, rs: &ResultSet) {
        if self.format == Format::Json {
            return self.json(&json!(rs));
        }
        println!("{}", rs.columns.join("\t"));
        for row in &rs.rows {
            let cells: Vec<String> = row
                .iter()
                .map(|v| match v {
                    Value::String(s) => s.clone(),
                    Value::Null => "NULL".into(),
                    other => other.to_string(),
                })
                .collect();
            println!("{}", cells.join("\t"));
        }
        let tail = if rs.columns.is_empty() {
            rs.messages.join(" ")
        } else {
            let n = rs.rows.len();
            format!("{n} row{}{}", if n == 1 { "" } else { "s" }, if rs.truncated { " (truncated)" } else { "" })
        };
        println!("{}", self.paint(DIM, &format!("{tail} · {}", format::duration(rs.elapsed_ms))));
    }

    pub fn catalog(&self, catalog: &culprit_engine::catalog::Catalog) {
        if self.format == Format::Json {
            return self.json(&json!(catalog));
        }
        for t in &catalog.tables {
            let cols: Vec<String> = t.columns.iter().map(|c| format!("{} {}", c.name, c.data_type)).collect();
            println!(
                "{}.{}{}  {}",
                t.schema,
                self.paint(BOLD, &t.name),
                if t.view { " (view)" } else { "" },
                self.paint(DIM, &cols.join(", "))
            );
        }
        if !catalog.databases.is_empty() {
            println!("{}", self.paint(DIM, &format!("databases: {}", catalog.databases.join(", "))));
        }
    }

    pub fn saved_queries(&self, queries: &[culprit_engine::saved::SavedQuery]) {
        if self.format == Format::Json {
            return self.json(&json!({ "queries": queries }));
        }
        if queries.is_empty() {
            println!("No saved queries yet. Save one with `culprit saved add <name> --sql \"…\"`.");
        }
        for q in queries {
            let first = q.sql.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("");
            let target = q.connection.as_deref().map(|c| format!("  ·  {c}")).unwrap_or_default();
            println!("{}{}", self.paint(BOLD, &q.name), self.paint(DIM, &target));
            let first: String = first.chars().take(100).collect();
            println!("  {}", self.paint(DIM, &first));
        }
    }

    pub fn saved_query(&self, query: &culprit_engine::saved::SavedQuery) {
        if self.format == Format::Json {
            return self.json(&json!(query));
        }
        println!("{}", self.paint(BOLD, &query.name));
        if let Some(description) = &query.description {
            println!("{}", self.paint(DIM, description));
        }
        println!("{}", query.sql.trim());
    }

    pub fn index_advice(&self, a: &culprit_core::IndexAdvice) {
        if self.format == Format::Json {
            return self.json(&json!(a));
        }
        let indent =
            |text: &str, prefix: &str| text.lines().map(|l| format!("{prefix}{l}")).collect::<Vec<_>>().join("\n");
        println!(
            "{}  {}",
            self.paint(BOLD, &format!("Index advisor · {}", a.connection)),
            self.paint(DIM, &format!("usage counted {}", a.counters))
        );
        if a.candidates.is_empty() && a.cleanups.is_empty() && a.tables.is_empty() {
            println!("\nNothing to add or clean up.");
        }
        if !a.candidates.is_empty() {
            println!("\n{}", self.paint(BOLD, &format!("Add ({})", a.candidates.len())));
            for (n, c) in a.candidates.iter().enumerate() {
                let weight = match (c.db_time_share, c.queries.len()) {
                    (Some(share), q) if q > 0 => format!(
                        "{} of DB time · {} quer{}",
                        format::percent(share),
                        q,
                        if q == 1 { "y" } else { "ies" }
                    ),
                    (_, q) if q > 0 => format!("{} quer{}", q, if q == 1 { "y" } else { "ies" }),
                    _ => String::new(),
                };
                let include =
                    if c.include.is_empty() { String::new() } else { format!(" include ({})", c.include.join(", ")) };
                println!(
                    "  {:>2}  {}  {}",
                    n + 1,
                    self.paint(BOLD, &format!("{}.{} ({}){include}", c.schema, c.table, c.keys.join(", "))),
                    self.paint(DIM, &weight)
                );
                println!("{}", indent(&c.reason, "      "));
                if let Some(w) = &c.widens {
                    println!("{}", self.paint(DIM, &format!("      widens {w}")));
                }
                if c.requests.len() > 1 {
                    println!("{}", self.paint(DIM, &format!("      merges {} optimizer requests", c.requests.len())));
                }
                if let Some(sql) = &c.sql {
                    println!("{}", self.paint(BLUE, &indent(sql, "      ")));
                }
                if let Some(cost) = &c.write_cost {
                    println!("{}", self.paint(DIM, &format!("      {cost}")));
                }
            }
        }
        if !a.cleanups.is_empty() {
            println!("\n{}", self.paint(BOLD, &format!("Clean up ({})", a.cleanups.len())));
            for c in &a.cleanups {
                let kind = match c.kind {
                    culprit_core::CleanupKind::Duplicate => "duplicate",
                    culprit_core::CleanupKind::Prefix => "covered",
                    culprit_core::CleanupKind::Overlap => "overlaps",
                    culprit_core::CleanupKind::Unused => "unused",
                };
                println!(
                    "  {}  {}  {}",
                    self.paint(YELLOW, &format!("{kind:<9}")),
                    self.paint(BOLD, &format!("{}.{}.{}", c.schema, c.table, c.index)),
                    self.paint(DIM, &c.size_bytes.map(format::bytes).unwrap_or_default())
                );
                println!("{}", indent(&c.reason, "             "));
                if let Some(sql) = &c.sql {
                    println!("{}", self.paint(BLUE, &indent(sql, "             ")));
                }
            }
        }
        if !a.tables.is_empty() {
            println!("\n{}", self.paint(BOLD, &format!("Tables ({})", a.tables.len())));
            for t in &a.tables {
                println!("  {}  {}", self.paint(BOLD, &format!("{}.{}", t.schema, t.table)), t.reason);
            }
        }
        for note in &a.notes {
            println!("\n{}", self.paint(DIM, note));
        }
    }

    pub fn drift(&self, drift: &Drift) {
        if self.format == Format::Json {
            return self.json(&json!(drift));
        }
        let side = |s: &culprit_core::DriftSide| {
            format!("{} ({}, {})", s.connection, format!("{:?}", s.environment).to_lowercase(), s.server())
        };
        println!("{}  vs  {}", self.paint(BOLD, &side(&drift.a)), self.paint(BOLD, &side(&drift.b)));
        println!("{}\n", drift.summary);
        for (kind, heading) in [
            (DriftKind::Index, "Indexes"),
            (DriftKind::Table, "Tables"),
            (DriftKind::Column, "Columns"),
            (DriftKind::Setting, "Settings"),
            (DriftKind::Version, "Version"),
        ] {
            let items: Vec<_> = drift.items.iter().filter(|i| i.kind == kind).collect();
            if items.is_empty() {
                continue;
            }
            println!("{}", self.paint(BOLD, heading));
            for item in items {
                let mark = match item.presence {
                    DriftPresence::OnlyA => self.paint(YELLOW, "◀"),
                    DriftPresence::OnlyB => self.paint(YELLOW, "▶"),
                    DriftPresence::Different => self.paint(RED, "≠"),
                    DriftPresence::Renamed => self.paint(DIM, "~"),
                };
                println!("  {mark} {}", item.title);
                if let (Some(sql), Some(target)) = (&item.sql, &item.sql_for) {
                    println!("{}", self.paint(DIM, &format!("      -- on {target}")));
                    for line in sql.lines() {
                        println!("      {}", self.paint(GREEN, line));
                    }
                }
            }
            println!();
        }
    }

    pub fn changes(&self, c: &Changes) {
        if self.format == Format::Json {
            return self.json(&json!(c));
        }
        let span = |p: &Period| format!("{} → {}", short_time(&p.start), short_time(&p.end));
        println!("{}", self.paint(BOLD, &format!("What changed on {}", c.connection)));
        println!("{}", self.paint(DIM, &format!("recent {}  vs  baseline {}", span(&c.recent), span(&c.baseline))));
        println!("{}\n", c.summary);
        // A note the summary already says (why there's nothing to compare) isn't repeated.
        let notes: Vec<&String> = c.notes.iter().filter(|n| !c.summary.contains(n.as_str())).collect();
        for note in &notes {
            println!("{}", self.paint(DIM, note));
        }
        if !notes.is_empty() {
            println!();
        }
        let shown: Vec<_> = c.events.iter().filter(|e| !e.minor).collect();
        if !shown.is_empty() {
            println!("{}", self.paint(BOLD, "Around then"));
            for e in shown.iter().take(12) {
                println!("  {}  {}", e.title, self.paint(DIM, &e.when()));
            }
            if shown.len() > 12 {
                println!("  {}", self.paint(DIM, &format!("… {} more (--json lists all)", shown.len() - 12)));
            }
            println!();
        }
        for (status, heading, style) in [
            (ChangeStatus::Regressed, "Got slower", RED),
            (ChangeStatus::New, "New", YELLOW),
            (ChangeStatus::Improved, "Got faster", GREEN),
            (ChangeStatus::Stopped, "Stopped running", BLUE),
            (ChangeStatus::NotComparable, "Not comparable", DIM),
            (ChangeStatus::LowSample, "Too few runs", DIM),
        ] {
            let items: Vec<_> = c.queries.iter().filter(|q| q.status == status).collect();
            if items.is_empty() {
                continue;
            }
            println!("{} {}", self.paint(style, "●"), self.paint(BOLD, &format!("{heading} ({})", items.len())));
            for q in items.iter().take(12) {
                let text: String = q.text.split_whitespace().collect::<Vec<_>>().join(" ");
                let text = if text.chars().count() > 100 {
                    format!("{}…", text.chars().take(99).collect::<String>())
                } else {
                    text
                };
                println!("  {}  {}", self.paint(DIM, &format!("{:>8}", q.query_id)), q.headline);
                println!("            {}", self.paint(DIM, &text));
            }
            if items.len() > 12 {
                println!("  {}", self.paint(DIM, &format!("… {} more (--json lists all)", items.len() - 12)));
            }
            println!();
        }
        if c.unchanged > 0 {
            let noun = if c.unchanged == 1 { "query" } else { "queries" };
            println!("{}", self.paint(DIM, &format!("{} {noun} ran about the same.", c.unchanged)));
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

fn query_json(report: &Report, q: &QueryStat) -> Value {
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
        "db_time_share": w.map(|w| w.share(q.total_time_ms)),
        // Per day only from a day's worth of history (20 hours); `runs` and `window_hours` are what was measured.
        "runs_per_day": w.filter(|w| w.window_hours >= 20.0).map(|w| w.per_day(q.executions)),
        "window_hours": w.map(|w| w.window_hours),
        "mean_time_ms": q.mean_time_ms,
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

fn compact_report(report: &Report, path: &Path) -> Value {
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
            "window_hours": w.window_hours,
            "total_time_ms": w.total_time_ms,
            "query_count": w.query_count,
            "total_executions": w.total_executions,
        })),
        "summary": report.summary,
        "findings": report.findings,
        "locked": report.locked,
        "notes": s.notes,
        "next": report.findings.iter().take(3).map(|f| format!("culprit show {} '{}'", s.connection, f.id)).collect::<Vec<_>>(),
    })
}

fn markdown_report(report: &Report) -> String {
    let s = &report.snapshot;
    let mut out = format!(
        "# Culprit report: {}\n\n{} · database `{}` · {}\n\n**{}**\n\n",
        s.connection,
        s.server.product,
        s.server.database,
        culprit_core::changes::dated_time(&s.taken_at),
        report.summary.headline
    );
    let (active, stopped): (Vec<&Finding>, Vec<&Finding>) = report.findings.iter().partition(|f| !f.inactive);
    let table = |out: &mut String, list: &[&Finding], from: usize| {
        out.push_str("| # | Severity | Finding | Impact | Id |\n|---|---|---|---|---|\n");
        for (i, f) in list.iter().enumerate() {
            out.push_str(&format!(
                "| {} | {:?} | {} | {} | `{}` |\n",
                from + i + 1,
                f.severity,
                md_cell(&f.title),
                md_cell(&f.impact.label),
                f.id.replace('`', "'")
            ));
        }
    };
    table(&mut out, &active, 0);
    if !stopped.is_empty() {
        out.push_str(&format!(
            "\n## Stopped running ({})\n\nTheir queries no longer run; a deploy may have changed them.\n\n",
            stopped.len()
        ));
        table(&mut out, &stopped, active.len());
    }
    out
}

/// The engine as people know it: a MySQL connection whose last scan met MariaDB is MariaDB.
fn engine_name(c: &ConnectionInfo) -> &'static str {
    let mariadb = c.last_report.as_ref().and_then(|r| r.product.as_deref()).is_some_and(|p| p.contains("MariaDB"));
    if c.engine == Engine::Mysql && mariadb { "MariaDB" } else { c.engine.label() }
}

/// Database text in a Markdown table cell: pipes would split the row, and HTML would render where the report is pasted.
fn md_cell(text: &str) -> String {
    text.replace('|', "\\|").replace('<', "&lt;").replace('>', "&gt;").replace('\n', " ")
}

fn kb(value: f64) -> String {
    if value >= 1024.0 * 1024.0 {
        format!("{:.1} GB", value / 1024.0 / 1024.0)
    } else if value >= 1024.0 {
        format!("{:.1} MB", value / 1024.0)
    } else {
        format!("{value:.0} KB")
    }
}

fn bar(fraction: f64, width: usize) -> String {
    let filled = ((fraction.clamp(0.0, 1.0) * width as f64).round() as usize).min(width);
    format!("{}{}", "█".repeat(filled), "░".repeat(width - filled))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report() -> Report {
        let text =
            std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../fixtures/mssql-demo.report.json"))
                .expect("fixture");
        serde_json::from_str(&text).expect("report")
    }

    #[test]
    fn stopped_findings_get_their_own_section_and_text_stays_text() {
        let mut r = report();
        let stopped = r.findings.iter().filter(|f| f.inactive).count();
        r.findings[0].inactive = true;
        r.findings[1].title = "orders <img src=x onerror=alert(1)> | split".into();
        let md = markdown_report(&r);
        assert!(md.contains(&format!("## Stopped running ({})", stopped + 1)), "{md}");
        assert!(md.contains("&lt;img") && !md.contains("<img"));
        assert!(md.contains("\\| split"));
    }
}
