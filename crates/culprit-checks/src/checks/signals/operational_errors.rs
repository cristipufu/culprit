use super::*;
use crate::evidence::{Observation, Requirement as Evidence};
use culprit_core::operations::OperationalCounterKind as Kind;
use culprit_core::{Actionability, FileScope};
pub fn operational_errors_info() -> CheckInfo {
    CheckInfo {id:"operational-errors".into(),title:"Archive, replication and redo failures".into(),category:Category::Maintenance,engines:vec![Engine::Postgres,Engine::Mysql],requires:vec![],description:"WAL archive failures, logical subscription errors, standby recovery cancellations and InnoDB redo buffer waits. Counter history is kept separate from newly observed errors.".into()}
}
check!(
    OperationalErrorsCheck: operational_errors_info, operational_errors,
    &[Evidence::Observed(
        Observation::OperationalCounters,
        &[
            culprit_core::source_id::ARCHIVER,
            culprit_core::source_id::SUBSCRIPTION_ERRORS,
            culprit_core::source_id::STANDBY_CONFLICTS,
            culprit_core::source_id::REDO_PRESSURE,
        ],
    )]
);
pub fn operational_errors(s: &Snapshot) -> Vec<Finding> {
    s.operations.counters.iter().filter_map(|c| {
        if !c.count.is_finite() || c.count <= 0.0 {return None;}
        let (key,label,why,fix) = match c.kind {
            Kind::ArchiveFailures => ("archive","WAL archive failures","The archive command or library failed. This is a shared server counter, and a later successful archive may have resolved the incident.","Check current archiver progress and server logs for the failure, archive destination capacity and access. Confirm retained WAL and backup/recovery requirements; never remove WAL files to clear the counter."),
            Kind::SubscriptionApplyErrors => ("subscription-apply","Logical replication apply errors","The subscriber could not apply some incoming changes. Counters do not identify the conflicting row or establish that replication is still stopped.","Inspect the subscription worker state and server log. Resolve the reported schema, constraint or data conflict with the replication owner, then verify apply resumes. Do not skip transactions without proving correctness."),
            Kind::SubscriptionSyncErrors => ("subscription-sync","Logical replication synchronization errors","Initial table synchronization failed. A retained error count does not establish that the table is still unsynchronized.","Inspect subscription relation states and the worker's server-log error. Repair permissions, schema or capacity as indicated, then verify affected tables finish synchronization."),
            Kind::StandbyConflicts => ("standby-conflicts","Standby queries cancelled for recovery","Recovery cancelled queries whose snapshots, locks or buffers conflicted with replay. This records actual cancellations, not ordinary replica lag or a claim that every read failed.","Correlate cancelled requests with recovery-conflict logs and query duration. Consider shorter reads or a suitable reporting replica; feedback and longer replay delays trade cancellations for primary bloat, retained WAL or replica lag."),
            Kind::RedoLogWaits => ("redo-waits","Redo buffer waits (whole server)","InnoDB waited for redo buffer space to become available. This counter does not measure wait duration or prove that this database's requests were delayed.","Compare counter growth, redo write/flush latency and representative write activity across the server. Inspect transaction size, log-buffer capacity and storage before testing a setting change; preserve durability requirements."),
        };
        let now=parse_epoch_seconds(&s.taken_at);
        let start=now.map(|n|n-f64::from(s.options.as_ref().map_or(24*60,|o|o.window_in_minutes()))*60.0);
        let recent=c.before.as_ref().filter(|b| parse_epoch_seconds(&b.at).zip(start).is_some_and(|(a,start)|a>=start)
            && parse_epoch_seconds(&b.at).zip(now).is_some_and(|(a,n)|a<n) && c.count >= b.count);
        let delta=recent.map(|b|c.count-b.count);
        let shared=c.scope==FileScope::Server;
        let current=delta.is_some_and(|d|d>=10.0) && !shared;
        let mut f=finding("operational-errors",&format!("{key}:{}",c.subject),Category::Maintenance,subject(SubjectKind::Database,&c.subject));
        f.title=if let Some(delta)=delta {format!("{label}: {} newly counted between scans",format::count(delta))} else {format!("{label}: {} recorded in the source's history",format::count(c.count))};
        if delta == Some(0.0) {return None;}
        f.summary=format!("{} {}{}{} {}",c.subject,why,c.since.as_deref().map(|a|format!(" The total covers since {a}.")).unwrap_or_else(||" Its statistics period is unknown.".into()),recent.map(|b|format!(" The comparison covers {} to {}.",b.at,s.taken_at)).unwrap_or_default(),if shared {"It is context for the whole server and is not graded as a problem in this database."}else if recent.is_none(){"No comparable counter interval inside the requested period establishes new failures; this is historical information."}else{"These are newly recorded events over the stated interval, not proof that the cause persists now."});
        f.severity=if current {Severity::Medium}else{Severity::Low};f.actionability=if current{Actionability::NeedsAttention}else{Actionability::Information};f.confidence=Confidence::High;f.impact.score=base_score(f.severity);f.impact.label=if let Some(d)=delta{format!("{} between scans",format::count(d))}else{"Historical counter".into()};
        f.metrics=vec![Metric::new("Source total",c.count,Unit::Count)];if let Some(d)=delta{f.metrics.push(Metric::new("New events between scans",d,Unit::Count));}
        if let Some(a)=&c.last_event_at{f.metrics.push(Metric::text("Latest recorded event",a));}
        f.fix=Some(Fix{summary:fix.into(),sql:None,cost:None});f.verify=vec!["Compare two scans over a stated interval: new errors or waits should stop increasing, and the relevant worker/archive or application operations should succeed. Lifetime totals need not return to zero.".into()];Some(f)
    }).collect()
}
