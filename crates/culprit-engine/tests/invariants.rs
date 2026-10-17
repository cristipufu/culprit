//! Rules that hold for every finding of every demo fixture, read the way the app opens it: not one reported example,
//! the rule itself (a finding on statements that stopped gives no SQL to create anything, a verify names a number a
//! later scan can show, …). A new rule adds a test here that iterates over [`demo_reports`].

use culprit_core::Report;

/// Every demo fixture's report, by name, as `culprit_engine::reading` opens it (the same files as the golden test).
pub(crate) fn demo_reports() -> Vec<(String, Report)> {
    crate::golden::demo_files()
        .into_iter()
        .map(|(name, path)| {
            let report = culprit_engine::reading::open(&path).unwrap_or_else(|e| panic!("{name}: {e:?}"));
            (name, report)
        })
        .collect()
}

#[test]
fn every_demo_fixture_opens() {
    let reports = demo_reports();
    assert!(reports.iter().all(|(_, r)| !r.findings.is_empty()), "every demo has findings");
}
