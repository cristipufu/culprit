//! SQLite's own findings, from what the file records about itself (`crates/culprit-sqlite`): whether the planner has
//! statistics, and how much of the file is free pages. What only the application knows isn't judged: whether several
//! processes share the file decides if the rollback journal hurts, so the journal mode is shown as a setting, never as
//! a finding.

use culprit_core::{
    Capability, Category, Confidence, Finding, Fix, Metric, Severity, Snapshot, SubjectKind, Unit, format,
};

use crate::support::{base_score, finding, subject};

/// Free pages worth a finding: this many bytes, and this share of the file.
const FREE_BYTES_AT_LEAST: f64 = 16.0 * 1024.0 * 1024.0;
const FREE_SHARE_AT_LEAST: f64 = 0.25;

/// The file's findings, under the `config` check.
pub fn settings(snapshot: &Snapshot) -> Vec<Finding> {
    [statistics(snapshot), free_space(snapshot)].into_iter().flatten().collect()
}

/// No planner statistics while the planner has indexes to choose between.
fn statistics(s: &Snapshot) -> Option<Finding> {
    if s.server.has(Capability::Statistics) {
        return None;
    }
    let secondary = s.indexes.iter().filter(|i| !i.is_primary_key).count();
    if secondary == 0 {
        return None;
    }
    // On tables this small (rows counted), a wrong guess costs nothing worth a finding.
    let rows_known = !s.tables.is_empty() && s.tables.iter().all(|t| t.rows.is_some());
    if rows_known && s.tables.iter().filter_map(|t| t.rows).fold(0.0, f64::max) < 10_000.0 {
        return None;
    }
    let mut f = finding(
        "config",
        "sqlite-statistics",
        Category::Maintenance,
        subject(SubjectKind::Database, &s.server.database),
    );
    f.title = "SQLite has no statistics to choose between this file's indexes".into();
    f.summary = format!(
        "The file has never been analyzed (it has no sqlite_stat1), so SQLite falls back on fixed guesses about how big each table is and how well each index narrows a search. With {secondary} secondary {} to choose from, a guess can pick the wrong one, or a full scan.{}",
        if secondary == 1 { "index" } else { "indexes" },
        if rows_known { "" } else { " Culprit can't tell each table's size either." }
    );
    f.severity = Severity::Low;
    f.impact.score = base_score(Severity::Low);
    f.impact.label = "No statistics".into();
    f.confidence = Confidence::High;
    f.metrics.push(Metric::new("Secondary indexes", secondary as f64, Unit::Count));
    f.fix = Some(Fix {
        summary: "Run ANALYZE once, then PRAGMA optimize from time to time (when a connection closes is the usual place): it re-analyzes only what changed.".into(),
        sql: Some("ANALYZE;".into()),
        cost: Some("ANALYZE reads every index once and holds the write lock while it stores the results: on a large file, run it when writes are quiet.".into()),
    });
    f.verify = vec!["Scan again: the file has sqlite_stat1, and this finding is gone.".into()];
    Some(f)
}

/// Pages that deleted rows left behind, when they are many and a large part of the file.
fn free_space(s: &Snapshot) -> Option<Finding> {
    let number =
        |name: &str| s.settings.iter().find(|x| x.name == name).and_then(|x| x.value.trim().parse::<f64>().ok());
    let (free, pages, page_size) = (number("freelist_count")?, number("page_count")?, number("page_size")?);
    let free_bytes = free * page_size;
    let share = free / pages.max(1.0);
    if free_bytes < FREE_BYTES_AT_LEAST || share < FREE_SHARE_AT_LEAST {
        return None;
    }
    // auto_vacuum: 0 none, 1 full, 2 incremental.
    let incremental = number("auto_vacuum") == Some(2.0);
    let mut f = finding(
        "config",
        "sqlite-free-pages",
        Category::Maintenance,
        subject(SubjectKind::Database, &s.server.database),
    );
    f.title = format!("{} of the file is free pages ({} of it)", format::bytes(free_bytes), format::percent(share));
    f.summary = "Deleted rows leave their pages in the file for SQLite to reuse; the file doesn't shrink on its own. That's fine when new rows will fill them again. If not, the file stays larger than its data, which every backup and copy pays for.".into();
    f.severity = Severity::Low;
    f.impact.score = base_score(Severity::Low);
    f.impact.label = format!("{} free", format::bytes(free_bytes));
    f.confidence = Confidence::High;
    f.metrics.push(Metric::new("Free pages", free_bytes, Unit::Bytes));
    f.metrics.push(Metric::new("Share of the file", share, Unit::Ratio));
    f.fix = Some(if incremental {
        Fix {
            summary: "The file already vacuums incrementally: release the free pages now.".into(),
            sql: Some("PRAGMA incremental_vacuum;".into()),
            cost: None,
        }
    } else {
        Fix {
            summary: "VACUUM rewrites the file without its free pages. To give space back after large deletes from then on, switch auto_vacuum to INCREMENTAL first (it takes effect with that VACUUM) and run PRAGMA incremental_vacuum when convenient.".into(),
            sql: Some("PRAGMA auto_vacuum = INCREMENTAL;\nVACUUM;".into()),
            cost: Some("VACUUM holds the write lock for its whole run and can need up to twice the original file's size in free disk space.".into()),
        }
    });
    f.verify = vec!["Scan again: free pages are a small part of the file.".into()];
    Some(f)
}
