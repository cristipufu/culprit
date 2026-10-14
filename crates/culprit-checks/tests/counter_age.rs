//! How long the usage counters have counted decides whether "never read" means anything. PostgreSQL keeps its counters
//! across a clean restart and discards them in a crash recovery that keeps the server's start time, so only the age the
//! scan measured counts there, never the time since the start.

use culprit_checks::analyze;
use culprit_core::Snapshot;

fn snapshot() -> Snapshot {
    let text = std::fs::read_to_string(format!("{}/../../fixtures/postgres-demo.json", env!("CARGO_MANIFEST_DIR")))
        .expect("fixture");
    let mut s: Snapshot = serde_json::from_str(&text).expect("snapshot");
    // Started 40 days ago, with an index nothing has read.
    s.server.started_at = Some("2026-09-01T00:00:00Z".into());
    let mut idle = s.indexes.iter().find(|i| i.table == "orders").expect("an index on orders").clone();
    idle.name = "orders_idle_idx".into();
    idle.columns = vec!["note".into()];
    idle.include.clear();
    idle.filter = None;
    idle.is_primary_key = false;
    idle.is_unique = false;
    idle.is_clustered = false;
    idle.is_disabled = false;
    idle.reads = Some(0.0);
    idle.size_bytes = Some(8.0 * 1024.0 * 1024.0);
    idle.partition_of = None;
    idle.created_at_latest = None;
    s.indexes.push(idle);
    s
}

fn flags_idle_index(s: Snapshot) -> bool {
    analyze(s).findings.iter().any(|f| {
        f.id.starts_with("unused-index:")
            && (f.title.contains("orders_idle_idx") || f.summary.contains("orders_idle_idx"))
    })
}

#[test]
fn postgres_counters_of_unknown_age_say_nothing_about_absence() {
    let mut s = snapshot();
    if let Some(p) = s.pressure.as_mut() {
        p.uptime_hours = None;
    }
    assert!(!flags_idle_index(s), "the server's start time isn't the counters' age on PostgreSQL");
}

#[test]
fn postgres_counters_measured_over_weeks_do() {
    let mut s = snapshot();
    s.pressure.get_or_insert_with(Default::default).uptime_hours = Some(24.0 * 40.0);
    assert!(flags_idle_index(s));
}
