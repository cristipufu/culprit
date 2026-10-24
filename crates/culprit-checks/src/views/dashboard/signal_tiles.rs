//! The dashboard's storage, connections, replication and failing-queries tiles: one sentence each from the server's signals.

use super::*;

/// The worst finding of one check, for a tile that leads with it.
/// Findings arrive ranked by impact; among equally severe ones the first is the most important. (`max_by_key` keeps
/// the last of equal maximums, hence the reversal.)
pub(super) fn worst_finding<'a>(report: &'a Report, check: &str) -> Option<&'a Finding> {
    report.findings.iter().filter(|f| f.check == check && f.is_actionable()).rev().max_by_key(|f| f.severity)
}

/// A collector's note, when it explains why a tile has nothing to show.
pub(super) fn collector_note(s: &Snapshot, names: &[&str]) -> Option<String> {
    s.notes
        .iter()
        .find(|n| names.iter().any(|name| culprit_core::same_source(name, &n.collector)))
        .map(|n| n.message.clone())
}

pub(super) fn file_name(s: &Snapshot, f: &FileIo) -> String {
    match (s.server.engine, f.kind, f.database.as_deref()) {
        (Engine::Mssql, FileKind::Log, Some(db)) => format!("{db} log"),
        (Engine::Mssql, _, Some(db)) if db.eq_ignore_ascii_case("tempdb") => "tempdb".into(),
        (Engine::Postgres, FileKind::Log, _) => "WAL".into(),
        (Engine::Postgres, FileKind::Temp, _) => "temporary files".into(),
        (Engine::Postgres, _, _) => "tables and indexes".into(),
        _ => f.name.clone(),
    }
}

/// A latency people can read: cache-fast storage in microseconds ("3 µs", as the Health panel says), not "0.00 ms".
/// Storage latency in one unit family: microseconds below a millisecond ("40 µs", "630 µs"), so SQL Server's 0.04 ms
/// and MySQL's 8 µs read alike.
pub(super) fn latency(ms: f64) -> String {
    if ms < 1.0 { format!("{} µs", (ms * 1000.0).round().max(1.0)) } else { format::duration(ms) }
}

/// The latency a file is waited on for (reads of data, writes of the log) and how many operations it averages.
pub(super) fn waited_latency(f: &FileIo) -> Option<(f64, f64, &'static str)> {
    if f.kind == FileKind::Log {
        f.avg_write_ms().map(|avg| (avg, f.writes, f.write_label()))
    } else {
        f.avg_read_ms().map(|avg| (avg, f.reads, "read"))
    }
}

