//! Health counters describe what was measured. A ratio, a lifetime peak or a shared server counter alone cannot
//! establish pressure in this database. This preparation also corrects grades and wording in saved scans.

use super::*;
use culprit_core::{ResourceSample, Signal, SignalScope};

fn epoch(at: &str) -> Option<i64> {
    parse_epoch_seconds(at).map(|t| t.floor() as i64)
}

pub(super) type ResourcePick = fn(&ResourceSample) -> Option<f64>;

pub(super) struct ResourceUsage {
    pub average: f64,
    pub peak: f64,
    pub at_limit: f64,
    pub hours: f64,
    pub measured: bool,
}

/// Queue and cleanup grades use the final findings, including their amount, scope and confidence rules. A tile
/// cannot independently escalate a one-off queue or a harmless transaction just because it crossed a raw threshold.
pub(crate) fn grade_findings(snapshot: &mut Snapshot, findings: &[Finding]) {
    let Some(p) = snapshot.pressure.as_mut() else { return };
    for s in &mut p.signals {
        let check = match s.key.as_str() {
            "memory-grants-pending" => "memory-queue",
            "threadpool-waiting" if s.label.contains("in this database") => "worker-pressure",
            "oldest-transaction" => "held-cleanup",
            _ => continue,
        };
        let severity = findings.iter().filter(|f| f.check == check && f.is_actionable()).map(|f| f.severity).max();
        if s.value == 0.0 && s.key != "oldest-transaction" {
            s.status = SignalStatus::Good;
            s.healthy =
                Some("No requests in this queue when the scan ran. Historical waits are reported separately.".into());
            continue;
        }
        s.status = match severity {
            Some(Severity::High) => SignalStatus::Bad,
            Some(Severity::Medium) => SignalStatus::Watch,
            _ if s.value == 0.0 => SignalStatus::Good,
            _ => SignalStatus::Info,
        };
        if let Some(f) = findings.iter().find(|f| f.check == check && f.is_actionable()) {
            s.healthy = Some(f.title.clone());
        } else if s.key == "oldest-transaction" {
            s.healthy = Some("A cleanup holder observed at the scan's instant; no long-held-cleanup finding is supported by this scan.".into());
        }
    }
}

/// Azure's samples cover 15 seconds each. A sparse set of points spread over an hour is not an hour of evidence;
/// each dimension needs half an hour of actual samples and at least 80% coverage before it carries a verdict.
pub(super) fn resource_usage(samples: &[ResourceSample], pick: ResourcePick) -> Option<ResourceUsage> {
    let mut values: Vec<_> = samples
        .iter()
        .filter_map(|s| {
            let value = pick(s).filter(|v| v.is_finite() && (0.0..=100.0).contains(v))?;
            let at = epoch(&s.at)?;
            Some((at, value))
        })
        .collect();
    values.sort_by_key(|(at, _)| *at);
    values.dedup_by_key(|(at, _)| *at);
    let seconds = (values.last()?.0 - values.first()?.0 + 15).max(15) as f64;
    let n = values.len() as f64;
    Some(ResourceUsage {
        average: values.iter().map(|(_, v)| v).sum::<f64>() / n,
        peak: values.iter().map(|(_, v)| *v).fold(0.0, f64::max),
        at_limit: values.iter().filter(|(_, v)| *v >= 95.0).count() as f64 / n,
        hours: seconds / 3600.0,
        measured: n >= 120.0 && seconds >= 1800.0 && n * 15.0 / seconds >= 0.8,
    })
}

