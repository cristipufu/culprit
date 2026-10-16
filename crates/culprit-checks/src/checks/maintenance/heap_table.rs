//! The `heap-table` check: Heaps grow forwarded records and can't be read in key order; most OLTP tables should be clustered.

use super::*;

pub fn heap_table_info() -> CheckInfo {
    CheckInfo {
        id: "heap-table".into(),
        title: "Large tables without a clustered index".into(),
        category: Category::Maintenance,
        engines: vec![Engine::Mssql],
        requires: vec![Capability::TableStats],
        description:
            "Heaps grow forwarded records and can't be read in key order; most OLTP tables should be clustered.".into(),
    }
}

check!(HeapTableCheck: heap_table_info, heap_table);

pub fn heap_table(snapshot: &Snapshot) -> Vec<Finding> {
    if snapshot.server.engine != Engine::Mssql {
        return Vec::new();
    }
    snapshot
        .tables
        .iter()
        .filter(|t| t.is_heap == Some(true) && t.rows.unwrap_or(0.0) >= 100_000.0)
        .map(|t| {
            let table_name = t.qualified();
            let rows = t.rows.unwrap_or(0.0);
            let mut f = finding("heap-table", &table_name, Category::Maintenance, subject(SubjectKind::Table, &table_name));
            let forwarded = t.forwarded_fetches.filter(|f| *f > 0.0);
            let days = counter_days_for_rate(snapshot);
            let forwarded_per_day = forwarded.and_then(|f| counter_rate_per_day(snapshot, f));
            // Said "a day" only over 20 hours of counters; under that, what was counted and since when.
            let said_per_day = forwarded_per_day.filter(|_| days.is_some_and(per_day_is_measured));
            f.title = match (forwarded_per_day, forwarded, days) {
                (Some(per_day), Some(total), Some(days)) if per_day >= 1000.0 => format!(
                    "Reads of {} follow forwarding pointers {}",
                    t.name,
                    how_many(snapshot, total, days, "time", "times")
                ),
                _ => format!("{} ({} rows) has no clustered index", t.name, format::count(rows)),
            };
            f.summary = format!(
                "{table_name} is stored as a heap. Updates that grow a row leave forwarding pointers behind, deleted space isn't reclaimed well, and every nonclustered index lookup has to go through a row identifier.{}{}",
                forwarded
                    .map(|total| format!(
                        " Reads have followed a forwarding pointer to another page {} times {}{}: each one is an extra random read.",
                        format::count(total),
                        since_phrase(snapshot),
                        said_per_day.map(|d| format!(" (about {} a day)", format::count(d))).unwrap_or_default()
                    ))
                    .unwrap_or_default(),
                if t.has_primary_key { " It does have a primary key, just not a clustered one." } else { " It has no primary key either." }
            );
            // What a heap costs here is the extra reads it causes, not its size: an insert-only log heap costs nothing.
            f.severity =
                if forwarded_per_day.is_some_and(|d| d >= 100_000.0) { Severity::Medium } else { Severity::Low };
            if let Some(d) = said_per_day {
                f.metrics.push(Metric::new("Forwarded reads per day", d, Unit::Count));
            }
            f.impact.score = base_score(f.severity);
            f.impact.label = format!("{} rows", format::count(rows));
            f.confidence = Confidence::Medium;
            f.metrics.push(Metric::new("Rows", rows, Unit::Rows));
            if let Some(b) = t.total_bytes {
                f.metrics.push(Metric::new("Size", b, Unit::Bytes));
            }
            f.fix = Some(Fix {
                summary: "Add a clustered index, usually on the primary key or an ever-increasing column such as an identity or creation time. Plan it as a maintenance operation: it rewrites the table.".into(),
                sql: None,
                cost: t.total_bytes.map(|b| format!("Rewrites about {} of data.", format::bytes(b))),
            });
            f.caveats.push("Staging tables that are bulk-loaded and truncated can be fine as heaps.".into());
            f
        })
        .collect()
}
