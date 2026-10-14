//! Culprit's checks, and the pipeline that turns a snapshot into a report.
//!
//! - [`pipeline`]: the stages in order (prepare, detect, refine, rank) and how to extend them. Start here.
//! - [`all`]: every built-in check. A check looks for one kind of problem: an `<name>_info()` describing it and
//!   `<name>(&Snapshot)` returning findings, registered below.
//! - `refine`: rules over every check's findings (one cause, one finding; statements that are history).
//! - The other modules are the checks, grouped by what they look at, and the views built from a report (dashboard,
//!   digest, index advice, drift, agent brief).

mod actual;
mod advisor;
mod brief;
mod config;
mod config_health;
pub mod dashboard;
mod deadlocks;
mod digest;
pub mod drift;
mod index_health;
mod indexes;
mod insights;
mod maintenance;
pub mod pipeline;
mod pressure;
mod queries;
mod refine;
mod sensitivity;
mod signals;
mod summary;
pub(crate) mod support;
mod trends;
mod tuning;
pub mod waits;

use std::sync::Arc;

use culprit_core::{Capability, Check, CheckInfo, Finding, LockedCheck, Report, ServerInfo, Snapshot};

pub use advisor::index_advice;
pub use brief::agent_brief;
pub use dashboard::dashboard;
pub use digest::{digest, digest_since};
pub use drift::drift;
pub use pipeline::{Pipeline, Prepare, Refine};
pub(crate) use refine::{query_is_history, query_stopped, ran_once, without_database};
pub use summary::summarize;

struct Builtin {
    info: fn() -> CheckInfo,
    run: fn(&Snapshot) -> Vec<Finding>,
}

impl Check for Builtin {
    fn info(&self) -> CheckInfo {
        (self.info)()
    }

    fn run(&self, snapshot: &Snapshot) -> Vec<Finding> {
        (self.run)(snapshot)
    }
}

pub fn all() -> Vec<Box<dyn Check>> {
    let builtins = [
        Builtin { info: indexes::missing_index_info, run: indexes::missing_index },
        Builtin { info: indexes::filtered_scan_info, run: indexes::filtered_scan },
        Builtin { info: indexes::unused_index_info, run: indexes::unused_index },
        Builtin { info: indexes::duplicate_index_info, run: indexes::duplicate_index },
        Builtin { info: indexes::unindexed_fk_info, run: indexes::unindexed_fk },
        Builtin { info: index_health::overlapping_index_info, run: index_health::overlapping_index },
        Builtin { info: index_health::over_indexed_info, run: index_health::over_indexed },
        Builtin { info: index_health::scan_heavy_info, run: index_health::scan_heavy },
        Builtin { info: tuning::tuning_advice_info, run: tuning::tuning_advice },
        Builtin { info: tuning::query_family_info, run: tuning::query_family },
        Builtin { info: queries::heavy_query_info, run: queries::heavy_query },
        Builtin { info: queries::chatty_query_info, run: queries::chatty_query },
        Builtin { info: queries::plan_warnings_info, run: queries::plan_warnings },
        Builtin { info: insights::plan_insights_info, run: insights::plan_insights },
        Builtin { info: queries::plan_regression_info, run: queries::plan_regression },
        Builtin { info: trends::regression_info, run: trends::regression },
        Builtin { info: queries::unstable_query_info, run: queries::unstable_query },
        Builtin { info: sensitivity::parameter_sensitive_info, run: sensitivity::parameter_sensitive },
        Builtin { info: queries::temp_spill_info, run: queries::temp_spill },
        Builtin { info: maintenance::stale_stats_info, run: maintenance::stale_stats },
        Builtin { info: maintenance::heap_table_info, run: maintenance::heap_table },
        Builtin { info: maintenance::dead_rows_info, run: maintenance::dead_rows },
        Builtin { info: maintenance::xid_age_info, run: maintenance::xid_age },
        Builtin { info: config::config_info, run: config::config },
        Builtin { info: deadlocks::deadlocks_info, run: deadlocks::deadlocks },
        Builtin { info: pressure::memory_grant_info, run: pressure::memory_grant },
        Builtin { info: pressure::memory_queue_info, run: pressure::memory_queue },
        Builtin { info: pressure::compile_pressure_info, run: pressure::compile_pressure },
        Builtin { info: pressure::worker_pressure_info, run: pressure::worker_pressure },
        Builtin { info: pressure::held_cleanup_info, run: pressure::held_cleanup },
        Builtin { info: pressure::network_wait_info, run: pressure::network_wait },
        Builtin { info: pressure::tier_limit_info, run: pressure::tier_limit },
        Builtin { info: pressure::checkpoint_pressure_info, run: pressure::checkpoint_pressure },
        Builtin { info: pressure::row_lock_waits_info, run: pressure::row_lock_waits },
        Builtin { info: signals::failing_query_info, run: signals::failing_query },
        Builtin { info: signals::planning_time_info, run: signals::planning_time },
        Builtin { info: signals::jit_overhead_info, run: signals::jit_overhead },
        Builtin { info: signals::log_volume_info, run: signals::log_volume },
        Builtin { info: signals::lock_time_info, run: signals::lock_time },
        Builtin { info: signals::big_result_info, run: signals::big_result },
        Builtin { info: signals::hot_rows_info, run: signals::hot_rows },
        Builtin { info: signals::lock_escalations_info, run: signals::lock_escalations },
        Builtin { info: signals::last_page_insert_info, run: signals::last_page_insert },
        Builtin { info: signals::untrusted_constraint_info, run: signals::untrusted_constraint },
        Builtin { info: signals::missing_histogram_info, run: signals::missing_histogram },
        Builtin { info: signals::hot_updates_info, run: signals::hot_updates },
        Builtin { info: signals::table_bloat_info, run: signals::table_bloat },
        Builtin { info: signals::large_rows_info, run: signals::large_rows },
        Builtin { info: signals::slow_storage_info, run: signals::slow_storage },
        Builtin { info: signals::connection_churn_info, run: signals::connection_churn },
        Builtin { info: signals::replica_lag_info, run: signals::replica_lag },
        Builtin { info: actual::last_actual_plans_info, run: actual::last_actual_plans },
    ];
    builtins.into_iter().map(|b| Box::new(b) as Box<dyn Check>).collect()
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
