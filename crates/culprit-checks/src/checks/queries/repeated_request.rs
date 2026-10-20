use super::*;
use crate::evidence::{Observation, Requirement as Evidence};
pub fn repeated_request_info() -> CheckInfo {
    CheckInfo{id:"repeated-request".into(),title:"Repeated statements within logged requests".into(),category:Category::Queries,engines:vec![Engine::Mssql,Engine::Postgres,Engine::Mysql,Engine::Sqlite],requires:vec![Capability::QueryLog],description:"Repeated SQL shapes correlated by explicit request_id in structured logs. Repetition identifies a caller to inspect; it does not prove an N+1 bug or removable work.".into()}
}
check!(
    RepeatedRequestCheck: repeated_request_info, repeated_request,
    &[Evidence::Observed(Observation::RequestCounts, &[])]
);
pub fn repeated_request(s: &Snapshot) -> Vec<Finding> {
    s.queries().iter().filter_map(|q|{
        let r=q.requests.as_ref()?;if r.repeated_requests<3||r.most_calls<10{return None;}
        let mut f=finding("repeated-request",&q.id,Category::Queries,subject(SubjectKind::Query,&q.id));
        f.title=format!("The same statement repeats at least 10 times in {} logged requests",r.repeated_requests);
        f.summary=format!("Explicit request_id values correlate this statement with {} recorded requests; one contains {} timed SQL records. Repeated per-object loads, intentional batches or retries can explain this. The log does not prove an N+1 bug, network round trips or redundant work.{} Request identifiers are hashed during reading and are not stored in the report.",r.requests,r.most_calls,if r.sample_truncated{" Request counting reached its bounded sample limit; these counts are a lower bound."}else{""});
        f.severity=Severity::Low;f.confidence=Confidence::Medium;f.impact.score=base_score(Severity::Low);f.impact.label=format!("{} repeated request groups",r.repeated_requests);f.queries=query_refs(&[q]);
        f.metrics=vec![Metric::new("Logged request groups",r.requests as f64,Unit::Count),Metric::new("Groups with at least 10 calls",r.repeated_requests as f64,Unit::Count),Metric::new("Most calls in one group",r.most_calls as f64,Unit::Count)];
        f.fix=Some(Fix{summary:"Inspect the caller for the correlated request groups. If it loads one object at a time unnecessarily, test a set-based query or appropriate eager loading while preserving ordering, permissions, limits and result semantics. Keep required batches and retries; SQLite's embedded calls have no server network round trip.".into(),sql:None,cost:None});
        f.verify=vec!["Compare equivalent request groups before and after: unnecessary SQL records and total time should fall while results, permission boundaries and retry correctness remain the same.".into()];Some(f)
    }).collect()
}
