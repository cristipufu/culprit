//! Pressure checks: memory grants, compile load, worker and connection limits, open transactions that hold cleanup
//! back, results the application reads slowly, cloud tier limits, forced checkpoints and row-lock waits. Pure functions
//! of the snapshot's waits, pressure signals and resource samples.

use culprit_core::waits::category_key;
use culprit_core::{
    Category, CheckInfo, CleanupHolder, Confidence, Engine, Finding, Fix, Metric, Pressure, QueryStat, Severity,
    SignalStatus, SignalUnit, Snapshot, SubjectKind, Unit, format,
};

use crate::support::*;

const MB: f64 = 1024.0;
/// A minute of waiting an hour: below it, a share of waiting is a share of very little.
const MINUTE_MS: f64 = 60_000.0;

fn pressure(snapshot: &Snapshot) -> Option<&Pressure> {
    snapshot.pressure.as_ref()
}

fn signal(snapshot: &Snapshot, key: &str) -> Option<f64> {
    pressure(snapshot)?.signal(key).map(|s| s.value)
}

/// Hours the since-startup counters cover, when that is at least an hour: a rate over less is no rate (see
/// [`counter_days_for_rate`]).
fn counter_hours(snapshot: &Snapshot) -> Option<f64> {
    pressure(snapshot)?.uptime_hours.filter(|h| *h >= culprit_core::RATE_MIN_HOURS)
}

/// How long the database-level waits (`snapshot.waits`) cover, and whether that is a measured window (Query Store's
/// intervals) rather than totals since the server started. `None` when unknown or under an hour.
fn waits_window(snapshot: &Snapshot) -> (Option<f64>, bool) {
    let windowed = snapshot.server.engine == Engine::Mssql && app_queries(snapshot).any(|q| !q.waits.is_empty());
    let hours = if windowed {
        snapshot.workload.as_ref().map(|w| w.window_hours)
    } else {
        pressure(snapshot).and_then(|p| p.uptime_hours)
    };
    (hours.filter(|h| *h >= culprit_core::RATE_MIN_HOURS), windowed)
}

/// A single sighting at the moment of the scan, with nothing on record: worth watching, not acting on.
const SEEN_ONCE: &str = "Seen once, when the scan ran, with nothing on record before it: sample Waits → Right now a few \
                         times before acting on it.";

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

/// Server-level findings key on `server`, never the connection's name: finding ids travel into exports, PR comments
/// and agent briefs, which must not name the connection. They only need to be stable within one connection's reports.
fn server_key(_: &Snapshot) -> String {
    "server".to_string()
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
            if let Some(q) = q {
                f.queries = query_refs(&[q]);
                // What the grant costs this query is its own time waiting for memory, not all of its time: the rest
                // is running, which a smaller grant doesn't change.
                let waited: f64 =
                    q.waits.iter().filter(|w| category_key(&w.category) == "memory").map(|w| w.wait_ms).sum();
                if let Some(w) = snapshot.workload.as_ref().filter(|_| waited > 0.0) {
                    let impact = QueryImpact { share: w.share(waited), per_day_ms: w.per_day_measured(waited) };
                    apply_query_impact(&mut f, &impact);
                }
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
        description: "Waiting for memory takes a real amount of time (a minute an hour or more), or queries in this database were queueing for a memory grant when the scan ran.".into(),
    }
}

