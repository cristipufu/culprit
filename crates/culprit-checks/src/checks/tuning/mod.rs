//! What the engine already knows: SQL Server's own tuning recommendations, forced plans that stopped applying, and
//! statements the application sends with literals baked in (so each is compiled and tracked on its own).

use culprit_core::{
    Capability, Category, CheckInfo, Confidence, Engine, Finding, Fix, ForcedPlan, IndexStat, Metric, NodeKind,
    QueryRef, QueryStat, RecommendedIndex, Severity, Snapshot, SubjectKind, TuningRecommendation, Unit, format,
};

use crate::indexes::{is_unused, leading_index, merge_candidates, redundant_indexes, split_object};
use culprit_core::Check;

use crate::support::*;

mod query_family;
mod tuning_advice;

pub use query_family::*;
pub use tuning_advice::*;

/// The checks this area registers (`crate::all()` runs every area's), in the order their findings tie.
pub(crate) const CHECKS: &[&dyn Check] = &[&TuningAdviceCheck, &QueryFamilyCheck];

/// Whether SQL Server's automatic plan correction (FORCE_LAST_GOOD_PLAN) runs for this database, as it reports it
/// (`actual_state`): `None` where the scan didn't read it.
pub(crate) fn automatic_plan_correction(snapshot: &Snapshot) -> Option<bool> {
    let option = snapshot
        .tuning
        .as_ref()?
        .automatic_tuning
        .iter()
        .find(|o| o.name.eq_ignore_ascii_case("FORCE_LAST_GOOD_PLAN"))?;
    Some(option.actual.eq_ignore_ascii_case("ON"))
}

/// What every fix that forces a plan by hand says where automatic plan correction is on: SQL Server forces the last
/// good plan itself once it is sure of a regression, and unforces one that turns out slower, while a plan forced by hand
/// stays forced until someone unforces it. `None` where it is off or unknown.
pub(crate) fn hand_forcing_note(snapshot: &Snapshot) -> Option<String> {
    automatic_plan_correction(snapshot).filter(|on| *on).map(|_| {
        "Automatic plan correction (FORCE_LAST_GOOD_PLAN) is on for this database: SQL Server forces the last good plan itself once it is sure a regression costs enough (sys.dm_db_tuning_recommendations lists the ones it found), and unforces a plan it forced that turns out slower. A plan forced by hand stays forced until someone unforces it, whatever changes after."
            .to_string()
    })
}
