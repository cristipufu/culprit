//! One boundary for replaying a report without turning its prepared view into collected evidence.

use std::sync::Arc;

use culprit_core::{
    AnalysisOrigin, BaselineReference, ChangeEvent, Check, CollectionProvenance, ComponentVersion,
    PlanParsingProvenance, Report, Snapshot,
};

pub(crate) struct Input {
    pub snapshot: Snapshot,
    pub collected: Option<Box<Snapshot>>,
    pub recorded_changes: Vec<ChangeEvent>,
    pub collection: Option<CollectionProvenance>,
    pub origin: AnalysisOrigin,
    pub baselines: Vec<BaselineReference>,
    last_analyzer: Option<ComponentVersion>,
    previous_analyzer: Option<ComponentVersion>,
}

impl From<Snapshot> for Input {
    fn from(snapshot: Snapshot) -> Self {
        Self {
            snapshot,
            collected: None,
            recorded_changes: Vec::new(),
            collection: None,
            origin: AnalysisOrigin::Snapshot,
            baselines: Vec::new(),
            last_analyzer: None,
            previous_analyzer: None,
        }
    }
}

impl From<Report> for Input {
    fn from(report: Report) -> Self {
        let Report { snapshot, evidence, provenance, .. } = report;
        let mut input = if let Some(e) = evidence {
            let mut input = Self::from(*e.input);
            input.collected = e.collected;
            input.recorded_changes = e.recorded_changes;
            input.origin = AnalysisOrigin::RetainedInput;
            input
        } else {
            let mut input = Self::from(snapshot);
            input.origin = AnalysisOrigin::PreparedReport;
            input
        };
        if let Some(p) = provenance {
            input.collection = p.collection;
            input.last_analyzer = Some(p.analyzer);
            input.previous_analyzer = p.previous_analyzer;
            input.baselines = p.baselines;
        }
        input
    }
}

pub(crate) fn reread(mut input: Input, extra: &[Arc<dyn Check>]) -> Report {
    let parsing = crate::plans::reparse_saved_traced(&mut input.snapshot);
    run(input, extra, parsing)
}

pub(crate) fn shared(mut input: Input, extra: &[Arc<dyn Check>]) -> Report {
    // Parse before redaction drops native text. Retaining the unredacted collection would bypass that boundary.
    let parsing = crate::plans::reparse_saved_traced(&mut input.snapshot);
    input.snapshot = culprit_core::redact::redact_snapshot(&input.snapshot);
    input.collected = None;
    input.recorded_changes.clear();
    run(input, extra, parsing)
}

pub(crate) fn collected(
    snapshot: Snapshot,
    collected: Snapshot,
    collection: CollectionProvenance,
    recorded_changes: Vec<ChangeEvent>,
    history_inputs: Vec<BaselineReference>,
    extra: &[Arc<dyn Check>],
) -> Report {
    let mut input = Input::from(snapshot);
    input.collected = Some(Box::new(collected));
    input.collection = Some(collection);
    input.recorded_changes = recorded_changes;
    input.origin = AnalysisOrigin::Collected;
    input.baselines = history_inputs;
    run(input, extra, Vec::new())
}

fn run(input: Input, extra: &[Arc<dyn Check>], parsing: Vec<PlanParsingProvenance>) -> Report {
    let mut report = culprit_checks::analyze_with(input.snapshot, extra);
    if let Some(p) = &mut report.provenance {
        p.origin = input.origin;
        p.collection = input.collection;
        // Replaying twice with the same build does not erase the last different analyzer or grow a version chain.
        p.previous_analyzer = if input.last_analyzer.as_ref() == Some(&p.analyzer) {
            input.previous_analyzer
        } else {
            input.last_analyzer
        };
        p.parsing = parsing;
        // Numeric baselines and workload history are rebuilt from the actual input. Index-age evidence belongs to
        // the engine's historical events, retained separately because checks consume the dates they established.
        for reference in input.baselines.into_iter().filter(|r| r.purpose == culprit_core::BaselinePurpose::IndexAge) {
            if !p.baselines.contains(&reference) {
                p.baselines.push(reference);
            }
        }
    }
    if let Some(e) = &mut report.evidence {
        e.collected = input.collected;
        e.recorded_changes = input.recorded_changes;
    }
    report
}
