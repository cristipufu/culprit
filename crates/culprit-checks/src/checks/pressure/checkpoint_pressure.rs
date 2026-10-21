//! Requested checkpoints, with no inference of WAL pressure from the shared counter alone.

use super::*;
use crate::evidence::Requirement as Evidence;

pub fn checkpoint_pressure_info() -> CheckInfo {
    CheckInfo {
        id: "checkpoint-pressure".into(),
        title: "Requested checkpoints".into(),
        category: Category::Configuration,
        engines: vec![Engine::Postgres],
        requires: vec![],
        description:
            "The server's requested checkpoints, with the possible causes and the evidence needed before changing WAL settings."
                .into(),
    }
}

check!(
    CheckpointPressureCheck: checkpoint_pressure_info, checkpoint_pressure,
    &[Evidence::Pressure { signal: Some(culprit_core::signal_id::CHECKPOINTS_REQUESTED), timed_files: false, complete: true }]
);

pub fn checkpoint_pressure(snapshot: &Snapshot) -> Vec<Finding> {
    let (Some(requested), Some(total)) = (
        signal(snapshot, culprit_core::signal_id::CHECKPOINTS_REQUESTED),
        signal(snapshot, culprit_core::signal_id::CHECKPOINTS),
    ) else {
        return Vec::new();
    };
    if requested < 30.0 || total < 10.0 || snapshot.server.platform().is_some_and(|p| p.applies_wal_in_storage()) {
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
    f.title = format!("{:.0}% of the server's recorded checkpoints were requested rather than timed", requested);
    f.summary = format!(
        "{} of {} checkpoints across the whole server were requested rather than timed since the checkpoint counters were reset. Requests can come from WAL reaching max_wal_size{}, a manual CHECKPOINT, a base backup or CREATE DATABASE. This counter does not distinguish their causes or measure write latency, so it does not establish that WAL is too small or that this database has write pressure.",
        format::count(total * requested / 100.0),
        format::count(total),
        current.as_deref().map(|c| format!(" ({c})")).unwrap_or_default()
    );
    f.severity = Severity::Low;
    f.impact.score = base_score(f.severity) + requested / 10.0;
    f.impact.label = format!("{:.0}% requested", requested);
    f.confidence = Confidence::Medium;
    f.metrics = vec![
        Metric::new("Requested checkpoints", requested / 100.0, Unit::Ratio),
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
            "Inspect the checkpoint log (log_checkpoints) during normal load before changing anything. If it confirms frequent checkpoints caused by WAL filling max_wal_size, check free disk space and consider giving WAL more room: {} Backups and manual checkpoints need no WAL-size fix.",
            change.how
        ),
        sql: None,
        cost: Some("WAL takes more disk, and crash recovery can take a little longer.".into()),
    });
    f.verify = vec!["Check checkpoint reasons and write latency during comparable load. After a confirmed WAL-size change, WAL-triggered checkpoints should be less frequent; the requested-checkpoint ratio alone is not a health target.".into()];
    vec![f]
}
