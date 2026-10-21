//! Read-only operational evidence. Each counter keeps its own scope/reset, and a comparison is made only between
//! matching sources in scans of the same database. Missing measurements never become zero.
use crate::{FileKind, FileScope, Snapshot};
use serde::{Deserialize, Serialize};
use ts_rs::TS;

#[derive(Serialize, Deserialize, TS, Clone, Debug, Default)]
#[serde(default)]
#[ts(export)]
pub struct OperationalStats {
    pub log_failures: Vec<LoggedFailure>,
    pub spaces: Vec<SpaceUsage>,
    pub counters: Vec<OperationalCounter>,
    pub sequences: Vec<SequenceCapacity>,
    pub multixacts: Vec<FreezeAge>,
    pub sqlite: Option<SqliteFileHealth>,
    pub cluster: Option<ClusterHealth>,
}

#[derive(Serialize, Deserialize, TS, Clone, Debug)]
#[ts(export)]
pub struct LoggedFailure {
    pub query_id: String,
    /// Normalized SQL only, never the failed values or the application's error message.
    pub statement: String,
    pub code: String,
    pub count: f64,
    pub first_at: Option<String>,
    pub last_at: Option<String>,
}

#[derive(Serialize, Deserialize, TS, Clone, Debug)]
#[ts(export)]
pub struct RequestEvidence {
    /// Counts describe the bounded set of request identifiers actually present in the log.
    pub requests: u32,
    pub repeated_requests: u32,
    pub most_calls: u32,
    pub sample_truncated: bool,
}

#[derive(Serialize, Deserialize, TS, Clone, Debug)]
#[ts(export)]
pub struct SpaceUsage {
    #[serde(default)]
    pub parts: Vec<SpacePart>,
    pub name: String,
    pub kind: FileKind,
    pub scope: FileScope,
    pub total_bytes: f64,
    pub used_bytes: f64,
    pub growth_enabled: Option<bool>,
    pub limit_bytes: Option<f64>,
    pub reuse_wait: Option<String>,
}

#[derive(Serialize, Deserialize, TS, Clone, Debug)]
#[ts(export)]
pub struct SpacePart {
    pub name: String,
    pub bytes: f64,
}

#[derive(Serialize, Deserialize, TS, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
#[ts(export)]
pub enum OperationalCounterKind {
    ArchiveFailures,
    SubscriptionApplyErrors,
    SubscriptionSyncErrors,
    StandbyConflicts,
    RedoLogWaits,
}

#[derive(Serialize, Deserialize, TS, Clone, Debug)]
#[ts(export)]
pub struct CounterReading {
    pub at: String,
    pub count: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub reference: Option<crate::ScanReference>,
}

#[derive(Serialize, Deserialize, TS, Clone, Debug)]
#[ts(export)]
pub struct OperationalCounter {
    pub kind: OperationalCounterKind,
    pub subject: String,
    pub scope: FileScope,
    pub count: f64,
    pub since: Option<String>,
    pub last_event_at: Option<String>,
    pub before: Option<CounterReading>,
}

#[derive(Serialize, Deserialize, TS, Clone, Debug)]
#[ts(export)]
pub struct FreezeAge {
    pub subject: String,
    pub age: f64,
    pub freeze_max_age: Option<f64>,
}

#[derive(Serialize, Deserialize, TS, Clone, Debug)]
#[ts(export)]
pub struct SequenceCapacity {
    pub name: String,
    pub table: Option<String>,
    /// Decimal strings preserve BIGINT precision right up to exhaustion.
    pub current: String,
    pub increment: String,
    pub minimum: String,
    pub maximum: String,
    pub cycles: bool,
    /// A sequence may report the end of a reserved cache rather than the last value returned to a caller.
    pub allocated: bool,
}

impl SequenceCapacity {
    pub fn remaining(&self) -> Option<(u128, u128)> {
        let (now, step, min, max) = (
            self.current.parse::<i128>().ok()?,
            self.increment.parse::<i128>().ok()?,
            self.minimum.parse::<i128>().ok()?,
            self.maximum.parse::<i128>().ok()?,
        );
        if self.cycles || step == 0 || min > max {
            return None;
        }
        let remaining = if step > 0 {
            if now > max { 0 } else { max.abs_diff(now) }
        } else if now < min {
            0
        } else {
            now.abs_diff(min)
        };
        Some((remaining / step.unsigned_abs(), max.abs_diff(min) / step.unsigned_abs()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn decimal38_and_descending_steps_keep_their_exact_remaining_range() {
        let mut s = SequenceCapacity {
            name: "items.id".into(),
            table: None,
            current: "99999999999999999999999999999999999998".into(),
            increment: "1".into(),
            minimum: "-99999999999999999999999999999999999999".into(),
            maximum: "99999999999999999999999999999999999999".into(),
            cycles: false,
            allocated: true,
        };
        assert_eq!(s.remaining().unwrap().0, 1);
        s.current = "-99999999999999999999999999999999999997".into();
        s.increment = "-2".into();
        assert_eq!(s.remaining().unwrap().0, 1);
    }
}

#[derive(Serialize, Deserialize, TS, Clone, Debug)]
#[ts(export)]
pub struct SqliteFileHealth {
    pub wal_bytes: Option<f64>,
    pub before: Option<CounterReading>,
}

#[derive(Serialize, Deserialize, TS, Clone, Debug, Default)]
#[ts(export)]
pub struct ClusterHealth {
    pub ready: Option<bool>,
    pub connected: Option<bool>,
    pub primary_component: Option<bool>,
    pub local_state: Option<String>,
    pub flow_control_paused: Option<f64>,
}

pub fn compare(previous: &Snapshot, current: &mut Snapshot) {
    if !previous.same_database(current)
        || crate::format::epoch_seconds(&previous.taken_at)
            .zip(crate::format::epoch_seconds(&current.taken_at))
            .is_none_or(|(a, b)| a >= b)
    {
        return;
    }
    for counter in &mut current.operations.counters {
        counter.before = previous
            .operations
            .counters
            .iter()
            .find(|p| {
                p.kind == counter.kind
                    && p.subject == counter.subject
                    && p.scope == counter.scope
                    && p.since.is_some()
                    && p.since == counter.since
                    && counter.count >= p.count
            })
            .map(|p| CounterReading {
                at: previous.taken_at.clone(),
                count: p.count,
                reference: Some(crate::ScanReference::of(previous)),
            });
    }
    if let Some(sqlite) = &mut current.operations.sqlite {
        sqlite.before = previous.operations.sqlite.as_ref().and_then(|p| p.wal_bytes).map(|count| CounterReading {
            at: previous.taken_at.clone(),
            count,
            reference: Some(crate::ScanReference::of(previous)),
        });
    }
}
