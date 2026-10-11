//! Pressure checks: memory grants, compile load, worker and connection limits, open transactions that hold cleanup
//! back, results the application reads slowly, cloud tier limits, forced checkpoints and row-lock waits. Pure functions
//! of the snapshot's waits, pressure signals and resource samples.

use culprit_core::waits::category_key;
use culprit_core::{
    Category, CheckInfo, Confidence, Engine, Finding, Fix, Metric, Pressure, QueryStat, Severity, SignalUnit, Snapshot,
    SubjectKind, Unit, format,
};

use crate::support::*;

const MB: f64 = 1024.0;

fn pressure(snapshot: &Snapshot) -> Option<&Pressure> {
    snapshot.pressure.as_ref()
}

fn signal(snapshot: &Snapshot, key: &str) -> Option<f64> {
    pressure(snapshot)?.signal(key).map(|s| s.value)
}

fn uptime_phrase(snapshot: &Snapshot) -> String {
    let engine = snapshot.server.name();
    match pressure(snapshot).and_then(|p| p.uptime_hours) {
        Some(h) if h >= 47.0 => format!("since {engine} started {} days ago", (h / 24.0).round()),
        Some(h) if h >= 1.5 => format!("since {engine} started {} hours ago", h.round()),
        Some(h) => format!("since {engine} started {} minutes ago", (h * 60.0).round().max(1.0)),
        None => format!("since {engine} started"),
    }
}

fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    chars.next().map(|c| c.to_uppercase().collect::<String>() + chars.as_str()).unwrap_or_default()
}

fn server_key(snapshot: &Snapshot) -> String {
    snapshot.connection.clone()
}

fn kb(value: f64) -> String {
    format::bytes(value * 1024.0)
}

fn by_hash<'a>(snapshot: &'a Snapshot, hash: &str) -> Option<&'a QueryStat> {
    app_queries(snapshot).find(|q| q.query_hash.as_deref().is_some_and(|h| h.eq_ignore_ascii_case(hash)))
}

// ── memory-grant ───────────────────────────────────────────────────────────────────────────────────

pub fn memory_grant_info() -> CheckInfo {
    CheckInfo {
        id: "memory-grant".into(),
        title: "Queries that reserve far more memory than they use".into(),
        category: Category::Queries,
        engines: vec![Engine::Mssql],
        requires: vec![],
        description: "Statements whose memory grant (reserved before they run, for sorts and hashes) is many times what they use, so other queries queue for memory.".into(),
    }
}

pub fn memory_grant(snapshot: &Snapshot) -> Vec<Finding> {
    let Some(p) = pressure(snapshot) else { return Vec::new() };
    let queueing = signal(snapshot, "memory-grants-pending").unwrap_or(0.0) > 0.0;
    let mut findings: Vec<Finding> = p
        .grants
        .iter()
        .filter(|g| g.executions >= 5.0 && g.avg_granted_kb >= 50.0 * MB && g.avg_used_kb <= g.avg_granted_kb * 0.25)
        .map(|g| {
            let q = by_hash(snapshot, &g.query_hash);
            let key = q.map(|q| q.id.clone()).unwrap_or_else(|| g.query_hash.clone());
            let mut f = finding("memory-grant", &key, Category::Queries, subject(SubjectKind::Query, &key));
            let handle = q.map(query_handle).unwrap_or_else(|| "a query".into());
            f.title = format!(
                "{} reserves {} of memory per run and uses {}",
                capitalize(&handle),
                kb(g.avg_granted_kb),
                kb(g.avg_used_kb)
            );
            let used = if g.avg_granted_kb > 0.0 { g.avg_used_kb / g.avg_granted_kb } else { 0.0 };
            f.summary = format!(
                "Before each run SQL Server sets aside {} to sort and hash, sized by how many rows it expects; runs use {} ({}). \
                 While a run holds memory it doesn't use, other queries can queue for it (RESOURCE_SEMAPHORE). It ran {} times since its plan was cached; the largest grant was {}.",
                kb(g.avg_granted_kb),
                kb(g.avg_used_kb),
                format::percent(used),
                format::count(g.executions),
                kb(g.max_granted_kb)
            );
            if let Some(q) = q
                && let Some(impact) = query_impact(snapshot, &[q])
            {
                apply_query_impact(&mut f, &impact);
                f.queries = query_refs(&[q]);
            }
            let wasted_gb = (g.avg_granted_kb - g.avg_used_kb) / MB / MB;
            f.severity = if queueing && wasted_gb >= 1.0 {
                Severity::High
            } else if queueing || wasted_gb >= 0.5 {
                Severity::Medium
            } else {
                Severity::Low
            };
            f.impact.score = f.impact.score.max(base_score(f.severity));
            f.impact.label = format!("{} reserved, {} used", kb(g.avg_granted_kb), kb(g.avg_used_kb));
            f.confidence = Confidence::High;
            f.metrics = vec![
                Metric::new("Reserved per run", g.avg_granted_kb * 1024.0, Unit::Bytes),
                Metric::new("Used per run", g.avg_used_kb * 1024.0, Unit::Bytes),
                Metric::new("Runs", g.executions, Unit::Count),
            ];
            f.fix = Some(Fix {
                summary: "Fix the row estimate that sizes the grant: refresh statistics on the tables it reads and look in its plan for estimates far above actual rows. On SQL Server 2019+ (compatibility level 150+) memory grant feedback shrinks repeated grants by itself. As a stopgap, cap the grant on this statement.".into(),
                sql: Some("-- Stopgap, appended to the statement: limit its grant to 5% of query memory\nOPTION (MAX_GRANT_PERCENT = 5);".into()),
                cost: None,
            });
            f.verify = vec!["Re-scan: memory reserved per run should be close to memory used.".into()];
            f
        })
        .collect();
    findings.sort_by(|a, b| b.impact.score.total_cmp(&a.impact.score));
    findings.truncate(5);
    findings
}

