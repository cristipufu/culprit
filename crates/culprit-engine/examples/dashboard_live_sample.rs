//! The web demo replays Rust's session assessments, including ending any of its planted sessions.
use std::collections::BTreeMap;
use std::path::Path;

use culprit_core::{BlockNode, DashboardLive, Engine, LiveState, LockWait};

fn locks(node: &BlockNode, out: &mut Vec<LockWait>) {
    out.extend(node.lock.iter().cloned());
    for child in &node.children {
        locks(child, out);
    }
}

fn main() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let recorded: BTreeMap<Engine, LiveState> =
        serde_json::from_slice(&std::fs::read(root.join("fixtures/live.sample.json")).unwrap()).unwrap();
    let mut output: BTreeMap<String, BTreeMap<String, DashboardLive>> = BTreeMap::new();
    for entry in std::fs::read_dir(root.join("fixtures")).unwrap() {
        let path = entry.unwrap().path();
        if !path.to_string_lossy().ends_with(".report.json") {
            continue;
        }
        let report = culprit_engine::reading::open(&path).unwrap();
        let engine = report.snapshot.server.engine;
        let Some(captured) = recorded.get(&engine) else { continue };
        // This is a small, deliberately planted demo (currently three sessions per engine).
        assert!(captured.sessions.len() <= 8, "keep the demo's session combinations bounded");
        let mut captured_locks = Vec::new();
        for chain in captured.blocking.iter().flat_map(|b| &b.chains) {
            locks(&chain.tree, &mut captured_locks);
        }
        let mut cases = BTreeMap::new();
        for mask in 0..(1 << captured.sessions.len()) {
            let ended: Vec<&str> = captured
                .sessions
                .iter()
                .enumerate()
                .filter(|(i, _)| mask & (1 << i) != 0)
                .map(|(_, s)| s.id.as_str())
                .collect();
            let mut live = captured.clone();
            live.sessions.retain(|s| !ended.contains(&s.id.as_str()));
            for session in &mut live.sessions {
                if session.blocked_by.as_deref().is_some_and(|id| ended.contains(&id)) {
                    session.blocked_by = None;
                    session.wait = None;
                    session.wait_ms = None;
                }
            }
            let relevant: Vec<_> =
                captured_locks.iter().filter(|l| !ended.contains(&l.session.as_str())).cloned().collect();
            live.blocking = culprit_core::blocking::analyze(engine, &live.sessions, &relevant);
            let mut remaining: Vec<_> = live.sessions.iter().map(|s| &s.id).collect();
            remaining.sort();
            let key = serde_json::to_string(&remaining).unwrap();
            cases.insert(key, culprit_checks::dashboard::refresh(culprit_checks::dashboard(&report), Ok(live)));
        }
        let mut incomplete = captured.clone();
        incomplete.sessions.clear();
        incomplete.blocking = None;
        incomplete.notes = vec!["Lock instrumentation unavailable".into()];
        cases.insert(
            "incomplete".into(),
            culprit_checks::dashboard::refresh(culprit_checks::dashboard(&report), Ok(incomplete)),
        );
        cases.insert(
            "unavailable".into(),
            culprit_checks::dashboard::refresh(
                culprit_checks::dashboard(&report),
                Err(culprit_core::Error::new(
                    culprit_core::ErrorCode::PermissionDenied,
                    "Session visibility was denied",
                )),
            ),
        );
        output.insert(report.snapshot.connection, cases);
    }
    let path = root.join("fixtures/dashboard-live.sample.json");
    std::fs::write(&path, serde_json::to_vec(&output).unwrap()).unwrap();
    eprintln!("wrote {}", path.display());
}
