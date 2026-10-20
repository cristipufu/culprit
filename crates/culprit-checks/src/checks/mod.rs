//! Every built-in check, one directory per area and one file per check. A check looks for one kind of problem: a pure
//! function of the snapshot that returns findings (`<name>(&Snapshot) -> Vec<Finding>`), with its description
//! (`<name>_info()`), declared as a type with [`check!`] and listed in its area's `CHECKS`. Checks never touch a connection, and each follows the evidence rules
//! in `crate::support` (docs/ARCHITECTURE.md lists them).
//!
//! | Area | What it looks at |
//! |---|---|
//! | `queries/` | statements: their time, round trips, plan warnings, plan insights, regressions and plan changes, parameter sensitivity, spread, spills |
//! | `indexes/`, `index_health/` | indexes to add, unused, duplicate or overlapping ones, foreign keys, scans |
//! | `signals/` | beyond time: failures, storage, connections, locks, replication, table shape |
//! | `pressure/` | the server: memory, compiles, workers, cleanup, tier limits, checkpoints |
//! | `maintenance/` | statistics, heaps, dead rows, transaction ID age |
//! | `tuning/` | what the engine already recommends, and statements sent with literals |
//! | `config.rs`, `config_health.rs`, `sqlite.rs` | settings, per engine |
//! | `deadlocks.rs`, `actual.rs` | deadlocks, and whether the engine keeps actual plans |

use culprit_core::Check;

/// Declares a check as a type: `check!(HeavyQueryCheck: heavy_query_info, heavy_query)` makes `HeavyQueryCheck`, a
/// `culprit_core::Check` whose description and run are the check's two functions. Built-in checks are the same thing
/// an embedding application plugs in: a type implementing `Check`, listed in its area's `CHECKS`.
macro_rules! check {
    ($ty:ident: $info:ident, $run:ident) => {
        check!($ty: $info, $run, &[]);
    };
    ($ty:ident: $info:ident, $run:ident, $requirements:expr) => {
        #[doc = concat!("The `", stringify!($run), "` check, as a `culprit_core::Check`.")]
        pub struct $ty;

        impl culprit_core::Check for $ty {
            fn info(&self) -> culprit_core::CheckInfo {
                $info()
            }

            fn run(&self, snapshot: &culprit_core::Snapshot) -> Vec<culprit_core::Finding> {
                $run(snapshot)
            }

            fn evidence(&self, snapshot: &culprit_core::Snapshot) -> culprit_core::CheckEvidence {
                crate::evidence::assess($requirements, snapshot)
            }
        }
    };
}

pub(crate) mod actual;
pub(crate) mod config;
pub(crate) mod config_health;
pub(crate) mod deadlocks;
pub(crate) mod index_health;
pub(crate) mod indexes;
pub(crate) mod maintenance;
pub(crate) mod pressure;
pub(crate) mod queries;
pub(crate) mod signals;
pub(crate) mod sqlite;
pub(crate) mod tuning;

/// Every area's checks, in the order they run: each area lists its own (`<area>::CHECKS`), so a new check is its file
/// and one line in its area.
pub(crate) const AREAS: &[&[&dyn Check]] = &[
    indexes::CHECKS,
    index_health::CHECKS,
    tuning::CHECKS,
    queries::CHECKS,
    maintenance::CHECKS,
    config::CHECKS,
    deadlocks::CHECKS,
    pressure::CHECKS,
    signals::CHECKS,
    actual::CHECKS,
];
