//! The `last-page-insert` check: Indexes keyed on an ever-increasing value (identity, sequence, insert time) where concurrent inserts wait for the latch on the last page (PAGELATCH_EX).

use super::*;
use crate::evidence::Requirement as Evidence;

pub fn last_page_insert_info() -> CheckInfo {
    CheckInfo {
        id: "last-page-insert".into(),
        title: "Inserts that queue for the last page".into(),
        category: Category::Indexes,
        engines: vec![Engine::Mssql],
        requires: vec![],
        description: "Indexes keyed on an ever-increasing value (identity, sequence, insert time) where concurrent inserts wait for the latch on the last page (PAGELATCH_EX).".into(),
    }
}

check!(LastPageInsertCheck: last_page_insert_info, last_page_insert, &[Evidence::Unknown]);

pub fn last_page_insert(snapshot: &Snapshot) -> Vec<Finding> {
    snapshot
        .indexes
        .iter()
        .filter_map(|i| {
            let c = i.contention.as_ref()?;
            (c.sequential_key
                && c.page_latch_waits >= 1_000.0
                && c.page_latch_wait_ms >= 10_000.0
                && c.optimize_for_sequential_key != Some(true))
            .then_some((i, c))
        })
        .map(|(i, c)| {
            let table = index_label(i);
            let mut f = finding(
                "last-page-insert",
                &format!("{table}.{}", i.name),
                Category::Indexes,
                subject(SubjectKind::Index, format!("{table}.{}", i.name)),
            );
            // Inserts explain the waits only where there are about as many of them: a small counter or queue table
            // whose few rows are updated over and over waits on its hot pages without inserting much, and pacing
            // inserts wouldn't help it.
            let inserts = c.leaf_inserts.is_none_or(|n| n >= c.page_latch_waits);
            if inserts {
                f.title = format!("Inserts into {} queue for the last page of {}", i.table, i.name);
                f.summary = format!(
                    "{} starts with a value that only grows ({}), so every insert goes to the same last page. Concurrent inserts wait for each other's latch on it (PAGELATCH_EX): {} waits, {} in all since the index was loaded. It gets worse with more concurrent writers.",
                    i.name,
                    i.columns.first().cloned().unwrap_or_default(),
                    format::count(c.page_latch_waits),
                    format::duration(c.page_latch_wait_ms)
                );
            } else {
                f.title = format!("Sessions queue for the latch on a few hot pages of {}", i.name);
                f.summary = format!(
                    "{} waited {} times for page latches ({} in all) since the index was loaded, but took only {} inserts: the waits are sessions reading and changing the same few rows over and over (a counter, a queue table), not inserts on its last page.",
                    i.name,
                    format::count(c.page_latch_waits),
                    format::duration(c.page_latch_wait_ms),
                    format::count(c.leaf_inserts.unwrap_or(0.0))
                );
            }
            // Counted since the index was loaded, with no time for each wait: graded on a rate measured over an hour or
            // more (ten minutes a day High, one Medium), never on the total.
            let rate = counter_rate_per_day(snapshot, c.page_latch_wait_ms);
            f.severity = since_start_severity(rate, 600_000.0, 60_000.0);
            if rate.is_none() {
                f.caveats.push(history_caveat(snapshot, "latch waits"));
            }
            severity_score(&mut f);
            f.impact.label = format!("{} latch waits", format::duration(c.page_latch_wait_ms));
            f.confidence = Confidence::Medium;
            f.metrics = vec![
                Metric::new("Page latch waits", c.page_latch_waits, Unit::Count),
                Metric::new("Time waiting", c.page_latch_wait_ms, Unit::Ms),
            ];
            if let Some(inserts) = c.leaf_inserts {
                f.metrics.push(Metric::new("Inserts", inserts, Unit::Count));
            }
            f.fix = Some(match c.optimize_for_sequential_key {
                _ if !inserts => Fix {
                    summary: "Spread the hot rows (several counter rows summed when read, or a queue that hands out rows by key rather than from one end), and keep the transactions that change them short. OPTIMIZE_FOR_SEQUENTIAL_KEY paces inserts only, so it wouldn't help here.".into(),
                    sql: None,
                    cost: None,
                },
                Some(_) => Fix {
                    summary: "SQL Server 2019 can pace concurrent inserts into a sequential key to help sustain throughput when many writers compete for the last page: try OPTIMIZE_FOR_SEQUENTIAL_KEY on this index and compare throughput. It's a metadata change and takes effect at once.".into(),
                    sql: Some(format!(
                        "ALTER INDEX {} ON {} SET (OPTIMIZE_FOR_SEQUENTIAL_KEY = ON);",
                        quote(Engine::Mssql, &i.name),
                        qualified(Engine::Mssql, &i.schema, &i.table)
                    )),
                    cost: None,
                },
                None => Fix {
                    summary: "Before SQL Server 2019 the remedies change the key: lead the index with a value that spreads inserts (a hashed or partitioned key column), or batch inserts so fewer sessions compete.".into(),
                    sql: None,
                    cost: None,
                },
            });
            f.verify = if inserts && c.optimize_for_sequential_key.is_some() {
                vec![
                    "Re-scan to confirm OPTIMIZE_FOR_SEQUENTIAL_KEY is ON. Compare completed inserts per second and the time to insert the same batch before and after, under comparable writer concurrency: the goal is better insert throughput under contention.".into(),
                    "Page-latch waits can continue, and BTREE_INSERT_FLOW_CONTROL waits may appear. Compare measurements over equal intervals; cumulative wait totals need not stop growing.".into(),
                ]
            } else {
                vec![format!(
                    "Compare completed {} and their time under comparable load before and after. Compare the increase in page-latch wait time over equal intervals for that work; cumulative wait totals need not stop growing.",
                    if inserts { "inserts per second" } else { "row changes per second" }
                )]
            };
            f
        })
        .collect()
}
