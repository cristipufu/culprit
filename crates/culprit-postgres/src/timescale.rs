//! TimescaleDB's public information/size APIs. A hypertable is a logical object: PostgreSQL's empty parent heap is
//! neither its row count nor its physical size. Managed chunks remain linked to their owner; internal compressed and
//! continuous-aggregate materialization tables never receive application schema/index advice.

use crate::{
    probe::Probe,
    rows::{Row, query},
    sources,
};
use culprit_core::signals::{HypertableChunk, HypertableStorage};
use culprit_core::{Coverage, Snapshot, TableStorage};
use std::collections::{BTreeMap, BTreeSet};
use tokio_postgres::Client;

fn managed_schema(schema: &str) -> bool {
    schema.starts_with("_timescaledb_") || schema == "timescaledb_information"
}

pub async fn collect(client: &Client, probe: &Probe, snapshot: &mut Snapshot) {
    let Some(schema) = probe.timescale_schema.as_deref() else { return };
    let api = culprit_core::Engine::Postgres.dialect().quote(schema);
    let mut coverage = Coverage::default();
    let sql = format!(
        "SELECT h.hypertable_schema AS schema_name, h.hypertable_name AS table_name,
                h.num_chunks, h.compression_enabled,
                (SELECT count(*) FROM timescaledb_information.chunks c
                  WHERE c.hypertable_schema = h.hypertable_schema AND c.hypertable_name = h.hypertable_name
                    AND c.is_compressed) AS compressed_chunks,
                (SELECT string_agg(d.column_name, chr(31) ORDER BY d.dimension_number)
                   FROM timescaledb_information.dimensions d
                  WHERE d.hypertable_schema = h.hypertable_schema AND d.hypertable_name = h.hypertable_name) AS dimensions,
                {api}.approximate_row_count(format('%I.%I', h.hypertable_schema, h.hypertable_name)::regclass) AS rows,
                sizes.table_bytes, sizes.index_bytes, sizes.toast_bytes, sizes.total_bytes
           FROM timescaledb_information.hypertables h
           LEFT JOIN LATERAL {api}.hypertable_detailed_size(format('%I.%I', h.hypertable_schema, h.hypertable_name)::regclass) sizes ON true
          WHERE h.hypertable_schema !~ '^_timescaledb_'"
    );
    let Some(hypertables) = coverage.read_rows(&sources::HYPERTABLES, query(client, &sql).await) else {
        // Empty-parent counters must not become positive evidence when the extension's real catalog is unavailable.
        unresolved_catalog(snapshot);
        if let Some(workload) = &mut snapshot.workload {
            for plan in workload.queries.iter_mut().filter_map(|q| q.plan.as_mut()) {
                fn unresolved(node: &mut culprit_core::PlanNode) {
                    node.facts_mut().table_kind = Some("timescale-unresolved".into());
                    for child in &mut node.children {
                        unresolved(child);
                    }
                }
                unresolved(&mut plan.root);
            }
        }
        strip_managed(snapshot);
        coverage.append_to(snapshot);
        return;
    };
    let hypertables: Vec<Hypertable> = hypertables.iter().map(Hypertable::of).collect();
    apply_hypertables(snapshot, &hypertables);
    let sql = format!(
        "SELECT h.hypertable_schema, h.hypertable_name, sizes.chunk_schema, sizes.chunk_name,
                c.is_compressed, sizes.table_bytes, sizes.index_bytes, sizes.toast_bytes, sizes.total_bytes,
                {api}.approximate_row_count(format('%I.%I', sizes.chunk_schema, sizes.chunk_name)::regclass) AS rows
           FROM timescaledb_information.hypertables h
           CROSS JOIN LATERAL {api}.chunks_detailed_size(format('%I.%I', h.hypertable_schema, h.hypertable_name)::regclass) sizes
           JOIN timescaledb_information.chunks c ON c.chunk_schema = sizes.chunk_schema AND c.chunk_name = sizes.chunk_name
          WHERE h.hypertable_schema !~ '^_timescaledb_'"
    );
    let chunks = coverage.read_rows(&sources::CHUNKS, query(client, &sql).await);
    let roots: BTreeSet<_> = hypertables.iter().map(|r| (r.schema.clone(), r.name.clone())).collect();
    if let Some(chunks) = chunks {
        let mut by_owner = BTreeMap::<(String, String), BTreeMap<(String, String), HypertableChunk>>::new();
        for row in chunks {
            let owner = (row.string("hypertable_schema"), row.string("hypertable_name"));
            let chunk = HypertableChunk {
                schema: row.string("chunk_schema"),
                name: row.string("chunk_name"),
                rows: row.number("rows"),
                total_bytes: row.number("total_bytes"),
                compressed: row.flag("is_compressed"),
            };
            let key = (chunk.schema.clone(), chunk.name.clone());
            by_owner
                .entry(owner)
                .or_default()
                .entry(key)
                .and_modify(|old| {
                    old.total_bytes = old.total_bytes.zip(chunk.total_bytes).map(|(a, b)| a + b);
                })
                .or_insert(chunk);
        }
        for table in &mut snapshot.tables {
            if let Some(hypertable) = table.storage.as_mut().and_then(|s| s.hypertable.as_mut()) {
                hypertable.chunk_details = Some(
                    by_owner
                        .remove(&(table.schema.clone(), table.name.clone()))
                        .unwrap_or_default()
                        .into_values()
                        .collect(),
                );
            }
        }
    }
    // pg_stat_user_indexes counts physical chunk indexes, not the logical parent index. Do not grade the parent's
    // empty counters as unused, or advise users to drop extension-managed indexes individually.
    snapshot.indexes.retain(|i| !managed_schema(&i.schema));
    for index in &mut snapshot.indexes {
        if roots.contains(&(index.schema.clone(), index.table.clone())) {
            index.reads = None;
            index.scans = None;
            index.writes = None;
            index.last_read = None;
            index.size_bytes = None;
        }
    }
    let sql = format!(
        "SELECT h.hypertable_schema AS schema_name, h.hypertable_name AS table_name, i.indexname,
                {api}.hypertable_index_size(format('%I.%I', i.schemaname, i.indexname)::regclass) AS bytes
           FROM timescaledb_information.hypertables h
           JOIN pg_indexes i ON i.schemaname = h.hypertable_schema AND i.tablename = h.hypertable_name
          WHERE h.hypertable_schema !~ '^_timescaledb_'"
    );
    if let Some(indexes) = coverage.read_rows(&sources::HYPERTABLE_INDEXES, query(client, &sql).await) {
        for row in indexes {
            if let Some(index) = snapshot.indexes.iter_mut().find(|i| {
                i.schema == row.string("schema_name")
                    && i.table == row.string("table_name")
                    && i.name == row.string("indexname")
            }) {
                index.size_bytes = row.number("bytes");
            }
        }
    }
    strip_managed(snapshot);
    for stat in &mut snapshot.statistics {
        if let Some(table) = snapshot.tables.iter().find(|t| t.schema == stat.schema && t.name == stat.table) {
            stat.rows = table.rows;
            stat.modifications = table.modifications_since_analyze;
            stat.last_updated = table.last_analyze.clone();
        }
    }
    coverage.note("timescale-indexes", "Hypertable index sizes include managed chunks. Logical parent index usage is not recorded by PostgreSQL's parent counters; Culprit leaves it unknown and does not call those indexes unused.");
    coverage.append_to(snapshot);
}

