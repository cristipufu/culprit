//! What a scan could read. A provider reads from sources: Query Store, the index usage view, pg_stat_statements,
//! performance_schema digests, the catalog… Each source either delivers a capability or doesn't, and one that fails
//! becomes a note and a locked capability with the reason and the fix, never a failed scan. Every provider keeps that
//! account with a [`Coverage`], so "what this login can see" means the same thing on every engine and checks that need
//! a missing capability are listed as locked instead of running on missing data.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::error::{Error, ErrorCode};
use crate::model::{Capability, CollectorNote, MissingCapability};

/// What actually happened when a source was read. Notes are prose, never a success/failure protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "kebab-case")]
#[ts(export)]
pub enum SourceState {
    Collected,
    Empty,
    Partial,
    Unavailable,
    NotApplicable,
}

/// Source names are open identifiers so an embedding provider can declare its own sources.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct SourceOutcome {
    pub source: String,
    pub state: SourceState,
    pub reason: Option<String>,
}

/// Canonical built-in source identifiers shared between engine collectors and check evidence contracts.
/// External providers may still record custom source names.
pub mod source_id {
    pub const CATALOG: &str = "catalog";
    pub const TABLES: &str = "tables";
    pub const INDEXES: &str = "indexes";
    pub const COLUMNS: &str = "columns";
    pub const FOREIGN_KEYS: &str = "foreign_keys";
    pub const STATISTICS: &str = "statistics";
    pub const SIZES: &str = "sizes";
    pub const TABLE_STORAGE: &str = "table storage";
    pub const CONSTRAINTS: &str = "constraints";
    pub const TIMESCALE_HYPERTABLES: &str = "timescale-hypertables";
    pub const TIMESCALE_CHUNKS: &str = "timescale-chunks";
    pub const TIMESCALE_INDEXES: &str = "timescale-indexes";
    pub const DOCDB_EPOCHS: &str = "docdb-counter-epochs";
    pub const DOCDB_INDEX_USAGE: &str = "docdb-index-usage";
    pub const DOCDB_PLAN_HISTORY: &str = "docdb-plan-history";
    pub const DOCDB_ACTIVITY_HISTORY: &str = "docdb-activity-history";

    pub const WORKLOAD: &str = "workload";
    pub const QUERY_STORE: &str = "query_store";
    pub const QUERY_WAITS: &str = "query_waits";
    pub const PLAN_CACHE: &str = "plan_cache";
    pub const QUERY_LOG: &str = "query log";
    pub const QUERY_COSTS: &str = "query costs";
    pub const QUERY_COMPILATION: &str = "query compilation";
    pub const QUERY_ERRORS: &str = "query errors";
    pub const QUERY_FAMILIES: &str = "query_families";
    pub const PLAN_CACHE_USE: &str = "plan cache use";
    pub const HISTOGRAMS: &str = "histograms";

    pub const MISSING_INDEXES: &str = "missing_indexes";
    pub const FORWARDED_FETCHES: &str = "forwarded_fetches";
    pub const TUNING: &str = "tuning";
    pub const INDEX_CONTENTION: &str = "index contention";
    pub const LIVE_LOCKS: &str = "live_locks";
    pub const METADATA_LOCKS: &str = "metadata_locks";

    pub const SETTINGS: &str = "settings";
    pub const ADVICE_SETTINGS: &str = "advice settings";
    pub const SERVER_VARIABLES: &str = "server variables";
    pub const WAITS: &str = "waits";
    pub const WAIT_TOTALS: &str = "wait totals";
    pub const GLOBAL_STATUS: &str = "global status";
    pub const PERFORMANCE_COUNTERS: &str = "performance counters";
    pub const DATABASE_STATS: &str = "database statistics";
    pub const RESOURCES: &str = "resources";
    pub const RESOURCE_USE: &str = "resource use";
    pub const CHECKPOINTS: &str = "checkpoints";
    pub const OPEN_TRANSACTIONS: &str = "open transactions";
    pub const CLEANUP_SLOTS: &str = "cleanup slots";
    pub const CLEANUP_PREPARED: &str = "cleanup prepared";
    pub const PENDING_GRANTS: &str = "memory grants";
    pub const GRANTS: &str = "memory grants per query";
    pub const FILE_LATENCY: &str = "file latency";
    pub const SESSIONS: &str = "sessions";
    pub const CONNECTIONS: &str = "connections";
    pub const LOGINS: &str = "logins";
    pub const REPLICATION: &str = "replication";
    pub const SIGNALS: &str = "signals";