pub fn memory_queue(snapshot: &Snapshot) -> Vec<Finding> {
    let total: f64 = snapshot.waits.iter().map(|w| w.wait_ms).sum();
    let memory: f64 = snapshot.waits.iter().filter(|w| category_key(&w.category) == "memory").map(|w| w.wait_ms).sum();
    let share = if total > 0.0 { memory / total } else { 0.0 };
    // A share needs an amount behind it: 6% of a few seconds of waiting over hours is nobody queueing.
    let (window, windowed) = waits_window(snapshot);
    let memory_per_hour = window.map(|h| memory / h);
    let history = share >= 0.05 && memory_per_hour.is_some_and(|m| m >= MINUTE_MS);
    // Memory grants alone (RESOURCE_SEMAPHORE), from the wait totals since the server started. The Memory category
    // also holds allocation waits (CMEMTHREAD, MEMORY_ALLOCATION_EXT), which no grant change helps.
    let grant_ms = signal(snapshot, "grant-wait-ms").unwrap_or(0.0);
    let grants_per_hour = counter_hours(snapshot).map(|h| grant_ms / h);
    let grants = grants_per_hour.is_some_and(|g| g >= MINUTE_MS);
    let pending = signal(snapshot, "memory-grants-pending").unwrap_or(0.0);
    if !history && !grants && pending <= 0.0 {
        return Vec::new();
    }
    let grant_story = grants || pending > 0.0;
    let seen_once = !history && !grants;
    let mut f = finding(
        "memory-queue",
        &server_key(snapshot),
        Category::Configuration,
        subject(SubjectKind::Database, snapshot.server.database.clone()),
    );
    let instance = !snapshot.server.on(culprit_core::Platform::AzureSqlDatabase);
    f.title = if pending > 0.0 {
        format!(
            "{} waiting for a memory grant when the scan ran",
            if pending == 1.0 {
                "1 query in this database was".to_string()
            } else {
                format!("{} queries in this database were", format::count(pending))
            }
        )
    } else if history {
        format!(
            "Waiting for memory takes {} of all waiting (about {} an hour)",
            format::percent(share),
            format::duration(memory_per_hour.unwrap_or(0.0))
        )
    } else {
        format!("Queries waited {} for memory grants {}", format::duration(grant_ms), uptime_phrase(snapshot))
    };
    let mut parts = Vec::new();
    if history {
        let window = if windowed {
            snapshot.workload.as_ref().map(|w| w.window_phrase()).unwrap_or_default()
        } else {
            format!("the time {}", uptime_phrase(snapshot))
        };
        parts.push(format!(
            "Waits for memory are {} of all waiting in {window}, about {} an hour. They include memory grants and memory allocations inside the server; the totals don't say which.",
            format::percent(share),
            format::duration(memory_per_hour.unwrap_or(0.0))
        ));
    }
    if grants {
        parts.push(format!(
            "{}, {} waited {} for memory grants (RESOURCE_SEMAPHORE), about {} an hour on average; when isn't recorded.",
            capitalize(&uptime_phrase(snapshot)),
            if instance { "queries across the instance" } else { "this database's queries" },
            format::duration(grant_ms),
            format::duration(grants_per_hour.unwrap_or(0.0))
        ));
    }
    if pending > 0.0 {
        parts.push(format!(
            "When the scan ran, {} waiting for a grant before {} could start.",
            if pending == 1.0 { "1 query was".to_string() } else { format!("{} queries were", format::count(pending)) },
            if pending == 1.0 { "it" } else { "they" }
        ));
    }
    parts.push(if grant_story {
        "A query needs its memory grant to sort or hash before it starts; when the pool is spoken for, the rest wait. The usual cause is a few queries that reserve far more than they use.".into()
    } else {
        "Grant waits (RESOURCE_SEMAPHORE) mean queries queue for memory to sort or hash; allocation waits (CMEMTHREAD) mean many sessions contend for one of the server's memory objects, often while compiling.".to_string()
    });
    f.summary = parts.join(" ");
    // A measured window can say High; totals since startup describe the past and say at most Medium; one sighting
    // with nothing on record is Low.
    f.severity = if seen_once && pending < 5.0 {
        Severity::Low
    } else if history && windowed && share >= 0.2 && memory_per_hour.is_some_and(|m| m >= 10.0 * MINUTE_MS) {
        Severity::High
    } else {
        Severity::Medium
    };
    f.impact.score = base_score(f.severity) + if history { share.min(1.0) * 40.0 } else { 0.0 };
    f.impact.label = if history {
        format!("{} of waits", format::percent(share))
    } else if pending > 0.0 {
        format!("{} waiting", format::count(pending))
    } else {
        format!("{} grant waits", format::duration(grant_ms))
    };
    f.confidence = if seen_once {
        Confidence::Low
    } else if pending > 0.0 || (history && windowed) {
        Confidence::High
    } else {
        Confidence::Medium
    };
    if seen_once {
        f.caveats.push(SEEN_ONCE.into());
    }
    if history {
        f.metrics.push(Metric::new("Share of waits", share, Unit::Ratio));
        f.metrics.push(Metric::new("Memory waits per hour", memory_per_hour.unwrap_or(0.0), Unit::Ms));
    }
    if grants {
        f.metrics.push(Metric::new("Grant waits since start", grant_ms, Unit::Ms));
    }
    if pending > 0.0 {
        f.metrics.push(Metric::new("Waiting now", pending, Unit::Count));
    }
    f.fix = Some(Fix {
        summary: if grant_story {
            "Shrink the biggest over-sized grants first (see the memory-grant findings), then look at plans that sort or hash large scans. Raising max server memory, or the tier, treats the symptom.".into()
        } else {
            "Sample Waits → Right now while it happens to see which wait it is. For grant waits (RESOURCE_SEMAPHORE), shrink the biggest over-sized grants (the memory-grant findings); for allocation waits (CMEMTHREAD), parameterize one-off statements so fewer compile, and keep parallelism in check.".into()
        },
        sql: None,
        cost: None,
    });
    f.verify = vec!["Re-scan, or sample Right now in Waits: memory waits should fall under a minute an hour and nothing should be waiting for a grant.".into()];
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
    let Some(compiles) = pressure(snapshot).and_then(|p| p.signal("compiles-per-batch")) else { return Vec::new() };
    // An ungraded ratio is one the collector won't judge: on Azure SQL Database the counters are the server's behind
    // the database (its elastic pool's too), and a handful of batches says nothing about plan reuse.
    if compiles.status == SignalStatus::Info {
        return Vec::new();
    }
    let ratio = compiles.value;
    // The load behind the ratio, averaged over at least an hour since the start.
    let (Some(batches), Some(_)) = (signal(snapshot, "batch-requests"), counter_hours(snapshot)) else {
        return Vec::new();
    };
    if ratio < 15.0 || batches < 5.0 {
        return Vec::new();
    }
    let per_min = batches * ratio / 100.0 * 60.0;
    let mut f = finding(
        "compile-pressure",
        &server_key(snapshot),
        Category::Configuration,
        subject(SubjectKind::Database, snapshot.server.database.clone()),
    );
    f.title = format!(
        "SQL Server compiles a new plan for {:.0}% of batches (about {} a minute)",
        ratio,
        format::count(per_min)
    );
    f.summary = format!(
        "{}, the instance averaged {}/s batches across all its databases and compiled a new plan for {:.0}% of them, \
         recompiles not counted. Healthy systems reuse plans and compile for under 10%. Compiling costs CPU on every \
         call; this usually means the application sends statements with literal values instead of parameters.",
        capitalize(&uptime_phrase(snapshot)),
        format::count(batches),
        ratio
    );
    // An average since the start describes the past: at most Medium.
    f.severity = if ratio >= 25.0 { Severity::Medium } else { Severity::Low };
    f.impact.score = base_score(f.severity) + ratio / 5.0;
    f.impact.label = format!("{:.0}% of batches compile", ratio);
    f.confidence = Confidence::Medium;
    f.metrics = vec![
        Metric::new("Compiles per 100 batches", ratio, Unit::Count),
        Metric::new("Compiles per minute", per_min, Unit::Count),
    ];
    if let Some(recompiles) = signal(snapshot, "recompiles-per-batch").filter(|r| *r > 0.0) {
        f.metrics.push(Metric::new("Recompiles per 100 batches", recompiles, Unit::Count));
    }
    // What cuts compiles is a plan that serves every value; caching one-off plans as stubs saves memory but compiles
    // just as often, so it isn't the fix here (the plan-cache finding offers it for the memory).
    f.fix = Some(Fix {
        summary: "Send parameters instead of literal values (parameterized queries in the driver or ORM), so one plan serves every value. If you can't change the application, forced parameterization does it for the whole database.".into(),
        sql: Some(format!(
            "-- Prefer fixing the app. As a database-wide fallback:\nALTER DATABASE {} SET PARAMETERIZATION FORCED;",
            quote(Engine::Mssql, &snapshot.server.database)
        )),
        cost: Some("Forced parameterization changes plans for every query, and some may suit one value badly: test it first and watch the regression findings.".into()),
    });
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
        Engine::Sqlite => Vec::new(),
        Engine::Mssql => {
            let waiting = signal(snapshot, "threadpool-waiting").unwrap_or(0.0);
            let hours = counter_hours(snapshot);
            // The total since the start; scans before Culprit kept it stored an hourly average instead.
            let total = signal(snapshot, "threadpool-wait-ms").or_else(|| {
                let p = pressure(snapshot)?;
                let s = p.signal("threadpool-waits")?;
                Some(s.value * p.uptime_hours.unwrap_or(1.0).max(1.0))
            });
            // An average since the start says how much, not when: one bad hour days ago averages the same as a
            // steady trickle. Only over at least an hour of counters.
            let per_hour = total.zip(hours).map(|(t, h)| t / h);
            let history = per_hour.is_some_and(|p| p >= 10_000.0);
            if waiting <= 0.0 && !history {
                return Vec::new();
            }
            let mut f = finding("worker-pressure", &key, Category::Configuration, subject(SubjectKind::Database, &key));
            f.title = if waiting > 0.0 {
                format!(
                    "{} waiting for a worker thread when the scan ran (THREADPOOL)",
                    if waiting == 1.0 {
                        "1 request was".to_string()
                    } else {
                        format!("{} requests were", format::count(waiting))
                    }
                )
            } else {
                format!(
                    "Requests waited {} for a worker thread {} (THREADPOOL)",
                    format::duration(total.unwrap_or(0.0)),
                    uptime_phrase(snapshot)
                )
            };
            let mut parts = Vec::new();
            if history {
                parts.push(format!(
                    "{}, requests on this instance waited {} in total for a worker thread, about {} an hour on average; \
                     the counters don't say when.",
                    capitalize(&uptime_phrase(snapshot)),
                    format::duration(total.unwrap_or(0.0)),
                    format::duration(per_hour.unwrap_or(0.0))
                ));
            }
            if waiting > 0.0 {
                parts.push(format!(
                    "When the scan ran, {} on the instance waiting for one.",
                    if waiting == 1.0 {
                        "1 request was".to_string()
                    } else {
                        format!("{} requests were", format::count(waiting))
                    }
                ));
            }
            parts.push(
                "A request that can't get a worker can't even begin, so the app sees timeouts while the server looks idle. \
                 It is almost always a symptom: a blocking chain holding many workers, or many parallel queries each taking several."
                    .into(),
            );
            f.summary = parts.join(" ");
            // A queue at the moment of the scan is starvation now; totals since the start describe the past (at most
            // Medium); a task or two seen once, with nothing on record, is a moment worth watching.
            f.severity = if waiting >= 5.0 {
                Severity::High
            } else if history && per_hour.is_some_and(|p| p >= MINUTE_MS) {
                Severity::Medium
            } else {
                Severity::Low
            };
            f.impact.score = base_score(f.severity);
            f.impact.label = "THREADPOOL waits".into();
            f.confidence = match (history, waiting > 0.0) {
                (true, true) => Confidence::High,
                (true, false) => Confidence::Medium,
                _ => Confidence::Low,
            };
            if !history {
                f.caveats.push(SEEN_ONCE.into());
            }
            if let Some(t) = total.filter(|t| *t > 0.0) {
                f.metrics.push(Metric::new("Waited since start", t, Unit::Ms));
            }
            if waiting > 0.0 {
                f.metrics.push(Metric::new("Waiting now", waiting, Unit::Count));
            }
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
            let peak = signal(snapshot, "max-used-connections");
            let hit_limit = peak.is_some_and(|v| v >= 100.0);
            if used < 85.0 && !hit_limit {
                return Vec::new();
            }
            // When the busiest moment was (MySQL 5.7.5+ keeps it); without it, only that it happened since the start.
            let peak_ago = signal(snapshot, "max-used-connections-ago");
            let when = match peak_ago {
                Some(s) => format!("{} ago", format::duration(s * 1000.0)),
                None => uptime_phrase(snapshot),
            };
            let mut f = finding(
                "worker-pressure",
                &key,
                Category::Configuration,
                subject(SubjectKind::Setting, "max_connections"),
            );
            f.title = if used >= 85.0 {
                format!("Connections are at {:.0}% of max_connections", used)
            } else {
                format!("{} reached its connection limit once, {when}", snapshot.server.name())
            };
            let peak_note = match peak.filter(|_| used < 85.0) {
                Some(p) if p > 100.0 => format!(
                    " At its busiest it had {:.0}% of today's max_connections open: the limit was lowered after that.",
                    p
                ),
                Some(_) => format!(
                    " That was the busiest moment since {} started; it isn't near the limit now ({:.0}% in use).",
                    snapshot.server.name(),
                    used
                ),
                None => String::new(),
            };
            f.summary = format!(
                "When every connection slot is taken, new connections are refused and the application errors out.{peak_note} {} \
                 Each connection costs memory, so the fix is fewer, busier connections rather than a higher limit.",
                if snapshot.server.engine == Engine::Postgres {
                    "Idle connections still hold a backend process each."
                } else {
                    "Idle connections still hold a thread and buffers each."
                }
            );
            // Now is measured; a peak since the start is one moment in the past: at most Medium, and Low unless it
            // was within the last day.
            f.severity = if used >= 95.0 {
                Severity::High
            } else if used >= 85.0 || peak_ago.is_some_and(|s| s <= 86_400.0) {
                Severity::Medium
            } else {
                Severity::Low
            };
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
        description: "The transaction that holds this database's cleanup back: one with uncommitted changes or row versions here (SQL Server), the oldest snapshot, replication slot or prepared transaction behind VACUUM's horizon (PostgreSQL), or the oldest read view behind InnoDB's purge (MySQL).".into(),
    }
}

