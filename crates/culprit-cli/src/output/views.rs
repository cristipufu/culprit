use culprit_core::{LiveState, Plan, PlanNode, ResultSet, Severity, format};
use serde_json::{Value, json};

use super::{BLUE, BOLD, DIM, Format, GREEN, Out, RED, YELLOW, bar, kb};

impl Out {
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
                parts.push("% = share of timed operator work".into());
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
        if let Some(s) = &summary {
            if let Some(granted) = s.memory_granted_kb
                && s.memory_used_kb.is_none()
            {
                println!("Memory granted: {}; memory used was not recorded.", kb(granted));
            }
            if !s.triggers.is_empty() {
                println!(
                    "Trigger work (BEFORE timing may overlap its modifying operator; do not sum with operator time):"
                );
                for t in &s.triggers {
                    println!(
                        "  {}: {}{}",
                        t.name,
                        format::duration(t.time_ms),
                        t.calls.map(|c| format!(" across {} calls", format::count(c))).unwrap_or_default()
                    );
                }
            }
        }
        let total_time = own.as_ref().map(|t| t.values().sum::<f64>()).filter(|t| *t > 0.0);
        let flagged: Vec<u32> = found.items.iter().filter_map(|i| i.nodes.first().copied()).collect();
        self.plan_node(plan, &plan.root, 0, own.as_ref().zip(total_time), &flagged);
        // Shares of the estimated cost: why a step shows none ("—").
        if total_time.is_none()
            && let Some(note) = culprit_engine::export::shares_note(plan, None)
        {
            println!("{}", self.paint(DIM, &format!("   —  {note}")));
        }
        self.plan_insights(&found, plan);
    }

    /// Insights under the tree: what's wrong, how to fix it, how to check, most important first.
    fn plan_insights(&self, found: &culprit_core::insights::PlanInsights, plan: &Plan) {
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
                        // A share of the work only where the plan states one (`insights::stated_share`): not a rule's
                        // own weight, nor on a plan without costs, a cheap plan or a step whose cost rests on a guess.
                        match culprit_core::insights::stated_share(plan, i) {
                            Some(share) => format!("{} of the work · ", format::percent(share)),
                            None => String::new(),
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

    pub fn live(&self, live: &LiveState) {
        if self.format == Format::Json {
            return self.json(&json!(live));
        }
        let blocked: Vec<_> = live.sessions.iter().filter(|s| s.blocked_by.is_some()).collect();
        if blocked.is_empty() {
            println!(
                "{}",
                if live.blocking_incomplete() {
                    "No blocker was identified; lock visibility is incomplete."
                } else {
                    "No blocking was observed."
                }
            );
        } else {
            println!(
                "{} {} blocked",
                self.paint(RED, "!"),
                if blocked.len() == 1 { "1 session".to_string() } else { format!("{} sessions", blocked.len()) }
            );
        }
        for note in &live.notes {
            println!("Collection limitation: {note}");
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
            self.paint(
                DIM,
                &if a.counters.is_empty() {
                    "index usage counters unavailable".to_string()
                } else {
                    format!("usage counted {}", a.counters)
                },
            )
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
}
