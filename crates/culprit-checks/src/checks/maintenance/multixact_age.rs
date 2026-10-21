use super::*;
use crate::evidence::{Observation, Requirement as Evidence};
pub fn multixact_age_info() -> CheckInfo {
    CheckInfo { id:"multixact-age".into(),title:"MultiXact wraparound risk".into(),category:Category::Maintenance,engines:vec![Engine::Postgres],requires:vec![],description:"The independent PostgreSQL MultiXact age limit, which can reject row-locking writes even when ordinary transaction ID age is healthy.".into() }
}
check!(
    MultixactAgeCheck: multixact_age_info, multixact_age,
    &[Evidence::Observed(Observation::Multixacts, &[culprit_core::source_id::MULTIXACTS])]
);
pub fn multixact_age(s: &Snapshot) -> Vec<Finding> {
    let Some(age) = s
        .operations
        .multixacts
        .iter()
        .filter(|a| {
            a.age.is_finite()
                && a.age
                    >= a.freeze_max_age
                        .filter(|v| v.is_finite() && *v > 0.0)
                        .map(|v| (v * 2.0).max(500_000_000.0))
                        .unwrap_or(800_000_000.0)
                        .min(1_200_000_000.0)
        })
        .max_by(|a, b| a.age.total_cmp(&b.age))
    else {
        return vec![];
    };
    let mut f = finding(
        "multixact-age",
        &s.server.database,
        Category::Maintenance,
        subject(SubjectKind::Database, &s.server.database),
    );
    f.title = format!("MultiXact IDs are {} old in {}", format::count(age.age), age.subject);
    f.summary="MultiXact IDs track shared row locks and have their own wraparound limit, independent of ordinary transaction IDs. If vacuum cannot advance them, PostgreSQL eventually rejects writes that require a new MultiXact. A healthy XID-age result does not rule this out. This observation is the current age, not a forecast of time remaining.".into();
    f.severity = if age.age >= 1_200_000_000.0 { Severity::High } else { Severity::Medium };
    f.confidence = Confidence::High;
    f.impact.score = base_score(f.severity);
    f.impact.label = format!("{} IDs old", format::count(age.age));
    f.metrics =
        vec![Metric::new("MultiXact age", age.age, Unit::Count), Metric::text("Oldest observed subject", &age.subject)];
    f.fix=Some(Fix {summary:"Inspect aggressive vacuum progress and transactions holding row locks, then vacuum the oldest affected tables under an operational plan. MultiXact cleanup differs from XID cleanup: replication slots do not directly retain MultiXacts, so removing slots is not a fix for this age.".into(),sql:None,cost:Some("Aggressive vacuum adds I/O and must finish to advance the oldest age; inspect locks and capacity before scheduling it.".into())});
    f.verify=vec!["Re-scan and confirm mxid_age(datminmxid) and the oldest tables' mxid_age(relminmxid) fall. Verify row-locking writes proceed and later vacuum cycles keep age controlled.".into()];
    vec![f]
}