    pub const LOG_CAPACITY: &str = "log_capacity";
    pub const FILE_CAPACITY: &str = "file_capacity";
    pub const TEMP_CAPACITY: &str = "temp_capacity";
    pub const SEQUENCE_CAPACITY: &str = "sequence_capacity";
    pub const MULTIXACTS: &str = "multixacts";
    pub const ARCHIVER: &str = "archiver";
    pub const SUBSCRIPTION_ERRORS: &str = "subscription_errors";
    pub const STANDBY_CONFLICTS: &str = "standby_conflicts";
    pub const REDO_PRESSURE: &str = "redo_pressure";
    pub const CLUSTER_HEALTH: &str = "cluster_health";
    pub const WAL_FILE: &str = "wal_file";
    pub const DEADLOCKS: &str = "deadlocks";

    pub const ALL: &[&str] = &[
        CATALOG,
        TABLES,
        INDEXES,
        COLUMNS,
        FOREIGN_KEYS,
        STATISTICS,
        SIZES,
        TABLE_STORAGE,
        CONSTRAINTS,
        TIMESCALE_HYPERTABLES,
        TIMESCALE_CHUNKS,
        TIMESCALE_INDEXES,
        DOCDB_EPOCHS,
        DOCDB_INDEX_USAGE,
        DOCDB_PLAN_HISTORY,
        DOCDB_ACTIVITY_HISTORY,
        WORKLOAD,
        QUERY_STORE,
        QUERY_WAITS,
        PLAN_CACHE,
        QUERY_LOG,
        QUERY_COSTS,
        QUERY_COMPILATION,
        QUERY_ERRORS,
        QUERY_FAMILIES,
        PLAN_CACHE_USE,
        HISTOGRAMS,
        MISSING_INDEXES,
        FORWARDED_FETCHES,
        TUNING,
        INDEX_CONTENTION,
        LIVE_LOCKS,
        METADATA_LOCKS,
        SETTINGS,
        ADVICE_SETTINGS,
        SERVER_VARIABLES,
        WAITS,
        WAIT_TOTALS,
        GLOBAL_STATUS,
        PERFORMANCE_COUNTERS,
        DATABASE_STATS,
        RESOURCES,
        RESOURCE_USE,
        CHECKPOINTS,
        OPEN_TRANSACTIONS,
        CLEANUP_SLOTS,
        CLEANUP_PREPARED,
        PENDING_GRANTS,
        GRANTS,
        FILE_LATENCY,
        SESSIONS,
        CONNECTIONS,
        LOGINS,
        REPLICATION,
        SIGNALS,
        LOG_CAPACITY,
        FILE_CAPACITY,
        TEMP_CAPACITY,
        SEQUENCE_CAPACITY,
        MULTIXACTS,
        ARCHIVER,
        SUBSCRIPTION_ERRORS,
        STANDBY_CONFLICTS,
        REDO_PRESSURE,
        CLUSTER_HEALTH,
        WAL_FILE,
        DEADLOCKS,
    ];
}

pub fn same_source(a: &str, b: &str) -> bool {
    let normalize = |byte: u8| if byte == b' ' { b'_' } else { byte.to_ascii_lowercase() };
    a.len() == b.len() && a.bytes().map(normalize).eq(b.bytes().map(normalize))
}

impl SourceOutcome {
    /// Combine independently read parts conservatively. A successful part cannot erase a failed part.
    fn merge(&mut self, other: Self) {
        use SourceState::*;
        self.state = match (self.state, other.state) {
            (a, b) if a == b => a,
            (Collected, Empty) | (Empty, Collected) => Collected,
            _ => Partial,
        };
        if let Some(reason) = other.reason {
            match &mut self.reason {
                Some(existing) if existing != &reason => {
                    existing.push(' ');
                    existing.push_str(&reason);
                }
                None => self.reason = Some(reason),
                _ => {}
            }
        }
    }
}

