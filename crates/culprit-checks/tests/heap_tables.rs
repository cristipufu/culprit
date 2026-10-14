//! A heap costs the extra reads it causes, not its size: an insert-only log heap costs nothing. Medium needs forwarded
//! reads measured as a rate (at least an hour of counters), at 100K a day.

use culprit_checks::analyze;
use culprit_core::{Severity, Snapshot};

fn snapshot(forwarded: Option<f64>, counter_hours: f64) -> (Snapshot, String) {
    let text = std::fs::read_to_string(format!("{}/../../fixtures/mssql-demo.json", env!("CARGO_MANIFEST_DIR")))
        .expect("fixture");
    let mut s: Snapshot = serde_json::from_str(&text).expect("snapshot");
    s.pressure.get_or_insert_with(Default::default).uptime_hours = Some(counter_hours);
    let t = s.tables.first_mut().expect("a table");
    t.is_heap = Some(true);
    t.rows = Some(10_000_000.0);
    t.forwarded_fetches = forwarded;
    let id = format!("heap-table:{}", t.qualified());
    (s, id)
}

fn severity(forwarded: Option<f64>, counter_hours: f64) -> Option<Severity> {
    let (s, id) = snapshot(forwarded, counter_hours);
    analyze(s).findings.into_iter().find(|f| f.id == id).map(|f| f.severity)
}

#[test]
fn a_big_heap_without_forwarded_reads_is_low() {
    assert_eq!(severity(None, 240.0), Some(Severity::Low));
}

#[test]
fn measured_forwarded_reads_make_it_medium() {
    // 5M over ten days: 500K a day.
    assert_eq!(severity(Some(5_000_000.0), 240.0), Some(Severity::Medium));
}

#[test]
fn a_total_from_half_an_hour_of_counters_is_no_rate() {
    assert_eq!(severity(Some(5_000_000.0), 0.5), Some(Severity::Low));
}