/// Average read latency of data and temporary files, and write latency of the log: what queries and commits wait on.
pub(super) fn storage(report: &Report) -> Tile {
    let s = &report.snapshot;
    let family = family(&s.server);
    let merged = s.pressure.as_ref().map(|p| merge_logs(&p.files)).unwrap_or_default();
    let files: Vec<&FileIo> =
        merged.iter().filter(|f| f.kind != FileKind::Other && waited_latency(f).is_some()).collect();
    if files.is_empty() {
        // An engine fact only when the scan proves it; otherwise just what this scan lacks (an older scan, a login
        // that can't read the views).
        let why = match s.server.engine {
            Engine::Postgres if is_off(setting(s, "track_io_timing")) => {
                "PostgreSQL times reads and writes only with track_io_timing on.".to_string()
            }
            Engine::Mysql if !s.server.has(Capability::PerformanceSchema) => {
                format!("{family} times no file I/O while performance_schema is off.")
            }
            _ => "This scan has no storage timings.".to_string(),
        };
        let mut t = Tile::new(TileKey::Storage, TileStatus::Unavailable, why);
        t.detail = collector_note(s, &[culprit_core::source_id::FILE_LATENCY, "storage", "files", "signals"]);
        return t;
    }
    // Enough operations for an average to mean something: the floor the slow-storage check uses.
    let worst = |log: bool| {
        files
            .iter()
            .filter(|f| (f.kind == FileKind::Log) == log)
            .filter_map(|f| waited_latency(f).filter(|(_, ops, _)| *ops >= 1_000.0).map(|(avg, _, _)| avg))
            .max_by(f64::total_cmp)
    };
    let (read, log) = (worst(false), worst(true));
    let (status, takeaway) = match worst_finding(report, "slow-storage") {
        Some(f) => (severity_status(f.severity), format!("{}.", f.title.trim_end_matches('.'))),
        None => {
            let mut parts = Vec::new();
            if let Some(avg) = read {
                parts.push(format!("reads average {}", latency(avg)));
            }
            if let Some(avg) = log {
                parts.push(format!(
                    "log {} {}",
                    if files.iter().any(|f| f.kind == FileKind::Log && f.write_label() == "flush") {
                        "flushes"
                    } else {
                        "writes"
                    },
                    latency(avg)
                ));
            }
            if parts.is_empty() {
                (TileStatus::Info, "Too little I/O since startup to judge storage speed.".to_string())
            } else {
                (TileStatus::Info, format!("Recorded storage averages: {}.", parts.join(", ")))
            }
        }
    };
    let mut t = Tile::new(TileKey::Storage, status, takeaway);
    if let Some(avg) = read {
        t.metrics.push(TileMetric::new("read-ms", "Avg read", Some(avg), latency(avg)));
    }
    if let Some(avg) = log {
        t.metrics.push(TileMetric::new(
            "log-write-ms",
            if files.iter().any(|f| f.kind == FileKind::Log && f.write_label() == "flush") {
                "Avg log flush"
            } else {
                "Avg log write"
            },
            Some(avg),
            latency(avg),
        ));
    }
    // The slowest files, each by what it's waited on for.
    let mut rows: Vec<(f64, TileItem)> = files
        .iter()
        .filter_map(|f| {
            let (avg, ops, what) = waited_latency(f)?;
            (ops >= 1_000.0).then(|| {
                let id = format!("{}:{}", f.database.as_deref().unwrap_or(""), f.name);
                let mut item = TileItem::new(ItemKind::Signal, id, file_name(s, f));
                item.value = Some(format!("{} per {what}", latency(avg)));
                if f.scope == Some(culprit_core::FileScope::Server) {
                    item.label.push_str(" (whole server)");
                }
                if f.scope == Some(culprit_core::FileScope::Database)
                    && f.database.as_deref().is_none_or(|db| db == s.server.database)
                {
                    item.label.push_str(" (this database)");
                }
                let prefix = format!("slow-storage:{}:", f.database.as_deref().unwrap_or(""));
                item.status = Some(
                    report
                        .findings
                        .iter()
                        .filter(|g| g.is_actionable() && g.id.starts_with(&prefix))
                        .filter(|g| {
                            g.metrics.iter().any(|m| {
                                matches!(m.label.as_str(), "File" | "Files")
                                    && m.text
                                        .as_ref()
                                        .is_some_and(|names| names == &f.name || names.split(", ").any(|n| n == f.name))
                            })
                        })
                        .map(|g| severity_status(g.severity))
                        .max()
                        .unwrap_or(TileStatus::Info),
                );
                (avg, item)
            })
        })
        .collect();
    rows.sort_by(|a, b| b.0.total_cmp(&a.0));
    t.items = rows.into_iter().map(|(_, i)| i).take(4).collect();
    // The counters' own window, worded like every other one (a statistics reset on PostgreSQL, else the restart).
    t.detail =
        Some(files.iter().map(|f| format!("{}: {}", file_name(s, f), f.coverage(s))).collect::<Vec<_>>().join("; "));
    t
}

/// A pressure signal as a tile row: its label, value and grade.
pub(super) fn signal_item(sig: &Signal) -> TileItem {
    let mut item = TileItem::new(ItemKind::Signal, sig.key.clone(), sig.label.clone());
    item.value = Some(signal_value(sig));
    item.status = Some(signal_status(sig.status));
    item
}

