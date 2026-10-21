//! Instance-wide compilation context, only when this database also has literal query families to inspect.

use super::*;
use crate::evidence::Requirement as Evidence;

pub fn compile_pressure_info() -> CheckInfo {
    CheckInfo {
        id: "compile-pressure".into(),
        title: "Instance compilation context".into(),
        category: Category::Configuration,
        engines: vec![Engine::Mssql],
        requires: vec![],
        description: "Shared compilation counters provide context for this database's literal query families; measured Query Store costs establish its compilation impact.".into(),
    }
}

check!(
    CompilePressureCheck: compile_pressure_info, compile_pressure,
    &[Evidence::Pressure { signal: Some(culprit_core::signal_id::COMPILES_PER_BATCH), timed_files: false, complete: false }]
);

pub fn compile_pressure(snapshot: &Snapshot) -> Vec<Finding> {
    let Some(compiles) = pressure(snapshot).and_then(|p| p.signal(culprit_core::signal_id::COMPILES_PER_BATCH)) else {
        return Vec::new();
    };
    // Shared counters alone establish no compilation problem here; retained only as context when this database's own literal families make inspection worthwhile. Query Store supplies the database's measured compile costs.
    if snapshot.server.on(culprit_core::Platform::AzureSqlDatabase) || snapshot.query_families.is_empty() {
        return Vec::new();
    }
    let ratio = compiles.value;
    // The load behind the ratio, averaged over at least an hour since the start.
    let (Some(batches), Some(_)) = (signal(snapshot, culprit_core::signal_id::BATCH_REQUESTS), counter_hours(snapshot))
    else {
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
        "The SQL Server instance recorded compilation for {:.0}% of batches (about {} a minute)",
        ratio,
        format::count(per_min)
    );
    f.summary = format!(
        "{}, the instance averaged {}/s batches across all its databases and compiled a new plan for {:.0}% of them, \
         recompiles not counted. This ratio does not identify this database's compiles or measure its CPU pressure. This database has statements with different literal values; its own Query Store compile costs can show whether they warrant work.",
        capitalize(&uptime_phrase(snapshot)),
        format::count(batches),
        ratio
    );
    // Historical instance-wide context, without assigning the server's cost to this database.
    f.severity = Severity::Low;
    f.impact.score = base_score(f.severity) + ratio / 5.0;
    f.impact.label = format!("{:.0}% of batches compile", ratio);
    f.confidence = Confidence::Medium;
    f.metrics = vec![
        Metric::new("Compiles per 100 batches", ratio, Unit::Count),
        Metric::new("Compiles per minute", per_min, Unit::Count),
    ];
    if let Some(recompiles) = signal(snapshot, culprit_core::signal_id::RECOMPILES_PER_BATCH).filter(|r| *r > 0.0) {
        f.metrics.push(Metric::new("Recompiles per 100 batches", recompiles, Unit::Count));
    }
    f.fix = Some(Fix {
        summary: "Inspect this database's Query Store compile counts and durations and its query-family findings. If those measurements show costly repeated compilation, parameterize the affected statements. Do not change database-wide parameterization from the instance's ratio alone.".into(),
        sql: None,
        cost: None,
    });
    f.verify = vec!["Compare this database's measured compilation costs under comparable load before and after the change. The instance-wide ratio alone cannot verify a fix in this database.".into()];
    vec![f]
}
