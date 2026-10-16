//! The `memory-queue` check: Waiting for memory takes a real amount of time (a minute an hour or more), or queries in this database were queueing for a memory grant when the scan ran.

use super::*;

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

check!(MemoryQueueCheck: memory_queue_info, memory_queue);

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
    // Without Query Store's per-database waits, the totals are the instance's since it started: every database's.
    let shared = instance && !windowed;
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
            "Waiting for memory takes {} of {} waiting (about {} an hour)",
            format::percent(share),
            if shared { "the instance's" } else { "all" },
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
            "Waits for memory are {} of all waiting in {window}, about {} an hour{}. They include memory grants and memory allocations inside the server; the totals don't say which.",
            format::percent(share),
            format::duration(memory_per_hour.unwrap_or(0.0)),
            if shared { ", across every database on the instance" } else { "" }
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
    // The instance's totals, with nothing of this database's queueing, don't say this database is the one waiting: Low.
    f.severity = if (seen_once && pending < 5.0) || (shared && pending <= 0.0) {
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
