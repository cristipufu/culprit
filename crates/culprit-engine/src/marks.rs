//! Marks: moments put on a connection's timeline by a person, a CI pipeline or a coding agent ("deploy v1.4.2",
//! "migration 0042 applied"), so charts and What changed line regressions up with them. Kept next to the connection's
//! reports in `marks.json`, so they move with a rename and go with the connection.

use std::path::{Path, PathBuf};

use culprit_core::changes::epoch;
use culprit_core::{ChangeEvent, Error, EventKind, EventSource, Result};
use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::private_fs;

/// Marks kept per connection; the oldest go first.
const MAX_MARKS: usize = 500;
const MAX_TEXT: usize = 200;

#[derive(Serialize, Deserialize, TS, Clone, Debug, PartialEq)]
#[ts(export)]
pub struct Mark {
    pub id: String,
    /// When it happened (UTC, ISO 8601), which can be earlier than when it was marked.
    pub at: String,
    pub text: String,
    /// "cli", "app", or the coding agent's name.
    pub by: String,
    pub created_at: String,
}

impl Mark {
    pub fn event(&self) -> ChangeEvent {
        let mut event = ChangeEvent::new(EventKind::Mark, EventSource::Mark, &self.id, &self.at, self.text.clone());
        event.by = Some(self.by.clone());
        event
    }
}

fn path(dir: &Path) -> PathBuf {
    dir.join("marks.json")
}

pub fn list(dir: &Path) -> Vec<Mark> {
    let mut marks: Vec<Mark> =
        std::fs::read(path(dir)).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
    marks.sort_by(|a, b| a.at.cmp(&b.at));
    marks
}

fn save(dir: &Path, marks: &[Mark]) -> Result<()> {
    private_fs::create_dir(dir).map_err(|e| Error::internal(format!("{}: {e}", dir.display())))?;
    let json = serde_json::to_vec_pretty(marks).map_err(|e| Error::internal(e.to_string()))?;
    private_fs::write_atomic(&path(dir), &json).map_err(|e| Error::internal(format!("{}: {e}", dir.display())))
}

/// Put `text` on the timeline at `at` (default: now). One line, at most 200 characters, not in the future.
pub fn add(dir: &Path, text: &str, at: Option<&str>, by: &str) -> Result<Mark> {
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if text.is_empty() {
        return Err(Error::invalid("A mark needs a few words, e.g. \"deploy v1.4.2\""));
    }
    if text.chars().count() > MAX_TEXT {
        return Err(Error::invalid(format!("A mark is at most {MAX_TEXT} characters")));
    }
    let now = chrono::Utc::now();
    let at = match at {
        Some(at) => {
            let t = epoch(at).ok_or_else(|| {
                Error::invalid(format!("`{at}` isn't a timestamp"))
                    .with_hint("Use ISO 8601 with a time zone, e.g. 2026-10-11T14:05:00Z")
            })?;
            if t > now.timestamp() as f64 + 60.0 {
                return Err(Error::invalid("A mark can't be in the future"));
            }
            chrono::DateTime::from_timestamp(t as i64, 0).unwrap_or(now)
        }
        None => now,
    };
    let mut marks = list(dir);
    let mut stamp = now.timestamp_millis();
    while marks.iter().any(|m| m.id == format!("m{stamp}")) {
        stamp += 1;
    }
    let mark = Mark {
        id: format!("m{stamp}"),
        at: at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        text,
        by: by.chars().take(60).collect(),
        created_at: now.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
    };
    marks.push(mark.clone());
    marks.sort_by(|a, b| a.at.cmp(&b.at));
    if marks.len() > MAX_MARKS {
        marks.drain(..marks.len() - MAX_MARKS);
    }
    save(dir, &marks)?;
    Ok(mark)
}

pub fn remove(dir: &Path, id: &str) -> Result<Mark> {
    let mut marks = list(dir);
    let index = marks.iter().position(|m| m.id == id).ok_or_else(|| {
        Error::not_found(format!("No mark '{id}' on this connection"))
            .with_hint("`culprit events <connection>` lists them")
    })?;
    let removed = marks.remove(index);
    save(dir, &marks)?;
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marks_round_trip() {
        let dir = std::env::temp_dir().join(format!("culprit-marks-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let a = add(&dir, "  deploy\n v1.4.2 ", Some("2026-10-11T12:00:00Z"), "cli").unwrap();
        assert_eq!(a.text, "deploy v1.4.2");
        assert_eq!(a.at, "2026-10-11T12:00:00Z");
        assert!(add(&dir, "", None, "cli").is_err());
        assert!(add(&dir, "later", Some("2999-01-01T00:00:00Z"), "cli").is_err());
        let b = add(&dir, "migration 0042", Some("2026-10-11T11:00:00Z"), "claude-code").unwrap();
        let listed = list(&dir);
        assert_eq!(listed.iter().map(|m| m.text.as_str()).collect::<Vec<_>>(), vec!["migration 0042", "deploy v1.4.2"]);
        assert_eq!(b.event().by.as_deref(), Some("claude-code"));
        remove(&dir, &a.id).unwrap();
        assert_eq!(list(&dir).len(), 1);
        assert!(remove(&dir, "nope").is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
