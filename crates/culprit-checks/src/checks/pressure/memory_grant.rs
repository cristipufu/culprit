//! The `memory-grant` check: Statements whose memory grant (reserved before they run, for sorts and hashes) is many times what they use, so other queries queue for memory.

use super::*;
use crate::evidence::{QueryMeasure, Requirement as Evidence};

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

check!(MemoryGrantCheck: memory_grant_info, memory_grant, &[Evidence::Query(QueryMeasure::Grants)]);

pub fn memory_grant(snapshot: &Snapshot) -> Vec<Finding> {
    let Some(p) = pressure(snapshot) else { return Vec::new() };
    // Queueing is on record (grant waits of a minute an hour or more), or several queries were queueing when the scan
    // ran; one sighting is what memory-queue calls Low, not grounds for High here.
    let pending = signal(snapshot, "memory-grants-pending").unwrap_or(0.0);
    let grant_waits = counter_hours(snapshot).map(|h| signal(snapshot, "grant-wait-ms").unwrap_or(0.0) / h);
    let queueing = pending >= 5.0 || grant_waits.is_some_and(|g| g >= MINUTE_MS);
    // Recent runs that use a fair part of their grant (memory grant feedback shrank it) are fixed already.
    let still_oversized = |g: &&GrantStat| match (g.last_granted_kb, g.last_used_kb) {
        (Some(granted), Some(used)) if granted > 0.0 => granted >= 50.0 * MB && used <= granted * 0.25,
        _ => true,
    };
    let mut findings: Vec<Finding> = p
        .grants
        .iter()
        .filter(|g| g.executions >= 5.0 && g.avg_granted_kb >= 50.0 * MB && g.avg_used_kb <= g.avg_granted_kb * 0.25)
        .filter(still_oversized)
        .map(|g| {
            let q = by_hash(snapshot, &g.query_hash);
            let key = q.map(|q| q.id.clone()).unwrap_or_else(|| g.query_hash.clone());
            let mut f = finding("memory-grant", &key, Category::Queries, subject(SubjectKind::Query, &key));
            let handle = q.map(query_handle).unwrap_or_else(|| "a query".into());
            let (reserves, uses) = if q.is_some_and(|q| crate::all_stopped(snapshot, &[q])) {
                ("reserved", "used")
            } else {
                ("reserves", "uses")
            };
            f.title = format!(
                "{} {reserves} {} of memory per run and {uses} {}",
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
                    // A day's worth only of runs that came that way, never a burst's spread over the window.
                    let per_day_ms = w.per_day_measured(waited).filter(|_| w.runs_per_day_of(q).is_some());
                    let impact = QueryImpact { share: w.share_of(waited, q), per_day_ms };
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
