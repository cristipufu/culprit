//! SQL Server untrusted/disabled constraints and PostgreSQL/YSQL deferred validation. Native YSQL findings describe
//! unvalidated data without assuming PostgreSQL planner or lock behavior.

use super::*;
use crate::evidence::Requirement as Evidence;

pub fn untrusted_constraint_info() -> CheckInfo {
    CheckInfo {
        id: "untrusted-constraint".into(),
        title: "Unvalidated or disabled constraints".into(),
        category: Category::Maintenance,
        engines: vec![Engine::Mssql, Engine::Postgres, Engine::Yugabyte],
        requires: vec![],
        description: "SQL Server foreign keys and check constraints added or re-enabled WITH NOCHECK (or switched off), PostgreSQL CHECK constraints added NOT VALID, and YugabyteDB CHECK or foreign-key constraints awaiting validation.".into(),
    }
}

check!(
    UntrustedConstraintCheck: untrusted_constraint_info,
    untrusted_constraint,
    &[Evidence::Source(culprit_core::source_id::CONSTRAINTS)]
);

/// Whether PostgreSQL's planner reads these CHECK constraints at all: constraint exclusion looks at inheritance
/// children only (constraint_exclusion = partition, the default), or at every table with constraint_exclusion = on.
pub(super) fn planner_reads_checks(snapshot: &Snapshot, list: &[&ConstraintIssue]) -> bool {
    list.iter().any(|c| c.inheritance_child)
        || snapshot.setting("constraint_exclusion").is_some_and(|s| crate::config::is_on(&s.value))
}

pub fn untrusted_constraint(snapshot: &Snapshot) -> Vec<Finding> {
    let mssql = snapshot.server.engine == Engine::Mssql;
    let ysql = snapshot.server.engine == Engine::Yugabyte;
    let engine = snapshot.server.engine;
    let mut by_table: BTreeMap<(String, String), Vec<&ConstraintIssue>> = BTreeMap::new();
    for c in &snapshot.constraints {
        by_table.entry((c.schema.clone(), c.table.clone())).or_default().push(c);
    }
    by_table
        .into_iter()
        .map(|((schema, table), list)| {
            let name = format!("{schema}.{table}");
            let mut f = finding("untrusted-constraint", &name, Category::Maintenance, subject(SubjectKind::Table, &name));
            let disabled = list.iter().filter(|c| c.disabled).count();
            let kind = |c: &ConstraintIssue| match c.kind {
                ConstraintKind::ForeignKey => "foreign key",
                ConstraintKind::Check => "check constraint",
            };
            f.title = match (list.as_slice(), mssql) {
                ([one], _) if one.disabled => format!("The {} {} on {table} is switched off", kind(one), one.name),
                ([one], true) => format!("SQL Server doesn't trust the {} {} on {table}", kind(one), one.name),
                ([one], false) if ysql => format!("The {} {} on {table} was never validated", kind(one), one.name),
                ([one], false) => format!("The check constraint {} on {table} was never validated", one.name),
                (_, true) => format!("SQL Server doesn't trust {} constraints on {table}", list.len()),
                (_, false) if ysql => format!("{} constraints on {table} were never validated", list.len()),
                (_, false) => format!("{} check constraints on {table} were never validated", list.len()),
            };
            let names = format::list(&list.iter().map(|c| c.name.clone()).collect::<Vec<_>>());
            f.summary = if mssql {
                format!(
                    "{names} {} created or re-enabled WITH NOCHECK, so existing rows were never checked against {}. The optimizer only relies on trusted constraints: it can't remove a join to the referenced table when no columns from it are used, or rule out values a check constraint excludes, so it plans work it could skip.{}",
                    if list.len() == 1 { "was" } else { "were" },
                    if list.len() == 1 { "it" } else { "them" },
                    if disabled > 0 { " A switched-off constraint doesn't check new rows either, so the data may already break it." } else { "" }
                )
            } else if ysql {
                format!(
                    "{names} {} added NOT VALID. YugabyteDB checks new and changed rows but has not checked the rows that existed when these constraints were added. Some old rows may violate them. Deferred validation can be deliberate during a migration; this catalog state does not establish a query-plan regression.",
                    if list.len() == 1 { "was" } else { "were" }
                )
            } else if planner_reads_checks(snapshot, &list) {
                format!(
                    "{names} {} added NOT VALID, so rows that existed then were never checked. Until it is validated the planner can't use it to skip the table when a query's filter contradicts it (constraint exclusion{}).",
                    if list.len() == 1 { "was" } else { "were" },
                    if list.iter().any(|c| c.inheritance_child) { ", which it applies to this inheritance child" } else { ", which constraint_exclusion = on applies to every table" }
                )
            } else {
                // The planner reads CHECK constraints only on inheritance children (constraint_exclusion = partition,
                // the default): here a NOT VALID one is about the data, and often deliberate.
                format!(
                    "{names} {} added NOT VALID: PostgreSQL checks new and changed rows against {} but never checked the rows that existed then, so some may break {}. That is often deliberate: NOT VALID adds a constraint without a long scan under lock. The planner doesn't read it here either way (constraint exclusion looks at CHECK constraints only on inheritance children unless constraint_exclusion is on). Validating proves the old rows, and lets later steps skip their own full scan: SET NOT NULL (12+) and ATTACH PARTITION when the constraint implies them.",
                    if list.len() == 1 { "was" } else { "were" },
                    if list.len() == 1 { "it" } else { "them" },
                    if list.len() == 1 { "it" } else { "them" }
                )
            };
            f.severity = if disabled > 0 { Severity::Medium } else { Severity::Low };
            severity_score(&mut f);
            f.impact.label = format!("{} {}", list.len(), if ysql { "not validated" } else { "untrusted" });
            f.confidence = Confidence::High;
            f.metrics = vec![Metric::new("Constraints", list.len() as f64, Unit::Count)];
            let target = qualified(engine, &schema, &table);
            f.fix = Some(if mssql {
                Fix {
                    summary: format!(
                        "Check the existing rows so SQL Server trusts {} again. It reads the whole table and locks it while it runs: schedule it off-peak. It fails if a row breaks the constraint; fix those rows first.",
                        if list.len() == 1 { "it" } else { "them" }
                    ),
                    sql: Some(
                        list.iter()
                            .map(|c| format!("ALTER TABLE {target} WITH CHECK CHECK CONSTRAINT {};", quote(engine, &c.name)))
                            .collect::<Vec<_>>()
                            .join("\n"),
                    ),
                    cost: Some("Reads the whole table".into()),
                }
            } else {
                Fix {
                    summary: if ysql {
                        "Validate existing rows in a planned maintenance window after checking this YugabyteDB version's validation support. Validation can issue distributed scans and acquire locks. It fails if a row breaks a constraint; fix those rows first. Do not assume PostgreSQL lock or concurrent-write guarantees.".into()
                    } else {
                        "Validate it: PostgreSQL scans the table but takes only a SHARE UPDATE EXCLUSIVE lock, so reads and writes continue. It fails if a row breaks the constraint; fix those rows first.".into()
                    },
                    sql: Some(
                        list.iter()
                            .map(|c| format!("ALTER TABLE {target} VALIDATE CONSTRAINT {};", quote(engine, &c.name)))
                            .collect::<Vec<_>>()
                            .join("\n"),
                    ),
                    cost: Some(if ysql { "Distributed table scans and validation locks" } else { "Reads the whole table" }.into()),
                }
            });
            f.verify = vec!["Re-scan: the constraint should no longer be listed.".into()];
            f
        })
        .collect()
}
