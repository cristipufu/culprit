//! Culprit's checks, and the pipeline that turns a snapshot into a report.
//!
//! - [`pipeline`]: the stages in order (prepare, detect, refine, rank) and how to extend them. Start here.
//! - [`all`]: every built-in check. A check looks for one kind of problem: an `<name>_info()` describing it and
//!   `<name>(&Snapshot)` returning findings, registered below.
//! - `refine`: rules over every check's findings (one cause, one finding; statements that are history).
//! - The other modules are the checks, grouped by what they look at, and the views built from a report (dashboard,
//!   digest, index advice, drift, agent brief).

mod checks;
mod evidence;
pub mod pipeline;
mod refine;
mod summary;
pub(crate) mod support;
mod views;

// Each area by its own name, wherever it lives: `crate::queries`, `culprit_checks::dashboard`.
pub(crate) use checks::{
    config, config_health, deadlocks, index_health, indexes, pressure, queries, signals, sqlite, tuning,
};
pub(crate) use views::{advisor, brief, digest};
pub use views::{dashboard, drift, waits};

use std::sync::Arc;

use culprit_core::{Capability, Check, CheckInfo, LockedCheck, Report, ServerInfo, Snapshot};

pub use advisor::index_advice;
pub use brief::{
    TOTAL_WAIT_METRICS, agent_brief, finding_no_share_reason, finding_step, finding_step_share, gone_after_rescan,
    metric_text, no_share_reason, plan_shares_note, query_share,
};
pub use dashboard::dashboard;
pub use digest::{digest, digest_since};
pub use drift::drift;
pub use pipeline::{Pipeline, Prepare, Refine};
pub(crate) use refine::{all_stopped, query_is_history, query_stopped, ran_once, without_database};
pub use summary::summarize;
pub use views::plan_advice::{annotate_plans, query_plan_insights};

/// Every built-in check: each area's own list (`checks::AREAS`), in the order they run.
pub fn all() -> Vec<Box<dyn Check>> {
    checks::AREAS.iter().flat_map(|area| area.iter()).map(|c| Box::new(Registered(*c)) as Box<dyn Check>).collect()
}

/// A built-in check as the pipeline holds checks: owned, like one an embedding application plugs in.
struct Registered(&'static dyn Check);

impl Check for Registered {
    fn info(&self) -> CheckInfo {
        self.0.info()
    }

    fn run(&self, snapshot: &Snapshot) -> Vec<culprit_core::Finding> {
        self.0.run(snapshot)
    }

    fn evidence(&self, snapshot: &Snapshot) -> culprit_core::CheckEvidence {
        self.0.evidence(snapshot)
    }
}

pub fn infos() -> Vec<CheckInfo> {
    infos_with(&[])
}

/// The built-in pipeline with checks plugged in by an embedding application.
fn with_extra(extra: &[Arc<dyn Check>]) -> Pipeline {
    extra.iter().fold(Pipeline::builtin(), |pipeline, check| pipeline.with_check(check.clone()))
}

/// Every check's metadata, including plugged-in ones. A plugged-in check whose id is already taken is ignored.
pub fn infos_with(extra: &[Arc<dyn Check>]) -> Vec<CheckInfo> {
    with_extra(extra).checks().iter().map(|c| c.info()).collect()
}

fn missing_for(info: &CheckInfo, server: &ServerInfo) -> Vec<Capability> {
    info.requires.iter().copied().filter(|c| !server.has(*c)).collect()
}

/// Checks that apply to this engine but can't run with what this login can see.
pub fn locked(server: &ServerInfo) -> Vec<LockedCheck> {
    locked_with(server, &[])
}

pub fn locked_with(server: &ServerInfo, extra: &[Arc<dyn Check>]) -> Vec<LockedCheck> {
    infos_with(extra)
        .into_iter()
        .filter(|info| info.engines.contains(&server.engine))
        .filter_map(|info| {
            let missing = missing_for(&info, server);
            (!missing.is_empty()).then_some(LockedCheck { check: info, missing })
        })
        .collect()
}

/// Run the built-in pipeline (see [`pipeline`]): prepare the snapshot, run every applicable check, refine, rank and
/// summarize.
pub fn analyze(snapshot: Snapshot) -> Report {
    Pipeline::builtin().run(snapshot)
}

/// [`analyze`] with extra checks plugged in by an embedding application. Plugged-in findings are refined, ranked and
/// summarized exactly like built-in ones; a plugged-in check whose id is already taken is ignored.
pub fn analyze_with(snapshot: Snapshot, extra: &[Arc<dyn Check>]) -> Report {
    with_extra(extra).run(snapshot)
}