// ── memory-queue ───────────────────────────────────────────────────────────────────────────────────

pub fn memory_queue_info() -> CheckInfo {
    CheckInfo {
        id: "memory-queue".into(),
        title: "Queries waiting in line for memory".into(),
        category: Category::Configuration,
        engines: vec![Engine::Mssql],
        requires: vec![],
        description: "Memory-grant waits (RESOURCE_SEMAPHORE) take a real share of waiting, or queries were queueing for a grant when the scan ran.".into(),
    }
}

pub fn memory_queue(snapshot: &Snapshot) -> Vec<Finding> {
    let total: f64 = snapshot.waits.iter().map(|w| w.wait_ms).sum();
    let memory: f64 = snapshot.waits.iter().filter(|w| category_key(&w.category) == "memory").map(|w| w.wait_ms).sum();
    let share = if total > 0.0 { memory / total } else { 0.0 };
    let pending = signal(snapshot, "memory-grants-pending").unwrap_or(0.0);
    if share < 0.05 && pending <= 0.0 {
        return Vec::new();
    }
    let mut f = finding(
        "memory-queue",
        &server_key(snapshot),
        Category::Configuration,
        subject(SubjectKind::Database, server_key(snapshot)),
    );
    f.title = if pending > 0.0 {
        format!("{} queries were waiting in line for memory when the scan ran", format::count(pending))
    } else {
        format!("Waiting for memory takes {} of all waiting", format::percent(share))
    };
    let mut parts = Vec::new();
    if share > 0.0 {
        parts.push(format!("Memory-grant waits are {} of all waiting.", format::percent(share)));
    }
    if pending > 0.0 {
        parts.push(format!(
            "When the scan ran, {} queries were waiting for a grant before they could start.",
            format::count(pending)
        ));
    }
    parts.push("A query needs its memory grant to sort or hash before it starts; when the pool is spoken for, the rest wait (RESOURCE_SEMAPHORE). The usual cause is a few queries that reserve far more than they use.".into());
    f.summary = parts.join(" ");
    f.severity = if share >= 0.2 { Severity::High } else { Severity::Medium };
    f.impact.score = base_score(f.severity) + share * 40.0;
    f.impact.label = if share > 0.0 {
        format!("{} of waits", format::percent(share))
    } else {
        format!("{} waiting", format::count(pending))
    };
    f.confidence = Confidence::High;
    f.metrics.push(Metric::new("Share of waits", share, Unit::Ratio));
    if pending > 0.0 {
        f.metrics.push(Metric::new("Waiting now", pending, Unit::Count));
    }
    f.fix = Some(Fix {
        summary: "Shrink the biggest over-sized grants first (see the memory-grant findings), then look at plans that sort or hash large scans. Raising max server memory, or the tier, treats the symptom.".into(),
        sql: None,
        cost: None,
    });
    f.verify = vec!["Re-scan, or sample Right now in Waits: memory waits should fall under 1% and nothing should be waiting for a grant.".into()];
    vec![f]
}

// ── compile-pressure ───────────────────────────────────────────────────────────────────────────────

pub fn compile_pressure_info() -> CheckInfo {
    CheckInfo {
        id: "compile-pressure".into(),
        title: "Plans compiled over and over".into(),
        category: Category::Configuration,
        engines: vec![Engine::Mssql],
        requires: vec![],
        description:
            "A large share of batches compile a new plan instead of reusing one, which costs CPU on every call.".into(),
    }
}