/// New connections per second and connections in use: an application that connects for every request pays for it
/// on every request.
pub(super) fn connections(report: &Report) -> Tile {
    let s = &report.snapshot;
    let p = s.pressure.as_ref();
    let rate = p.and_then(|p| p.signal(culprit_core::signal_id::NEW_CONNECTIONS));
    let used = p.and_then(|p| p.signal(culprit_core::signal_id::CONNECTIONS));
    if rate.is_none() && used.is_none() {
        let why = match s.server.engine {
            Engine::Postgres if !culprit_core::version_at_least(&s.server.version, [14, 0, 0]) => {
                "PostgreSQL counts sessions from version 14; this server doesn't.".to_string()
            }
            engine if !engine.has_server() => format!("{engine} has no server, so no connections to count."),
            _ => "This scan has no connection counters.".to_string(),
        };
        let mut t = Tile::new(TileKey::Connections, TileStatus::Unavailable, why);
        t.detail = collector_note(
            s,
            &[culprit_core::source_id::CONNECTIONS, culprit_core::source_id::LOGINS, culprit_core::source_id::SESSIONS],
        );
        return t;
    }
    let (status, takeaway) = match worst_finding(report, "connection-churn") {
        Some(f) => (severity_status(f.severity), format!("{}.", f.title.trim_end_matches('.'))),
        None => match rate {
            Some(r) if r.value >= 1.0 => (
                TileStatus::Info,
                format!("{} new connections a second over the recorded counter history.", format::count(r.value)),
            ),
            Some(r) => (
                TileStatus::Info,
                format!(
                    "{} new connections a minute over the recorded counter history.",
                    format::count((r.value * 60.0).round())
                ),
            ),
            None => {
                (TileStatus::Info, "This scan has no count of new connections; the ones in use are below.".to_string())
            }
        },
    };
    let mut t = Tile::new(TileKey::Connections, status, takeaway);
    // Under one a second, a per-minute count reads better than "0.0".
    if let Some(r) = rate {
        t.metrics.push(if r.value >= 1.0 {
            TileMetric::new("per-second", "New per second", Some(r.value), format::count(r.value))
        } else {
            let per_minute = (r.value * 60.0).round();
            TileMetric::new("per-minute", "New per minute", Some(per_minute), format::count(per_minute))
        });
    }
    if let Some(u) = used {
        let item = signal_item(u);
        // Running out of connections is worse than opening too many.
        if let Some(st @ (TileStatus::Bad | TileStatus::Watch)) = item.status {
            t.status = t.status.max(st);
        }
        t.items.push(item);
    }
    for key in [
        culprit_core::signal_id::SESSION_LENGTH,
        culprit_core::signal_id::POOLED_REUSE,
        culprit_core::signal_id::THREAD_CACHE_MISSES,
        culprit_core::signal_id::SESSIONS_ABANDONED,
    ] {
        if let Some(sig) = p.and_then(|p| p.signal(key)) {
            t.items.push(signal_item(sig));
        }
    }
    t.items.truncate(4);
    t
}

pub(super) fn lag_text(r: &Replica) -> String {
    r.lag_seconds
        .map(|sec| format!("{} behind", format::duration(sec * 1000.0)))
        .or_else(|| r.lag_bytes.map(|b| format!("{} of log", format::bytes(b))))
        .or_else(|| r.state.clone())
        .unwrap_or_else(|| "in sync".into())
}

/// How far replicas and replication slots are behind, worst first.
pub(super) fn replication(report: &Report) -> Tile {
    let s = &report.snapshot;
    let family = family(&s.server);
    let replicas = s.pressure.as_ref().map(|p| p.replicas.as_slice()).unwrap_or_default();
    if replicas.is_empty() {
        if let Some(note) = collector_note(s, &[culprit_core::source_id::REPLICATION]) {
            let mut t = Tile::new(
                TileKey::Replication,
                TileStatus::Unavailable,
                format!("Couldn't read {family}'s replication state."),
            );
            t.detail = Some(note);
            return t;
        }
        let none = match s.server.engine {
            Engine::Yugabyte => {
                return Tile::new(
                    TileKey::Replication,
                    TileStatus::Unavailable,
                    "Raft replica lag was not measured; an empty PostgreSQL replication view cannot establish DocDB replication health.",
                );
            }
            Engine::Postgres => {
                "No replicas or replication slots: nothing can fall behind or hold WAL back.".to_string()
            }
            Engine::Mssql => "No availability group or geo-replica: nothing can fall behind.".to_string(),
            Engine::Mysql => format!("{family} isn't a replica here and reports none: nothing can fall behind."),
            Engine::Sqlite => "A SQLite file has no replicas: nothing can fall behind.".to_string(),
        };
        return Tile::new(TileKey::Replication, TileStatus::Good, none);
    }
    let (status, takeaway) = match worst_finding(report, "replica-lag") {
        Some(f) => (severity_status(f.severity), format!("{}.", f.title.trim_end_matches('.'))),
        None => {
            (TileStatus::Info, format!("Replication state recorded for {}.", plural(replicas.len(), "copy", "copies")))
        }
    };
    let mut t = Tile::new(TileKey::Replication, status, takeaway);
    let mut rows: Vec<&Replica> = replicas.iter().collect();
    rows.sort_by(|a, b| {
        b.lag_seconds
            .unwrap_or(0.0)
            .total_cmp(&a.lag_seconds.unwrap_or(0.0))
            .then(b.lag_bytes.unwrap_or(0.0).total_cmp(&a.lag_bytes.unwrap_or(0.0)))
    });
    t.items = rows
        .into_iter()
        .take(4)
        .map(|r| {
            let role = match r.role {
                ReplicaRole::Replica => "replica",
                ReplicaRole::ThisServer => "this server",
                ReplicaRole::Slot => "slot",
            };
            let mut item = TileItem::new(ItemKind::Signal, r.name.clone(), format!("{} ({role})", r.name));
            item.value = Some(lag_text(r));
            item.status = Some(
                report
                    .findings
                    .iter()
                    .filter(|f| f.check == "replica-lag" && f.is_actionable() && f.subject.name == r.name)
                    .map(|f| severity_status(f.severity))
                    .max()
                    .unwrap_or(TileStatus::Info),
            );
            item
        })
        .collect();
    t
}