/// One source a provider reads from its engine: a statistics view, the catalog, a log. Each provider declares its
/// sources in one list (`<provider>/src/sources.rs`), so what a scan reads on an engine is a table anyone can read, and
/// reading one goes through [`Coverage::read`], so what a failing source means is said once for every engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Source {
    /// Its name in the scan's notes: "tables", "query_store".
    pub name: &'static str,
    /// What it reads, in a sentence: "Reading {reads} failed".
    pub reads: &'static str,
    /// What the checks get from it: the capabilities a failure locks. Empty for a source that only adds detail.
    pub delivers: &'static [Capability],
}

/// The capabilities a scan's sources delivered, the ones they couldn't (with why and how to fix it), and notes.
#[derive(Default, Debug, Clone)]
pub struct Coverage {
    have: Vec<Capability>,
    missing: Vec<MissingCapability>,
    notes: Vec<CollectorNote>,
    sources: Vec<SourceOutcome>,
}

impl Coverage {
    /// Start from what a probe already found: the capabilities the login and server settings allow, and what they
    /// don't. Sources then confirm them, or [`Coverage::lost`] one they couldn't read after all.
    pub fn with(have: Vec<Capability>, missing: Vec<MissingCapability>) -> Self {
        Coverage { have, missing, ..Self::default() }
    }

    pub fn recorded(&mut self, source: &Source, state: SourceState, reason: Option<String>) {
        self.record(SourceOutcome { source: source.name.into(), state, reason });
    }

    /// Finish decoding a successful read. A previous failure or partial result is never cleared by this update.
    pub fn completed(&mut self, source: &Source, rows: usize) {
        let state = if rows == 0 { SourceState::Empty } else { SourceState::Collected };
        if let Some(existing) = self.sources.iter_mut().find(|s| same_source(&s.source, source.name))
            && matches!(existing.state, SourceState::Collected | SourceState::Empty)
        {
            existing.state = state;
        } else {
            self.recorded(source, state, None);
        }
    }

    fn record(&mut self, outcome: SourceOutcome) {
        if let Some(existing) = self.sources.iter_mut().find(|s| same_source(&s.source, &outcome.source)) {
            existing.merge(outcome);
        } else {
            self.sources.push(outcome);
        }
    }

    /// Some objects or measurements could not be read, though the source delivered useful data.
    pub fn partial(&mut self, source: &Source, reason: impl Into<String>) {
        let reason = reason.into();
        self.note(source.name, reason.clone());
        self.recorded(source, SourceState::Partial, Some(reason));
    }

    /// The engine or configuration does not have this source. This is different from an unreadable source.
    pub fn not_applicable(&mut self, source: &Source, reason: impl Into<String>) {
        let reason = reason.into();
        if let Some(existing) = self.sources.iter_mut().find(|s| same_source(&s.source, source.name))
            && matches!(existing.state, SourceState::Collected | SourceState::Empty)
        {
            // A successful configuration probe may establish that the source does not apply.
            existing.state = SourceState::NotApplicable;
            existing.reason = Some(reason);
        } else {
            self.recorded(source, SourceState::NotApplicable, Some(reason));
        }
    }

    /// Source outcomes, including successful reads. `finish` retains its original public tuple contract.
    pub fn outcomes(&self) -> Vec<SourceOutcome> {
        self.sources.clone()
    }

    /// Attach a later collector's evidence and capability changes without losing the main collector's account.
    pub fn append_to(mut self, snapshot: &mut crate::Snapshot) {
        for outcome in std::mem::take(&mut self.sources) {
            if let Some(existing) = snapshot.sources.iter_mut().find(|s| same_source(&s.source, &outcome.source)) {
                existing.merge(outcome);
            } else {
                snapshot.sources.push(outcome);
            }
        }
        let (have, missing, notes) = self.finish();
        for capability in have {
            if !snapshot.server.has(capability) {
                snapshot.server.capabilities.push(capability);
            }
            snapshot.server.missing.retain(|m| m.capability != capability);
        }
        for missing in missing {
            snapshot.server.capabilities.retain(|c| *c != missing.capability);
            snapshot.server.missing.retain(|m| m.capability != missing.capability);
            snapshot.server.missing.push(missing);
        }
        snapshot.server.capabilities.sort();
        snapshot.notes.extend(notes);
    }

