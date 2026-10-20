use super::*;
use crate::evidence::{Observation, Requirement as Evidence};
use culprit_core::Actionability;
pub fn logged_errors_info() -> CheckInfo {
    CheckInfo{id:"logged-errors".into(),title:"SQL errors recorded by the application".into(),category:Category::Queries,engines:vec![Engine::Sqlite],requires:vec![Capability::QueryLog],description:"Recognized error codes in structured query logs, including errors with no duration. Counts describe log records, not failed business requests or zero-cost executions.".into()}
}
check!(LoggedErrorsCheck: logged_errors_info, logged_errors, &[Evidence::Observed(Observation::LogFailures, &[])]);
pub fn logged_errors(s: &Snapshot) -> Vec<Finding> {
    s.operations.log_failures.iter().filter(|e|e.count>0.0).map(|e|{
        let (why,fix)=match e.code.as_str(){
            "SQLITE_BUSY"=>("SQLite reported a conflicting connection or transaction. The caller may already retry successfully; these records do not establish failed user requests.","Inspect transaction duration and retry outcomes on the application's own connections. Keep write transactions short and serialize writers where appropriate. Set a suitable busy handler on each application connection; changing Culprit's connection does not change the app."),
            "SQLITE_LOCKED"=>("SQLite reported a conflict within the connection or a shared-cache connection. This differs from ordinary cross-connection SQLITE_BUSY and can require finishing an outstanding statement.","Inspect open statements and cursors on the application's connection, schema changes and any shared-cache use. Finalize or reset the conflicting statement when correct. A longer busy timeout does not resolve SQLITE_LOCKED; preserve transaction semantics when retrying."),
            "SQLITE_BUSY_SNAPSHOT"=>("A read transaction could not upgrade its older WAL snapshot to a writer. Waiting longer cannot make that snapshot current.","Roll back and retry the complete transaction with a fresh snapshot. If appropriate for its semantics, obtain the write transaction before reading (BEGIN IMMEDIATE). Do not retry only the failed statement inside the same obsolete snapshot."),
            "SQLITE_CONSTRAINT"=>("A constraint rejected an operation. Some applications deliberately use this for uniqueness or conflict handling.","Check the constraint and caller's intended conflict behavior. Keep integrity checks; verify the application handles expected conflicts correctly before changing SQL or the schema."),
            "SQLITE_FULL"=>("SQLite could not extend storage because a database or filesystem limit was reached. This is an observed error, not a claim that the limit is still reached now.","Check current filesystem headroom, database page limits and the application's storage quota. Restore capacity with the owner while preserving the database, WAL and a valid backup."),
            "SQLITE_READONLY"=>("The application's connection could not perform a write. Culprit's own read-only connection is separate.","Check the application's open mode, filesystem access and WAL/sidecar access using its own process identity. Preserve intended read-only deployments."),
            "SQLITE_IOERR"|"SQLITE_CORRUPT"=>("The application recorded a storage or database-integrity error. The log alone cannot establish its extent or whether recovery succeeded.","Preserve the original files and logs. Investigate storage and restore/recovery options with the owner before any repair; use an appropriate backup and offline integrity investigation rather than rewriting the live file."),
            _=>("The log records a SQL error whose detailed code is not recognized. Its raw message and values were not retained.","Inspect the original error in the application's log, its transaction context and retry outcome. Measure a representative successful execution separately when timing is missing."),
        };
        let now=parse_epoch_seconds(&s.taken_at);let recent=e.last_at.as_deref().and_then(parse_epoch_seconds).zip(now).is_some_and(|(a,n)|a<=n && a>=n-f64::from(s.options.as_ref().map_or(24*60,|o|o.window_in_minutes()))*60.0);
        let critical=recent&&matches!(e.code.as_str(),"SQLITE_FULL"|"SQLITE_IOERR"|"SQLITE_CORRUPT");
        let mut f=finding("logged-errors",&format!("{}:{}",e.query_id,e.code),Category::Queries,subject(SubjectKind::Query,&e.query_id));
        f.title=format!("{} appears in {} application log records",e.code,format::count(e.count));
        f.summary=format!("{why} These are records in the supplied log, which can include retries or duplicate logging.{} Missing durations are not counted as instant executions.",e.last_at.as_deref().map(|a|format!(" Latest dated record: {a}.")).unwrap_or_else(||" Their dates are unknown.".into()));
        f.severity=if critical{Severity::Medium}else{Severity::Low};f.actionability=if critical{Actionability::NeedsAttention}else{Actionability::Information};f.confidence=Confidence::High;f.impact.score=base_score(f.severity);f.impact.label=format!("{} logged records",format::count(e.count));
        f.metrics=vec![Metric::text("Code",&e.code),Metric::text("Normalized statement",&e.statement),Metric::new("Log records",e.count,Unit::Count)];
        if let Some(q)=s.workload.as_ref().and_then(|w|w.query(&e.query_id)){f.queries=crate::support::query_refs(&[q]);}
        f.fix=Some(Fix{summary:fix.into(),sql:None,cost:None});f.verify=vec!["Compare a fresh, dated log interval and application request/retry outcomes. Confirm the operation succeeds with its intended transaction semantics; old log records need not disappear.".into()];f
    }).collect()
}