/// Statements whose runs fail or are cancelled (timeouts), from the counts the engine keeps per statement.
pub(super) fn failures(report: &Report) -> Tile {
    let s = &report.snapshot;
    if let Some(history) = &s.operations.termination_history {
        let count = history.events.len();
        let mut tile = Tile::new(
            TileKey::Failures,
            TileStatus::Info,
            if count == 0 {
                "No unexpected termination records were retained in this read; ordinary SQL errors are not counted here.".into()
            } else {
                format!(
                    "{} in the native buffer; this is not a failure rate.",
                    plural(count, "unexpected termination record remains", "unexpected termination records remain")
                )
            },
        );
        tile.title = "Recorded terminations".into();
        tile.detail = Some(format!(
            "{} · {} to {} · {}. {}The evictable buffer is independent of statement statistics and does not include all SQL errors, cancellations or deadlocks.",
            history.source,
            history.requested_start,
            history.requested_end,
            history.visibility,
            if history.partial { "Partial read. " } else { "" }
        ));
        tile.metrics.push(TileMetric::new("records", "Retained records", Some(count as f64), count.to_string()));
        tile.items = history
            .events
            .iter()
            .take(4)
            .enumerate()
            .map(|(i, e)| {
                let mut item = TileItem::new(ItemKind::Signal, i.to_string(), e.reason.label());
                item.value = Some(e.ended_at.clone());
                item
            })
            .collect();
        return tile;
    }
    if let Some(reason) = s.source_problem(&[culprit_core::source_id::DOCDB_TERMINATIONS]) {
        let mut tile =
            Tile::new(TileKey::Failures, TileStatus::Unavailable, "Native termination history was unavailable.");
        tile.detail = Some(reason);
        return tile;
    }
    if !s.operations.log_failures.is_empty() {
        let total: f64 = s.operations.log_failures.iter().map(|f| f.count).sum();
        let found: Vec<_> = report.findings.iter().filter(|f| f.check == "logged-errors").collect();
        let worst = found.iter().filter(|f| f.is_actionable()).max_by_key(|f| f.severity);
        let mut tile = Tile::new(
            TileKey::Failures,
            worst.map_or(TileStatus::Info, |f| severity_status(f.severity)),
            worst.map_or_else(
                || format!("{} SQL error records in the supplied application log.", format::count(total)),
                |f| f.title.clone(),
            ),
        );
        tile.detail=Some("Counts are logged records, including possible retries and duplicates. Dates cover only records with a recorded time zone; missing durations are excluded from performance totals.".into());
        tile.metrics.push(TileMetric::new("records", "Error log records", Some(total), format::count(total)));
        tile.items = found
            .iter()
            .take(4)
            .map(|f| {
                let mut item = TileItem::new(ItemKind::Finding, f.id.clone(), f.title.clone());
                item.finding_id = Some(f.id.clone());
                item.status = Some(if f.is_actionable() { severity_status(f.severity) } else { TileStatus::Info });
                item
            })
            .collect();
        return tile;
    }
    let family = family(&s.server);
    let Some(w) = &s.workload else { return no_workload(TileKey::Failures, "failed runs aren't counted") };
    let counted: Vec<(&QueryStat, f64)> =
        app_queries(s).filter_map(|q| q.costs.as_ref()?.unfinished().map(|u| (q, u))).collect();
    if counted.is_empty() {
        let why = match s.server.engine {
            Engine::Postgres => {
                "PostgreSQL doesn't count failed runs per statement; its log does (log_min_error_statement)."
                    .to_string()
            }
            _ if w.kind() == Some(culprit_core::WorkloadKind::SlowLog) => format!(
                "The slow query log keeps runs that finished; {family} counts errors per statement only in performance_schema."
            ),
            _ if w.log.is_some() => format!(
                "The {} log records how long statements took, not whether they failed.",
                w.log.as_deref().unwrap_or_default()
            ),
            _ => format!("{family} reported no failed or cancelled runs per statement in this scan."),
        };
        return Tile::new(TileKey::Failures, TileStatus::Unavailable, why);
    }
    let mut failing: Vec<(&QueryStat, f64, f64)> = counted
        .into_iter()
        .filter(|(_, u)| *u > 0.0)
        .map(|(q, u)| (q, u, if q.executions > 0.0 { (u / q.executions).min(1.0) } else { 1.0 }))
        .collect();
    failing.sort_by(|a, b| b.1.total_cmp(&a.1));
    let total: f64 = failing.iter().map(|f| f.1).sum();
    let window = window_phrase(w);
    let (status, takeaway) = match worst_finding(report, "failing-query") {
        Some(f) => (severity_status(f.severity), format!("{}.", f.title.trim_end_matches('.'))),
        None if failing.is_empty() => (TileStatus::Good, format!("No statement failed or timed out over {window}.")),
        None => (
            TileStatus::Info,
            format!(
                "{} didn't finish over {window}; the recorded counts do not establish a current error-rate problem.",
                capitalized(&plural(total.round() as usize, "run", "runs"))
            ),
        ),
    };
    let mut t = Tile::new(TileKey::Failures, status, takeaway);
    t.metrics.push(TileMetric::new("statements", "Statements", Some(failing.len() as f64), failing.len().to_string()));
    t.metrics.push(TileMetric::new("runs", "Unfinished runs", Some(total), format::count(total)));
    t.items = failing
        .iter()
        .take(4)
        .map(|(q, unfinished, rate)| {
            let mut item = TileItem::new(ItemKind::Query, q.id.clone(), query_handle(q));
            item.query_id = Some(q.id.clone());
            // Fewer than five unfinished runs is too few to grade (a grey dot), so the value says how few.
            item.value = Some(if *unfinished < 5.0 {
                format!("{} of {} runs", format::count(*unfinished), format::count(q.executions.max(*unfinished)))
            } else if *rate >= 0.999 {
                "every run".into()
            } else {
                format!("{} of runs", format::percent(*rate))
            });
            item.share = Some(*rate);
            let id = format!("failing-query:{}", q.id);
            item.finding_id = report.findings.iter().find(|f| f.id == id).map(|f| f.id.clone());
            item.status = Some(
                report
                    .findings
                    .iter()
                    .find(|f| f.id == id && f.is_actionable())
                    .map(|f| severity_status(f.severity))
                    .unwrap_or(TileStatus::Info),
            );
            item
        })
        .collect();
    t
}

