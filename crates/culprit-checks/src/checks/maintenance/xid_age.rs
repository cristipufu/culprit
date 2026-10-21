//! The `xid-age` check: Transaction ID age approaching the point where PostgreSQL stops accepting writes to protect data.

use super::*;
use crate::evidence::Requirement as Evidence;

pub fn xid_age_info() -> CheckInfo {
    CheckInfo {
        id: "xid-age".into(),
        title: "Transaction ID wraparound risk".into(),
        category: Category::Maintenance,
        engines: vec![Engine::Postgres],
        requires: vec![Capability::TableStats],
        description:
            "Transaction ID age approaching the point where PostgreSQL stops accepting writes to protect data.".into(),
    }
}

check!(XidAgeCheck: xid_age_info, xid_age, &[Evidence::Tables(crate::evidence::TableMeasure::Xid)]);

pub(super) const WRAPAROUND: f64 = 2_000_000_000.0;

pub fn xid_age(snapshot: &Snapshot) -> Vec<Finding> {
    if snapshot.server.engine != Engine::Postgres {
        return Vec::new();
    }
    let db_age = snapshot.setting("datfrozenxid_age").and_then(|s| s.value.parse::<f64>().ok());
    let oldest = snapshot
        .tables
        .iter()
        .filter(|t| t.xid_age.is_some())
        .max_by(|a, b| a.xid_age.unwrap_or(0.0).total_cmp(&b.xid_age.unwrap_or(0.0)));
    let age = db_age.or(oldest.and_then(|t| t.xid_age)).unwrap_or(0.0);
    // Autovacuum starts freezing a table on its own once its age passes autovacuum_freeze_max_age (200M by default,
    // and often raised on big databases): an age a little past it is freezing in progress. Twice it, or 500M when the
    // setting is unknown or low, means freezing is falling behind; never later than 1.2B, with 2B the hard stop.
    let freeze_max = snapshot.setting("autovacuum_freeze_max_age").and_then(|s| s.value.trim().parse::<f64>().ok());
    let threshold = freeze_max.map_or(500_000_000.0, |m| (m * 2.0).max(500_000_000.0)).min(1_200_000_000.0);
    if age < threshold {
        return Vec::new();
    }
    let remaining = (WRAPAROUND - age).max(0.0);
    let mut f = finding(
        "xid-age",
        &snapshot.server.database,
        Category::Maintenance,
        subject(SubjectKind::Database, &snapshot.server.database),
    );
    f.title = format!("Transaction IDs are {} of the way to wraparound", format::percent(age / WRAPAROUND));
    f.summary = format!(
        "The oldest unfrozen transaction ID in this database is {} transactions old.{} At about 2 billion PostgreSQL stops accepting writes until a full vacuum completes. {} transactions remain.",
        format::count(age),
        freeze_max
            .filter(|m| age >= *m)
            .map(|m| format!(
                " Autovacuum starts freezing a table at autovacuum_freeze_max_age ({}), and this is {:.1} times that: freezing isn't keeping up.",
                format::count(m),
                age / m.max(1.0)
            ))
            .unwrap_or_default(),
        format::count(remaining)
    );
    f.severity = if age >= 1_200_000_000.0 { Severity::High } else { Severity::Medium };
    f.impact.score = if age >= 1_500_000_000.0 {
        95.0
    } else if age >= 1_200_000_000.0 {
        70.0
    } else {
        35.0
    };
    f.impact.label = format!("{} left", format::count(remaining));
    f.confidence = Confidence::High;
    f.metrics.push(Metric::new("Transaction ID age", age, Unit::Count));
    f.metrics.push(Metric::new("Remaining before wraparound", remaining, Unit::Count));
    if let Some(t) = oldest {
        f.metrics.push(Metric::text("Oldest table", t.qualified()));
    }
    f.fix = Some(Fix {
        summary: "Freeze the oldest tables first, and look for what holds vacuum back: long-running or idle-in-transaction sessions, abandoned replication slots, or prepared transactions.".into(),
        sql: Some(match oldest {
            Some(t) => format!("VACUUM (FREEZE, VERBOSE) {};", qualified(Engine::Postgres, &t.schema, &t.name)),
            None => "VACUUM (FREEZE, VERBOSE);".into(),
        }),
        cost: Some("A freeze vacuum reads the whole table; it doesn't block reads or writes.".into()),
    });
    vec![f]
}
