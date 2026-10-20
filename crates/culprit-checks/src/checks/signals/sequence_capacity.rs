use super::*;
pub fn sequence_capacity_info() -> CheckInfo {
    CheckInfo { id: "sequence-capacity".into(), title: "Identity and sequence allocation limits".into(), category: Category::Maintenance, engines: vec![Engine::Mssql,Engine::Postgres,Engine::Mysql], requires: vec![], description: "Non-cycling identities, sequences and auto-increment columns approaching their numeric bounds; cache allocation is not a forecast of remaining requests.".into() }
}
check!(SequenceCapacityCheck: sequence_capacity_info, sequence_capacity);
pub fn sequence_capacity(s: &Snapshot) -> Vec<Finding> {
    s.operations.sequences.iter().filter_map(|seq| {
        let (remaining,total) = seq.remaining()?;
        if total == 0 { return None; }
        let fraction = remaining as f64 / total as f64;
        if fraction > 0.01 && (remaining > 10_000 || fraction > 0.1) { return None; }
        let mut f = finding("sequence-capacity", &seq.name, Category::Maintenance, subject(SubjectKind::Database, &seq.name));
        f.title = format!("{} is near its numeric limit ({} recorded steps remain)",seq.name,format::count(remaining as f64));
        f.summary = format!("The recorded value is {}, increment {}, with bounds {} to {} and cycling off. {} Caching, skipped values and per-connection allocation rules mean this is not the number of application requests left or a time-to-failure estimate. Reaching the bound can reject inserts or allocations; reusing old numbers can violate uniqueness.",seq.current,seq.increment,seq.minimum,seq.maximum,if seq.allocated { "The source may record reserved cache space or the next auto-increment value rather than the last value returned to a caller." } else { "This is the allocation state observed by the source." });
        f.severity = if remaining <= 10_000 { Severity::Medium } else { Severity::Low };
        f.confidence = Confidence::High; f.impact.score=base_score(f.severity);f.impact.label=format!("{} steps to bound",format::count(remaining as f64));
        f.metrics=vec![Metric::text("Recorded value",&seq.current),Metric::text("Minimum",&seq.minimum),Metric::text("Maximum",&seq.maximum),Metric::new("Steps after recorded allocation",remaining as f64,Unit::Count)];
        f.fix=Some(Fix { summary:"Plan a migration to a larger numeric range, including referencing columns, constraints, indexes and application types. Validate cache/increment behavior and rollout locking first. Do not reseed, restart or enable cycling without proving values cannot collide.".into(),sql:None,cost:Some("Changing column types and dependent keys may rewrite data and require locks; plan and test the migration.".into()) });
        f.verify=vec!["Check the new type and allocation bounds, confirm foreign keys and application values remain compatible, and verify inserts or sequence allocations under representative load.".into()];Some(f)
    }).collect()
}
