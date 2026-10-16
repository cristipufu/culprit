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
