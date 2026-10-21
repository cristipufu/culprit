use std::sync::Arc;

use culprit_core::{AnalysisOrigin, AnalysisStage, Check, CheckInfo, Finding, Report, Snapshot};

use crate::pipeline::{Pipeline, Prepare, Refine};

fn snapshot() -> Snapshot {
    serde_json::from_str(include_str!("../../../fixtures/mssql-demo.json")).unwrap()
}

struct CatalogRemoved;
impl Prepare for CatalogRemoved {
    fn name(&self) -> &str {
        "test catalog removed"
    }
    fn version(&self) -> Option<&str> {
        Some("2")
    }
    fn prepare(&self, snapshot: &mut Snapshot) {
        snapshot.tables.clear();
    }
}

struct NoChange;
impl Refine for NoChange {
    fn name(&self) -> &str {
        "test no change"
    }
    fn refine(&self, _snapshot: &Snapshot, _findings: &mut Vec<Finding>) {}
}

struct Extra {
    id: String,
    version: Option<&'static str>,
}
impl Check for Extra {
    fn info(&self) -> CheckInfo {
        let mut info = crate::all()[0].info();
        info.id = self.id.clone();
        info
    }
    fn version(&self) -> Option<&str> {
        self.version
    }
    fn run(&self, _snapshot: &Snapshot) -> Vec<Finding> {
        Vec::new()
    }
}

#[test]
fn original_input_survives_preparation_and_serialization() {
    let original = snapshot();
    assert!(!original.tables.is_empty());
    let expected = serde_json::to_value(&original).unwrap();
    let report = Pipeline::builtin().with_prepare(Arc::new(CatalogRemoved)).run(original);
    assert!(report.snapshot.tables.is_empty());
    assert_eq!(serde_json::to_value(report.analysis_input()).unwrap(), expected);
    let evidence = report.evidence.as_ref().unwrap();
    assert!(evidence.collected.is_none(), "a supplied snapshot does not identify pristine provider collection");
    let provenance = report.provenance.as_ref().unwrap();
    assert_eq!(provenance.origin, AnalysisOrigin::Snapshot);
    assert_eq!(provenance.analyzer.version.as_deref(), Some(env!("CARGO_PKG_VERSION")));
    assert!(provenance.collection.is_none());
    let reread: Report = serde_json::from_value(serde_json::to_value(&report).unwrap()).unwrap();
    assert_eq!(serde_json::to_value(reread.analysis_input()).unwrap(), expected);
    assert!(reread.snapshot.tables.is_empty());
}

#[test]
fn only_registered_extensions_keep_their_own_versions_and_order() {
    let builtin = crate::all()[0].info().id;
    let report = Pipeline::builtin()
        .with_check(Arc::new(Extra { id: "example-provenance".into(), version: Some("1.2") }))
        .with_check(Arc::new(Extra { id: "example-provenance".into(), version: Some("99") }))
        .with_check(Arc::new(Extra { id: builtin, version: Some("99") }))
        .with_prepare(Arc::new(CatalogRemoved))
        .with_refine(Arc::new(NoChange))
        .run(snapshot());
    let p = report.provenance.as_ref().unwrap();
    assert_eq!(p.extensions.len(), 3);
    assert_eq!(p.extensions[0].stage, AnalysisStage::Check);
    assert_eq!(p.extensions[0].version.as_deref(), Some("1.2"));
    assert_eq!(p.extensions[1].stage, AnalysisStage::Prepare);
    assert_eq!(p.extensions[1].version.as_deref(), Some("2"));
    assert_eq!(p.extensions[2].stage, AnalysisStage::Refine);
    assert_eq!(p.extensions[2].version, None);
    assert_eq!(p.pipeline.checks.last().map(String::as_str), Some("example-provenance"));
    assert_eq!(p.pipeline.prepare.last().map(String::as_str), Some("test catalog removed"));
    assert_eq!(p.pipeline.refine.last().map(String::as_str), Some("test no change"));
}

#[test]
fn old_reports_do_not_invent_original_evidence_or_versions() {
    let mut old: serde_json::Value =
        serde_json::from_str(include_str!("../../../fixtures/mssql-demo.report.json")).unwrap();
    old.as_object_mut().unwrap().remove("provenance");
    old.as_object_mut().unwrap().remove("evidence");
    let report: Report = serde_json::from_value(old).unwrap();
    assert!(report.provenance.is_none());
    assert!(report.evidence.is_none());
    assert_eq!(serde_json::to_value(report.analysis_input()).unwrap(), serde_json::to_value(&report.snapshot).unwrap());
    let json = serde_json::to_value(report).unwrap();
    assert!(json.get("evidence").is_none());
    assert!(json.get("provenance").is_none());
}

#[test]
fn pure_analysis_is_deterministic_and_shared_evidence_is_redacted() {
    let original = snapshot();
    let first = crate::analyze(original.clone());
    let second = crate::analyze(original.clone());
    assert_eq!(serde_json::to_value(first).unwrap(), serde_json::to_value(second).unwrap());
    let mut planted = original;
    planted.workload.as_mut().unwrap().queries[0].text = "SELECT 'PROVENANCE_PRIVATE_VALUE'".into();
    let shared = crate::analyze(culprit_core::redact::redact_snapshot(&planted));
    assert!(!serde_json::to_string(&shared).unwrap().contains("PROVENANCE_PRIVATE_VALUE"));
    assert!(planted.queries()[0].text.contains("PROVENANCE_PRIVATE_VALUE"));
}