pub fn compile_pressure(snapshot: &Snapshot) -> Vec<Finding> {
    let Some(ratio) = signal(snapshot, "compiles-per-batch") else { return Vec::new() };
    // Without the server's uptime (Azure SQL Database) there is no rate, so the ratio alone has to be clearly high.
    let batches = signal(snapshot, "batch-requests");
    match batches {
        Some(b) if ratio < 15.0 || b < 5.0 => return Vec::new(),
        None if ratio < 25.0 => return Vec::new(),
        _ => {}
    }
    let compiles_per_min = batches.map(|b| b * ratio / 100.0 * 60.0);
    let mut f = finding(
        "compile-pressure",
        &server_key(snapshot),
        Category::Configuration,
        subject(SubjectKind::Database, server_key(snapshot)),
    );
    f.title = match compiles_per_min {
        Some(per_min) => {
            format!(
                "SQL Server compiles a new plan for {:.0}% of batches (about {} a minute)",
                ratio,
                format::count(per_min)
            )
        }
        None => format!("SQL Server compiles a new plan for {:.0}% of batches", ratio),
    };
    f.summary = format!(
        "{}, {} compiled a plan for {:.0}% of them. Healthy systems reuse plans and compile for under 10%. \
         Compiling costs CPU on every call; this usually means the application sends statements with literal values instead of parameters.",
        capitalize(&uptime_phrase(snapshot)),
        batches
            .map(|b| format!("it averaged {}/s batches and", format::count(b)))
            .unwrap_or_else(|| "across all batches it".into()),
        ratio
    );
    f.severity = if ratio >= 40.0 { Severity::High } else { Severity::Medium };
    f.impact.score = base_score(f.severity) + ratio / 5.0;
    f.impact.label = format!("{:.0}% of batches compile", ratio);
    f.confidence = Confidence::Medium;
    f.metrics = vec![Metric::new("Compiles per 100 batches", ratio, Unit::Count)];
    if let Some(per_min) = compiles_per_min {
        f.metrics.push(Metric::new("Compiles per minute", per_min, Unit::Count));
    }
    let azure = snapshot.server.cloud.as_deref() == Some("Azure SQL Database");
    f.fix = Some(Fix {
        summary: "Send parameters instead of literal values (parameterized queries in the driver or ORM). Meanwhile, cache one-off plans more cheaply so they stop crowding out the reusable ones.".into(),
        sql: Some(if azure {
            "ALTER DATABASE SCOPED CONFIGURATION SET OPTIMIZE_FOR_AD_HOC_WORKLOADS = ON;".into()
        } else {
            "EXEC sys.sp_configure N'show advanced options', 1; RECONFIGURE;\nEXEC sys.sp_configure N'optimize for ad hoc workloads', 1; RECONFIGURE;".into()
        }),
        cost: None,
    });
    f.caveats = vec!["Forced parameterization (ALTER DATABASE … SET PARAMETERIZATION FORCED) also cuts compiles, but it changes plans for every query: test it first.".into()];
    f.verify =
        vec!["Sample Right now in Waits (or re-scan): compilations should fall under 10% of batch requests.".into()];
    vec![f]
}

// ── worker-pressure ────────────────────────────────────────────────────────────────────────────────

pub fn worker_pressure_info() -> CheckInfo {
    CheckInfo {
        id: "worker-pressure".into(),
        title: "Running out of workers or connections".into(),
        category: Category::Configuration,
        engines: vec![Engine::Mssql, Engine::Postgres, Engine::Mysql],
        requires: vec![],
        description: "SQL Server sessions waiting for a worker thread (THREADPOOL), or PostgreSQL/MySQL connections close to max_connections.".into(),
    }
}

pub fn worker_pressure(snapshot: &Snapshot) -> Vec<Finding> {
    let key = server_key(snapshot);
    match snapshot.server.engine {
        Engine::Mssql => {
            let waiting = signal(snapshot, "threadpool-waiting").unwrap_or(0.0);
            let per_hour = signal(snapshot, "threadpool-waits").unwrap_or(0.0);
            if waiting <= 0.0 && per_hour < 10_000.0 {
                return Vec::new();
            }
            let mut f = finding("worker-pressure", &key, Category::Configuration, subject(SubjectKind::Database, &key));
            f.title = "Requests wait for a free worker thread before they can start (THREADPOOL)".into();
            f.summary = format!(
                "{}{}. A request that can't get a worker can't even begin, so the app sees timeouts while the server looks idle. \
                 It is almost always a symptom: a blocking chain holding many workers, or many parallel queries each taking several.",
                if per_hour > 0.0 {
                    format!(
                        "SQL Server waited {} an hour for workers {}",
                        format::duration(per_hour),
                        uptime_phrase(snapshot)
                    )
                } else {
                    "SQL Server ran short of worker threads".into()
                },
                if waiting > 0.0 {
                    format!(", and {} tasks were waiting when the scan ran", format::count(waiting))
                } else {
                    String::new()
                }
            );
            f.severity = Severity::High;
            f.impact.score = base_score(Severity::High);
            f.impact.label = "THREADPOOL waits".into();
            f.confidence = Confidence::High;
            f.fix = Some(Fix {
                summary: "Find the head blocker in Live and end the transaction behind the chain; then lower parallelism so each query takes fewer workers (cost threshold for parallelism, MAXDOP). Raising max worker threads only hides it.".into(),
                sql: None,
                cost: None,
            });
            f.verify =
                vec!["Re-scan: THREADPOOL waits should stop growing and nothing should wait for a worker.".into()];
            vec![f]
        }
        Engine::Postgres | Engine::Mysql => {
            let Some(used) = signal(snapshot, "connections") else { return Vec::new() };
            let hit_limit = signal(snapshot, "max-used-connections").is_some_and(|v| v >= 100.0);
            if used < 85.0 && !hit_limit {
                return Vec::new();
            }
            let mut f = finding(
                "worker-pressure",
                &key,
                Category::Configuration,
                subject(SubjectKind::Setting, "max_connections"),
            );
            f.title = if used >= 85.0 {
                format!("Connections are at {:.0}% of max_connections", used)
            } else {
                format!("{} hit its connection limit {}", snapshot.server.name(), uptime_phrase(snapshot))
            };
            f.summary = format!(
                "When every connection slot is taken, new connections are refused and the application errors out. {} \
                 Each connection costs memory, so the fix is fewer, busier connections rather than a higher limit.",
                if snapshot.server.engine == Engine::Postgres {
                    "Idle connections still hold a backend process each."
                } else {
                    "Idle connections still hold a thread and buffers each."
                }
            );
            f.severity = if used >= 95.0 || hit_limit { Severity::High } else { Severity::Medium };
            f.impact.score = base_score(f.severity);
            f.impact.label = format!("{:.0}% of connections", used);
            f.confidence = Confidence::High;
            f.metrics = vec![Metric::new("Connections in use", used / 100.0, Unit::Ratio)];
            f.fix = Some(Fix {
                summary: if snapshot.server.engine == Engine::Postgres {
                    "Put a connection pooler in front (PgBouncer, or the application's pool with a smaller maximum) and close idle sessions. Raise max_connections only with memory to spare (it needs a restart).".into()
                } else {
                    "Size the application's connection pools to what the server can run (a few per core), close idle sessions, and use ProxySQL or the cloud proxy if many clients connect. Raise max_connections only with memory to spare.".into()
                },
                sql: None,
                cost: None,
            });
            f.verify = vec!["Re-scan: connections in use should stay under 80% of max_connections.".into()];
            vec![f]
        }
    }
}

