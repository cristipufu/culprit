//! The `log-volume` check: Statements that write a large share of the WAL (PostgreSQL 13+) or transaction log (SQL Server 2017+), or many kilobytes of it per row changed. Log volume drives replication lag, backup size and checkpoints. MySQL keeps no redo bytes per statement.

use super::*;

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

check!(LogVolumeCheck: log_volume_info, log_volume);

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
            let per_row = (rows >= 1_000.0).then(|| bytes / rows);
            // A share of the log is a threshold only with a volume a day behind it: 30% of a quiet hour is nothing.
            let heavy_share = share >= 0.3 && window_at_least(snapshot, bytes, GB);
            let heavy_rows = per_row.is_some_and(|b| b >= 4_096.0) && bytes >= 100.0 * MB;
            (heavy_share || heavy_rows).then_some((q, costs, bytes, share, per_row))
        })
        .map(|(q, costs, bytes, share, per_row)| {
            let mut f = finding("log-volume", &q.id, Category::Queries, subject(SubjectKind::Query, &q.id));
            let handle = query_handle(q);
            f.title = match per_row {
                Some(b) if b >= 4_096.0 => format!("{} writes {} of {log} per row it changes", capitalize(&handle), format::bytes(b)),
                // The share is of what statements wrote: autovacuum's and checkpoints' WAL isn't in it.
                _ => format!("{} writes {} of the {log} statements write", capitalize(&handle), format::percent(share)),
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
                window_phrase(snapshot),
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
            let busy = share >= 0.5 && window_at_least(snapshot, bytes, GB);
            // Wide rows of log are Medium with a volume behind them: 100 MB a day, not 100 MB over a month.
            let wide = per_row.is_some_and(|b| b >= 8_192.0) && window_at_least(snapshot, bytes, 100.0 * MB);
            f.severity = if busy || wide { Severity::Medium } else { Severity::Low };
            with_query(snapshot, &mut f, q);
            severity_score(&mut f);
            f.impact.label = format!("{} of {log}", format::bytes(bytes));
            f.confidence = Confidence::High;
            f.metrics = vec![Metric::new("Log written", bytes, Unit::Bytes), Metric::new("Share of log", share, Unit::Ratio)];
            if let Some(b) = per_row {
                f.metrics.push(Metric::new("Per row", b, Unit::Bytes));
            }
            f.fix = Some(if pg {
                // wal_compression only where it is known to be off; on or unknown, the checkpoint interval is the lever.
                let compress = image_share.filter(|s| *s >= 0.5).map(|_| setting_change(&snapshot.server, "wal_compression", "on"));
                let images = match (&compress, compression) {
                    (Some(change), _) => format!(" Full-page images dominate and wal_compression is off: turning it on shrinks them (lz4 or zstd on PostgreSQL 15+). {}", change.how),
                    (None, Some(true)) => " wal_compression already shrinks full-page images; a longer checkpoint interval (checkpoint_timeout, max_wal_size) logs fewer of them.".to_string(),
                    (None, _) => " If full-page images dominate, a longer checkpoint interval (checkpoint_timeout, max_wal_size) logs fewer of them, and wal_compression shrinks them if it is off.".to_string(),
                };
                Fix {
                    summary: format!("Touch each page once per checkpoint: batch the changes (one statement for many rows, sorted by key) rather than many scattered single-row writes. Every index on the table adds its own WAL: drop indexes nobody reads (see Indexes).{images}"),
                    sql: compress.and_then(|c| c.sql),
                    cost: None,
                }
            } else {
                Fix {
                    summary: "Log per row this high usually means wide rows rewritten whole, many indexes updated, or one transaction per row. Batch writes into transactions of about a thousand rows, update only the columns that change, and drop indexes nobody reads (see Indexes).".into(),
                    sql: None,
                    cost: None,
                }
            });
            f.verify = vec![format!("Re-scan: its share of the {log} and the bytes per row should drop.")];
            f
        })
        .collect();
    findings.sort_by(|a, b| b.impact.score.total_cmp(&a.impact.score));
    findings.truncate(5);
    findings
}
