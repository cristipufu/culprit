//! What a scan could read. A provider reads from sources: Query Store, the index usage view, pg_stat_statements,
//! performance_schema digests, the catalog… Each source either delivers a capability or doesn't, and one that fails
//! becomes a note and a locked capability with the reason and the fix, never a failed scan. Every provider keeps that
//! account with a [`Coverage`], so "what this login can see" means the same thing on every engine and checks that need
//! a missing capability are listed as locked instead of running on missing data.

use serde::Serialize;

use crate::error::{Error, ErrorCode};
use crate::model::{Capability, CollectorNote, MissingCapability};

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
}

impl Coverage {
    /// Start from what a probe already found: the capabilities the login and server settings allow, and what they
    /// don't. Sources then confirm them, or [`Coverage::lost`] one they couldn't read after all.
    pub fn with(have: Vec<Capability>, missing: Vec<MissingCapability>) -> Self {
        Coverage { have, missing, notes: Vec::new() }
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
            Ok(value) => Some(value),
            Err(error) => {
                self.source_failed(source, &error);
                None
            }
        }
    }

    /// Record that reading `source` failed with `error` (see [`Coverage::read`]), for a source read in parts.
    pub fn source_failed(&mut self, source: &Source, error: &Error) {
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

    /// Take in what another part of the scan found (sources read on another connection, concurrently).
    pub fn merge(&mut self, other: Coverage) {
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

#[cfg(test)]
mod tests {
    use super::*;

    const INDEXES: Source = Source {
        name: "indexes",
        reads: "index statistics",
        delivers: &[Capability::IndexUsage, Capability::TableStats],
    };

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