// ── held-cleanup ───────────────────────────────────────────────────────────────────────────────────

pub fn held_cleanup_info() -> CheckInfo {
    CheckInfo {
        id: "held-cleanup".into(),
        title: "A long open transaction holds cleanup back".into(),
        category: Category::Maintenance,
        engines: vec![Engine::Mssql, Engine::Postgres, Engine::Mysql],
        requires: vec![],
        description: "The oldest open transaction keeps old row versions alive: the SQL Server version store, PostgreSQL's VACUUM horizon, or InnoDB's undo history.".into(),
    }
}

pub fn held_cleanup(snapshot: &Snapshot) -> Vec<Finding> {
    let Some(t) = pressure(snapshot).and_then(|p| p.oldest_transaction.as_ref()) else { return Vec::new() };
    let engine = snapshot.server.engine;
    let held_gb = t.held_bytes.unwrap_or(0.0) / 1024.0 / 1024.0 / 1024.0;
    let versions = t.held_versions.unwrap_or(0.0);
    let old = t.seconds >= 1800.0;
    let big = match engine {
        Engine::Mssql => held_gb >= 1.0 && t.seconds >= 300.0,
        Engine::Mysql => versions >= 1_000_000.0,
        Engine::Postgres => false,
    };
    if !old && !big {
        return Vec::new();
    }
    let open = format::duration(t.seconds * 1000.0);
    let idle = t.state.as_deref().is_some_and(|s| s.contains("idle"));
    let mut f = finding(
        "held-cleanup",
        &t.session,
        Category::Maintenance,
        subject(SubjectKind::Database, server_key(snapshot)),
    );
    f.title = match engine {
        Engine::Mssql => match t.held_bytes {
            Some(b) if b > 0.0 => format!(
                "Session {} has kept a transaction open for {open}; the version store holds {}",
                t.session,
                format::bytes(b)
            ),
            _ => format!("Session {} has kept a transaction open for {open}", t.session),
        },
        Engine::Postgres => format!(
            "Session {} has kept a transaction open for {open}{}, so VACUUM can't clean up behind it",
            t.session,
            if idle { " doing nothing" } else { "" }
        ),
        Engine::Mysql => match t.held_versions {
            Some(v) if v > 0.0 => format!(
                "A transaction in session {} has been open for {open}; InnoDB keeps {} old row versions for it",
                t.session,
                format::count(v)
            ),
            _ => format!("A transaction in session {} has been open for {open}", t.session),
        },
    };
    let program =
        t.program.as_deref().filter(|p| !p.is_empty()).map(|p| format!(" It comes from {p}.")).unwrap_or_default();
    f.summary = match engine {
        Engine::Mssql => format!(
            "While a transaction that reads row versions stays open, SQL Server keeps every version made since it started in tempdb's version store, and reads have to walk past them.{program} Short transactions keep the version store small."
        ),
        Engine::Postgres => format!(
            "PostgreSQL can't remove rows deleted or updated after the oldest open transaction began, so tables and indexes bloat and scans read dead rows.{}{program}",
            if idle { " This one is idle: the application opened it and never committed." } else { "" }
        ),
        Engine::Mysql => format!(
            "InnoDB keeps old versions of changed rows until the oldest transaction that might read them ends; the longer the history, the slower reads and purge get.{program}"
        ),
    };
    f.severity = if t.seconds >= 3.0 * 3600.0 || held_gb >= 10.0 || versions >= 5_000_000.0 {
        Severity::High
    } else {
        Severity::Medium
    };
    f.impact.score = base_score(f.severity);
    f.impact.label = format!("open {open}");
    f.confidence = Confidence::High;
    f.metrics.push(Metric::new("Open for", t.seconds * 1000.0, Unit::Ms));
    if let Some(b) = t.held_bytes.filter(|b| *b > 0.0) {
        f.metrics.push(Metric::new("Version store", b, Unit::Bytes));
    }
    if let Some(v) = t.held_versions.filter(|v| *v > 0.0) {
        f.metrics.push(Metric::new("Old row versions", v, Unit::Count));
    }
    let end = match engine {
        Engine::Mssql => format!("KILL {};", t.session),
        Engine::Postgres => format!("SELECT pg_terminate_backend({});", t.session),
        Engine::Mysql => format!("KILL {};", t.session),
    };
    f.fix = Some(Fix {
        summary: "Find out what the application does in that session and make it commit or roll back promptly; transactions should never wait on a person or a slow call. After checking it is safe, a DBA can end the session, which rolls its transaction back.".into(),
        sql: Some(format!("-- Ends the session and rolls back its open transaction\n{end}")),
        cost: Some("Ending the session discards that transaction's uncommitted changes.".into()),
    });
    f.verify = vec!["Re-scan: the oldest open transaction should be minutes old, not hours.".into()];
    vec![f]
}