fn strip_managed(snapshot: &mut Snapshot) {
    snapshot.tables.retain(|t| !managed_schema(&t.schema));
    snapshot.indexes.retain(|i| !managed_schema(&i.schema));
    snapshot.columns.retain(|c| !managed_schema(&c.schema));
    snapshot.foreign_keys.retain(|f| !managed_schema(&f.schema));
    snapshot.statistics.retain(|s| !managed_schema(&s.schema));
    snapshot.constraints.retain(|c| !managed_schema(&c.schema));
    // The database-wide freeze age still covers chunk heaps. Do not issue maintenance advice on managed chunks.
    snapshot
        .operations
        .multixacts
        .retain(|age| age.subject == snapshot.server.database || !age.subject.starts_with("_timescaledb_"));
}

fn unresolved_catalog(snapshot: &mut Snapshot) {
    for table in &mut snapshot.tables {
        table.kind = Some("timescale-unresolved".into());
        clear_parent_counters(table);
        table.rows = None;
        table.total_bytes = None;
        table.data_bytes = None;
        table.index_bytes = None;
        table.storage = None;
    }
    for index in &mut snapshot.indexes {
        index.reads = None;
        index.scans = None;
        index.writes = None;
        index.last_read = None;
        index.size_bytes = None;
    }
    for stat in &mut snapshot.statistics {
        stat.rows = None;
        stat.modifications = None;
        stat.last_updated = None;
    }
}

fn clear_parent_counters(table: &mut culprit_core::TableStat) {
    table.in_row_bytes = None;
    table.lob_bytes = None;
    table.dead_rows = None;
    table.xid_age = None;
    table.last_vacuum = None;
    table.last_analyze = None;
    table.seq_scans = None;
    table.seq_rows_read = None;
    table.index_scans = None;
    table.inserts = None;
    table.updates = None;
    table.deletes = None;
    table.modifications_since_analyze = None;
}

struct Hypertable {
    schema: String,
    name: String,
    rows: Option<f64>,
    table_bytes: Option<f64>,
    index_bytes: Option<f64>,
    toast_bytes: Option<f64>,
    total_bytes: Option<f64>,
    storage: HypertableStorage,
}

