use super::*;

fn report() -> Report {
    let text = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../fixtures/mssql-demo.report.json"))
        .expect("fixture");
    serde_json::from_str(&text).expect("report")
}

#[test]
fn stopped_findings_get_their_own_section_and_text_stays_text() {
    let mut r = report();
    let context = r.findings.iter().filter(|f| !f.is_actionable()).count();
    r.findings[0].inactive = true;
    r.findings[1].title = "orders <img src=x onerror=alert(1)> | split".into();
    let md = markdown_report(&r);
    assert!(md.contains(&format!("## Information and history ({})", context + 1)), "{md}");
    assert!(md.contains("&lt;img") && !md.contains("<img"));
    assert!(md.contains("\\| split"));
}
