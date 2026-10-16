//! The check registry stays whole as checks are added: every check written under `src/checks/` is listed in its area's
//! `CHECKS` (and so run by `all()`), once, with an id of its own.

use std::collections::BTreeSet;
use std::path::Path;

fn written_checks(dir: &Path, out: &mut Vec<String>) {
    for entry in std::fs::read_dir(dir).expect("checks directory") {
        let path = entry.expect("entry").path();
        if path.is_dir() {
            written_checks(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            let text = std::fs::read_to_string(&path).expect("source");
            for line in text.lines() {
                if let Some(rest) = line.strip_prefix("pub fn ")
                    && let Some(name) = rest.strip_suffix("_info() -> CheckInfo {")
                {
                    out.push(name.to_string());
                }
            }
        }
    }
}

#[test]
fn every_check_written_is_registered_once() {
    let mut written = Vec::new();
    written_checks(&Path::new(env!("CARGO_MANIFEST_DIR")).join("src/checks"), &mut written);
    let checks = culprit_checks::all();
    assert_eq!(
        checks.len(),
        written.len(),
        "a check under src/checks isn't in its area's CHECKS list (or is listed twice): written {written:?}"
    );
    let ids: BTreeSet<String> = checks.iter().map(|c| c.info().id).collect();
    assert_eq!(ids.len(), checks.len(), "two checks share an id");
    for c in &checks {
        let info = c.info();
        assert!(!info.engines.is_empty(), "{} applies to no engine", info.id);
        assert!(!info.description.is_empty() && !info.title.is_empty(), "{} has no title or description", info.id);
    }
}
