use std::collections::{BTreeMap, BTreeSet};

use culprit_core::{Engine, plan_diff};

#[path = "../examples/support/plan_samples.rs"]
mod captured;

#[test]
fn every_product_has_a_parseable_comparison_from_its_native_parser() {
    let expected: BTreeMap<_, _> = Engine::ALL
        .into_iter()
        .flat_map(|engine| engine.spec().products().iter().map(move |product| (product.id, engine)))
        .collect();
    let mut seen = BTreeSet::new();
    for sample in captured::samples() {
        assert!(seen.insert(sample.product), "Duplicate sample: {:?}", sample.product);
        for (name, plan) in sample.names.iter().zip(&sample.plans) {
            assert!(!name.trim().is_empty());
            assert_eq!(Some(&plan.engine), expected.get(&sample.product), "{:?}", sample.product);
        }
        assert!(
            !plan_diff(&sample.plans[0], &sample.plans[1]).changes.is_empty(),
            "{:?} needs distinct plans",
            sample.product
        );
    }
    assert_eq!(seen, expected.into_keys().collect(), "Add captured samples for every product in EngineSpec::products");
}

#[test]
fn bundled_samples_match_the_captured_native_plans_and_current_analysis() {
    // Compare the generator's serialized output directly: round-tripping computed floats through a second
    // deserialization can change their last bit even when the generated artifact is current.
    let fresh = serde_json::to_string(&serde_json::to_value(captured::samples()).unwrap()).unwrap();
    assert!(
        fresh == include_str!("../../../fixtures/plan-samples.json"),
        "Refresh plan samples with dev/refresh-demo.py"
    );
}