impl Hypertable {
    fn of(row: &Row) -> Self {
        Self {
            schema: row.string("schema_name"),
            name: row.string("table_name"),
            rows: row.number("rows"),
            table_bytes: row.number("table_bytes"),
            index_bytes: row.number("index_bytes"),
            toast_bytes: row.number("toast_bytes"),
            total_bytes: row.number("total_bytes"),
            storage: HypertableStorage {
                chunks: row.text("num_chunks").and_then(|v| v.parse().ok()),
                compressed_chunks: row.text("compressed_chunks").and_then(|v| v.parse().ok()),
                dimensions: row.list("dimensions"),
                compression_enabled: row.flag("compression_enabled"),
                chunk_details: None,
            },
        }
    }
}

fn apply_hypertables(snapshot: &mut Snapshot, rows: &[Hypertable]) {
    // Distributed hypertable size APIs may return one row per data node. The metadata and logical row estimate are
    // repeated, while byte counts are additive.
    let mut groups = BTreeMap::<(String, String), Vec<&Hypertable>>::new();
    for row in rows {
        groups.entry((row.schema.clone(), row.name.clone())).or_default().push(row);
    }
    for ((schema, name), rows) in groups {
        let Some(table) = snapshot.tables.iter_mut().find(|t| t.schema == schema && t.name == name) else { continue };
        let first = rows[0];
        table.kind = Some("hypertable".into());
        table.rows = first.rows;
        let sum = |field: fn(&Hypertable) -> Option<f64>| rows.iter().map(|r| field(r)).sum::<Option<f64>>();
        table.total_bytes = sum(|r| r.total_bytes);
        table.index_bytes = sum(|r| r.index_bytes);
        table.data_bytes = sum(|r| r.table_bytes).zip(sum(|r| r.toast_bytes)).map(|(a, b)| a + b);
        clear_parent_counters(table);
        table.storage = Some(TableStorage { hypertable: Some(first.storage.clone()), ..Default::default() });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot() -> Snapshot {
        serde_json::from_str(include_str!("../../../fixtures/postgres-demo.json")).unwrap()
    }

    fn metadata(snapshot: &Snapshot, bytes: Option<f64>) -> Hypertable {
        let table = &snapshot.tables[0];
        Hypertable {
            schema: table.schema.clone(),
            name: table.name.clone(),
            rows: Some(120000.0),
            table_bytes: bytes,
            toast_bytes: Some(10.0),
            index_bytes: Some(20.0),
            total_bytes: bytes.map(|n| n + 30.0),
            storage: HypertableStorage {
                chunks: Some(14),
                compressed_chunks: Some(7),
                dimensions: vec!["observed_at".into()],
                compression_enabled: true,
                chunk_details: None,
            },
        }
    }

    #[test]
    fn logical_storage_replaces_parent_heap_counters_without_changing_other_tables() {
        let mut snapshot = snapshot();
        let other = serde_json::to_value(&snapshot.tables[1]).unwrap();
        let metadata = metadata(&snapshot, Some(1000.0));
        apply_hypertables(&mut snapshot, &[metadata]);
        let table = &snapshot.tables[0];
        assert_eq!(table.rows, Some(120000.0));
        assert_eq!(table.total_bytes, Some(1030.0));
        assert_eq!(table.kind.as_deref(), Some("hypertable"));
        assert_eq!(table.storage.as_ref().unwrap().hypertable.as_ref().unwrap().compressed_chunks, Some(7));
        assert!(table.dead_rows.is_none() && table.xid_age.is_none() && table.seq_scans.is_none());
        assert!(table.modifications_since_analyze.is_none() && table.inserts.is_none());
        assert_eq!(serde_json::to_value(&snapshot.tables[1]).unwrap(), other);
    }

    #[test]
    fn node_sizes_are_additive_but_repeated_row_estimates_are_not() {
        let mut snapshot = snapshot();
        let a = metadata(&snapshot, Some(1000.0));
        let b = metadata(&snapshot, Some(2000.0));
        apply_hypertables(&mut snapshot, &[a, b]);
        assert_eq!(snapshot.tables[0].rows, Some(120000.0));
        assert_eq!(snapshot.tables[0].total_bytes, Some(3060.0));
        let missing = metadata(&snapshot, None);
        apply_hypertables(&mut snapshot, &[missing]);
        assert!(snapshot.tables[0].total_bytes.is_none());
    }

    #[test]
    fn unavailable_native_catalog_never_grades_empty_parent_counters() {
        let mut snapshot = snapshot();
        unresolved_catalog(&mut snapshot);
        assert!(snapshot.tables.iter().all(|t| t.rows.is_none()
            && t.seq_scans.is_none()
            && t.inserts.is_none()
            && t.last_analyze.is_none()
            && t.total_bytes.is_none()));
        assert!(snapshot.indexes.iter().all(|i| i.reads.is_none() && i.last_read.is_none() && i.size_bytes.is_none()));
        assert!(
            snapshot
                .statistics
                .iter()
                .all(|s| s.rows.is_none() && s.modifications.is_none() && s.last_updated.is_none())
        );
    }
}
