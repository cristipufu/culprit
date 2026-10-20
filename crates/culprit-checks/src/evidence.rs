//! One account of whether a negative check result was actually assessed. Findings may still be produced from partial
//! evidence, but an absent source, a short history or a missing measurement can never be called healthy.

use culprit_core::{CheckEvidence, Snapshot};

pub(crate) fn source_failed(s: &Snapshot, names: &[&str]) -> Option<String> {
    s.notes.iter().find(|n| names.iter().any(|name| n.collector.replace(' ', "_") == *name)).map(|n| n.message.clone())
}

pub(crate) fn index_catalog(s: &Snapshot) -> Option<String> {
    source_failed(s, &["indexes", "foreign_keys", "columns"])
}

pub(crate) fn for_check(id: &str, s: &Snapshot) -> CheckEvidence {
    use CheckEvidence::*;
    let insufficient = |why: &str| Insufficient(why.into());
    let operational = match id {
        "logged-errors" => Some((!s.operations.log_failures.is_empty(), &[][..])),
        "repeated-request" => Some((s.queries().iter().any(|q| q.requests.is_some()), &[][..])),
        "storage-capacity" => {
            Some((!s.operations.spaces.is_empty(), &["log_capacity", "file_capacity", "temp_capacity"][..]))
        }
        "sequence-capacity" => Some((!s.operations.sequences.is_empty(), &["sequence_capacity"][..])),
        "multixact-age" => Some((!s.operations.multixacts.is_empty(), &["multixacts"][..])),
        "operational-errors" => Some((
            !s.operations.counters.is_empty(),
            &["archiver", "subscription_errors", "standby_conflicts", "redo_pressure"][..],
        )),
        "cluster-health" => Some((s.operations.cluster.is_some(), &["cluster_health"][..])),
        "sqlite-wal" => Some((s.operations.sqlite.as_ref().is_some_and(|w| w.wal_bytes.is_some()), &["wal_file"][..])),
        _ => None,
    };
    if let Some((known, sources)) = operational {
        if let Some(reason) = source_failed(s, sources) {
            return if known { Insufficient(reason) } else { Unavailable(reason) };
        }
        return if known {
            Available
        } else {
            insufficient(
                "This source was not recorded, was not applicable, or did not expose visible objects. No clean result can be established from its absence.",
            )
        };
    }
    if matches!(
        id,
        "unindexed-fk" | "duplicate-index" | "overlapping-index" | "over-indexed" | "filtered-scan" | "missing-index"
    ) && let Some(reason) = index_catalog(s)
    {
        return Unavailable(format!("Current schema evidence was incomplete: {reason}"));
    }
    if id == "unindexed-fk" && s.foreign_keys.is_empty() {
        return NotApplicable("No foreign keys were collected in this database.".into());
    }
    if id == "unused-index" && !crate::indexes::watched_long_enough(s) {
        return insufficient(
            "At least a week of comparable index-usage counters is needed to assess absence of reads.",
        );
    }
    let query_check = matches!(
        id,
        "heavy-query"
            | "chatty-query"
            | "query-family"
            | "plan-warnings"
            | "plan-insights"
            | "plan-regression"
            | "regression"
            | "unstable-query"
            | "parameter-sensitive"
            | "temp-spill"
            | "memory-grant"
            | "network-wait"
            | "failing-query"
            | "planning-time"
            | "jit-overhead"
            | "log-volume"
            | "lock-time"
            | "big-result"
            | "missing-histogram"
            | "deep-pagination"
            | "partition-access"
    );
    if query_check {
        let Some(w) = s.workload.as_ref() else {
            if id == "memory-grant" && s.pressure.as_ref().is_some_and(|p| !p.grants.is_empty()) {
                return insufficient("Current grant evidence is available, but statement history was not collected.");
            }
            return Unavailable(s.no_workload_reason().unwrap_or_else(|| "No statement history was collected.".into()));
        };
        if w.queries.is_empty() {
            return insufficient(
                "No statements were captured; this does not establish the absence of workload problems.",
            );
        }
        if matches!(id, "plan-warnings" | "plan-insights" | "deep-pagination" | "partition-access") {
            let plans = w.queries.iter().filter(|q| q.plan.is_some()).count();
            if plans < w.queries.len() {
                return insufficient(
                    "Only some statements have captured plans; unmeasured runs and uncaptured statements remain unassessed.",
                );
            }
            return insufficient(
                "Captured plans were inspected. Actual rows, timing and runtime-only problems are assessed only where that plan records them.",
            );
        }
        if id == "regression" && !w.timeline.as_ref().is_some_and(|t| t.len() >= 4) {
            return insufficient("A comparable baseline and later workload are needed to assess regressions.");
        }
        if id == "plan-regression" && !w.queries.iter().any(|q| q.plans.len() > 1) {
            return insufficient("No comparable earlier and later plan variants were recorded.");
        }
        let costs =
            |f: fn(&culprit_core::QueryCosts) -> bool| w.queries.iter().all(|q| q.costs.as_ref().is_some_and(f));
        let known = match id {
            "unstable-query" => {
                w.queries.iter().all(|q| q.stddev_time_ms.is_some() && q.max_time_ms.is_some() && q.executions >= 10.0)
            }
            "parameter-sensitive" => w.queries.iter().all(|q| q.plans.len() > 1 || q.spread.is_some()),
            "failing-query" => costs(|c| c.failed.is_some() || c.aborted.is_some()),
            "planning-time" => costs(|c| c.plan_ms.is_some()),
            "jit-overhead" => costs(|c| c.jit_ms.is_some()),
            "log-volume" => costs(|c| c.log_bytes.is_some()),
            "big-result" => w.queries.iter().all(|q| q.rows_per_execution().is_some()),
            "missing-histogram" => s.histograms.is_some(),
            "temp-spill" => w.queries.iter().all(|q| {
                q.temp_writes.is_some() || q.plan.as_ref().is_some_and(|p| p.kind == culprit_core::PlanKind::Actual)
            }),
            "memory-grant" => s.pressure.as_ref().is_some_and(|p| !p.grants.is_empty()),
            "lock-time" | "network-wait" => w.queries.iter().any(|q| !q.waits.is_empty()),
            _ => true,
        };
        if !known {
            return insufficient("This scan did not record the measurements this check needs.");
        }
        return Available;
    }
    let Some(p) = s.pressure.as_ref() else {
        if matches!(
            id,
            "memory-queue"
                | "compile-pressure"
                | "worker-pressure"
                | "held-cleanup"
                | "tier-limit"
                | "checkpoint-pressure"
                | "row-lock-waits"
                | "slow-storage"
                | "connection-churn"
                | "replica-lag"
        ) {
            return Unavailable("The source measurements needed for this assessment were not collected.".into());
        }
        return insufficient("This scan does not establish complete coverage of this check.");
    };
    let key = match id {
        "memory-queue" => Some("memory-grants-pending"),
        "compile-pressure" => Some("compiles-per-batch"),
        "worker-pressure" => Some("connections"),
        "checkpoint-pressure" => Some("checkpoints-requested"),
        "row-lock-waits" => Some("row-lock-avg"),
        _ => None,
    };
    if key.is_some_and(|k| p.signal(k).is_none()) {
        return insufficient("The required pressure measurement was not recorded by this scan.");
    }
    match id {
        "slow-storage" if !p.files.iter().any(|f| f.read_ms.is_some() || f.write_ms.is_some()) => {
            insufficient("Storage operations were not timed in this scan.")
        }
        "held-cleanup" | "replica-lag" => insufficient(
            "Visibility of cleanup holders or replication depends on the collected sources and this login's permissions; these sources do not establish complete coverage.",
        ),
        "config" if s.settings.is_empty() => insufficient("No settings were collected."),
        "deadlocks" if s.deadlocks.is_none() => insufficient("No deadlock evidence was collected."),
        "memory-queue" | "worker-pressure" | "row-lock-waits" | "slow-storage" | "checkpoint-pressure" => Available,
        _ => insufficient("The available evidence does not establish complete coverage of this check."),
    }
}