pub(crate) fn grade_on_evidence(snapshot: &mut Snapshot) {
    let engine = snapshot.server.engine;
    let azure = snapshot.server.on(culprit_core::Platform::AzureSqlDatabase);
    // Never divide lifetime Query Store compilations by the selected window's runs. Older saved scans held a
    // prorated estimate without the original compilation totals: withdraw it rather than presenting it as measured.
    if engine == Engine::Mssql
        && let Some(w) = snapshot.workload.as_mut()
    {
        for q in &mut w.queries {
            if let Some(c) = &mut q.costs {
                if let Some(h) = c.compile_history.as_ref().filter(|h| h.within_window) {
                    c.plan_ms = Some(h.total_ms);
                    c.plans = Some(h.count);
                } else {
                    c.plan_ms = None;
                    c.plans = None;
                }
            }
        }
    }
    let selected = snapshot.options.as_ref().map(|o| o.window_in_minutes());
    let scan_at = epoch(&snapshot.taken_at);
    if let (Some(minutes), Some(end)) = (selected, scan_at) {
        let start = end - i64::from(minutes) * 60;
        snapshot.resources.retain(|s| epoch(&s.at).is_some_and(|t| t > start && t <= end));
        if let Some(d) = &mut snapshot.deadlocks {
            let hours = d.since_hours.min(f64::from(minutes) / 60.0);
            d.deadlocks.retain(|d| {
                d.at.as_deref().and_then(epoch).is_none_or(|t| t > end - (hours * 3600.0) as i64 && t <= end)
            });
            d.since_hours = hours;
        }
    }
    snapshot.resources.retain(|s| epoch(&s.at).is_some());
    snapshot.resources.sort_by_key(|s| epoch(&s.at));
    snapshot.resources.dedup_by_key(|s| epoch(&s.at));
    if snapshot.pressure.is_none() && !snapshot.resources.is_empty() {
        snapshot.pressure = Some(Pressure::default());
    }
    let Some(p) = snapshot.pressure.as_mut() else { return };
    if azure {
        // There is no database-scoped equivalent of the instance's batches/sec or cache page life. Query Store's
        // own compile totals replace the shared compile ratio when available; no server counter stands in for it.
        p.signals.retain(|s| {
            !matches!(
                s.key.as_str(),
                "batch-requests"
                    | "compiles-per-batch"
                    | "recompiles-per-batch"
                    | "page-life"
                    | "new-connections"
                    | "pooled-reuse"
            )
        });
    }
    for r in &mut p.replicas {
        r.lag_seconds = r.effective_lag_seconds(engine);
    }
    if engine == Engine::Mysql
        && let Some(t) = &mut p.oldest_transaction
    {
        t.held_versions = None;
    }
    let counted = p.uptime_hours.is_some_and(|h| h >= culprit_core::RATE_MIN_HOURS);
    for s in &mut p.signals {
        let explanation = match s.key.as_str() {
            "signal-wait" => {
                s.label = "Scheduling share of recorded waits".into();
                Some(
                    "The share of recorded waiting spent ready to run, before a worker was scheduled. This is not CPU utilization and alone does not establish CPU pressure.",
                )
            }
            "cache-hit" => Some(
                "Reads served by PostgreSQL's shared_buffers; misses may be served by the operating system cache. The ratio alone does not establish slow storage or a memory shortage.",
            ),
            "buffer-pool-hit" => Some(
                "The share of reads served by InnoDB's buffer pool across the whole server. A miss ratio alone does not establish a memory shortage in this database.",
            ),
            "page-life" => Some(
                "A server cache counter at the scan's instant. There is no universal healthy number of seconds; cache size, workload and measured I/O matter.",
            ),
            "tmp-disk-tables" => Some(
                "The share of internal temporary tables written to disk across the whole server. Table sizes and the statements' measured cost determine whether this needs work.",
            ),
            "temp-bytes" => Some(
                "Temporary data written by this database over its statistics period. This amount alone does not establish a performance problem; the findings identify costly spills.",
            ),
            "deadlocks" => Some(
                "A count over the statistics period, not deadlocks happening now. The deadlock finding uses the amount and period recorded.",
            ),
            "rollbacks" => Some(
                "Rollbacks can be deliberate application behavior. This historical share alone does not establish failed requests.",
            ),
            "row-lock-avg" => Some(
                "An average across the whole server's recorded row-lock waits, not a current wait. The lock findings use the amount of waiting too.",
            ),
            "max-used-connections" => Some(
                "The busiest moment since these counters started, compared with today's limit. This does not establish a connection shortage now.",
            ),
            "checkpoints-requested" => Some(
                "Requested checkpoints include WAL-triggered checkpoints, manual CHECKPOINT, backups and CREATE DATABASE. This shared historical ratio does not identify their cause or establish current write pressure.",
            ),
            "new-connections" => Some(
                "The historical rate of new connections. A busy application can use a pool and still open connections; the rate alone does not prove pooling is missing.",
            ),
            "session-length" => Some(
                "Average session lifetime over the statistics period. Session length alone does not measure connection overhead or request latency.",
            ),
            "sessions-abandoned" => Some(
                "The share of sessions that ended through a lost client connection over the statistics period, not sessions failing now.",
            ),
            "pooled-reuse" => Some(
                "SQL Server's recorded connection resets compared with new logins, across the server. Some pools reuse a connection without resetting it, so this does not prove whether the application pools connections.",
            ),
            "thread-cache-misses" => Some(
                "The share of connections needing a new server thread across the whole server. This does not measure application connection pooling or its cost.",
            ),
            "version-store" => Some(
                "Space occupied by row versions now. Keeping versions is normal; the amount alone does not establish a problem. Long transactions and storage pressure need their own evidence.",
            ),
            "history-length" => Some(
                "Committed undo logs awaiting purge across the whole server. This is not a count of rows, and a large value alone does not establish a purge problem in this database.",
            ),
            "threadpool-waits" => Some(
                "Average worker wait time over the counters' history; this does not establish a worker shortage now. The worker finding checks the amount and the database's own evidence.",
            ),
            "compiles-per-batch" => Some(
                "New plan compilations compared with batches across the server. Compilation is normal; the ratio alone does not establish pressure in this database.",
            ),
            _ => None,
        };
        if let Some(explanation) = explanation {
            s.healthy = Some(explanation.into());
            s.status = SignalStatus::Info;
        }
        // These counters are shared even when the login connected to just one database.
        let shared = match engine {
            Engine::Yugabyte | Engine::Mysql => !matches!(s.key.as_str(), "oldest-transaction"),
            Engine::Mssql => {
                matches!(
                    s.key.as_str(),
                    "batch-requests"
                        | "compiles-per-batch"
                        | "recompiles-per-batch"
                        | "page-life"
                        | "new-connections"
                        | "pooled-reuse"
                ) || (!azure
                    && matches!(
                        s.key.as_str(),
                        "signal-wait" | "threadpool-waits" | "threadpool-wait-ms" | "grant-wait-ms"
                    ))
            }
            Engine::Postgres => matches!(s.key.as_str(), "connections" | "checkpoints" | "checkpoints-requested"),
            Engine::Sqlite => false,
        };
        if shared {
            if !s.label.contains("whole server") && !s.label.contains("whole instance") {
                s.label.push_str(" (whole server)");
            }
            s.status = SignalStatus::Info;
        }
        if s.scope == SignalScope::SinceStartup && !counted {
            s.status = SignalStatus::Info;
        }
        if matches!(s.key.as_str(), "threadpool-waiting" | "memory-grants-pending") && s.value > 0.0 {
            // One observed queue is worth a look, not proof of sustained starvation.
            s.status = SignalStatus::Watch;
            s.healthy = Some("Requests waiting at the scan's instant. Sample again to establish whether the queue persists before changing settings.".into());
        }
        // A legacy worker count came from the whole instance with no database filter.
        if s.key == "threadpool-waiting" && !s.label.contains("in this database") {
            s.status = SignalStatus::Info;
            if !s.label.contains("whole instance") {
                s.label.push_str(" (whole instance)");
            }
        }
    }
    let dimensions: [(&str, &str, ResourcePick); 5] = [
        ("cpu-use", "CPU used against its limit", |s| s.cpu_pct),
        ("data-io", "Storage I/O used against its limit", |s| s.data_io_pct),
        ("log-write-use", "Log throughput used against its limit", |s| s.log_write_pct),
        ("workers-use", "Worker slots used against their limit", |s| s.workers_pct),
        ("memory-use", "Memory allocated against its limit", |s| s.memory_pct),
    ];
    for (key, label, pick) in dimensions {
        p.signals.retain(|s| s.key != key);
        let Some(u) = resource_usage(&snapshot.resources, pick) else {
            continue;
        };
        let status = if key == "memory-use" || !u.measured {
            SignalStatus::Info
        } else if u.at_limit >= 0.3 {
            SignalStatus::Bad
        } else if u.at_limit >= 0.1 {
            SignalStatus::Watch
        } else {
            SignalStatus::Good
        };
        let covers_selection = selected.zip(scan_at).is_some_and(|(minutes, end)| {
            let first = snapshot.resources.iter().filter(|s| pick(s).is_some()).filter_map(|s| epoch(&s.at)).min();
            let last = snapshot.resources.iter().filter(|s| pick(s).is_some()).filter_map(|s| epoch(&s.at)).max();
            first.is_some_and(|t| t <= end - i64::from(minutes) * 60 + 30) && last.is_some_and(|t| t >= end - 30)
        });
        let coverage = selected
            .map(|m| {
                format!(
                    "{} selected; {} of resource samples available. ",
                    culprit_core::window_label(m),
                    culprit_core::span_words(u.hours)
                )
            })
            .unwrap_or_default();
        let signal = Signal {
            healthy: Some(format!(
                "{coverage}Average over {} of recorded samples; peak {}. At or above 95% of this database's limit in {} of samples. {}",
                culprit_core::span_words(u.hours),
                format::percent_value(u.peak),
                format::percent(u.at_limit),
                if key == "memory-use" {
                    "Allocated memory includes cache; a full cache is normal and is not graded as memory pressure."
                } else if u.measured {
                    "A warning needs at least 10% of samples near the limit."
                } else {
                    "Too little coverage to judge sustained pressure; at least 30 minutes of samples are needed."
                }
            )),
            status,
            ..Signal::new(
                key,
                label,
                u.average,
                SignalUnit::Percent,
                if covers_selection { SignalScope::Window } else { SignalScope::LastHour },
            )
        };
        p.signals.push(signal);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use culprit_core::{SnapshotOptions, signals::CompileHistory};

    fn azure(cpu: impl Fn(usize) -> f64) -> Snapshot {
        let mut s = crate::waits::snapshot_with(Engine::Mssql);
        s.server.cloud = Some("Azure SQL Database".into());
        s.taken_at = "2026-10-11T11:00:00Z".into();
        s.options = Some(SnapshotOptions::default().with_window_minutes(60));
        s.pressure = Some(Pressure { uptime_hours: Some(256.0), ..Pressure::default() });
        let start = parse_epoch_seconds("2026-10-11T10:00:15Z").unwrap();
        s.resources = (0..240)
            .map(|i| ResourceSample {
                at: format::iso_from_epoch(start + i as f64 * 15.0),
                cpu_pct: Some(cpu(i)),
                data_io_pct: Some(0.0),
                log_write_pct: Some(0.0),
                workers_pct: Some(0.2),
                memory_pct: Some(20.0),
            })
            .collect();
        s
    }

    #[test]
    fn an_idle_database_uses_measured_cpu_and_cannot_be_alarmed_by_its_wait_ratio() {
        let mut s = azure(|_| 2.47);
        s.pressure.as_mut().unwrap().signals = vec![
            Signal::new("signal-wait", "Waiting for CPU", 20.04, SignalUnit::Percent, SignalScope::SinceStartup)
                .graded("under 20%", 20.0, 35.0, true),
            Signal::new("compiles-per-batch", "Plans compiled", 90.0, SignalUnit::Percent, SignalScope::SinceStartup)
                .graded("under 10%", 10.0, 25.0, true),
        ];
        let r = crate::analyze(s);
        let p = r.snapshot.pressure.as_ref().unwrap();
        let cpu = p.signal("cpu-use").unwrap();
        assert!((cpu.value - 2.47).abs() < 1e-9);
        assert_eq!((cpu.status, cpu.scope), (SignalStatus::Good, SignalScope::Window));
        assert_eq!(p.signal("signal-wait").unwrap().status, SignalStatus::Info);
        assert!(p.signal("compiles-per-batch").is_none(), "never substitute another database's compile ratio");
        assert!(!r.findings.iter().any(|f| f.check == "tier-limit"));
    }

    #[test]
    fn a_quiet_hour_keeps_its_measured_cpu_instead_of_displaying_zero() {
        let s = azure(|i| match i {
            0 => 3.86,
            1 => 0.91,
            2 => 0.57,
            3..=4 => 0.02,
            5..=12 => 0.01,
            _ => 0.0,
        });
        let r = crate::analyze(s);
        let cpu = r.snapshot.pressure.as_ref().unwrap().signal("cpu-use").unwrap();
        assert!((cpu.value - 0.02275).abs() < 1e-9);
        assert_eq!(cpu.status, SignalStatus::Good);
        assert_eq!(crate::views::dashboard::signal_value(cpu), "0.02%");
        assert!(cpu.healthy.as_ref().unwrap().contains("peak 3.86%"));
        assert!(!r.findings.iter().any(|f| f.check == "tier-limit"));
    }

    #[test]
    fn the_selected_window_changes_resource_values_and_only_real_sustained_pressure_warns() {
        let s = azure(|i| if i < 180 { 100.0 } else { 3.0 });
        let whole = crate::analyze(s.clone());
        assert_eq!(whole.snapshot.pressure.as_ref().unwrap().signal("cpu-use").unwrap().status, SignalStatus::Bad);
        assert!(whole.findings.iter().any(|f| f.id == "tier-limit:cpu" && f.severity == Severity::High));
        let mut legacy = s.clone();
        legacy.server.cloud = None;
        let legacy = crate::analyze(legacy);
        assert_eq!(
            legacy.snapshot.pressure.as_ref().unwrap().signal("cpu-use").unwrap().status,
            SignalStatus::Bad,
            "saved resource samples and their findings must agree even without platform metadata"
        );
        let mut recent = s;
        recent.options = Some(SnapshotOptions::default().with_window_minutes(15));
        let recent = crate::analyze(recent);
        let cpu = recent.snapshot.pressure.as_ref().unwrap().signal("cpu-use").unwrap();
        assert_eq!((cpu.value, cpu.status), (3.0, SignalStatus::Info));
        assert_eq!(recent.snapshot.resources.len(), 60);
        assert!(!recent.findings.iter().any(|f| f.check == "tier-limit"));
    }

    #[test]
    fn a_peak_sparse_points_or_missing_dimension_does_not_establish_sustained_pressure() {
        let mut s = azure(|i| if i == 0 { 100.0 } else { 2.0 });
        grade_on_evidence(&mut s);
        assert_eq!(s.pressure.as_ref().unwrap().signal("cpu-use").unwrap().status, SignalStatus::Good);
        s.resources = s.resources.into_iter().step_by(12).collect();
        for x in &mut s.resources {
            x.cpu_pct = Some(100.0);
        }
        grade_on_evidence(&mut s);
        assert_eq!(s.pressure.as_ref().unwrap().signal("cpu-use").unwrap().status, SignalStatus::Info);
        assert!(super::super::tier_limit(&s).is_empty());
        let mut s = azure(|_| 100.0);
        for x in s.resources.iter_mut().skip(8) {
            x.cpu_pct = None;
        }
        grade_on_evidence(&mut s);
        assert_eq!(s.pressure.as_ref().unwrap().signal("cpu-use").unwrap().status, SignalStatus::Info);
        assert!(super::super::tier_limit(&s).is_empty());
    }

    #[test]
    fn a_long_selection_does_not_relabel_one_hour_of_samples_as_a_week() {
        let mut s = azure(|_| 3.0);
        s.options = Some(SnapshotOptions::default().with_window_minutes(7 * 1440));
        grade_on_evidence(&mut s);
        let cpu = s.pressure.as_ref().unwrap().signal("cpu-use").unwrap();
        assert_eq!(cpu.scope, SignalScope::LastHour);
        assert!(cpu.healthy.as_ref().unwrap().contains("60 minutes of resource samples available"));
    }

    #[test]
    fn historical_ratios_sizes_and_peaks_are_context_even_after_a_long_uptime() {
        for engine in [Engine::Mssql, Engine::Postgres, Engine::Mysql] {
            let mut s = crate::waits::snapshot_with(engine);
            let keys = [
                "signal-wait",
                "cache-hit",
                "buffer-pool-hit",
                "page-life",
                "tmp-disk-tables",
                "temp-bytes",
                "deadlocks",
                "rollbacks",
                "row-lock-avg",
                "max-used-connections",
                "checkpoints-requested",
                "new-connections",
                "session-length",
                "sessions-abandoned",
                "pooled-reuse",
                "thread-cache-misses",
                "version-store",
                "history-length",
                "threadpool-waits",
                "compiles-per-batch",
            ];
            s.pressure = Some(Pressure {
                uptime_hours: Some(256.0),
                signals: keys
                    .iter()
                    .map(|key| {
                        Signal::new(key, key, 99.0, SignalUnit::Percent, SignalScope::SinceStartup)
                            .graded("bad", 1.0, 2.0, true)
                    })
                    .collect(),
                ..Pressure::default()
            });
            grade_on_evidence(&mut s);
            assert!(s.pressure.unwrap().signals.iter().all(|s| s.status == SignalStatus::Info), "{engine}");
        }
    }

    #[test]
    fn full_memory_allocation_and_an_empty_current_queue_do_not_assert_pressure() {
        let mut s = azure(|_| 2.0);
        for sample in &mut s.resources {
            sample.memory_pct = Some(100.0);
        }
        let p = s.pressure.as_mut().unwrap();
        p.signals.push(Signal::new(
            "memory-grants-pending",
            "Requests waiting for memory",
            0.0,
            SignalUnit::Count,
            SignalScope::Now,
        ));
        grade_on_evidence(&mut s);
        grade_findings(&mut s, &[]);
        let p = s.pressure.unwrap();
        assert_eq!(p.signal("memory-use").unwrap().status, SignalStatus::Info);
        assert_eq!(p.signal("memory-grants-pending").unwrap().status, SignalStatus::Good);
    }

    #[test]
    fn old_compilation_estimates_cannot_be_mixed_with_the_selected_period() {
        let mut s = azure(|_| 2.0);
        let q = &mut s.workload.as_mut().unwrap().queries[0];
        q.costs =
            Some(culprit_core::QueryCosts { plan_ms: Some(1_000_000.0), plans: Some(500.0), ..Default::default() });
        grade_on_evidence(&mut s);
        assert!(s.queries()[0].costs.as_ref().unwrap().plan_ms.is_none());
        let c = s.workload.as_mut().unwrap().queries[0].costs.as_mut().unwrap();
        c.compile_history =
            Some(CompileHistory { count: 50.0, total_ms: 5000.0, within_window: true, ..Default::default() });
        grade_on_evidence(&mut s);
        assert_eq!(s.queries()[0].costs.as_ref().unwrap().plan_ms, Some(5000.0));
    }
}
