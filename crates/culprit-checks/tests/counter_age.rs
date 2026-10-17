//! How long the usage counters have counted decides whether "never read" means anything. PostgreSQL keeps its counters
//! across a clean restart and discards them in a crash recovery that keeps the server's start time, so only the age the
//! scan measured counts there, never the time since the start.

use culprit_checks::{analyze, index_advice};
use culprit_core::Snapshot;
use culprit_core::format::{epoch_seconds, iso_from_epoch};

/// For a test that ages the usage counters by moving the server's start back: performance_schema's statement digests
/// counted from that start too (its `window_start` and `window_hours`). Otherwise digests that begin long after the
/// start read as counters that may have been reset since, and absence waits a week of the digests' own counting.
pub(crate) fn digests_counted_since_the_start(s: &mut Snapshot) {
    let Some(started) = s.server.started_at.clone() else { return };
    let (Some(from), Some(to)) = (epoch_seconds(&started), epoch_seconds(&s.taken_at)) else { return };
    let Some(w) = s.workload.as_mut().filter(|w| w.kind() == Some(culprit_core::WorkloadKind::PerformanceSchema))
    else {
        return;
    };
    w.window_start = Some(started);
    w.window_hours = w.window_hours.max((to - from) / 3600.0);
}

/// The MySQL demo with an unread index on its largest table, a server started ten days before the scan, and statement
/// digests that begin `digest_days` before it.
fn mysql_with_an_unread_index(digest_days: f64) -> Snapshot {
    let text = std::fs::read_to_string(format!("{}/../../fixtures/mysql-demo.json", env!("CARGO_MANIFEST_DIR")))
        .expect("fixture");
    let mut s: Snapshot = serde_json::from_str(&text).expect("snapshot");
    let taken = epoch_seconds(&s.taken_at).expect("taken_at");
    s.server.started_at = Some(iso_from_epoch(taken - 10.0 * 86_400.0));
    s.pressure.get_or_insert_with(Default::default).uptime_hours = Some(240.0);
    let w = s.workload.as_mut().expect("a workload");
    w.window_start = Some(iso_from_epoch(taken - digest_days * 86_400.0));
    w.window_hours = digest_days * 24.0;
    let mut idle = s.indexes.iter().find(|i| i.table == "orders").expect("an index on orders").clone();
    idle.name = "ix_orders_idle".into();
    idle.columns = vec!["note".into()];
    idle.include.clear();
    idle.filter = None;
    (idle.is_primary_key, idle.is_unique, idle.is_clustered, idle.is_disabled) = (false, false, false, false);
    idle.reads = Some(0.0);
    idle.size_bytes = Some(8.0 * 1024.0 * 1024.0);
    idle.created_at_latest = None;
    s.indexes.push(idle);
    s
}

// performance_schema's table and index counters start over with its statements (`sys.ps_truncate_all_tables()`), and
// the server's uptime doesn't. Where the digests begin long after the server started, the counters may have been reset
// then: "never read" waits a week of the digests' own counting, however long the server has been up.
#[test]
fn performance_schema_counters_that_may_be_reset_wait_a_week_of_their_own() {
    let flags = |s: Snapshot| {
        let report = analyze(s);
        report
            .findings
            .iter()
            .find(|f| f.id.starts_with("unused-index:") && f.title.contains("ix_orders_idle"))
            .map(|f| f.metrics.iter().find(|m| m.label == "Counting for").map(|m| m.value.round()))
    };
    // Ten days up, statements counted for two: the counters may be two days old.
    let s = mysql_with_an_unread_index(2.0);
    let advice = index_advice(&culprit_checks::analyze(s.clone()));
    assert_eq!(flags(s), None, "two days of digests say nothing about a week");
    assert!(
        advice.notes.iter().any(|n| n.contains("may have started over with performance_schema's statements")),
        "{:?}",
        advice.notes
    );
    // Eight days of digests (reset two days after the start) are a week of counting: unused, counted for 8 days.
    assert_eq!(flags(mysql_with_an_unread_index(8.0)), Some(Some(8.0)));
    // Digests from the start: the counters are the server's, ten days old.
    let mut s = mysql_with_an_unread_index(2.0);
    digests_counted_since_the_start(&mut s);
    assert_eq!(flags(s), Some(Some(10.0)));
}

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

// A duplicate's reads are a count only once the counters have counted for a week: "Reads 0" over seconds since a
// restart says nothing, so it's left out until then, and shown with how long it counted.
#[test]
fn a_duplicates_reads_are_shown_over_a_week_of_counters() {
    let metrics = |hours: f64| {
        let mut s = snapshot();
        s.pressure.get_or_insert_with(Default::default).uptime_hours = Some(hours);
        analyze(s)
            .findings
            .into_iter()
            .find(|f| f.id == "duplicate-index:public.products.products_sku_idx")
            .expect("products_sku_idx duplicates products_sku_key")
            .metrics
    };
    let fresh = metrics(0.01);
    assert!(!fresh.iter().any(|m| m.label.starts_with("Reads of")), "{fresh:?}");
    let weeks = metrics(24.0 * 40.0);
    assert!(weeks.iter().any(|m| m.label == "Reads of products_sku_idx" && m.value == 0.0), "{weeks:?}");
    assert!(weeks.iter().any(|m| m.label == "Counting for" && m.value == 40.0), "{weeks:?}");
}
