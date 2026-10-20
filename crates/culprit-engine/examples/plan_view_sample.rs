//! Recompute the browser's plan projections with Rust, including recorded actual/progress plans.
#[path = "../tests/support/plan_view_cases.rs"]
mod cases;

fn main() {
    use serde::Serialize;
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let path = root.join("fixtures/plan-view.sample.json");
    std::fs::write(&path, serde_json::to_vec(&cases::cases()).unwrap()).unwrap();
    let path = root.join("fixtures/actual.sample.json");
    let mut actual: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    for queries in actual["last_actual"].as_object_mut().unwrap().values_mut() {
        for value in queries.as_object_mut().unwrap().values_mut() {
            let plan: culprit_core::Plan = serde_json::from_value(value.clone()).unwrap();
            value["view"] = serde_json::to_value(culprit_checks::plan_view(&plan, None)).unwrap();
        }
    }
    for frames in actual["progress"].as_object_mut().unwrap().values_mut() {
        for frame in frames.as_array_mut().unwrap() {
            if frame["plan"].is_null() {
                continue;
            }
            let mut plan: culprit_core::Plan = serde_json::from_value(frame["plan"].clone()).unwrap();
            culprit_checks::present_progress_plan(&mut plan);
            frame["plan"]["view"] = serde_json::to_value(plan.view).unwrap();
        }
    }
    let mut text = Vec::new();
    let formatter = serde_json::ser::PrettyFormatter::with_indent(b" ");
    actual.serialize(&mut serde_json::Serializer::with_formatter(&mut text, formatter)).unwrap();
    text.push(b'\n');
    std::fs::write(path, text).unwrap();
}