// ── network-wait ───────────────────────────────────────────────────────────────────────────────────

pub fn network_wait_info() -> CheckInfo {
    CheckInfo {
        id: "network-wait".into(),
        title: "Results the application reads slowly".into(),
        category: Category::Queries,
        engines: vec![Engine::Mssql],
        requires: vec![],
        description: "Queries that spend much of their time waiting for the application to take results (ASYNC_NETWORK_IO), usually because they return large result sets.".into(),
    }
}

pub fn network_wait(snapshot: &Snapshot) -> Vec<Finding> {
    let mut findings: Vec<Finding> = app_queries(snapshot)
        .filter_map(|q| {
            let network: f64 = q.waits.iter().filter(|w| category_key(&w.category) == "network").map(|w| w.wait_ms).sum();
            let share = if q.total_time_ms > 0.0 { network / q.total_time_ms } else { 0.0 };
            let impact = query_impact(snapshot, &[q])?;
            if share < 0.25 || impact.share < 0.01 {
                return None;
            }
            let rows = q.rows.filter(|_| q.executions > 0.0).map(|r| r / q.executions);
            let mut f = finding("network-wait", &q.id, Category::Queries, subject(SubjectKind::Query, &q.id));
            apply_query_impact(&mut f, &impact);
            f.title = match rows {
                Some(r) if r >= 1_000.0 => format!(
                    "{} waits on the application {} of its time while sending {} rows per call",
                    capitalize(&query_handle(q)),
                    format::percent(share),
                    format::count(r)
                ),
                _ => format!("{} waits on the application {} of its time", capitalize(&query_handle(q)), format::percent(share)),
            };
            f.summary = format!(
                "SQL Server had its results ready but the application took them slowly (ASYNC_NETWORK_IO), so each run holds its locks and memory longer. \
                 That is {} of this query's time. Usually the result set is bigger than needed, or the application does work for each row before reading the next.",
                format::percent(share)
            );
            f.severity = severity_for_share(impact.share * share);
            f.confidence = Confidence::High;
            f.queries = query_refs(&[q]);
            f.metrics.push(Metric::new("Waiting on the app", share, Unit::Ratio));
            if let Some(r) = rows {
                f.metrics.push(Metric::new("Rows per call", r, Unit::Rows));
            }
            f.fix = Some(Fix {
                summary: "Return fewer rows (filter, or page with OFFSET/FETCH or keyset pagination), or read them faster: stream the results and do per-row work after the read, not while the result set is open.".into(),
                sql: None,
                cost: None,
            });
            f.verify = vec!["Re-scan: this query's Sending results (network) share should drop.".into()];
            Some(f)
        })
        .collect();
    findings.sort_by(|a, b| b.impact.score.total_cmp(&a.impact.score));
    findings.truncate(5);
    findings
}

// ── tier-limit ─────────────────────────────────────────────────────────────────────────────────────

pub fn tier_limit_info() -> CheckInfo {
    CheckInfo {
        id: "tier-limit".into(),
        title: "Hitting the cloud tier's limits".into(),
        category: Category::Configuration,
        engines: vec![Engine::Mssql],
        requires: vec![],
        description: "Azure SQL Database resources (CPU, data IO, log writes, workers) at the tier's limit for a real part of the last hour.".into(),
    }
}