    /// A source delivered `capability`.
    pub fn delivered(&mut self, capability: Capability) {
        if !self.have.contains(&capability) {
            self.have.push(capability);
        }
    }

    /// `capability` isn't available: the setting is off, the edition lacks it, the login can't read it. The first
    /// reason given for a capability is kept.
    pub fn lacking(&mut self, capability: Capability, reason: impl Into<String>, fix: Option<String>) {
        if !self.missing.iter().any(|m| m.capability == capability) {
            self.missing.push(MissingCapability { capability, reason: reason.into(), fix });
        }
    }

    /// A capability the probe expected that a source then couldn't deliver: no longer available, and missing with why.
    /// Unlike [`Coverage::lacking`], this overrides an earlier `delivered`, and its reason replaces one the probe gave:
    /// the source knows more about why.
    pub fn lost(&mut self, capability: Capability, reason: impl Into<String>, fix: Option<String>) {
        self.have.retain(|c| *c != capability);
        self.missing.retain(|m| m.capability != capability);
        self.lacking(capability, reason, fix);
    }

    /// A capability the probe expected that turned out to hold nothing (no statement could be planned, nothing was timed),
    /// without a missing reason of its own: the scan's notes, or the views that read it, say why.
    pub fn withdraw(&mut self, capability: Capability) {
        self.have.retain(|c| *c != capability);
    }

    /// Something the reader of the report should know about what `source` read, without a capability at stake.
    pub fn note(&mut self, source: &str, message: impl Into<String>) {
        self.notes.push(CollectorNote { collector: source.into(), message: message.into() });
    }

    /// Reading `source` failed: a note with the error, and `capability` locked with why. A permission error says the
    /// login lacks the permission, and `grant` (the statement that gives it) becomes the fix.
    pub fn failed(&mut self, source: &str, capability: Capability, error: &Error, grant: Option<String>) {
        self.record(SourceOutcome {
            source: source.into(),
            state: SourceState::Unavailable,
            reason: Some(error.message.clone()),
        });
        self.note(source, error.message.clone());
        let denied = error.code == ErrorCode::PermissionDenied;
        let reason = if denied {
            "This login lacks the permission to read it.".to_string()
        } else {
            format!("Reading it failed: {}", error.message)
        };
        let fix = if denied { grant.or_else(|| error.hint.clone()) } else { error.hint.clone() };
        self.lacking(capability, reason, fix);
    }

    /// What reading `source` gave: its value, or `None` with the failure recorded. A failing source never fails the scan:
    /// a note with the error, and each capability it delivers lost with the source's own reason (a permission error
    /// says the login lacks the permission, and the error's hint, the statement that grants it, is the fix).
    pub fn read<T>(&mut self, source: &Source, result: Result<T, Error>) -> Option<T> {
        match result {
            Ok(value) => {
                self.recorded(source, SourceState::Collected, None);
                Some(value)
            }
            Err(error) => {
                self.source_failed(source, &error);
                None
            }
        }
    }

    /// A successful empty catalog is evidence too; a failed catalog is never an empty one.
    pub fn read_rows<T>(&mut self, source: &Source, result: Result<Vec<T>, Error>) -> Option<Vec<T>> {
        match result {
            Ok(rows) => {
                self.recorded(source, if rows.is_empty() { SourceState::Empty } else { SourceState::Collected }, None);
                Some(rows)
            }
            Err(error) => {
                self.source_failed(source, &error);
                None
            }
        }
    }

    /// Record that reading `source` failed with `error` (see [`Coverage::read`]), for a source read in parts.
    pub fn source_failed(&mut self, source: &Source, error: &Error) {
        self.recorded(source, SourceState::Unavailable, Some(error.message.clone()));
        self.note(source.name, error.message.clone());
        let reason = if error.code == ErrorCode::PermissionDenied {
            format!("This login lacks the permission to read {}.", source.reads)
        } else {
            format!("Reading {} failed: {}", source.reads, error.message)
        };
        for capability in source.delivers {
            self.lost(*capability, reason.clone(), error.hint.clone());
        }
    }

