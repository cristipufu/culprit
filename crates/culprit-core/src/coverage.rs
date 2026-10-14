//! What a scan could read. A provider reads from sources: Query Store, the index usage view, pg_stat_statements,
//! performance_schema digests, the catalog… Each source either delivers a capability or doesn't, and one that fails
//! becomes a note and a locked capability with the reason and the fix, never a failed scan. Every provider keeps that
//! account with a [`Coverage`], so "what this login can see" means the same thing on every engine and checks that need
//! a missing capability are listed as locked instead of running on missing data.

use crate::error::{Error, ErrorCode};
use crate::model::{Capability, CollectorNote, MissingCapability};

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