pub fn tier_limit(snapshot: &Snapshot) -> Vec<Finding> {
    let samples = &snapshot.resources;
    if samples.len() < 8 {
        return Vec::new();
    }
    type Pick = fn(&culprit_core::ResourceSample) -> Option<f64>;
    let dims: [(&str, &str, Pick, &str, &str); 4] = [
        (
            "log-write",
            "Log writes",
            |s| s.log_write_pct,
            "writes are throttled",
            "Batch small writes into fewer transactions and avoid row-by-row changes; then consider a tier with more log throughput.",
        ),
        (
            "cpu",
            "CPU",
            |s| s.cpu_pct,
            "queries queue for a processor",
            "Fix the queries that use the most CPU first (Waits → CPU lists them); then consider more vCores.",
        ),
        (
            "data-io",
            "Data reads",
            |s| s.data_io_pct,
            "reads from disk are throttled",
            "Cut scans with the missing-index findings and check memory; then consider a tier with more IO.",
        ),
        (
            "workers",
            "Worker threads",
            |s| s.workers_pct,
            "new requests fail",
            "Look for blocking chains and heavy parallelism (they hold many workers); then consider a bigger tier.",
        ),
    ];
    let span_hours = days_between(&samples[0].at, &samples[samples.len() - 1].at).map(|d| d * 24.0).unwrap_or(1.0);
    let span =
        if span_hours >= 1.5 { format!("the last {} hours", span_hours.round()) } else { "the last hour".into() };
    dims.iter()
        .filter_map(|(key, label, pick, effect, fix)| {
            let values: Vec<f64> = samples.iter().filter_map(pick).collect();
            if values.len() < 8 {
                return None;
            }
            let at_limit = values.iter().filter(|v| **v >= 95.0).count() as f64 / values.len() as f64;
            if at_limit < 0.10 {
                return None;
            }
            let peak = values.iter().cloned().fold(0.0, f64::max);
            let mut f = finding("tier-limit", key, Category::Configuration, subject(SubjectKind::Database, server_key(snapshot)));
            f.title = format!("{label} hit the tier's limit {} of {span}: {effect}", format::percent(at_limit));
            f.summary = format!(
                "Azure caps each tier's {}. In {span} it was at 95% or more of that cap in {} of the 15-second samples (peak {:.0}%). At the cap, Azure makes requests wait rather than fail, so it shows up as slowness.",
                label.to_lowercase(),
                format::percent(at_limit),
                peak
            );
            f.severity = if at_limit >= 0.3 { Severity::High } else { Severity::Medium };
            f.impact.score = base_score(f.severity) + at_limit * 30.0;
            f.impact.label = format!("at limit {}", format::percent(at_limit));
            f.confidence = Confidence::High;
            f.metrics = vec![Metric::new("Time at the limit", at_limit, Unit::Ratio), Metric::new("Peak", peak / 100.0, Unit::Ratio)];
            f.fix = Some(Fix { summary: (*fix).into(), sql: None, cost: None });
            f.verify = vec![format!("Re-scan: {} should stay under 95% for most of the hour (the Health strip on Overview shows it).", label.to_lowercase())];
            Some(f)
        })
        .collect()
}

// ── checkpoint-pressure ────────────────────────────────────────────────────────────────────────────

pub fn checkpoint_pressure_info() -> CheckInfo {
    CheckInfo {
        id: "checkpoint-pressure".into(),
        title: "Checkpoints forced early".into(),
        category: Category::Configuration,
        engines: vec![Engine::Postgres],
        requires: vec![],
        description:
            "Many checkpoints start because WAL filled max_wal_size rather than on schedule, causing write spikes."
                .into(),
    }
}

pub fn checkpoint_pressure(snapshot: &Snapshot) -> Vec<Finding> {
    let (Some(requested), Some(total)) = (signal(snapshot, "checkpoints-requested"), signal(snapshot, "checkpoints"))
    else {
        return Vec::new();
    };
    if requested < 30.0 || total < 10.0 {
        return Vec::new();
    }
    let current = snapshot.settings.iter().find(|s| s.name == "max_wal_size").map(|s| s.value.clone());
    let mut f = finding(
        "checkpoint-pressure",
        "max_wal_size",
        Category::Configuration,
        subject(SubjectKind::Setting, "max_wal_size"),
    );
    f.title = format!("{:.0}% of checkpoints are forced early because WAL fills max_wal_size", requested);
    f.summary = format!(
        "A checkpoint writes every changed page to disk. On schedule (checkpoint_timeout) the work is spread out; forced by a full max_wal_size{} it happens all at once, causing write spikes and more full-page writes. {} of {} checkpoints since statistics were reset were forced.",
        current.as_deref().map(|c| format!(" ({c})")).unwrap_or_default(),
        format::count(total * requested / 100.0),
        format::count(total)
    );
    f.severity = if requested >= 60.0 { Severity::Medium } else { Severity::Low };
    f.impact.score = base_score(f.severity) + requested / 10.0;
    f.impact.label = format!("{:.0}% forced", requested);
    f.confidence = Confidence::High;
    f.metrics = vec![
        Metric::new("Forced checkpoints", requested / 100.0, Unit::Ratio),
        Metric::new("Checkpoints", total, Unit::Count),
    ];
    f.fix = Some(Fix {
        summary: "Give WAL more room between checkpoints so they run on schedule. Check free disk space first: WAL can grow to about this size.".into(),
        sql: Some("ALTER SYSTEM SET max_wal_size = '8GB';\nSELECT pg_reload_conf();".into()),
        cost: Some("WAL takes more disk, and crash recovery can take a little longer.".into()),
    });
    f.verify = vec!["Re-scan after a day of normal load: forced checkpoints should be under 10%.".into()];
    vec![f]
}

