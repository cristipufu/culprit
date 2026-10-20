use super::*;
use crate::evidence::{Observation, Requirement as Evidence};
use culprit_core::{Actionability, FileScope};

pub fn storage_capacity_info() -> CheckInfo {
    CheckInfo { id: "storage-capacity".into(), title: "Files and logs near their allocation limits".into(), category: Category::Configuration, engines: vec![Engine::Mssql], requires: vec![], description: "Current database file and transaction-log capacity, log reuse reasons, and shared tempdb allocation. Autogrowth room is distinguished from a hard limit.".into() }
}
check!(
    StorageCapacityCheck: storage_capacity_info, storage_capacity,
    &[Evidence::Observed(Observation::Spaces, &["log_capacity", "file_capacity", "temp_capacity"])]
);

pub fn storage_capacity(s: &Snapshot) -> Vec<Finding> {
    s.operations.spaces.iter().filter_map(|space| {
        if !space.total_bytes.is_finite() || !space.used_bytes.is_finite() || space.total_bytes <= 0.0 || space.used_bytes < 0.0 || space.used_bytes > space.total_bytes { return None; }
        let hard = space.growth_enabled == Some(false);
        let capacity = if hard { space.total_bytes } else { space.limit_bytes.filter(|v| *v >= space.total_bytes).unwrap_or(space.total_bytes) };
        let share = space.used_bytes / capacity;
        if share < 0.9 { return None; }
        let shared = space.scope == FileScope::Server;
        let constrained = hard || space.limit_bytes.is_some_and(|v| v <= space.total_bytes);
        let reuse = space.reuse_wait.as_deref().filter(|w| *w != "NOTHING");
        // A nearly occupied log with autogrowth room is ordinary operation, not a full-disk diagnosis.
        let actionable = !shared && constrained && share >= 0.95 && (space.kind != FileKind::Log || reuse.is_some());
        let mut f = finding("storage-capacity", &space.name, Category::Configuration, subject(SubjectKind::Database, &space.name));
        f.title = format!("{} uses {} of {} {}", space.name, format::bytes(space.used_bytes), format::bytes(capacity), if constrained { "available allocation" } else if capacity > space.total_bytes { "configured capacity" } else { "currently allocated space" });
        f.summary = format!("Measured when the scan ran: {} free in this allocation. {}{}{} This does not measure free filesystem space or predict when it will fill.", format::bytes((capacity-space.used_bytes).max(0.0)), if shared { "These files are shared by the whole instance; this does not attribute their use to the selected database. " } else { "" }, if constrained { "The files cannot grow beyond this allocation under their current settings. " } else { "Growth settings or additional storage may permit more space. Allocated space being mostly occupied alone is not an incident. " }, reuse.map(|r| format!("SQL Server currently reports log_reuse_wait_desc = {r}. ")).unwrap_or_default());
        f.severity = if actionable { Severity::Medium } else { Severity::Low };
        f.actionability = if actionable { Actionability::NeedsAttention } else { Actionability::Information };
        f.confidence = Confidence::High;
        f.impact.score = base_score(f.severity);
        f.impact.label = format!("{} occupied", format::percent(share));
        f.metrics = vec![Metric::new("Used",space.used_bytes,Unit::Bytes), Metric::new("Allocation",capacity,Unit::Bytes),Metric::new("Free in allocation",(capacity-space.used_bytes).max(0.0),Unit::Bytes)];
        f.metrics.extend(space.parts.iter().filter(|p|p.bytes.is_finite()&&p.bytes>=0.0).map(|p|Metric::new(&p.name,p.bytes,Unit::Bytes)));
        f.fix = Some(Fix { summary: if space.kind == FileKind::Log { "Inspect the reported reuse reason: restore scheduled log backups for LOG_BACKUP, examine the owning transaction for ACTIVE_TRANSACTION, or repair the affected replication consumer. Confirm retention/recovery requirements and real disk headroom before increasing limits. Shrinking the log does not repair retained log records.".into() } else if shared { "Inspect tempdb's user objects, internal objects and version-store allocation and the sessions using them. Check growth settings and disk headroom. Do not attribute shared use to this database without per-session evidence.".into() } else { "Check filesystem headroom and the file's growth/max-size configuration. Plan added capacity or appropriate retention with the owner; do not remove data or shrink files merely to clear this observation.".into() }, sql: None, cost: None });
        f.verify = vec!["Compare used/free allocation, growth settings and any log reuse reason in subsequent scans during representative load. Confirm writes can proceed and filesystem space remains available.".into()];
        Some(f)
    }).collect()
}
