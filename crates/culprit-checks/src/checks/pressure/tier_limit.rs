//! The `tier-limit` check: Azure SQL Database resources (CPU, data IO, log writes, workers) at the tier's limit for a real part of the last hour.

use super::*;

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

check!(TierLimitCheck: tier_limit_info, tier_limit);

/// Resource samples a tier-limit finding needs: half an hour of them.
pub(super) const TIER_MIN_HOURS: f64 = 0.5;

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
    // Minutes of samples (after a failover, a scale-up or a serverless resume) aren't "a real part of the hour": at
    // least half an hour of them, and the span said as it is.
    let Some(span_hours) = days_between(&samples[0].at, &samples[samples.len() - 1].at).map(|d| d * 24.0) else {
        return Vec::new();
    };
    if span_hours < TIER_MIN_HOURS {
        return Vec::new();
    }
    // Azure keeps about an hour of these: a full hour reads as one.
    let span = if (span_hours - 1.0).abs() < 0.04 {
        "the last hour".to_string()
    } else {
        format!("the last {}", culprit_core::span_words(span_hours))
    };
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
            f.verify = vec![format!("Re-scan: {} should stay under 95% for most of the time (the Health strip on Overview shows it).", label.to_lowercase())];
            Some(f)
        })
        .collect()
}
