use culprit_core::{DatabaseProduct, PlanSample};
use culprit_engine::plans::parse_plan;
use serde::Deserialize;

#[derive(Deserialize)]
struct Capture {
    product: DatabaseProduct,
    files: [String; 2],
    names: [String; 2],
}

/// Both the shipped samples and the browser's comparison sidecar use these captured native plans.
pub fn samples() -> Vec<PlanSample> {
    let catalog: Vec<Capture> = serde_json::from_str(include_str!("../../tests/plans/samples.json")).unwrap();
    let root = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/plans/");
    catalog
        .into_iter()
        .map(|capture| PlanSample {
            product: capture.product,
            names: capture.names,
            plans: capture.files.map(|file| {
                let text = std::fs::read_to_string(format!("{root}{file}")).unwrap();
                let mut plan = parse_plan(&text).unwrap_or_else(|error| panic!("{file}: {}", error.message));
                plan.raw = None;
                plan
            }),
        })
        .collect()
}
