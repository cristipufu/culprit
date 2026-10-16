//! The `checkpoint-pressure` check: Many checkpoints start because WAL filled max_wal_size rather than on schedule, causing write spikes.

use super::*;

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

check!(CheckpointPressureCheck: checkpoint_pressure_info, checkpoint_pressure);

pub fn checkpoint_pressure(snapshot: &Snapshot) -> Vec<Finding> {
    let (Some(requested), Some(total)) = (signal(snapshot, "checkpoints-requested"), signal(snapshot, "checkpoints"))
    else {
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
