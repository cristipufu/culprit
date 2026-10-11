use std::io::{IsTerminal, Write};
use std::path::Path;

use clap::ValueEnum;
use culprit_core::{
    AuthKind, CheckInfo, Error, Finding, LiveState, LockedCheck, Plan, PlanNode, QueryStat, Report, ResultSet,
    ServerInfo, Severity, format,
};
use culprit_engine::{ConnectionInfo, DiscoveredDatabase};
use serde_json::{Value, json};

use crate::TopBy;

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
                .map(|r| format!("{} opportunities · scanned {}", r.opportunities, r.taken_at))
                .unwrap_or_else(|| "never scanned".into());
            println!(
                "{}  {:<11}  {}  {}",
                self.paint(BOLD, &format!("{:<20}", c.name)),
                c.engine.label(),
                self.paint(DIM, &format!("{:<12}", env)),
                self.paint(DIM, &format!("{}  ·  {last}", c.display))
            );
        }
    }

    pub fn connection_added(&self, info: &ConnectionInfo) {
        if self.format == Format::Json {
            return self.json(&json!(info));
        }
        println!("Saved {} ({}, {}).", self.paint(BOLD, &info.name), info.engine.label(), info.display);
        println!("Next: culprit scan {}", info.name);
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
            println!("  {} {}", self.paint(GREEN, "✓"), cap.label());
        }
        for m in &server.missing {
            println!("  {} {} — {}", self.paint(RED, "✗"), m.capability.label(), m.reason);
            if let Some(fix) = &m.fix {
                println!("    {}", self.paint(DIM, &fix.replace('\n', "\n    ")));
            }
        }
        if !locked.is_empty() {
            println!();
            println!("{}", self.paint(BOLD, "Locked checks"));
            for l in locked {
                let needs: Vec<String> = l.missing.iter().map(|c| c.label().to_string()).collect();
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
            println!(
                "{}",
                self.paint(
                    DIM,
                    &format!(
                        "{} · {} queries · {} of database time per day",
                        w.source,
                        format::count(w.query_count),
                        format::duration(w.per_day(w.total_time_ms))
                    )
                )
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
        for (i, f) in list.iter().enumerate() {
            println!(
                "{:>3}  {}  {}  {}",
                i + 1,
                self.severity_mark(f.severity),
                f.title,
                self.paint(DIM, &format!("· {}", f.impact.label))
            );
            println!("       {}", self.paint(DIM, &f.id));
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
            if self.format == Format::Json {
                return self.json(&json!({ "queries": [] , "note": "No workload data in this report" }));
            }
            println!("No workload data in this report. See `culprit doctor {}`.", report.snapshot.connection);
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
        println!(
            "{}",
            self.paint(DIM, &format!("{} · {:.0} hours · share of DB time · runs/day · avg", w.source, w.window_hours))
        );
        for q in queries {
            let text = q.text.split_whitespace().collect::<Vec<_>>().join(" ");
            let short: String = text.chars().take(90).collect();
            println!(
                "{:>5}  {:>8}  {:>9}  {}  {}",
                format::percent(w.share(q.total_time_ms)),
                format::count(w.per_day(q.executions)),
                format::duration(q.mean_time_ms),
                self.paint(DIM, &format!("{:<8}", q.id)),
                short
            );
        }
    }

    pub fn plan(&self, plan: &Plan) {
        if self.format == Format::Json {
            let mut value = json!(plan);
            value["raw"] = Value::Null;
            return self.json(&value);
        }
        println!("{}", self.paint(DIM, &format!("{:?} plan · total cost {:.2}", plan.kind, plan.total_cost)));
        self.plan_node(plan, &plan.root, 0);
    }

    fn plan_node(&self, plan: &Plan, node: &PlanNode, depth: usize) {
        let pct = if plan.total_cost > 0.0 { node.self_cost / plan.total_cost } else { 0.0 };
        let rows = match node.act_rows {
            Some(a) => format!("{} → {}", format::count(node.est_rows_total()), format::count(a)),
            None => format::count(node.est_rows_total()),
        };
        let label = format!(
            "{}{}{}",
            node.op,
            node.object.as_deref().map(|o| format!(" {o}")).unwrap_or_default(),
            node.index.as_deref().map(|i| format!(" ({i})")).unwrap_or_default()
        );
        let pct_text = format!("{:>4}", format::percent(pct));
        let pct_text = if pct >= 0.3 { self.paint(RED, &pct_text) } else { pct_text };
        let warnings: Vec<&str> = node.warnings.iter().map(|w| w.label()).collect();
        println!(
            "{pct_text}  {}{}  {}{}",
            "  ".repeat(depth),
            label,
            self.paint(DIM, &format!("rows {rows}")),
            if warnings.is_empty() {
                String::new()
            } else {
                self.paint(YELLOW, &format!("  ⚠ {}", warnings.join(", ")))
            }
        );
        for child in &node.children {
            self.plan_node(plan, child, depth + 1);
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
            println!("{} {} session(s) blocked", self.paint(RED, "!"), blocked.len());
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
        println!(
            "{}",
            self.paint(
                DIM,
                &format!(
                    "{} rows{} · {}",
                    rs.rows.len(),
                    if rs.truncated { " (truncated)" } else { "" },
                    format::duration(rs.elapsed_ms)
                )
            )
        );
    }

    pub fn checks(&self, checks: &[CheckInfo]) {
        if self.format == Format::Json {
            return self.json(&json!(checks));
        }
        for c in checks {
            let engines: Vec<&str> = c.engines.iter().map(|e| e.label()).collect();
            println!("{}  {}", self.paint(BOLD, &format!("{:<16}", c.id)), c.title);
            println!("{}", self.paint(DIM, &format!("{:<18}{} · {}", "", engines.join(", "), c.description)));
        }
    }
}

fn query_json(report: &Report, q: &QueryStat) -> Value {
    let w = report.snapshot.workload.as_ref();
    let hot = q.plan.as_ref().map(|p| {
        let n = p.hottest();
        json!({
            "op": n.op,
            "object": n.object,
            "index": n.index,
            "predicate": n.predicate,
            "cost_share": if p.total_cost > 0.0 { n.self_cost / p.total_cost } else { 0.0 },
            "warnings": n.warnings,
        })
    });
    json!({
        "id": q.id,
        "text": q.text,
        "object_name": q.object_name,
        "db_time_share": w.map(|w| w.share(q.total_time_ms)),
        "runs_per_day": w.map(|w| w.per_day(q.executions)),
        "mean_time_ms": q.mean_time_ms,
        "max_time_ms": q.max_time_ms,
        "cpu_time_ms": q.cpu_time_ms,
        "rows_per_run": q.rows_per_execution(),
        "reads_per_run": q.reads_per_execution(),
        "plans": q.plans.len(),
        "hottest_operator": hot,
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
        s.connection, s.server.product, s.server.database, s.taken_at, report.summary.headline
    );
    out.push_str("| # | Severity | Finding | Impact | Id |\n|---|---|---|---|---|\n");
    for (i, f) in report.findings.iter().enumerate() {
        out.push_str(&format!(
            "| {} | {:?} | {} | {} | `{}` |\n",
            i + 1,
            f.severity,
            f.title.replace('|', "\\|"),
            f.impact.label,
            f.id
        ));
    }
    out
}
