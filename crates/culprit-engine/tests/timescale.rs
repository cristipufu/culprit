//! A real scan of the seeded TimescaleDB 2.31 database, replayed with today's parser and checks.
#![cfg(feature = "postgres")]

fn report() -> culprit_core::Report {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/timescale-demo.report.json");
    culprit_engine::reading::open(&path).unwrap()
}

#[test]
fn saved_scan_keeps_logical_storage_and_never_treats_parent_counters_as_chunk_usage() {
    let report = report();
    let snapshot = &report.snapshot;
    assert!(snapshot.server.product.starts_with("TimescaleDB 2.31.0"));
    let table = snapshot.table("public", "sensor_readings").unwrap();
    assert_eq!(table.rows, Some(120000.0));
    assert_eq!(table.total_bytes, Some(17162240.0));
    assert!(table.dead_rows.is_none() && table.seq_scans.is_none() && table.last_analyze.is_none());
    let storage = table.storage.as_ref().unwrap().hypertable.as_ref().unwrap();
    assert_eq!((storage.chunks, storage.compressed_chunks), (Some(14), Some(7)));
    let chunks = storage.chunk_details.as_ref().unwrap();
    assert_eq!(chunks.len(), 14);
    assert_eq!(chunks.iter().filter(|c| c.compressed).count(), 7);
    assert_eq!(snapshot.tables.len(), 2, "internal storage must not be counted as application tables");
    let indexes: Vec<_> = snapshot.indexes.iter().filter(|i| i.table == "sensor_readings").collect();
    assert!(!indexes.is_empty());
    assert!(indexes.iter().all(|i| i.reads.is_none() && i.scans.is_none() && i.size_bytes.is_some()));
    assert!(
        snapshot
            .sources
            .iter()
            .any(|s| s.source == "timescale-hypertables" && s.state == culprit_core::SourceState::Collected)
    );
}

#[test]
fn replay_rebuilds_chunk_ownership_from_saved_native_plans() {
    let report = report();
    let queries = &report.snapshot.workload.as_ref().unwrap().queries;
    let chunks: Vec<_> = queries
        .iter()
        .filter_map(|q| q.plan.as_ref())
        .flat_map(|p| p.nodes())
        .filter(|n| n.facts().table_kind.as_deref() == Some("timescale-chunk"))
        .collect();
    assert!(!chunks.is_empty(), "replayed native plans must retain hypertable context");
    assert!(chunks.iter().all(|n| n.facts().partition_parent.as_deref() == Some("public.sensor_readings")));
    for insight in queries.iter().filter_map(|q| q.plan.as_ref()).flat_map(culprit_core::insights::insights) {
        if let Some(sql) = insight.fix_sql {
            assert!(!sql.contains("ON _timescaledb_"), "{sql}");
            if sql.contains("sensor_readings") {
                assert!(!sql.contains("CONCURRENTLY"), "{sql}");
            }
        }
    }
}
