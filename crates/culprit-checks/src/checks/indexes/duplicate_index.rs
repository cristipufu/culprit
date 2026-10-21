//! The `duplicate-index` check: Indexes with the same keys as another index, or whose keys are a leading part of a wider one.

use super::*;
use crate::evidence::Requirement as Evidence;

pub fn duplicate_index_info() -> CheckInfo {
    CheckInfo {
        id: "duplicate-index".into(),
        title: "Duplicate and redundant indexes".into(),
        category: Category::Indexes,
        engines: vec![Engine::Mssql, Engine::Postgres, Engine::Mysql, Engine::Sqlite],
        requires: vec![Capability::TableStats],
        description: "Indexes with the same keys as another index, or whose keys are a leading part of a wider one."
            .into(),
    }
}

check!(DuplicateIndexCheck: duplicate_index_info, duplicate_index, &[Evidence::IndexCatalog]);

pub fn duplicate_index(snapshot: &Snapshot) -> Vec<Finding> {
    let engine = snapshot.server.engine;
    let mut out = Vec::new();
    for (a, b) in redundant_indexes(snapshot) {
        let exact = same_keys(a, b);
        let table_name = format!("{}.{}", a.schema, a.table);
        let mut f = finding(
            "duplicate-index",
            &format!("{}.{}", table_name, a.name),
            Category::Indexes,
            subject(SubjectKind::Index, format!("{}.{}", table_name, a.name)),
        );
        f.title = if exact {
            format!("{} duplicates {} on {}", a.name, b.name, a.table)
        } else {
            format!("{} is covered by the wider {} on {}", a.name, b.name, a.table)
        };
        f.summary = format!(
            "{} has keys ({}) and {} has keys ({}). {} Every write to {} maintains both.",
            a.name,
            a.columns.join(", "),
            b.name,
            b.columns.join(", "),
            if exact {
                "They are the same index twice.".to_string()
            } else {
                format!("Any query that can seek on {} can seek on {} instead.", a.name, b.name)
            },
            table_name
        );
        let size = a.size_bytes.unwrap_or(0.0);
        f.severity = if size >= 512.0 * 1024.0 * 1024.0 { Severity::Medium } else { Severity::Low };
        f.impact.score = base_score(f.severity) + (size / (1024.0 * 1024.0 * 1024.0)).min(8.0);
        f.impact.label = format!("{} redundant", format::bytes(size));
        f.confidence = if exact { Confidence::High } else { Confidence::Medium };
        f.metrics.push(Metric::new("Size", size, Unit::Bytes));
        // Usage counters a few seconds old say nothing about reads ("0" after a restart): the count is shown once they
        // have counted for a week, with how long.
        if let Some(r) = a.reads.filter(|_| watched_long_enough(snapshot)) {
            f.metrics.push(Metric::new(format!("Reads of {}", a.name), r, Unit::Count));
            if let Some(d) = crate::support::watched_days(snapshot) {
                f.metrics.push(Metric::new("Counting for", d, Unit::Days));
            }
        }
        f.fix = Some(Fix {
            summary: format!("Drop {} and keep {}.", a.name, b.name),
            sql: Some(drop_index_sql(engine, &a.schema, &a.table, &a.name)),
            cost: Some(format!("Frees {}.", format::bytes(size))),
        });
        f.verify = vec![
            format!(
                "Re-scan after removing {}: this finding should be gone, and {} should still exist.",
                a.name, b.name
            ),
            format!(
                "Compare queries on {table_name} over comparable periods before and after the change, under a similar workload: their time per run should stay the same or drop. If a query gets slower, check its plan before keeping the index removed."
            ),
        ];
        if !exact {
            f.caveats.push(format!(
                "{} is narrower, so scans of it read fewer pages. Keep it if a hot query scans it rather than seeking.",
                a.name
            ));
        }
        out.push(f);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn removing_a_duplicate_checks_the_retained_index_and_query_performance() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/mssql-demo.json");
        let snapshot: Snapshot = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        let findings = duplicate_index(&snapshot);
        let covered = findings.iter().find(|f| f.id == "duplicate-index:shop.orders.ix_orders_status").unwrap();
        assert!(covered.verify[0].contains("ix_orders_status_created should still exist"), "{:?}", covered.verify);
        assert!(covered.verify[1].contains("time per run should stay the same or drop"), "{:?}", covered.verify);
        assert!(covered.verify[1].contains("If a query gets slower, check its plan"), "{:?}", covered.verify);
        assert!(findings.iter().all(|f| !f.verify.is_empty()));
    }
}