// ── row-lock-waits ─────────────────────────────────────────────────────────────────────────────────

pub fn row_lock_waits_info() -> CheckInfo {
    CheckInfo {
        id: "row-lock-waits".into(),
        title: "Transactions waiting for row locks".into(),
        category: Category::Queries,
        engines: vec![Engine::Mysql],
        requires: vec![],
        description: "InnoDB row-lock waits are frequent and long, from long transactions or updates that scan instead of using an index.".into(),
    }
}

pub fn row_lock_waits(snapshot: &Snapshot) -> Vec<Finding> {
    let (Some(per_hour), Some(avg_ms)) = (signal(snapshot, "row-lock-waits"), signal(snapshot, "row-lock-avg")) else {
        return Vec::new();
    };
    if per_hour < 60.0 || avg_ms < 50.0 {
        return Vec::new();
    }
    let mut f = finding(
        "row-lock-waits",
        &server_key(snapshot),
        Category::Queries,
        subject(SubjectKind::Database, server_key(snapshot)),
    );
    // Under an hour of uptime the collector reports a plain count instead of a rate.
    let hourly =
        pressure(snapshot).and_then(|p| p.signal("row-lock-waits")).is_some_and(|s| s.unit == SignalUnit::PerHour);
    let how_often = if hourly {
        format!("{} times an hour", format::count(per_hour))
    } else {
        format!("{} times {}", format::count(per_hour), uptime_phrase(snapshot))
    };
    f.title = format!("Transactions wait for row locks {how_often}, {} each on average", format::duration(avg_ms));
    let top = app_queries(snapshot)
        .filter_map(|q| {
            let lock: f64 = q.waits.iter().filter(|w| category_key(&w.category) == "lock").map(|w| w.wait_ms).sum();
            (lock > 0.0).then_some((q, lock))
        })
        .max_by(|a, b| a.1.total_cmp(&b.1));
    f.summary = format!(
        "{}, InnoDB made transactions wait for row locks held by other transactions ({}), about {} each.{} \
         Lock waits grow with long transactions, and with UPDATE or DELETE statements whose filter isn't indexed: they lock every row they scan.",
        capitalize(&uptime_phrase(snapshot)),
        how_often,
        format::duration(avg_ms),
        top.map(|(q, ms)| format!(
            " {} waited longest ({} in total).",
            capitalize(&query_handle(q)),
            format::duration(ms)
        ))
        .unwrap_or_default()
    );
    if let Some((q, _)) = top {
        f.queries = query_refs(&[q]);
    }
    f.severity = if avg_ms >= 500.0 { Severity::High } else { Severity::Medium };
    f.impact.score = base_score(f.severity);
    f.impact.label = if hourly {
        format!("{}/h lock waits", format::count(per_hour))
    } else {
        format!("{} lock waits", format::count(per_hour))
    };
    f.confidence = Confidence::Medium;
    f.metrics = vec![
        Metric::new(if hourly { "Waits per hour" } else { "Waits" }, per_hour, Unit::Count),
        Metric::new("Average wait", avg_ms, Unit::Ms),
    ];
    f.fix = Some(Fix {
        summary: "Keep transactions short (no user input or slow calls inside them), make UPDATE and DELETE filter on indexed columns, and touch tables in the same order everywhere. Live shows who blocks whom right now.".into(),
        sql: None,
        cost: None,
    });
    f.verify = vec!["Re-scan: row-lock waits per hour and their average should both drop.".into()];
    vec![f]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::waits::snapshot_with;
    use culprit_core::{GrantStat, OpenTransaction, ResourceSample, Signal, SignalScope, SignalUnit, WaitStat};

    fn with_signals(engine: Engine, signals: Vec<Signal>) -> Snapshot {
        let mut s = snapshot_with(engine);
        s.pressure = Some(Pressure {
            uptime_hours: Some(72.0),
            signals,
            grants: vec![],
            oldest_transaction: None,
            ..Pressure::default()
        });
        s
    }

    fn sig(key: &str, value: f64) -> Signal {
        Signal::new(key, key, value, SignalUnit::Count, SignalScope::SinceStartup)
    }

    #[test]
    fn over_sized_grants_are_named_with_their_query() {
        let mut s = with_signals(Engine::Mssql, vec![sig("memory-grants-pending", 3.0)]);
        let hash = s.workload.as_mut().unwrap().queries[0].query_hash.clone().unwrap_or_else(|| "0xABC".into());
        s.workload.as_mut().unwrap().queries[0].query_hash = Some(hash.clone());
        s.pressure.as_mut().unwrap().grants = vec![GrantStat {
            query_hash: hash.to_lowercase(),
            executions: 40.0,
            avg_granted_kb: 2.0 * 1024.0 * 1024.0,
            avg_used_kb: 40.0 * 1024.0,
            max_granted_kb: 3.0 * 1024.0 * 1024.0,
        }];
        let f = memory_grant(&s);
        assert_eq!(f.len(), 1);
        assert!(f[0].title.contains("reserves 2.0 GB of memory per run and uses 40.0 MB"), "{}", f[0].title);
        assert_eq!(f[0].severity, Severity::High);
        assert_eq!(memory_queue(&s)[0].title, "3 queries were waiting in line for memory when the scan ran");
    }

    #[test]
    fn compile_pressure_needs_load_and_ratio() {
        let s = with_signals(Engine::Mssql, vec![sig("compiles-per-batch", 31.0), sig("batch-requests", 120.0)]);
        let f = compile_pressure(&s);
        assert!(f[0].title.contains("31% of batches (about 2.2K a minute)"), "{}", f[0].title);
        assert!(
            compile_pressure(&with_signals(
                Engine::Mssql,
                vec![sig("compiles-per-batch", 31.0), sig("batch-requests", 1.0)]
            ))
            .is_empty()
        );
        let azure = compile_pressure(&with_signals(Engine::Mssql, vec![sig("compiles-per-batch", 31.0)]));
        assert_eq!(azure[0].title, "SQL Server compiles a new plan for 31% of batches");
        assert!(compile_pressure(&with_signals(Engine::Mssql, vec![sig("compiles-per-batch", 18.0)])).is_empty());
    }

    #[test]
    fn open_transactions_are_explained_per_engine() {
        for (engine, held, versions, needle) in [
            (Engine::Mssql, Some(3.0 * 1024.0 * 1024.0 * 1024.0), None, "the version store holds 3.0 GB"),
            (Engine::Postgres, None, None, "VACUUM can't clean up"),
            (Engine::Mysql, None, Some(2_100_000.0), "InnoDB keeps 2.1M old row versions"),
        ] {
            let mut s = with_signals(engine, vec![]);
            s.pressure.as_mut().unwrap().oldest_transaction = Some(OpenTransaction {
                session: "92".into(),
                seconds: 3.0 * 3600.0,
                program: Some("billing-worker".into()),
                state: Some("idle in transaction".into()),
                held_bytes: held,
                held_versions: versions,
            });
            let f = held_cleanup(&s);
            assert!(f[0].title.contains(needle), "{engine:?}: {}", f[0].title);
            assert!(f[0].summary.contains("billing-worker"));
            assert_eq!(f[0].severity, Severity::High);
        }
    }

    #[test]
    fn connection_limits_for_postgres_and_mysql() {
        let f = worker_pressure(&with_signals(Engine::Postgres, vec![sig("connections", 92.0)]));
        assert_eq!(f[0].title, "Connections are at 92% of max_connections");
        assert!(worker_pressure(&with_signals(Engine::Mysql, vec![sig("connections", 40.0)])).is_empty());
        let threadpool = worker_pressure(&with_signals(Engine::Mssql, vec![sig("threadpool-waiting", 4.0)]));
        assert!(threadpool[0].title.contains("THREADPOOL"));
    }

    #[test]
    fn network_waits_name_big_result_sets() {
        let mut s = snapshot_with(Engine::Mssql);
        let q = &mut s.workload.as_mut().unwrap().queries[0];
        q.waits = vec![WaitStat {
            category: "Network".into(),
            wait_type: Some("Network IO".into()),
            wait_ms: q.total_time_ms * 0.6,
            tasks: None,
        }];
        q.rows = Some(q.executions * 80_000.0);
        let f = network_wait(&s);
        assert!(f[0].title.contains("60% of its time while sending 80K rows per call"), "{}", f[0].title);
    }

    #[test]
    fn tier_limits_from_resource_samples() {
        let mut s = snapshot_with(Engine::Mssql);
        s.resources = (0..20)
            .map(|i| ResourceSample {
                at: format!("2026-10-11T10:{:02}:00Z", i * 3),
                cpu_pct: Some(30.0),
                data_io_pct: Some(10.0),
                log_write_pct: Some(if i % 5 < 2 { 99.0 } else { 40.0 }),
                memory_pct: Some(80.0),
                workers_pct: Some(5.0),
            })
            .collect();
        let f = tier_limit(&s);
        assert_eq!(f.len(), 1);
        assert!(f[0].title.starts_with("Log writes hit the tier's limit 40% of the last hour"), "{}", f[0].title);
    }

    #[test]
    fn forced_checkpoints_and_row_locks() {
        let f = checkpoint_pressure(&with_signals(
            Engine::Postgres,
            vec![sig("checkpoints-requested", 62.0), sig("checkpoints", 50.0)],
        ));
        assert!(f[0].title.starts_with("62% of checkpoints are forced early"));
        let hourly = Signal { unit: SignalUnit::PerHour, ..sig("row-lock-waits", 340.0) };
        let f = row_lock_waits(&with_signals(Engine::Mysql, vec![hourly, sig("row-lock-avg", 120.0)]));
        assert!(f[0].title.contains("340 times an hour, 120 ms each"), "{}", f[0].title);
        // A count since a recent start is reported as a count, never stretched into a rate.
        let f = row_lock_waits(&with_signals(
            Engine::Mysql,
            vec![sig("row-lock-waits", 340.0), sig("row-lock-avg", 120.0)],
        ));
        assert!(f[0].title.contains("340 times since MySQL started 3 days ago"), "{}", f[0].title);
    }
}
