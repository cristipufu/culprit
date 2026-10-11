//! An application embedding the engine plugs a check in once; scans, `checks` and offline analysis all include it.

use std::path::PathBuf;
use std::sync::Arc;

use culprit_core::{Capability, Category, Check, CheckInfo, Engine, Finding, Snapshot};
use culprit_engine::Workspace;

struct Marker;

impl Check for Marker {
    fn info(&self) -> CheckInfo {
        CheckInfo {
            id: "acme.marker".into(),
            title: "Marker".into(),
            category: Category::Configuration,
            engines: vec![Engine::Postgres],
            requires: vec![Capability::TableStats],
            description: "Proves the plug-in ran.".into(),
        }
    }

    fn run(&self, snapshot: &Snapshot) -> Vec<Finding> {
        let report = culprit_checks::analyze(snapshot.clone());
        let mut f = report.findings.into_iter().next().expect("the demo has findings");
        f.id = "acme.marker:demo".into();
        f.check = "acme.marker".into();
        vec![f]
    }
}

#[test]
fn workspace_runs_plugged_in_checks_everywhere() {
    let home = std::env::temp_dir().join(format!("culprit-plugin-test-{}", std::process::id()));
    unsafe { std::env::set_var("CULPRIT_HOME", &home) };
    let ws = Workspace::open().expect("workspace").with_check(Arc::new(Marker));

    assert!(ws.checks().iter().any(|c| c.id == "acme.marker"));
    let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/postgres-demo.json");
    let report = ws.analyze_file(&fixture).expect("analyze");
    assert!(report.findings.iter().any(|f| f.id == "acme.marker:demo"));
    let _ = std::fs::remove_dir_all(home);
}