pub(super) fn statistics(report: &Report) -> Tile {
    let s = &report.snapshot;
    let locked = report.locked.len();
    let stats_findings: Vec<&Finding> =
        report.findings.iter().filter(|f| STATISTICS_FINDINGS.contains(&f.id.as_str())).collect();
    let mut tile = match &s.workload {
        None => {
            let missing = s.server.missing.iter().find(|m| m.capability.is_workload_source());
            // Red when a source of query history is missing and can be turned on; an engine that keeps none (SQLite,
            // whose statements can come from the application's log) isn't failing at anything.
            let turn_on = missing.filter(|m| m.capability != Capability::QueryLog);
            let mut t = Tile::new(
                TileKey::Statistics,
                if turn_on.is_some() { TileStatus::Bad } else { TileStatus::Unavailable },
                match missing {
                    Some(m) => format!("No query history: {}", sentence(&m.reason)),
                    None => format!("{} keeps no query history.", family(&s.server)),
                },
            );
            t.detail = Some(if turn_on.is_some() {
                "Until they're on, Culprit checks tables, indexes and settings only: no query list, plans or trends."
                    .into()
            } else {
                "Culprit checks its tables, indexes and settings, and plans any statement you give it.".into()
            });
            t.fix_sql = turn_on.and_then(|m| m.fix.as_deref()).and_then(fix_statement);
            t.items = s
                .server
                .missing
                .iter()
                .filter(|m| m.capability.is_workload_source() && m.capability != Capability::QueryLog)
                .map(|m| {
                    let mut item = TileItem::new(ItemKind::Capability, m.capability.id(), m.capability.label());
                    item.value = Some("off".into());
                    item.status = Some(TileStatus::Bad);
                    item
                })
                .collect();
            t
        }
        Some(w) => {
            let planned = app_queries(s).filter(|q| q.plan.is_some()).count();
            let window = window_phrase(w);
            let mut status = TileStatus::Good;
            let mut takeaway = if w.log.is_some() {
                // The phrase already names the log: "the 48 minutes the Rails log covers".
                format!("Over {window}: {}, {planned} with plans.", plural(w.query_count as usize, "query", "queries"))
            } else {
                format!(
                    "{} covers {window}: {}, {planned} with plans.",
                    w.source,
                    plural(w.query_count as usize, "query", "queries")
                )
            };
            if let (Some(used), Some(max)) = (
                setting(s, "query_store_options.current_storage_size_mb").and_then(|v| v.parse::<f64>().ok()),
                setting(s, "query_store_options.max_storage_size_mb").and_then(|v| v.parse::<f64>().ok()),
            ) && max > 0.0
                && used / max >= 0.9
                // With size-based cleanup on AUTO it trims its oldest data at 90% and never fills: that's how it runs.
                && !setting(s, "query_store_options.size_based_cleanup_mode_desc")
                    .is_some_and(|v| v.eq_ignore_ascii_case("AUTO"))
            {
                status = TileStatus::Watch;
                takeaway =
                    format!("Query Store is {} full; it stops recording when it fills.", format::percent(used / max));
            }
            // A log without timestamps covers a period nobody can say: not "thin", unknown.
            let short = if w.log.is_some() { !w.window_unknown() } else { !w.is_windowed() };
            if short && w.measurement_hours() < 1.0 {
                status = status.max(TileStatus::Watch);
                takeaway = if w.log.is_some() {
                    format!("Only {window}, so the numbers are thin: a longer log tells more.")
                } else {
                    format!("{} covers only {window}, so the numbers are thin until it has seen more.", w.source)
                };
            }
            if let Some(worst) = stats_findings.iter().rev().max_by_key(|f| f.severity) {
                status =
                    status.max(if worst.severity >= Severity::Medium { TileStatus::Watch } else { TileStatus::Info });
                if status >= TileStatus::Watch && worst.severity >= Severity::Medium {
                    takeaway = format!("{}.", worst.title.trim_end_matches('.'));
                }
            }
            let mut t = Tile::new(TileKey::Statistics, status, takeaway);
            // The slow query log only has statements over long_query_time: say so where its numbers are summarized.
            if w.kind() == Some(culprit_core::WorkloadKind::SlowLog) {
                t.detail = collector_note(s, &[culprit_core::source_id::WORKLOAD]);
            }
            t.metrics = vec![
                TileMetric::new("queries", "Queries", Some(w.query_count), format::count(w.query_count)),
                TileMetric::new("plans", "All queries with plans", Some(planned as f64), planned.to_string()),
                TileMetric::new("window-hours", "Covers", Some(w.measurement_hours()), span(w.measurement_hours())),
            ];
            t.items = stats_findings
                .iter()
                .map(|f| {
                    let mut item = TileItem::new(ItemKind::Finding, f.id.clone(), f.title.clone());
                    item.status = Some(severity_status(f.severity));
                    item.finding_id = Some(f.id.clone());
                    item
                })
                .collect();
            t.fix_sql = stats_findings.first().and_then(|f| f.fix.as_ref()).and_then(|f| f.sql.clone());
            t
        }
    };
    tile.metrics.push(TileMetric::new("locked", "Locked checks", Some(locked as f64), locked.to_string()));
    if locked > 0 && tile.detail.is_none() {
        let needs: Vec<String> = {
            let mut caps: Vec<&str> = report.locked.iter().flat_map(|l| l.missing.iter().map(|c| c.label())).collect();
            caps.sort_unstable();
            caps.dedup();
            caps.into_iter().map(str::to_string).collect()
        };
        tile.detail = Some(format!(
            "{} locked: {} need{} {}.",
            capitalized(&plural(locked, "check is", "checks are")),
            format::list(&report.locked.iter().map(|l| l.check.title.clone()).take(3).collect::<Vec<_>>()),
            if locked == 1 { "s" } else { "" },
            format::list(&needs)
        ));
    }
    tile
}
