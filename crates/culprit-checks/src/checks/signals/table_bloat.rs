//! The `table-bloat` check: Tables whose pages hold far fewer rows than they could, estimated from row counts and column widths in pg_stats (no extension needed). Every scan still reads the empty space.

use super::*;
use crate::evidence::Requirement as Evidence;

pub fn table_bloat_info() -> CheckInfo {
    CheckInfo {
        id: "table-bloat".into(),
        title: "Tables that are mostly empty space".into(),
        category: Category::Maintenance,
        engines: vec![Engine::Postgres],
        requires: vec![],
        description: "Tables whose pages hold far fewer rows than they could, estimated from row counts and column widths in pg_stats (no extension needed). Every scan still reads the empty space.".into(),
    }
}

check!(TableBloatCheck: table_bloat_info, table_bloat, &[Evidence::Tables(crate::evidence::TableMeasure::Bloat)]);

pub(super) fn table_heap_bytes(t: &TableStat) -> Option<f64> {
    t.data_bytes.or(t.total_bytes).filter(|b| *b > 0.0)
}

/// Dead rows explain the estimated empty space when their share of the row versions is at least this part of its
/// share: the estimate is good to tens of percent.
pub(crate) const DEAD_EXPLAINS_BLOAT: f64 = 0.9;

pub fn table_bloat(snapshot: &Snapshot) -> Vec<Finding> {
    let mut findings: Vec<Finding> = snapshot
        .tables
        .iter()
        .filter_map(|t| {
            let bloat = t.storage.as_ref()?.bloat_bytes?;
            let size = table_heap_bytes(t)?;
            let share = (bloat / size).min(1.0);
            ((bloat >= 100.0 * MB && share >= 0.4) || (bloat >= GB && share >= 0.3)).then_some((t, bloat, size, share))
        })
        .map(|(t, bloat, size, share)| {
            let name = t.qualified();
            let mut f = finding("table-bloat", &name, Category::Maintenance, subject(SubjectKind::Table, &name));
            f.title = format!("{} is about {} empty space (≈ {})", t.name, format::percent(share), format::bytes(bloat));
            f.facts.bloat = Some(culprit_core::FindingBloat {bytes:bloat,share});
            // The estimate counts every byte beyond the live rows, dead rows VACUUM hasn't cleared yet included. When
            // they account for it, VACUUM is the fix, not a rewrite.
            let dead = dead_share(t).filter(|d| *d >= share * DEAD_EXPLAINS_BLOAT);
            f.summary = format!(
                "From its row count and average row width, {}'s rows need about {}; it takes {}. New rows reuse freed space, but every scan still reads the empty pages, and the cache holds them. This is an estimate from statistics, good to tens of percent; pgstattuple measures it exactly.{}",
                t.name,
                format::bytes((size - bloat).max(0.0)),
                format::bytes(size),
                dead.map(|d| format!(
                    " {} of its row versions are dead rows VACUUM hasn't cleared yet, enough to be most of this space: vacuum it first and estimate again before rewriting it.",
                    format::percent(d)
                ))
                .unwrap_or_default()
            );
            f.severity = if bloat >= GB && dead.is_none() { Severity::Medium } else { Severity::Low };
            severity_score(&mut f);
            f.impact.label = format!("≈ {} empty", format::bytes(bloat));
            f.confidence = Confidence::Medium;
            f.metrics = vec![
                Metric::new("Empty space (estimated)", bloat, Unit::Bytes),
                Metric::new("Table size", size, Unit::Bytes),
                Metric::new("Share", share, Unit::Ratio),
            ];
            let target = qualified(Engine::Postgres, &t.schema, &t.name);
            f.fix = Some(if dead.is_some() {
                Fix {
                    summary: "Vacuum it: VACUUM clears the dead rows without blocking reads or writes, and new rows then reuse their space. Make autovacuum visit it sooner so they don't pile up again. Rewrite the table (pg_repack online, VACUUM FULL offline) only if the estimate stays high after a vacuum.".into(),
                    sql: Some(format!("VACUUM (ANALYZE) {target};")),
                    cost: Some("VACUUM doesn't block reads or writes".into()),
                }
            } else {
                Fix {
                    summary: "Measure it exactly first. To give the space back, rewrite the table: pg_repack does it online; VACUUM FULL does it while blocking all reads and writes. If the table keeps growing, make autovacuum visit it more often so freed space is reused sooner.".into(),
                    sql: Some(format!(
                        "-- Exact numbers (needs the pgstattuple extension)\nSELECT * FROM pgstattuple('{}');\n-- Rewrite offline: blocks reads and writes until it finishes\n-- VACUUM (FULL, ANALYZE) {target};",
                        target.replace('\'', "''")
                    )),
                    cost: Some("Rewrites the table".into()),
                }
            });
            f.verify = vec!["Re-scan after the rewrite: the estimate should fall under 20%.".into()];
            f
        })
        .collect();
    findings.sort_by(|a, b| b.impact.score.total_cmp(&a.impact.score));
    findings.truncate(5);
    findings
}
