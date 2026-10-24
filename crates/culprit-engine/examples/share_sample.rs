//! Writes `fixtures/share.sample.json` for the browser demo: plan comparisons and exports made by the same code the
//! desktop app runs, from plans captured on the demo databases and the demo reports. The demo can't run Rust, so
//! it replays these.
//!
//!     cargo run -p culprit-engine --example share_sample

use culprit_core::{Report, plan_diff_named};
use culprit_engine::export::{self, ExportOptions};
#[path = "support/plan_samples.rs"]
mod plan_samples;
use serde_json::{Value, json};

fn main() {
    let root = concat!(env!("CARGO_MANIFEST_DIR"), "/../..");
    let options = ExportOptions::default();
    let samples = plan_samples::samples();
    let bundled = format!("{root}/fixtures/plan-samples.json");
    std::fs::write(&bundled, serde_json::to_string(&serde_json::to_value(&samples).unwrap()).unwrap()).unwrap();
    let mut compares = Vec::new();
    for sample in samples {
        let [pa, pb] = sample.plans;
        let [name_a, name_b] = sample.names;
        let (name_a, name_b) = (name_a.as_str(), name_b.as_str());
        let forward = plan_diff_named(&pa, name_a, &pb, name_b);
        let backward = plan_diff_named(&pb, name_b, &pa, name_a);
        compares.push(json!({
            "product": sample.product,
            "names": [name_a, name_b],
            "a": pa,
            "b": pb,
            "diff": forward,
            "reverse": backward,
            "markdown": export::diff_markdown(&pa, &pb, &forward, name_a, name_b, &options),
            "html": export::diff_html(&pa, &pb, &forward, name_a, name_b, &options),
            "plan_markdown": [export::plan_markdown(&pa, name_a, &options), export::plan_markdown(&pb, name_b, &options)],
        }));
    }
    let mut findings = serde_json::Map::new();
    for file in [
        "mssql-demo.report.json",
        "mssql-demo-staging.report.json",
        "pg-demo.report.json",
        "mysql-demo.report.json",
        "mariadb-demo.report.json",
    ] {
        let report: Report =
            serde_json::from_str(&std::fs::read_to_string(format!("{root}/fixtures/{file}")).unwrap()).unwrap();
        for (i, f) in report.findings.iter().enumerate() {
            let mut entry = json!({ "markdown": export::finding_markdown(&report, f, &options) });
            if i < 3 {
                entry["html"] = Value::String(export::finding_html(&report, f, &options));
            }
            findings.insert(format!("{}|{}", report.snapshot.connection, f.id), entry);
        }
    }
    let out = json!({ "compares": compares, "findings": findings });
    let path = format!("{root}/fixtures/share.sample.json");
    std::fs::write(&path, serde_json::to_string(&out).unwrap()).unwrap();
    eprintln!("wrote {path}");
}
