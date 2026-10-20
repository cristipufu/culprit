//! The `log-volume` check: Statements that write a large share of the WAL (PostgreSQL 13+) or transaction log (SQL Server 2017+), or many kilobytes of it per row changed. Log volume drives replication lag, backup size and checkpoints. MySQL keeps no redo bytes per statement.

use super::*;
use crate::evidence::{QueryMeasure, Requirement as Evidence};

pub fn log_volume_info() -> CheckInfo {
    CheckInfo {
        id: "log-volume".into(),
        title: "Writes that produce most of the log".into(),
        category: Category::Queries,
        engines: vec![Engine::Mssql, Engine::Postgres],
        requires: vec![],
        description: "Statements that write a large share of the WAL (PostgreSQL 13+) or transaction log (SQL Server 2017+), or many kilobytes of it per row changed. Log volume drives replication lag, backup size and checkpoints. MySQL keeps no redo bytes per statement.".into(),
    }
}

check!(LogVolumeCheck: log_volume_info, log_volume, &[Evidence::Query(QueryMeasure::LogBytes)]);

pub fn log_volume(snapshot: &Snapshot) -> Vec<Finding> {
    let pg = snapshot.server.engine == Engine::Postgres;
    let log = if pg { "WAL" } else { "transaction log" };
    let mut findings: Vec<Finding> = app_queries(snapshot)
        .filter(|q| is_write(q))
        .filter_map(|q| {
            let costs = q.costs.as_ref()?;
            let bytes = costs.log_bytes?;
            let share = costs.log_share.unwrap_or(0.0);
            let rows = q.rows.unwrap_or(0.0);
            // Bytes per row whenever it changed rows: a few hundred rows of large values say as much as a million.
            let per_row = (rows > 0.0).then(|| bytes / rows);
            // A share of the log, or many kilobytes a row, is a threshold only with a volume a day behind it: 30% of a
            // quiet hour is nothing, and neither is 3 GB over three years.
            let heavy_share = share >= 0.3 && window_at_least(snapshot, q, bytes, GB);
            let heavy_rows = per_row.is_some_and(|b| b >= 4_096.0) && window_at_least(snapshot, q, bytes, 100.0 * MB);
            (heavy_share || heavy_rows).then_some((q, costs, bytes, share, per_row))
        })
        .map(|(q, costs, bytes, share, per_row)| {
            let mut f = finding("log-volume", &q.id, Category::Queries, subject(SubjectKind::Query, &q.id));
            let handle = query_handle(q);
            // A statement that stopped running is told in the past.
            let (writes, changes) =
                if crate::all_stopped(snapshot, &[q]) { ("wrote", "changed") } else { ("writes", "changes") };
            f.title = match per_row {
                Some(b) if b >= 4_096.0 => {
                    format!("{} {writes} {} of {log} per row it {changes}", capitalize(&handle), format::bytes(b))
                }
                // The share is of what statements wrote: autovacuum's and checkpoints' WAL isn't in it.
                _ => format!("{} {writes} {} of the {log} statements write", capitalize(&handle), format::percent(share)),
            };
            // pg_stat_statements counts full-page images, not their bytes. A whole page is 8 KB at most: PostgreSQL
            // leaves out the empty middle of each page, and wal_compression shrinks the rest, so 8 KB each is an upper
            // bound, and close to the truth only with compression off.
            // wal_compression is off, on, or a method (pglz, lz4, zstd from 15).
            let compression = pg
                .then(|| snapshot.setting("wal_compression"))
                .flatten()
                .map(|s| !matches!(s.value.trim().to_ascii_lowercase().as_str(), "off" | "false" | "0" | "no"));
            let images = costs.log_full_pages.filter(|n| *n > 0.0);
            let image_share = images.filter(|_| compression == Some(false)).map(|n| (n * 8_192.0 / bytes).min(1.0));
            f.summary = format!(
                "It wrote {} of {log} {}: {} of what statements wrote{}. Every byte of {log} is also written to backups and sent to replicas, so it drives replication lag, backup size and checkpoints.{}",
                format::bytes(bytes),
                ran_phrase(snapshot, q),
                format::percent(share),
                per_row.map(|b| format!(", about {} per row it changed", format::bytes(b))).unwrap_or_default(),
                match (image_share, images) {
                    (Some(s), _) if s >= 0.5 => format!(" Up to {} of it is full-page images: the first change to each page after a checkpoint logs the whole page, 8 KB at most.", format::percent(s)),
                    (None, Some(n)) if pg && n * 8_192.0 >= 0.5 * bytes => format!(
                        " {} of its records are full-page images: the first change to each page after a checkpoint logs the whole page{}.",
                        format::count(n),
                        if compression == Some(true) { ", compressed with wal_compression" } else { "" }
                    ),
                    _ => String::new(),
                }
            );
            let busy = share >= 0.5 && window_at_least(snapshot, q, bytes, GB);
            // Wide rows of log are Medium with a volume behind them: 100 MB a day, not 100 MB over a month.
            let wide = per_row.is_some_and(|b| b >= 8_192.0) && window_at_least(snapshot, q, bytes, 100.0 * MB);
            f.severity = if busy || wide { Severity::Medium } else { Severity::Low };
            with_query(snapshot, &mut f, q);
            // Ranked by the log it writes a day, not by the statement's share of database time: a quarter of a quiet
            // database's time spent writing a few megabytes of log a day is no log problem.
            let a_day = window_rate_per_day(snapshot, q, bytes).unwrap_or(0.0);
            f.impact.score = base_score(f.severity) + (a_day / MB).max(1.0).log10().min(4.0) * 3.0;
            f.impact.label = format!("{} of {log}", format::bytes(bytes));
            f.confidence = Confidence::High;
            f.metrics = vec![Metric::new("Log written", bytes, Unit::Bytes), Metric::new("Share of log", share, Unit::Ratio)];
            f.facts.log_write = Some(culprit_core::FindingLogWrite {bytes,per_row});
            if let Some(b) = per_row {
                f.metrics.push(Metric::new("Per row", b, Unit::Bytes));
            }
            // The table it changes: whether its rows are mostly large values (then the log is the data itself) and
            // whether it keeps an index nobody reads (then dropping it is advice, not a guess).
            let engine = snapshot.server.engine;
            let target = write_targets(q, engine)
                .into_iter()
                .find_map(|o| snapshot.tables.iter().find(|t| names_table(&o, &t.schema, &t.name)));
            let unread = target.is_some_and(|t| {
                snapshot.indexes_on(&t.schema, &t.name).any(|i| crate::indexes::is_unused(snapshot, i))
            });
            let drop_unread = if unread {
                format!(" Every index on the table adds its own {log}, and {} has one nobody reads: drop it (see Indexes).", target.map(|t| t.name.as_str()).unwrap_or("it"))
            } else {
                String::new()
            };
            let large = target.and_then(|t| large_values(t).map(|(average, share)| (t, average, share)));
            f.fix = Some(if let Some((t, average, large_share)) = large {
                // Large values are logged in full: batching or dropping indexes can't shrink that (large-rows' fix can).
                Fix {
                    summary: format!(
                        "The rows of {} average {}, {} of it large values, and every insert or update that sets them writes them to the {log} in full: batching the writes won't shrink that, smaller or fewer values will. For values that must stay, {}.{drop_unread}",
                        t.name,
                        format::bytes(average),
                        share_words(large_share),
                        compress_advice(engine)
                    ),
                    sql: None,
                    cost: None,
                }
            } else if pg {
                // wal_compression only where it is known to be off; on or unknown, the checkpoint interval is the lever.
                let compress = image_share.filter(|s| *s >= 0.5).map(|_| setting_change(&snapshot.server, "wal_compression", "on"));
                let images = match (&compress, compression) {
                    (Some(change), _) => format!(" Full-page images dominate and wal_compression is off: turning it on shrinks them (lz4 or zstd on PostgreSQL 15+). {}", change.how),
                    (None, Some(true)) => " wal_compression already shrinks full-page images; a longer checkpoint interval (checkpoint_timeout, max_wal_size) logs fewer of them.".to_string(),
                    (None, _) => " If full-page images dominate, a longer checkpoint interval (checkpoint_timeout, max_wal_size) logs fewer of them, and wal_compression shrinks them if it is off.".to_string(),
                };
                Fix {
                    summary: format!("Touch each page once per checkpoint: batch the changes (one statement for many rows, sorted by key) rather than many scattered single-row writes.{drop_unread}{images}"),
                    sql: compress.and_then(|c| c.sql),
                    cost: None,
                }
            } else if first_verb(main_statement(&q.text)).is_some_and(|(_, v)| v == "insert") {
                // An insert logs the rows it adds: nothing in them is "unchanged columns" to leave out.
                Fix {
                    summary: format!("An insert logs every row it adds in full, with an entry in each index: this much {log} is this much data. Store less per row (large values compressed as they're written, or kept in object storage with a reference in the row), and commit many rows per transaction rather than one.{drop_unread}"),
                    sql: None,
                    cost: None,
                }
            } else {
                Fix {
                    summary: format!("Log this heavy usually means wide rows rewritten whole, many indexes updated, or one transaction per row. Batch writes into transactions of about a thousand rows, and update only the columns that change.{drop_unread}"),
                    sql: None,
                    cost: None,
                }
            });
            // On running totals the log it wrote holds every run from before the change: two scans show it, per row
            // (batching writes more of it per run, not per row).
            let now = format!("{} for {} rows", format::bytes(bytes), format::count(q.rows.unwrap_or(0.0)));
            let counted = format!("{log} and the rows it changed");
            f.verify = vec![match crate::queries::verify_counts_between_scans(snapshot, &counted, &now) {
                Some(compare) => format!("Between two scans, the {log} it writes per row should drop: {compare}."),
                None => format!("Re-scan: its share of the {log} and the bytes per row should drop."),
            }];
            f
        })
        .collect();
    findings.sort_by(|a, b| b.impact.score.total_cmp(&a.impact.score));
    findings.truncate(5);
    findings
}
