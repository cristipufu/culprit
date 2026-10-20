//! Deadlocks that keep happening: grouped by pattern and tables, explained, with the fix that removes them.

use crate::evidence::Requirement as Evidence;
use std::collections::BTreeMap;

use culprit_core::format::friendly_date;
use culprit_core::{
    Category, CheckInfo, Confidence, Deadlock, DeadlockPattern, Engine, Finding, Fix, Metric, Severity, Snapshot,
    SubjectKind, Unit,
};

use culprit_core::Check;

use crate::support::*;

/// The checks this area registers (`crate::all()` runs every area's), in the order their findings tie.
pub(crate) const CHECKS: &[&dyn Check] = &[&DeadlocksCheck];

pub fn deadlocks_info() -> CheckInfo {
    CheckInfo {
        id: "deadlocks".into(),
        title: "Deadlocks".into(),
        category: Category::Queries,
        engines: vec![Engine::Mssql, Engine::Postgres, Engine::Mysql],
        requires: vec![],
        description: "Deadlocks in the last day, grouped by pattern (key lookup, opposite order, read-then-update, lock \
                      escalation, parallelism) with the fix that removes them. SQL Server reads system_health (Azure \
                      SQL Database its deadlock telemetry), MySQL the latest InnoDB report, PostgreSQL only keeps a counter."
            .into(),
    }
}

check!(DeadlocksCheck: deadlocks_info, deadlocks, &[Evidence::Deadlocks]);

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

/// Whether a deadlock is this database's. SQL Server's system_health and InnoDB's report are the whole server's: a
/// deadlock whose processes ran in other databases (SQL Server), or whose tables are all in other schemas (MySQL, where
/// a schema is a database), belongs to them. One that names neither can't be placed, and is kept.
pub(crate) fn in_this_database(d: &Deadlock, snapshot: &Snapshot) -> bool {
    let database = &snapshot.server.database;
    let mut named = d.processes.iter().filter_map(|p| p.database.as_deref()).peekable();
    if named.peek().is_some() {
        return named.any(|db| db.eq_ignore_ascii_case(database));
    }
    if snapshot.server.engine == Engine::Mysql {
        let mut schemas = d.objects.iter().filter_map(|o| o.split_once('.').map(|(s, _)| s)).peekable();
        if schemas.peek().is_some() {
            return schemas.any(|s| s.eq_ignore_ascii_case(database));
        }
    }
    true
}

/// How serious `n` dated deadlocks are over the `hours` the report covers: by how often, not how many. Ten in a day is
/// High, as is one a day for ten days or more; a few in a month is Medium, one or two Low.
pub(crate) fn deadlock_severity(n: usize, hours: f64) -> Severity {
    let per_day = n as f64 / (hours / 24.0).max(1.0);
    if n >= 10 && per_day >= 1.0 {
        Severity::High
    } else if n >= 3 && per_day >= 0.1 {
        Severity::Medium
    } else {
        Severity::Low
    }
}