    /// A failed shared connection prevents every source assigned to it from being read.
    pub fn sources_failed(&mut self, sources: &[Source], error: &Error) {
        for source in sources {
            self.source_failed(source, error);
        }
    }

    /// Take in what another part of the scan found (sources read on another connection, concurrently).
    pub fn merge(&mut self, other: Coverage) {
        for outcome in other.sources {
            self.record(outcome);
        }
        for capability in other.have {
            self.delivered(capability);
        }
        for m in other.missing {
            self.lacking(m.capability, m.reason, m.fix);
        }
        self.notes.extend(other.notes);
    }

    pub fn has(&self, capability: Capability) -> bool {
        self.have.contains(&capability)
    }

    /// The account for the snapshot: what was delivered (sorted), what wasn't (a capability one source delivered isn't
    /// missing because another couldn't), and the notes.
    pub fn finish(mut self) -> (Vec<Capability>, Vec<MissingCapability>, Vec<CollectorNote>) {
        self.missing.retain(|m| !self.have.contains(&m.capability));
        self.have.sort();
        (self.have, self.missing, self.notes)
    }
}

impl crate::Snapshot {
    pub fn source_outcome(&self, name: &str) -> Option<&SourceOutcome> {
        self.sources.iter().find(|s| same_source(&s.source, name))
    }

