//! Deadlocks that keep happening: grouped by pattern and tables, explained, with the fix that removes them.

use std::collections::BTreeMap;

use culprit_core::{
    Category, CheckInfo, Confidence, Deadlock, DeadlockPattern, Engine, Finding, Fix, Metric, Severity, Snapshot,
    SubjectKind, Unit,
};

use crate::support::*;

pub fn deadlocks_info() -> CheckInfo {
    CheckInfo {
        id: "deadlocks".into(),
        title: "Deadlocks".into(),
        category: Category::Queries,
        engines: vec![Engine::Mssql, Engine::Postgres, Engine::Mysql],
        requires: vec![],
        description: "Deadlocks in the last day, grouped by pattern (key lookup, opposite order, read-then-update, lock \
                      escalation, parallelism) with the fix that removes them. SQL Server reads system_health, MySQL the \
                      latest InnoDB report, Postgres only keeps a counter."
            .into(),
    }
}

fn count_word(n: usize) -> String {
    match n {
        2 => "Two".into(),
        3 => "Three".into(),
        _ => n.to_string(),
    }
}

fn tables(objects: &[String]) -> String {
    match objects.len() {
        0 => "the same rows".into(),
        1 => objects[0].clone(),
        n => format!("{} and {}", objects[..n - 1].join(", "), objects[n - 1]),
    }
}

pub fn deadlocks(snapshot: &Snapshot) -> Vec<Finding> {
    let Some(report) = snapshot.deadlocks.as_ref() else { return Vec::new() };
    let mut out = Vec::new();
    let window = if report.since_hours <= 24.0 {
        "the last day".to_string()
    } else {
        format!("the last {:.0} days", report.since_hours / 24.0)
    };

    let mut groups: BTreeMap<(DeadlockPattern, Vec<String>), Vec<&Deadlock>> = BTreeMap::new();
    for d in &report.deadlocks {
        groups.entry((d.pattern, d.objects.clone())).or_default().push(d);
    }
    for ((pattern, objects), list) in groups {
        let n = list.len();
        let latest = list.iter().max_by(|a, b| a.at.cmp(&b.at)).copied().unwrap_or(list[0]);
        let statements: std::collections::BTreeSet<String> = list
            .iter()
            .flat_map(|d| d.processes.iter().filter_map(|p| p.statement.as_deref()))
            .map(|s| s.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase())
            .collect();
        let who = if latest.processes.len() == 2 && statements.len() <= 2 {
            "Two queries".to_string()
        } else {
            format!("{} transactions", count_word(latest.processes.len().max(2)))
        };
        let times = if n == 1 { "once".to_string() } else { format!("{n} times") };
        let key =
            format!("{}:{}", serde_key(pattern), if objects.is_empty() { "unknown".into() } else { objects.join("+") });
        let subject_name = objects.first().cloned().unwrap_or_else(|| snapshot.server.database.clone());
        let kind = if objects.is_empty() { SubjectKind::Database } else { SubjectKind::Table };
        let mut f = finding("deadlocks", &key, Category::Queries, subject(kind, subject_name));
        f.severity = if n >= 10 {
            Severity::High
        } else if n >= 3 {
            Severity::Medium
        } else {
            Severity::Low
        };
        f.confidence = Confidence::High;
        f.title = format!("{who} deadlocked {times} in {window} ({} on {})", pattern.label(), tables(&objects));
        f.summary = format!("{} {}", latest.headline, latest.explanation);
        f.impact.score = base_score(f.severity) + (n as f64).min(20.0);
        f.impact.label = if n == 1 { "1 deadlock".into() } else { format!("{n} deadlocks") };
        f.metrics = vec![
            Metric { label: "Deadlocks".into(), value: n as f64, unit: Unit::Count, text: None },
            Metric { label: "Pattern".into(), value: 0.0, unit: Unit::Text, text: Some(pattern.label().into()) },
        ];
        if let Some(victim) = latest.victim() {
            let who = victim.application.clone().or(victim.login.clone()).unwrap_or_else(|| "the application".into());
            f.metrics.push(Metric { label: "Rolled back".into(), value: 0.0, unit: Unit::Text, text: Some(who) });
        }
        // Lead with the primary fix; when it has no SQL, offer the first alternative that does.
        if let Some(primary) = latest.fixes.first() {
            let mut summary = format!("{}: {}", primary.title, primary.detail);
            let mut sql = primary.sql.clone();
            if sql.is_none()
                && let Some(alt) = latest.fixes.iter().skip(1).find(|x| x.sql.is_some())
            {
                summary.push_str(&format!(" Or {}: {}", lowercase_first(&alt.title), alt.detail));
                sql = alt.sql.clone();
            }
            f.fix = Some(Fix { summary, sql, cost: None });
        }
        f.verify = vec![
            format!("Re-scan tomorrow: no new {} deadlocks on {}.", pattern.label(), tables(&objects)),
            "Live › Deadlocks shows the graph of each one, with both statements.".into(),
        ];
        f.last_seen = latest.at.clone();
        if let Some(note) = report.note.as_ref().filter(|n| n.contains("only keeps") || n.contains("most recent")) {
            f.caveats.push(note.clone());
        }
        out.push(f);
    }

    if report.deadlocks.is_empty()
        && let Some(counted) = report.counted.filter(|c| *c > 0.0)
    {
        let since =
            report.counted_since.as_deref().map(|s| format!(" since {}", &s[..s.len().min(10)])).unwrap_or_default();
        let mut f = finding(
            "deadlocks",
            "counter",
            Category::Queries,
            subject(SubjectKind::Database, snapshot.server.database.clone()),
        );
        f.severity = if counted >= 100.0 { Severity::Medium } else { Severity::Low };
        f.confidence = Confidence::Medium;
        f.title = format!("Postgres resolved {counted:.0} deadlock{}{since}", if counted == 1.0 { "" } else { "s" });
        f.summary = report.note.clone().unwrap_or_default();
        f.impact.score = base_score(f.severity);
        f.impact.label = format!("{counted:.0} deadlocks");
        f.fix = Some(Fix {
            summary: "Find \"deadlock detected\" in the server log and paste those lines into Live › Deadlocks: Culprit names \
                      the pattern. Most are fixed by changing rows in the same order everywhere and keeping transactions short."
                .into(),
            sql: None,
            cost: None,
        });
        f.verify = vec!["Re-scan later: the counter stops growing.".into()];
        out.push(f);
    }
    out
}

