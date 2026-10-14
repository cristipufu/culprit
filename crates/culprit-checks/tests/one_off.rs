//! A statement that ran once in the window is what happened, not a pattern to fix: the same on every engine and for
//! every check that rests on how statements ran.

use culprit_checks::{analyze, index_advice};
use culprit_core::{Severity, Snapshot};

fn snapshot(name: &str) -> Snapshot {
    let text =
        std::fs::read_to_string(format!("{}/../../fixtures/{name}", env!("CARGO_MANIFEST_DIR"))).expect("fixture");
    serde_json::from_str(&text).expect("snapshot")
}

const ONE_OFF: &str = "This statement ran once in";

#[test]
fn findings_about_a_single_run_are_low_and_say_so() {
    for name in ["mssql-demo.json", "postgres-demo.json", "mysql-demo.json"] {
        let mut s = snapshot(name);
        // The heaviest statement of each demo, as if it had run once.
        let workload = s.workload.as_mut().expect("the demo has a workload");
        let q =
            workload.queries.iter_mut().max_by(|a, b| a.total_time_ms.total_cmp(&b.total_time_ms)).expect("queries");
        let id = q.id.clone();
        q.executions = 1.0;
        q.mean_time_ms = q.total_time_ms;
        let once: Vec<String> = workload.queries.iter().filter(|q| q.executions <= 1.0).map(|q| q.id.clone()).collect();
        let report = analyze(s);

        let marked: Vec<_> =
            report.findings.iter().filter(|f| f.caveats.first().is_some_and(|c| c.starts_with(ONE_OFF))).collect();
        assert!(
            marked.iter().any(|f| f.queries.iter().any(|r| r.id == id)),
            "{name}: the findings about statement {id} say it ran once"
        );
        for f in &marked {
            assert!(
                f.queries.iter().all(|r| once.contains(&r.id)),
                "{name}: {} rests on statements that kept running",
                f.id
            );
            assert_eq!(f.severity, Severity::Low, "{name}: {}", f.id);
            assert!(!f.inactive, "{name}: {} is a one-off, not a statement that stopped running", f.id);
            assert!(f.verify.first().is_some_and(|v| v.contains("run again")), "{name}: {}", f.id);
        }
        // An index for a statement that ran once fixes nothing that keeps running.
        let advice = index_advice(&report);
        assert!(
            advice.candidates.iter().all(|c| c.queries.is_empty() || c.queries.iter().any(|r| r.id != id)),
            "{name}: an index is suggested for statement {id} alone"
        );
    }
}