pub fn deadlocks(snapshot: &Snapshot) -> Vec<Finding> {
    let Some(report) = snapshot.deadlocks.as_ref() else { return Vec::new() };
    let mut out = Vec::new();
    let window = if (report.since_hours - 24.0).abs() < 0.01 {
        "the last day".into()
    } else {
        format!("the last {}", culprit_core::span_words(report.since_hours))
    };

    let mut groups: BTreeMap<(DeadlockPattern, Vec<String>), Vec<&Deadlock>> = BTreeMap::new();
    for d in report.deadlocks.iter().filter(|d| in_this_database(d, snapshot)) {
        groups.entry((d.pattern, d.objects.clone())).or_default().push(d);
    }
    for ((pattern, objects), list) in groups {
        // Only a dated deadlock counts toward "in the last day": InnoDB keeps its latest one however old it is, and
        // one whose time couldn't be read may be from long ago.
        let n = list.iter().filter(|d| d.at.is_some()).count();
        let undated = list.len() - n;
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
        f.severity = deadlock_severity(n, report.since_hours);
        f.confidence = if n > 0 { Confidence::High } else { Confidence::Medium };
        // InnoDB keeps only its most recent deadlock: one in its report says the pattern happened at least once, and
        // when it last did, never how many times.
        let latest_only = snapshot.server.engine == Engine::Mysql;
        f.title = if n > 0 && latest_only {
            format!(
                "{who} deadlocked at least once, most recently {} ({} on {})",
                latest.at.as_deref().map(crate::queries::when_phrase).unwrap_or_default(),
                pattern.label(),
                tables(&objects)
            )
        } else if n > 0 {
            format!(
                "{who} deadlocked {times} in {window}{} ({} on {})",
                if undated > 0 { format!(", and {undated} more at an unknown time") } else { String::new() },
                pattern.label(),
                tables(&objects)
            )
        } else {
            format!(
                "{who} deadlocked at an unknown time ({} on {}): the server's latest deadlock report doesn't say when",
                pattern.label(),
                tables(&objects)
            )
        };
        f.summary = format!("{} {}", latest.headline, latest.explanation);
        let all = list.len();
        f.impact.score = base_score(f.severity) + (n as f64).min(20.0);
        // What InnoDB keeps is the latest one: a floor, as the title says ("at least once"), never a count.
        let at_least = if latest_only { "at least " } else { "" };
        f.impact.label = if all == 1 { format!("{at_least}1 deadlock") } else { format!("{at_least}{all} deadlocks") };
        if n == 0 {
            f.caveats.push("When it happened is unknown, so it may be from long ago: InnoDB keeps its most recent deadlock until the next one, however old. Check the error log (innodb_print_all_deadlocks) before acting on it.".into());
        }
        f.metrics = vec![
            Metric {
                label: if latest_only { "Deadlocks (at least)" } else { "Deadlocks" }.into(),
                value: all as f64,
                unit: Unit::Count,
                text: None,
            },
            Metric { label: "Pattern".into(), value: 0.0, unit: Unit::Text, text: Some(pattern.label().into()) },
        ];
        if let Some(who) = latest.victim().and_then(victim_text) {
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
            // "key-lookup deadlock" is a deadlock already: not "key-lookup deadlock deadlocks".
            format!(
                "Re-scan tomorrow: no new {} on {}.",
                match pattern.label() {
                    label if label.ends_with("deadlock") => format!("{label}s"),
                    label => format!("{label} deadlocks"),
                },
                tables(&objects)
            ),
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
        // The counter runs from the statistics' last reset. With no reset on record it runs from an unknown time
        // (statistics survive a clean restart), so it is a total, not a rate.
        let since = match report.counted_since.as_deref() {
            Some(s) => format!(" since {}", friendly_date(s, &snapshot.taken_at)),
            None => " since its statistics began (no reset on record, so it doesn't say when)".into(),
        };
        let per_day = report
            .counted_since
            .as_deref()
            .and_then(|s| days_between(s, &snapshot.taken_at))
            .filter(|d| d * 24.0 >= culprit_core::RATE_MIN_HOURS)
            .map(|d| counted / d);
        let mut f = finding(
            "deadlocks",
            "counter",
            Category::Queries,
            subject(SubjectKind::Database, snapshot.server.database.clone()),
        );
        // A count says how many, not when: Medium only for a real rate over a known span (ten a day).
        f.severity = if per_day.is_some_and(|d| d >= 10.0) { Severity::Medium } else { Severity::Low };
        f.confidence = Confidence::Medium;
        f.title = format!("PostgreSQL resolved {counted:.0} deadlock{}{since}", if counted == 1.0 { "" } else { "s" });
        f.summary = report.note.clone().unwrap_or_default();
        f.impact.score = base_score(f.severity);
        f.impact.label = if counted == 1.0 { "1 deadlock".into() } else { format!("{counted:.0} deadlocks") };
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

/// The tables a deadlock finding is about, read back from the id [`deadlocks`] gives it (`deadlocks:<pattern>:<table>+
/// <table>`): every one, where the subject names only the first. Empty for any other finding, and for a deadlock
/// whose tables weren't named.
pub(crate) fn deadlock_tables(f: &Finding) -> Vec<String> {
    let Some(rest) = f.id.strip_prefix("deadlocks:").filter(|_| f.check == "deadlocks") else { return Vec::new() };
    match rest.split_once(':') {
        Some((_, objects)) if objects != "unknown" => objects.split('+').map(str::to_string).collect(),
        _ => Vec::new(),
    }
}

/// Whose transaction was rolled back, said as what each name is: "session 202 (login root)", "session 55
/// (orders-api)". A login or an application alone is a session of it, never a bare name a reader takes for a table or
/// a person. `None` when the report names nothing.
pub(crate) fn victim_text(victim: &culprit_core::DeadlockProcess) -> Option<String> {
    let session = victim.session.as_deref().map(|s| format!("session {s}"));
    let by = victim.application.clone().or_else(|| victim.login.as_deref().map(|l| format!("login {l}")));
    match (session, by) {
        (Some(session), Some(by)) => Some(format!("{session} ({by})")),
        (Some(session), None) => Some(session),
        (None, Some(by)) => Some(format!("a session of {by}")),
        (None, None) => None,
    }
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
        let mut snapshot: Snapshot =
            serde_json::from_str(include_str!("../../../../fixtures/mssql-demo.json")).unwrap();
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

    // C1: system_health and InnoDB's report are the server's; another database's deadlocks aren't this one's.
    #[test]
    fn other_databases_deadlocks_are_left_out() {
        let mut snapshot: Snapshot =
            serde_json::from_str(include_str!("../../../../fixtures/mssql-demo.json")).unwrap();
        let mut elsewhere = deadlock("2026-10-11T11:00:00Z");
        for p in &mut elsewhere.processes {
            p.database = Some("billing".into());
        }
        snapshot.deadlocks = Some(DeadlockReport {
            deadlocks: vec![elsewhere.clone(), elsewhere.clone(), elsewhere],
            source: "system_health event files".into(),
            since_hours: 24.0,
            ..Default::default()
        });
        assert!(deadlocks(&snapshot).is_empty());

        // MySQL names no database per process; the tables' schema is the database.
        let mut mysql: Snapshot = serde_json::from_str(include_str!("../../../../fixtures/mysql-demo.json")).unwrap();
        let mut other = deadlock("2026-10-11T11:00:00Z");
        for p in &mut other.processes {
            p.database = None;
        }
        other.objects = vec!["billing.invoices".into()];
        let mut ours = other.clone();
        ours.objects = vec![format!("{}.orders", mysql.server.database)];
        mysql.deadlocks = Some(DeadlockReport {
            deadlocks: vec![other, ours],
            source: "InnoDB status".into(),
            since_hours: 24.0,
            ..Default::default()
        });
        let found = deadlocks(&mysql);
        assert_eq!(found.len(), 1);
        assert!(found[0].title.contains(".orders"), "{}", found[0].title);
        // The digest counts the same deadlocks as the check.
        let digest = crate::digest(&crate::analyze(mysql), None, 5, 5);
        assert_eq!(digest.deadlocks.as_ref().and_then(|d| d.count), Some(1));
    }

    // C1: InnoDB keeps its latest deadlock however old; one whose time is unknown claims no window and is Low.
    #[test]
    fn an_undated_deadlock_claims_no_window() {
        let mut snapshot: Snapshot =
            serde_json::from_str(include_str!("../../../../fixtures/mssql-demo.json")).unwrap();
        let mut undated = deadlock("2026-10-11T11:00:00Z");
        undated.at = None;
        snapshot.deadlocks = Some(DeadlockReport {
            deadlocks: vec![undated],
            source: "InnoDB status".into(),
            since_hours: 24.0,
            ..Default::default()
        });
        let f = &deadlocks(&snapshot)[0];
        assert!(!f.title.contains("the last day"), "{}", f.title);
        assert!(f.title.contains("at an unknown time"), "{}", f.title);
        assert_eq!(f.severity, Severity::Low);
        assert!(f.caveats.iter().any(|c| c.contains("may be from long ago")));
    }

    // C14: with no statistics reset on record, PostgreSQL's counter has no window: a total, Low.
    #[test]
    fn a_postgres_counter_without_a_reset_has_no_window() {
        let mut snapshot: Snapshot =
            serde_json::from_str(include_str!("../../../../fixtures/postgres-demo.json")).unwrap();
        snapshot.deadlocks = Some(DeadlockReport { counted: Some(400.0), since_hours: 24.0, ..Default::default() });
        let f = &deadlocks(&snapshot)[0];
        assert!(f.title.contains("since its statistics began"), "{}", f.title);
        assert_eq!(f.severity, Severity::Low);
    }

    #[test]
    fn postgres_counter_without_graphs() {
        let mut snapshot: Snapshot =
            serde_json::from_str(include_str!("../../../../fixtures/postgres-demo.json")).unwrap();
        snapshot.deadlocks = Some(DeadlockReport {
            counted: Some(3.0),
            counted_since: Some("2026-10-01T00:00:00Z".into()),
            since_hours: 24.0,
            ..Default::default()
        });
        let found = deadlocks(&snapshot);
        assert_eq!(found[0].title, "PostgreSQL resolved 3 deadlocks since Oct 1");
        snapshot.deadlocks = Some(DeadlockReport { counted: Some(0.0), ..Default::default() });
        assert!(deadlocks(&snapshot).is_empty());
    }
}