fn lowercase_first(text: &str) -> String {
    let mut chars = text.chars();
    chars.next().map(|c| c.to_lowercase().chain(chars).collect()).unwrap_or_default()
}

fn serde_key(pattern: DeadlockPattern) -> &'static str {
    match pattern {
        DeadlockPattern::KeyLookup => "key-lookup",
        DeadlockPattern::LockConversion => "lock-conversion",
        DeadlockPattern::OppositeOrder => "opposite-order",
        DeadlockPattern::LockEscalation => "lock-escalation",
        DeadlockPattern::Parallelism => "parallelism",
        DeadlockPattern::Cycle => "cycle",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use culprit_core::deadlock::classify;
    use culprit_core::{DeadlockProcess, DeadlockReport, DeadlockResource, LockHolder};

    fn deadlock(at: &str) -> Deadlock {
        let process = |id: &str, victim| DeadlockProcess {
            id: id.into(),
            session: Some(id.into()),
            victim,
            statement: Some(format!("UPDATE x {id}")),
            application: Some("orders-api".into()),
            database: Some("culprit_demo".into()),
            ..Default::default()
        };
        let resource = |object: &str, index: &str, holder: &str, waiter: &str| DeadlockResource {
            id: format!("{object}{index}"),
            kind: "keylock".into(),
            object: Some(object.into()),
            index: Some(index.into()),
            holders: vec![LockHolder { process: holder.into(), mode: Some("X".into()) }],
            waiters: vec![LockHolder { process: waiter.into(), mode: Some("S".into()) }],
        };
        let mut d = Deadlock::new(
            "system_health",
            Some(at.into()),
            vec![process("61", true), process("64", false)],
            vec![
                resource("shop.customers", "ix_customers_email", "64", "61"),
                resource("shop.customers", "PK_customers", "61", "64"),
            ],
        );
        classify(Engine::Mssql, &mut d);
        d
    }

    #[test]
    fn repeated_deadlocks_become_one_finding_per_pattern() {
        let mut snapshot: Snapshot = serde_json::from_str(include_str!("../../../fixtures/mssql-demo.json")).unwrap();
        snapshot.deadlocks = Some(DeadlockReport {
            deadlocks: (0..6).map(|i| deadlock(&format!("2026-10-11T1{i}:00:00Z"))).collect(),
            source: "system_health event files".into(),
            since_hours: 24.0,
            ..Default::default()
        });
        let found = deadlocks(&snapshot);
        assert_eq!(found.len(), 1);
        let f = &found[0];
        assert_eq!(f.id, "deadlocks:key-lookup:shop.customers");
        assert_eq!(f.title, "Two queries deadlocked 6 times in the last day (key-lookup deadlock on shop.customers)");
        assert_eq!(f.severity, Severity::Medium);
        assert_eq!(f.last_seen.as_deref(), Some("2026-10-11T15:00:00Z"));
        let fix = f.fix.as_ref().unwrap();
        assert!(fix.summary.starts_with("Make the read skip the lookup"));
        assert!(fix.summary.contains("Or let reads stop taking shared locks"));
        assert!(
            fix.sql
                .as_deref()
                .is_some_and(|s| s.contains("ALTER DATABASE [culprit_demo] SET READ_COMMITTED_SNAPSHOT ON"))
        );
    }

    #[test]
    fn postgres_counter_without_graphs() {
        let mut snapshot: Snapshot =
            serde_json::from_str(include_str!("../../../fixtures/postgres-demo.json")).unwrap();
        snapshot.deadlocks = Some(DeadlockReport {
            counted: Some(3.0),
            counted_since: Some("2026-10-01T00:00:00Z".into()),
            since_hours: 24.0,
            ..Default::default()
        });
        let found = deadlocks(&snapshot);
        assert_eq!(found[0].title, "Postgres resolved 3 deadlocks since 2026-10-01");
        snapshot.deadlocks = Some(DeadlockReport { counted: Some(0.0), ..Default::default() });
        assert!(deadlocks(&snapshot).is_empty());
    }
}