/// Transactions behind at which a replication slot's hold on VACUUM is worth a finding, and High.
const SLOT_BEHIND: f64 = 1_000_000.0;
const SLOT_FAR_BEHIND: f64 = 100_000_000.0;

pub fn held_cleanup(snapshot: &Snapshot) -> Vec<Finding> {
    let Some(t) = pressure(snapshot).and_then(|p| p.oldest_transaction.as_ref()) else { return Vec::new() };
    let engine = snapshot.server.engine;
    // Collectors report only what holds this database's cleanup; scans before they said what it is held a session.
    let holder = t.holder.unwrap_or(CleanupHolder::Session);
    let slot = holder == CleanupHolder::ReplicationSlot;
    let held_gb = t.held_bytes.unwrap_or(0.0) / 1024.0 / 1024.0 / 1024.0;
    let behind = t.xid_age.unwrap_or(0.0);
    // How long it has been open is what tells a forgotten transaction from a busy one; a slot has no start time, so
    // it is how far behind it holds the horizon.
    let old = if slot { behind >= SLOT_BEHIND } else { t.seconds >= 1800.0 };
    let big = engine == Engine::Mssql && held_gb >= 1.0 && t.seconds >= 300.0;
    if !old && !big {
        return Vec::new();
    }
    let open = format::duration(t.seconds * 1000.0);
    let idle = t.state.as_deref().is_some_and(|s| s.contains("idle"));
    let snapshot_reader = t.held_bytes.is_some_and(|b| b > 0.0) || t.state.as_deref() == Some("snapshot");
    let wrote = t.wrote == Some(true);
    let other_database = t.database.as_deref().filter(|d| !d.eq_ignore_ascii_case(&snapshot.server.database));
    let mut f = finding(
        "held-cleanup",
        &t.session,
        Category::Maintenance,
        subject(SubjectKind::Database, snapshot.server.database.clone()),
    );
    f.title = match (engine, holder) {
        (Engine::Mssql, _) => match t.held_bytes {
            Some(b) if b > 0.0 => format!(
                "Session {} has kept a transaction open for {open}; this database's version store holds {}",
                t.session,
                format::bytes(b)
            ),
            _ if wrote => {
                format!("Session {} has kept a transaction with uncommitted changes open for {open}", t.session)
            }
            _ => format!("Session {} has kept a transaction open for {open}", t.session),
        },
        (Engine::Postgres, CleanupHolder::ReplicationSlot) => {
            format!("Replication slot {} holds VACUUM back by {} transactions", t.session, format::count(behind))
        }
        (Engine::Postgres, CleanupHolder::PreparedTransaction) => format!(
            "Prepared transaction '{}' has waited {open} to be committed or rolled back, so VACUUM can't clean up behind it",
            t.session
        ),
        (Engine::Postgres, CleanupHolder::Session) => format!(
            "Session {} has kept a transaction open for {open}{}, so VACUUM can't clean up behind it",
            t.session,
            if idle { " doing nothing" } else { "" }
        ),
        (Engine::Mysql, _) => format!(
            "A transaction in session {} has been open for {open}, so InnoDB can't purge old row versions",
            t.session
        ),
        (Engine::Sqlite, _) => format!("A transaction has been open for {open}"),
    };
    let program =
        t.program.as_deref().filter(|p| !p.is_empty()).map(|p| format!(" It comes from {p}.")).unwrap_or_default();
    let behind_note = if behind > 0.0 {
        format!(" {} transactions have started since its snapshot.", format::count(behind))
    } else {
        String::new()
    };
    f.summary = match (engine, holder) {
        (Engine::Mssql, _) => {
            let mut parts = Vec::new();
            if snapshot_reader {
                parts.push("It reads or makes row versions, and while it stays open SQL Server keeps every version this database made since it started in the version store; reads have to walk past them.".to_string());
            }
            if wrote {
                parts.push("It changed data in this database and hasn't committed: SQL Server can't reuse the transaction log past its first change, so the log grows, and its locks stay held.".to_string());
            }
            if parts.is_empty() {
                parts.push(
                    "An open transaction keeps its locks, and the log can't be reused past its first change."
                        .to_string(),
                );
            }
            format!("{}{program} Short transactions keep both small.", parts.join(" "))
        }
        (Engine::Postgres, CleanupHolder::ReplicationSlot) => format!(
            "A replication slot with an xmin (a standby with hot_standby_feedback) keeps VACUUM from removing rows deleted or updated after it, in every database.{}",
            if t.state.as_deref() == Some("inactive slot") {
                " It is inactive: the standby that used it is gone or disconnected, so it never moves on."
            } else {
                ""
            }
        ),
        (Engine::Postgres, CleanupHolder::PreparedTransaction) => format!(
            "A prepared transaction (two-phase commit) keeps its locks and VACUUM's horizon until someone commits or rolls it back, and it survives restarts.{behind_note}{}",
            t.program.as_deref().map(|o| format!(" It was prepared by {o}.")).unwrap_or_default()
        ),
        (Engine::Postgres, CleanupHolder::Session) => format!(
            "PostgreSQL can't remove rows deleted or updated after this transaction's snapshot, so tables and indexes bloat and scans read dead rows.{behind_note}{}{program}",
            if idle { " This one is idle: the application opened it and never committed." } else { "" }
        ),
        (Engine::Mysql, _) => {
            let history = signal(snapshot, "history-length")
                .filter(|h| *h > 0.0)
                .map(|h| {
                    format!(" InnoDB's undo history (the whole server's) is {} undo logs long now.", format::count(h))
                })
                .unwrap_or_default();
            format!(
                "InnoDB keeps old versions of changed rows until the oldest read view that might need them closes, and purge is the whole server's: this transaction holds it back for every database.{history}{}{program}",
                other_database.map(|d| format!(" It runs in database {d}.")).unwrap_or_default()
            )
        }
        (Engine::Sqlite, _) => format!("It holds a lock on the file.{program}"),
    };
    f.severity = if t.seconds >= 3.0 * 3600.0 || held_gb >= 10.0 || (slot && behind >= SLOT_FAR_BEHIND) {
        Severity::High
    } else {
        Severity::Medium
    };
    f.impact.score = base_score(f.severity);
    f.impact.label =
        if slot { format!("{} transactions behind", format::count(behind)) } else { format!("open {open}") };
    f.confidence = Confidence::High;
    if !slot {
        f.metrics.push(Metric::new("Open for", t.seconds * 1000.0, Unit::Ms));
    }
    if let Some(b) = t.held_bytes.filter(|b| *b > 0.0) {
        f.metrics.push(Metric::new("Version store", b, Unit::Bytes));
    }
    if behind > 0.0 {
        f.metrics.push(Metric::new("Transactions behind", behind, Unit::Count));
    }
    f.fix = Some(match holder {
        CleanupHolder::ReplicationSlot => Fix {
            summary: "If the standby that used this slot is gone, drop the slot; if it is only disconnected, reconnect it so it moves on.".into(),
            sql: Some(format!("SELECT pg_drop_replication_slot('{}');", t.session.replace('\'', "''"))),
            cost: Some("A standby that still needs this slot can't resume from it and has to be rebuilt.".into()),
        },
        CleanupHolder::PreparedTransaction => Fix {
            summary: "Find what prepared it (the transaction manager or application doing two-phase commit) and have it commit or roll back. If nothing will, a DBA can roll it back.".into(),
            sql: Some(format!("ROLLBACK PREPARED '{}';", t.session.replace('\'', "''"))),
            cost: Some("Rolling it back discards its changes.".into()),
        },
        CleanupHolder::Session => {
            let end = engine.dialect().end_session(&t.session);
            Fix {
                summary: "Find out what the application does in that session and make it commit or roll back promptly; transactions should never wait on a person or a slow call. After checking it is safe, a DBA can end the session, which rolls its transaction back.".into(),
                sql: Some(format!("-- Ends the session and rolls back its open transaction\n{end}")),
                cost: Some("Ending the session discards that transaction's uncommitted changes.".into()),
            }
        }
    });
    f.verify = vec![if slot {
        "Re-scan: no replication slot should hold VACUUM back by millions of transactions.".into()
    } else {
        "Re-scan: the oldest open transaction should be minutes old, not hours.".into()
    }];
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
            let query = query_impact(snapshot, &[q])?;
            if share < 0.25 || query.share < 0.01 {
                return None;
            }
            let rows = q.rows.filter(|_| q.executions > 0.0).map(|r| r / q.executions);
            let mut f = finding("network-wait", &q.id, Category::Queries, subject(SubjectKind::Query, &q.id));
            // Its weight is the time spent waiting on the application, not the query's whole time.
            let w = snapshot.workload.as_ref()?;
            let impact = QueryImpact { share: w.share(network), per_day_ms: w.per_day_measured(network) };
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
            f.severity = severity_for_workload_share(snapshot, impact.share);
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
            "new requests are refused (error 10928)",
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
            let mut f = finding("tier-limit", key, Category::Configuration, subject(SubjectKind::Database, snapshot.server.database.clone()));
            f.title = format!("{label} hit the tier's limit {} of {span}: {effect}", format::percent(at_limit));
            // Throttled resources make requests wait; the worker limit refuses them.
            let at_cap = if *key == "workers" {
                "At the cap, Azure refuses new requests with error 10928 (the request limit for the database has been reached) until workers free up, so the application sees errors, not only slowness."
            } else {
                "At the cap, Azure makes requests wait rather than fail, so it shows up as slowness."
            };
            f.summary = format!(
                "Azure caps each tier's {}. In {span} it was at 95% or more of that cap in {} of the 15-second samples (peak {:.0}%). {at_cap}",
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
    // PostgreSQL counts every checkpoint something asked for as requested: WAL reaching max_wal_size, but also a
    // manual CHECKPOINT, a base backup or CREATE DATABASE. The counter doesn't say which.
    f.title = format!("{:.0}% of checkpoints start early, most likely because WAL fills max_wal_size", requested);
    f.summary = format!(
        "A checkpoint writes every changed page to disk. On schedule (checkpoint_timeout) the work is spread out; forced by a full max_wal_size{} it happens all at once, causing write spikes and more full-page writes. {} of {} checkpoints since statistics were reset were requested rather than timed. WAL filling max_wal_size is the usual reason; a manual CHECKPOINT, a base backup or CREATE DATABASE requests one too, so if those run often they explain part of it.",
        current.as_deref().map(|c| format!(" ({c})")).unwrap_or_default(),
        format::count(total * requested / 100.0),
        format::count(total)
    );
    f.severity = if requested >= 60.0 { Severity::Medium } else { Severity::Low };
    f.impact.score = base_score(f.severity) + requested / 10.0;
    f.impact.label = format!("{:.0}% forced", requested);
    f.confidence = Confidence::Medium;
    f.metrics = vec![
        Metric::new("Forced checkpoints", requested / 100.0, Unit::Ratio),
        Metric::new("Checkpoints", total, Unit::Count),
    ];
    // More room than it has now: 8GB, or twice the current size when that's already at least 8GB.
    let proposed_mb = current.as_deref().and_then(size_in_mb).map_or(8192.0, |mb| (mb * 2.0).max(8192.0));
    let proposed = if proposed_mb >= 1024.0 && proposed_mb % 1024.0 == 0.0 {
        format!("{:.0}GB", proposed_mb / 1024.0)
    } else {
        format!("{proposed_mb:.0}MB")
    };
    let change = setting_change(&snapshot.server, "max_wal_size", &proposed);
    f.fix = Some(Fix {
        summary: format!(
            "Give WAL more room between checkpoints so they run on schedule. {} Check free disk space first: WAL can grow to about this size.",
            change.how
        ),
        sql: change.sql,
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
        description: "InnoDB row-lock waits add up to real time (a minute an hour or more across the server), from long transactions or updates that scan instead of using an index.".into(),
    }
}

pub fn row_lock_waits(snapshot: &Snapshot) -> Vec<Finding> {
    let Some(avg_ms) = signal(snapshot, "row-lock-avg") else { return Vec::new() };
    // A rate needs at least an hour of counters.
    let Some(hours) = counter_hours(snapshot) else { return Vec::new() };
    let Some(waits) = pressure(snapshot).and_then(|p| p.signal("row-lock-waits")) else { return Vec::new() };
    let count = if waits.unit == SignalUnit::PerHour { waits.value * hours } else { waits.value };
    // The total wait time; scans before Culprit kept it are rebuilt from the count and the average.
    let total_ms = signal(snapshot, "row-lock-time").unwrap_or(count * avg_ms);
    let per_hour_ms = total_ms / hours;
    // What it costs is time, not how many waits: 61 waits of half a second an hour is half a minute.
    if per_hour_ms < MINUTE_MS {
        return Vec::new();
    }
    // Against this database's statement time an hour (the server's waits include every database's).
    let share = snapshot
        .workload
        .as_ref()
        .filter(|w| w.window_hours >= culprit_core::RATE_MIN_HOURS && w.total_time_ms > 0.0)
        .map(|w| per_hour_ms / (w.total_time_ms / w.window_hours));
    let mut f = finding(
        "row-lock-waits",
        &server_key(snapshot),
        Category::Queries,
        subject(SubjectKind::Database, snapshot.server.database.clone()),
    );
    let name = snapshot.server.name();
    f.title = format!(
        "Transactions on this server waited {} for row locks {}, {} each on average",
        format::duration(total_ms),
        uptime_phrase(snapshot),
        format::duration(avg_ms)
    );
    // A statement whose own time is mostly lock waits is where they show; one with a little lock time isn't named
    // as their cause.
    let top = app_queries(snapshot)
        .filter_map(|q| {
            let lock: f64 = q.waits.iter().filter(|w| category_key(&w.category) == "lock").map(|w| w.wait_ms).sum();
            (lock > 0.0).then_some((q, lock))
        })
        .max_by(|a, b| a.1.total_cmp(&b.1));
    f.summary = format!(
        "{}, InnoDB made transactions across the whole server (it counts every database together) wait for row locks \
         {} times, {} in all and about {} each: about {} an hour on average{}; the counters don't say when.{} \
         Lock waits grow with long transactions, and with UPDATE or DELETE statements whose filter isn't indexed: they lock every row they scan.",
        capitalize(&uptime_phrase(snapshot)),
        format::count(count),
        format::duration(total_ms),
        format::duration(avg_ms),
        format::duration(per_hour_ms),
        share
            .map(|s| format!(", against {} of statement time an hour in this database", format::percent(s.min(9.99))))
            .unwrap_or_default(),
        top.map(|(q, ms)| format!(
            " In this database, {} waited longest on locks ({} in total).",
            query_handle(q),
            format::duration(ms)
        ))
        .unwrap_or_default()
    );
    if let Some((q, ms)) = top
        && q.total_time_ms > 0.0
        && ms / q.total_time_ms >= 0.5
    {
        f.queries = query_refs(&[q]);
    }
    // Totals since the start describe the past: at most Medium, and Medium only when they are a real part of the
    // work (ten minutes an hour, or a tenth of this database's statement time).
    f.severity = if per_hour_ms >= 10.0 * MINUTE_MS || share.is_some_and(|s| s >= 0.1) {
        Severity::Medium
    } else {
        Severity::Low
    };
    f.impact.score = base_score(f.severity);
    f.impact.label = format!("{}/h lock waits", format::duration(per_hour_ms));
    f.confidence = Confidence::Medium;
    f.metrics = vec![
        Metric::new("Lock waits since start", count, Unit::Count),
        Metric::new("Time waiting since start", total_ms, Unit::Ms),
        Metric::new("Average wait", avg_ms, Unit::Ms),
    ];
    f.caveats.push(format!(
        "{name} counts row-lock waits for the whole server, so other databases' waits are in these numbers."
    ));
    f.fix = Some(Fix {
        summary: "Keep transactions short (no user input or slow calls inside them), make UPDATE and DELETE filter on indexed columns, and touch tables in the same order everywhere. Live shows who blocks whom right now.".into(),
        sql: None,
        cost: None,
    });
    f.verify = vec!["Sample Waits → Right now during normal load: it counts row-lock waits over the sample, which should be rare and short.".into()];
    vec![f]
}

/// A PostgreSQL size setting ("1GB", "512MB", "64kB", or a bare number of MB as `max_wal_size` reports it) in MB.
fn size_in_mb(text: &str) -> Option<f64> {
    let text = text.trim();
    let digits = text.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(text.len());
    let number: f64 = text[..digits].parse().ok()?;
    let factor = match text[digits..].trim().to_ascii_lowercase().as_str() {
        "" | "mb" => 1.0,
        "kb" => 1.0 / 1024.0,
        "gb" => 1024.0,
        "tb" => 1024.0 * 1024.0,
        _ => return None,
    };
    Some(number * factor)
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

    /// A signal the collector graded (it does for a server's own counters under load).
    fn graded(key: &str, value: f64) -> Signal {
        sig(key, value).graded("under 10% is healthy", 10.0, 25.0, true)
    }

    fn fixture(name: &str) -> Snapshot {
        let text =
            std::fs::read_to_string(format!("{}/../../fixtures/{name}", env!("CARGO_MANIFEST_DIR"))).expect("fixture");
        serde_json::from_str(&text).expect("snapshot")
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
        // The query never waited for memory itself: the grant claims none of its time.
        assert_eq!(f[0].impact.db_time_share, None);
        let q = memory_queue(&s);
        assert_eq!(q[0].title, "3 queries in this database were waiting for a memory grant when the scan ran");
    }

    // C10: a grant or a slow reader claims only the time it explains, not the whole query's.
    #[test]
    fn grants_and_network_waits_claim_only_their_own_time() {
        let mut s = with_signals(Engine::Mssql, vec![]);
        let total = s.workload.as_ref().unwrap().total_time_ms;
        let q = &mut s.workload.as_mut().unwrap().queries[0];
        q.query_hash = Some("0xABC".into());
        let query_ms = q.total_time_ms;
        q.waits = vec![WaitStat { category: "Memory".into(), wait_type: None, wait_ms: query_ms * 0.1, tasks: None }];
        s.pressure.as_mut().unwrap().grants = vec![GrantStat {
            query_hash: "0xabc".into(),
            executions: 40.0,
            avg_granted_kb: 2.0 * 1024.0 * 1024.0,
            avg_used_kb: 40.0 * 1024.0,
            max_granted_kb: 3.0 * 1024.0 * 1024.0,
        }];
        let f = memory_grant(&s);
        let share = f[0].impact.db_time_share.unwrap();
        assert!((share - query_ms * 0.1 / total).abs() < 1e-9, "{share}");

        let mut s = snapshot_with(Engine::Mssql);
        let total = s.workload.as_ref().unwrap().total_time_ms;
        let q = &mut s.workload.as_mut().unwrap().queries[0];
        let network = q.total_time_ms * 0.3;
        q.waits = vec![WaitStat { category: "Network".into(), wait_type: None, wait_ms: network, tasks: None }];
        let f = network_wait(&s);
        assert!((f[0].impact.db_time_share.unwrap() - network / total).abs() < 1e-9);
    }

    // C4: the demo's 5.9 s of Memory waits over 4.6 hours is a share of almost nothing.
    #[test]
    fn memory_waits_need_an_amount_not_only_a_share() {
        let s = fixture("mssql-demo.json");
        let memory: f64 = s.waits.iter().filter(|w| w.category == "Memory").map(|w| w.wait_ms).sum();
        assert!(memory > 5_000.0 && memory < 6_000.0);
        assert!(memory_queue(&s).is_empty(), "6% of waiting, but about a second an hour");

        // Twenty minutes an hour, a quarter of all waiting, in Query Store's window: a real queue.
        let mut s = fixture("mssql-demo.json");
        let hours = s.workload.as_ref().unwrap().window_hours;
        let other: f64 = s.waits.iter().filter(|w| w.category != "Memory").map(|w| w.wait_ms).sum();
        let planted = (20.0 * MINUTE_MS * hours).max(other / 3.0);
        s.waits.retain(|w| w.category != "Memory");
        s.waits.push(WaitStat {
            category: "Memory".into(),
            wait_type: Some("Memory".into()),
            wait_ms: planted,
            tasks: None,
        });
        let f = memory_queue(&s);
        assert_eq!(f[0].severity, Severity::High);
        // Query Store's Memory category mixes grants and allocations: no grant story without grant evidence.
        assert!(f[0].summary.contains("the totals don't say which"), "{}", f[0].summary);
        assert!(f[0].fix.as_ref().unwrap().summary.starts_with("Sample Waits"));
    }

    // C4: one query seen waiting once is a moment to watch, like THREADPOOL; RESOURCE_SEMAPHORE since the start
    // tells the grant story, at most Medium.
    #[test]
    fn a_single_pending_grant_is_low_and_grant_history_is_at_most_medium() {
        let s = with_signals(Engine::Mssql, vec![sig("memory-grants-pending", 1.0)]);
        let f = memory_queue(&s);
        assert_eq!((f[0].severity, f[0].confidence), (Severity::Low, Confidence::Low));
        assert!(f[0].caveats[0].starts_with("Seen once"));
        let s = with_signals(Engine::Mssql, vec![sig("grant-wait-ms", 72.0 * 30.0 * MINUTE_MS)]);
        let f = memory_queue(&s);
        assert_eq!(f[0].severity, Severity::Medium);
        assert!(
            f[0].title.starts_with("Queries waited 36 h for memory grants since SQL Server started 3 days ago"),
            "{}",
            f[0].title
        );
        assert!(f[0].summary.contains("RESOURCE_SEMAPHORE"));
        assert!(memory_queue(&with_signals(Engine::Mssql, vec![sig("grant-wait-ms", 30_000.0)])).is_empty());
    }

    #[test]
    fn compile_pressure_needs_load_and_ratio() {
        let s = with_signals(Engine::Mssql, vec![graded("compiles-per-batch", 31.0), sig("batch-requests", 120.0)]);
        let f = compile_pressure(&s);
        assert!(f[0].title.contains("31% of batches (about 2.2K a minute)"), "{}", f[0].title);
        // An average since the start: Medium at most.
        assert_eq!(f[0].severity, Severity::Medium);
        // The fix cuts compiles; caching one-off plans as stubs wouldn't.
        let sql = f[0].fix.as_ref().unwrap().sql.clone().unwrap();
        assert!(sql.contains("PARAMETERIZATION FORCED") && !sql.contains("optimize for ad hoc"), "{sql}");
        assert!(
            compile_pressure(&with_signals(
                Engine::Mssql,
                vec![graded("compiles-per-batch", 31.0), sig("batch-requests", 1.0)]
            ))
            .is_empty()
        );
        assert!(compile_pressure(&with_signals(Engine::Mssql, vec![graded("compiles-per-batch", 18.0)])).is_empty());
    }

    // C3: on Azure the counters cover the server behind the database (its elastic pool's too) and the collector
    // doesn't grade them; the check doesn't either.
    #[test]
    fn shared_compile_counters_raise_nothing() {
        let shared = Signal {
            healthy: Some("covers every database on the server behind this one, so it isn't graded here".into()),
            ..sig("compiles-per-batch", 45.0)
        };
        let mut s = with_signals(Engine::Mssql, vec![shared, sig("batch-requests", 300.0)]);
        s.server.cloud = Some("Azure SQL Database".into());
        assert!(compile_pressure(&s).is_empty());
        // Under an hour since the start there is no rate to judge.
        let mut s = with_signals(Engine::Mssql, vec![graded("compiles-per-batch", 45.0), sig("batch-requests", 300.0)]);
        s.pressure.as_mut().unwrap().uptime_hours = Some(0.4);
        assert!(compile_pressure(&s).is_empty());
    }

    fn transaction(session: &str, seconds: f64) -> OpenTransaction {
        OpenTransaction {
            session: session.into(),
            seconds,
            program: Some("billing-worker".into()),
            state: Some("idle in transaction".into()),
            holder: Some(CleanupHolder::Session),
            ..OpenTransaction::default()
        }
    }

    #[test]
    fn open_transactions_are_explained_per_engine() {
        for (engine, held, needle) in [
            (Engine::Mssql, Some(3.0 * 1024.0 * 1024.0 * 1024.0), "this database's version store holds 3.0 GB"),
            (Engine::Postgres, None, "VACUUM can't clean up"),
            (Engine::Mysql, None, "InnoDB can't purge old row versions"),
        ] {
            let mut s = with_signals(engine, vec![]);
            s.pressure.as_mut().unwrap().oldest_transaction =
                Some(OpenTransaction { held_bytes: held, ..transaction("92", 3.0 * 3600.0) });
            let f = held_cleanup(&s);
            assert!(f[0].title.contains(needle), "{engine:?}: {}", f[0].title);
            assert!(f[0].summary.contains("billing-worker"));
            assert_eq!(f[0].severity, Severity::High);
        }
    }

    // C2: InnoDB's history length is the whole server's and counts undo logs: it never makes a seconds-old
    // transaction the culprit, and it isn't called this transaction's row versions.
    #[test]
    fn a_young_mysql_transaction_is_not_blamed_for_the_servers_history() {
        let mut s = with_signals(Engine::Mysql, vec![sig("history-length", 2_100_000.0)]);
        s.pressure.as_mut().unwrap().oldest_transaction =
            Some(OpenTransaction { held_versions: Some(2_100_000.0), ..transaction("104", 2.0) });
        assert!(held_cleanup(&s).is_empty());
        s.pressure.as_mut().unwrap().oldest_transaction =
            Some(OpenTransaction { database: Some("billing".into()), ..transaction("104", 2.0 * 3600.0) });
        let f = held_cleanup(&s);
        assert!(f[0].summary.contains("2.1M undo logs"), "{}", f[0].summary);
        assert!(f[0].summary.contains("It runs in database billing."));
        assert!(!f[0].title.contains("row versions for it"));
    }

    // C2: a SQL Server writer holds the log and its locks; the version store is named only for a transaction that
    // reads or makes row versions.
    #[test]
    fn sql_server_says_what_the_transaction_holds() {
        let mut s = with_signals(Engine::Mssql, vec![]);
        s.pressure.as_mut().unwrap().oldest_transaction =
            Some(OpenTransaction { wrote: Some(true), state: Some("running".into()), ..transaction("71", 3600.0) });
        let f = held_cleanup(&s);
        assert!(f[0].title.contains("uncommitted changes"), "{}", f[0].title);
        assert!(f[0].summary.contains("can't reuse the transaction log"));
        assert!(!f[0].summary.contains("version store"), "{}", f[0].summary);
    }

    // C2: what holds PostgreSQL's horizon may be a replication slot or a prepared transaction, each with its own fix.
    #[test]
    fn postgres_slots_and_prepared_transactions_get_their_own_fix() {
        let mut s = with_signals(Engine::Postgres, vec![]);
        s.pressure.as_mut().unwrap().oldest_transaction = Some(OpenTransaction {
            session: "standby_1".into(),
            state: Some("inactive slot".into()),
            holder: Some(CleanupHolder::ReplicationSlot),
            xid_age: Some(150_000_000.0),
            ..OpenTransaction::default()
        });
        let f = held_cleanup(&s);
        assert_eq!(f[0].title, "Replication slot standby_1 holds VACUUM back by 150.0M transactions");
        assert_eq!(f[0].severity, Severity::High);
        let sql = f[0].fix.as_ref().unwrap().sql.clone().unwrap();
        assert!(sql.contains("pg_drop_replication_slot('standby_1')"), "{sql}");
        s.pressure.as_mut().unwrap().oldest_transaction = Some(OpenTransaction {
            session: "tx-17".into(),
            seconds: 7200.0,
            state: Some("prepared".into()),
            holder: Some(CleanupHolder::PreparedTransaction),
            xid_age: Some(40_000.0),
            ..OpenTransaction::default()
        });
        let f = held_cleanup(&s);
        assert!(f[0].title.starts_with("Prepared transaction 'tx-17' has waited 2 h "), "{}", f[0].title);
        assert_eq!(f[0].fix.as_ref().unwrap().sql.as_deref(), Some("ROLLBACK PREPARED 'tx-17';"));
        // A slot only a few transactions behind holds nothing worth a finding.
        s.pressure.as_mut().unwrap().oldest_transaction = Some(OpenTransaction {
            session: "standby_2".into(),
            holder: Some(CleanupHolder::ReplicationSlot),
            xid_age: Some(800.0),
            ..OpenTransaction::default()
        });
        assert!(held_cleanup(&s).is_empty());
    }

    #[test]
    fn connection_limits_for_postgres_and_mysql() {
        let f = worker_pressure(&with_signals(Engine::Postgres, vec![sig("connections", 92.0)]));
        assert_eq!(f[0].title, "Connections are at 92% of max_connections");
        assert!(worker_pressure(&with_signals(Engine::Mysql, vec![sig("connections", 40.0)])).is_empty());
    }

    // C14: one peak since the start is a moment in the past: Low unless it was within the last day, and a peak above
    // today's limit says the limit was lowered.
    #[test]
    fn a_connection_peak_since_startup_is_not_a_current_emergency() {
        let peak = |pct: f64, ago: f64| {
            worker_pressure(&with_signals(
                Engine::Mysql,
                vec![sig("connections", 20.0), sig("max-used-connections", pct), sig("max-used-connections-ago", ago)],
            ))
        };
        let f = peak(104.0, 9.0 * 86_400.0);
        assert_eq!(f[0].severity, Severity::Low);
        assert!(f[0].title.starts_with("MySQL reached its connection limit once, 9.0 days ago"), "{}", f[0].title);
        assert!(f[0].summary.contains("the limit was lowered after that"));
        assert_eq!(peak(100.0, 3600.0)[0].severity, Severity::Medium);
    }

    // C14: THREADPOOL totals since the start are said as totals, at most Medium; a queue seen now is High; one or two
    // requests seen once are Low.
    #[test]
    fn threadpool_waits_since_startup_are_totals_not_rates() {
        let f = worker_pressure(&with_signals(Engine::Mssql, vec![sig("threadpool-wait-ms", 72.0 * 120_000.0)]));
        assert_eq!(f[0].severity, Severity::Medium);
        assert!(
            f[0].title
                .starts_with("Requests waited 2 h 24 min for a worker thread since SQL Server started 3 days ago"),
            "{}",
            f[0].title
        );
        assert!(f[0].summary.contains("the counters don't say when"));
        let f = worker_pressure(&with_signals(Engine::Mssql, vec![sig("threadpool-waiting", 2.0)]));
        assert_eq!((f[0].severity, f[0].confidence), (Severity::Low, Confidence::Low));
        let f = worker_pressure(&with_signals(Engine::Mssql, vec![sig("threadpool-waiting", 6.0)]));
        assert_eq!(f[0].severity, Severity::High);
        assert!(f[0].title.contains("THREADPOOL"));
        // Under an hour since the start, a total is no rate: nothing to grade unless someone is waiting now.
        let mut s = with_signals(Engine::Mssql, vec![sig("threadpool-wait-ms", 900_000.0)]);
        s.pressure.as_mut().unwrap().uptime_hours = Some(0.5);
        assert!(worker_pressure(&s).is_empty());
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

    fn samples(pick: impl Fn(usize) -> (f64, f64)) -> Vec<ResourceSample> {
        (0..20)
            .map(|i| {
                let (log, workers) = pick(i);
                ResourceSample {
                    at: format!("2026-10-11T10:{:02}:00Z", i * 3),
                    cpu_pct: Some(30.0),
                    data_io_pct: Some(10.0),
                    log_write_pct: Some(log),
                    memory_pct: Some(80.0),
                    workers_pct: Some(workers),
                }
            })
            .collect()
    }

    #[test]
    fn tier_limits_from_resource_samples() {
        let mut s = snapshot_with(Engine::Mssql);
        s.resources = samples(|i| (if i % 5 < 2 { 99.0 } else { 40.0 }, 5.0));
        let f = tier_limit(&s);
        assert_eq!(f.len(), 1);
        assert!(f[0].title.starts_with("Log writes hit the tier's limit 40% of the last hour"), "{}", f[0].title);
        assert!(f[0].summary.contains("wait rather than fail"));
    }

    // C18: at the worker limit Azure refuses requests (error 10928); the finding says so in its title and summary.
    #[test]
    fn the_worker_limit_refuses_requests() {
        let mut s = snapshot_with(Engine::Mssql);
        s.resources = samples(|i| (10.0, if i % 2 == 0 { 100.0 } else { 60.0 }));
        let f = tier_limit(&s);
        assert!(f[0].title.ends_with("new requests are refused (error 10928)"), "{}", f[0].title);
        assert!(f[0].summary.contains("refuses new requests with error 10928"));
        assert!(!f[0].summary.contains("rather than fail"));
    }

    #[test]
    fn forced_checkpoints_name_every_cause() {
        let f = checkpoint_pressure(&with_signals(
            Engine::Postgres,
            vec![sig("checkpoints-requested", 62.0), sig("checkpoints", 50.0)],
        ));
        assert!(f[0].title.starts_with("62% of checkpoints start early, most likely because WAL fills max_wal_size"));
        assert!(f[0].summary.contains("a manual CHECKPOINT, a base backup or CREATE DATABASE"));
    }

    // C15: 61 waits an hour of half a second each is half a minute an hour: not a finding. What counts is the time,
    // the server's, since startup: at most Medium.
    #[test]
    fn row_lock_waits_are_graded_on_time_and_said_to_be_server_wide() {
        let hourly = |n: f64| Signal { unit: SignalUnit::PerHour, ..sig("row-lock-waits", n) };
        let quiet = with_signals(Engine::Mysql, vec![hourly(61.0), sig("row-lock-avg", 510.0)]);
        assert!(row_lock_waits(&quiet).is_empty());
        let mut s = with_signals(
            Engine::Mysql,
            vec![hourly(340.0), sig("row-lock-avg", 1200.0), sig("row-lock-time", 72.0 * 340.0 * 1200.0)],
        );
        let f = row_lock_waits(&s);
        assert!(f[0].title.starts_with("Transactions on this server waited"), "{}", f[0].title);
        assert!(f[0].title.contains("since MySQL started 3 days ago, 1.2 s each on average"), "{}", f[0].title);
        assert!(f[0].summary.contains("across the whole server"));
        assert!(f[0].severity <= Severity::Medium);
        assert!(f[0].caveats[0].contains("whole server"));
        // Under an hour of counters there is no rate.
        s.pressure.as_mut().unwrap().uptime_hours = Some(0.5);
        assert!(row_lock_waits(&s).is_empty());
    }
}
