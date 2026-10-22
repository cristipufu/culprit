use culprit_core::changes::{Period, short_time};
use culprit_core::{ChangeKind, ChangeStatus, Changes, Drift, DriftKind, DriftPresence, PlanDiff, format};
use serde_json::json;

use super::{BLUE, BOLD, DIM, Format, GREEN, Out, RED, YELLOW};

impl Out {
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

    pub fn drift(&self, drift: &Drift) {
        if self.format == Format::Json {
            return self.json(&json!(drift));
        }
        if self.format == Format::Sarif {
            return self.json(&super::sarif::drift_sarif(drift));
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
                } else {
                    match (&item.a, &item.b) {
                        (Some(before), Some(after))
                            if !item.title.contains(before.as_str()) || !item.title.contains(after.as_str()) =>
                        {
                            println!("      {}", self.paint(DIM, &format!("{before} → {after}")));
                        }
                        (Some(detail), None) | (None, Some(detail)) if !item.title.contains(detail.as_str()) => {
                            println!("      {}", self.paint(DIM, detail));
                        }
                        _ => {}
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
}