    /// Read typed outcomes first. Old scans have only notes; their conservative interpretation stays in this adapter.
    pub fn source_problem(&self, names: &[&str]) -> Option<String> {
        names.iter().find_map(|name| {
            if let Some(outcome) = self.source_outcome(name) {
                return matches!(outcome.state, SourceState::Partial | SourceState::Unavailable).then(|| {
                    outcome.reason.clone().unwrap_or_else(|| format!("{name} was not completely collected."))
                });
            }
            self.notes.iter().find(|n| same_source(&n.collector, name)).map(|n| n.message.clone())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const INDEXES: Source = Source {
        name: "indexes",
        reads: "index statistics",
        delivers: &[Capability::IndexUsage, Capability::TableStats],
    };

    #[test]
    fn empty_success_and_failure_have_different_outcomes() {
        let mut c = Coverage::default();
        assert_eq!(c.read_rows::<u8>(&INDEXES, Ok(vec![])), Some(vec![]));
        assert_eq!(c.outcomes()[0].state, SourceState::Empty);
        c.note(INDEXES.name, "This source has no visible rows.");
        assert_eq!(c.outcomes()[0].state, SourceState::Empty);
        c.source_failed(&INDEXES, &Error::internal("timeout"));
        assert_eq!(c.outcomes()[0].state, SourceState::Partial);
        c.completed(&INDEXES, 5);
        assert_eq!(c.outcomes()[0].state, SourceState::Partial, "a later successful part must not clear the failure");
    }

    #[test]
    fn parallel_source_parts_merge_conservatively_in_either_order() {
        for reverse in [false, true] {
            let mut a = Coverage::default();
            a.read_rows(&INDEXES, Ok(vec![1]));
            let mut b = Coverage::default();
            b.source_failed(&INDEXES, &Error::internal("timeout"));
            let combined = if reverse {
                b.merge(a);
                b
            } else {
                a.merge(b);
                a
            };
            assert_eq!(combined.outcomes().len(), 1);
            assert_eq!(combined.outcomes()[0].state, SourceState::Partial);
            assert_eq!(combined.outcomes()[0].reason.as_deref(), Some("timeout"));
        }
    }

    #[test]
    fn configuration_can_establish_inapplicability_but_a_failure_cannot() {
        let mut c = Coverage::default();
        c.read(&INDEXES, Ok(false));
        c.not_applicable(&INDEXES, "Feature disabled");
        assert_eq!(c.outcomes()[0].state, SourceState::NotApplicable);
        let mut failed = Coverage::default();
        failed.source_failed(&INDEXES, &Error::internal("permission denied"));
        failed.not_applicable(&INDEXES, "Feature disabled");
        assert_eq!(failed.outcomes()[0].state, SourceState::Partial);
    }

    #[test]
    fn old_snapshots_load_and_detail_collectors_retain_their_source_account() {
        let mut s: crate::Snapshot =
            serde_json::from_str(include_str!("../../../fixtures/postgres-demo.json")).unwrap();
        assert!(s.sources.is_empty());
        assert!(serde_json::to_value(&s).unwrap().get("sources").is_none());
        let mut main = Coverage::default();
        main.read_rows(&INDEXES, Ok(vec![1]));
        main.append_to(&mut s);
        let mut detail = Coverage::default();
        detail.source_failed(&INDEXES, &Error::internal("details timed out"));
        detail.append_to(&mut s);
        assert_eq!(s.source_outcome("indexes").unwrap().state, SourceState::Partial);
        assert!(!s.server.has(Capability::IndexUsage));
        assert!(!s.server.has(Capability::TableStats));
        assert!(
            s.server
                .missing
                .iter()
                .any(|m| m.capability == Capability::IndexUsage && m.reason.contains("details timed out"))
        );
        assert_eq!(s.notes.len(), 1);
        let saved = serde_json::to_string(&s).unwrap();
        let loaded: crate::Snapshot = serde_json::from_str(&saved).unwrap();
        assert_eq!(loaded.sources, s.sources);
        assert_eq!(loaded.source_problem(&["indexes"]).as_deref(), Some("details timed out"));
    }

    // One rule for every engine: a source that fails is a note and its capabilities locked with why, even ones the
    // probe expected, and the scan goes on.
    #[test]
    fn a_failed_source_is_a_note_and_its_capabilities_locked() {
        let mut c = Coverage::with(vec![Capability::IndexUsage], Vec::new());
        let read: Option<Vec<u8>> = c.read(&INDEXES, Err(Error::internal("statement timeout")));
        assert!(read.is_none());
        assert_eq!(c.read(&INDEXES, Ok(3)), Some(3));
        let (have, missing, notes) = c.finish();
        assert!(have.is_empty());
        assert_eq!(missing.len(), 2);
        assert_eq!(missing[0].reason, "Reading index statistics failed: statement timeout");
        assert_eq!(notes.len(), 1);
        // Denied: the login lacks the permission, and the grant is the fix.
        let denied = Error::new(ErrorCode::PermissionDenied, "permission denied for pg_stat_user_indexes")
            .with_hint("GRANT pg_read_all_stats TO app;");
        let mut c = Coverage::default();
        c.source_failed(&INDEXES, &denied);
        let (_, missing, _) = c.finish();
        assert_eq!(missing[0].reason, "This login lacks the permission to read index statistics.");
        assert_eq!(missing[0].fix.as_deref(), Some("GRANT pg_read_all_stats TO app;"));
    }

    #[test]
    fn a_capability_the_probe_expected_can_be_lost() {
        let mut c = Coverage::with(vec![Capability::TableStats, Capability::IndexUsage], Vec::new());
        c.lost(Capability::IndexUsage, "Reading it failed: timeout", None);
        let (have, missing, _) = c.finish();
        assert_eq!(have, vec![Capability::TableStats]);
        assert_eq!(missing[0].capability, Capability::IndexUsage);
    }

    #[test]
    fn a_failed_source_locks_its_capability_unless_another_delivered_it() {
        let mut workload = Coverage::default();
        workload.delivered(Capability::QueryStore);
        let mut schema = Coverage::default();
        let denied = Error::new(ErrorCode::PermissionDenied, "VIEW DATABASE STATE permission denied");
        schema.failed("index usage", Capability::IndexUsage, &denied, Some("GRANT VIEW DATABASE STATE TO app;".into()));
        schema.failed("query store", Capability::QueryStore, &Error::internal("timeout"), None);
        workload.merge(schema);
        let (have, missing, notes) = workload.finish();
        assert_eq!(have, vec![Capability::QueryStore]);
        assert_eq!(missing.len(), 1);
        assert_eq!(missing[0].capability, Capability::IndexUsage);
        assert_eq!(missing[0].reason, "This login lacks the permission to read it.");
        assert_eq!(missing[0].fix.as_deref(), Some("GRANT VIEW DATABASE STATE TO app;"));
        assert_eq!(notes.len(), 2);
    }
}
