//! Measured Azure SQL Database resource use near its tier's limit, over the available selected-period samples.

use super::*;

pub fn tier_limit_info() -> CheckInfo {
    CheckInfo {
        id: "tier-limit".into(),
        title: "Resource use near the cloud tier's limits".into(),
        category: Category::Configuration,
        engines: vec![Engine::Mssql],
        requires: vec![],
        description: "Azure SQL Database resources (CPU, data IO, log writes, workers) persistently near their limits in the available selected-period samples.".into(),
    }
}

check!(TierLimitCheck: tier_limit_info, tier_limit);

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
            "writes can be throttled at the cap",
            "Batch small writes into fewer transactions and avoid row-by-row changes; then consider a tier with more log throughput.",
        ),
        (
            "cpu",
            "CPU",
            |s| s.cpu_pct,
            "queries can queue at the cap",
            "Fix the queries that use the most CPU first (Waits → CPU lists them); then consider more vCores.",
        ),
        (
            "data-io",
            "Data reads",
            |s| s.data_io_pct,
            "reads can be throttled at the cap",
            "Cut scans with the missing-index findings and check memory; then consider a tier with more IO.",
        ),
        (
            "workers",
            "Worker threads",
            |s| s.workers_pct,
            "new requests can be refused at the cap (error 10928)",
            "Look for blocking chains and heavy parallelism (they hold many workers); then consider a bigger tier.",
        ),
    ];
    dims.iter()
        .filter_map(|(key, label, pick, effect, fix)| {
            let usage = super::health::resource_usage(samples, *pick)?;
            if !usage.measured { return None; }
            let span = format!("the last {}", culprit_core::span_words(usage.hours));
            let (at_limit, peak) = (usage.at_limit, usage.peak);
            if at_limit < 0.10 {
                return None;
            }
            let mut f = finding("tier-limit", key, Category::Configuration, subject(SubjectKind::Database, snapshot.server.database.clone()));
            f.title = format!("{label} reached 95% of the tier's limit in {} of samples over {span}", format::percent(at_limit));
            // Throttled resources make requests wait; the worker limit refuses them.
            let at_cap = if *key == "workers" {
                "At the cap, Azure refuses new requests with error 10928 (the request limit for the database has been reached) until workers free up, so the application sees errors, not only slowness."
            } else {
                "At the cap, Azure makes requests wait rather than fail, so it shows up as slowness."
            };
            f.summary = format!(
                "Azure caps each tier's {}. In {span} it was at 95% or more of that cap in {} of the 15-second samples (peak {:.0}%). This is measured resource use, not a share of waits: {effect}. {at_cap} Samples near the cap alone do not prove throttling or failed requests occurred.",
                label.to_lowercase(),
                format::percent(at_limit),
                peak
            );
            f.severity = if at_limit >= 0.3 { Severity::High } else { Severity::Medium };
            f.impact.score = base_score(f.severity) + at_limit * 30.0;
            f.impact.label = format!("near limit {}", format::percent(at_limit));
            f.confidence = Confidence::High;
            f.metrics = vec![Metric::new("Samples at or above 95%", at_limit, Unit::Ratio), Metric::new("Peak", peak / 100.0, Unit::Ratio)];
            f.fix = Some(Fix { summary: (*fix).into(), sql: None, cost: None });
            f.verify = vec![format!("Re-scan: {} should stay under 95% for most of the time (the Health strip on Overview shows it).", label.to_lowercase())];
            Some(f)
        })
        .collect()
}
