//! PostgreSQL partitions and operator classes: what the index checks judge at the parent, and which indexes on the same
//! column serve different queries.

use culprit_checks::{analyze, index_advice};
use culprit_core::{ForeignKey, IndexStat, Snapshot, TableStat};

fn snapshot() -> Snapshot {
    let text = std::fs::read_to_string(format!("{}/../../fixtures/postgres-demo.json", env!("CARGO_MANIFEST_DIR")))
        .expect("fixture");
    let mut s: Snapshot = serde_json::from_str(&text).expect("snapshot");
    // A week and more of usage counters, so "never read" can be said at all.
    s.server.started_at = Some("2026-09-01T00:00:00Z".into());
    if let Some(p) = s.pressure.as_mut() {
        p.uptime_hours = Some(24.0 * 40.0);
    }
    s
}

fn index(s: &Snapshot, table: &str, name: &str, columns: &[&str]) -> IndexStat {
    let mut i = s.indexes.iter().find(|i| i.table == "orders").expect("an index to copy").clone();
    i.table = table.into();
    i.name = name.into();
    i.columns = columns.iter().map(|c| c.to_string()).collect();
    i.include.clear();
    i.filter = None;
    i.is_primary_key = false;
    i.is_unique = false;
    i.is_clustered = false;
    i.is_disabled = false;
    i.reads = Some(0.0);
    i.size_bytes = Some(8.0 * 1024.0 * 1024.0);
    i.partition_of = None;
    i.opclasses = None;
    i.created_at_latest = None;
    i
}

fn partition(s: &Snapshot, name: &str) -> TableStat {
    let mut t = s.tables.iter().find(|t| t.name == "orders").expect("orders").clone();
    t.name = name.into();
    t.partition_of = Some("public.events".into());
    t
}

fn ids(s: Snapshot) -> Vec<String> {
    analyze(s).findings.into_iter().map(|f| f.id).collect()
}

#[test]
fn an_index_with_another_operator_class_is_not_a_duplicate() {
    let mut s = snapshot();
    let plain = index(&s, "orders", "orders_note_idx", &["note"]);
    let mut pattern = index(&s, "orders", "orders_note_pattern_idx", &["note"]);
    pattern.reads = Some(500.0);
    s.indexes.push(plain.clone());
    s.indexes.push(pattern.clone());
    // Two plain indexes on the same column: one duplicates the other.
    assert!(ids(s.clone()).iter().any(|id| id.starts_with("duplicate-index:public.orders.orders_note")));
    // text_pattern_ops serves LIKE 'abc%', the default serves ORDER BY and ranges: neither duplicates the other.
    let at = s.indexes.len() - 1;
    s.indexes[at].opclasses = Some(vec!["text_pattern_ops".into()]);
    assert!(!ids(s).iter().any(|id| id.starts_with("duplicate-index:public.orders.orders_note")));
}

#[test]
fn a_partitions_copy_of_an_index_is_judged_at_its_parent() {
    let mut s = snapshot();
    for month in ["events_2026_08", "events_2026_09"] {
        s.tables.push(partition(&s, month));
        let mut copy = index(&s, month, &format!("{month}_tenant_idx"), &["tenant_id"]);
        copy.partition_of = Some("public.events_tenant_idx".into());
        s.indexes.push(copy);
        // The same index twice on a partition: the copy of the parent's, and the parent's other copy.
        let mut twin = index(&s, month, &format!("{month}_tenant_idx1"), &["tenant_id"]);
        twin.partition_of = Some("public.events_tenant_idx1".into());
        s.indexes.push(twin);
    }
    let report = analyze(s);
    let on_partitions = |id: &String| id.contains("events_2026_");
    assert!(
        !report.findings.iter().any(|f| on_partitions(&f.id)),
        "{:?}",
        report.findings.iter().map(|f| &f.id).collect::<Vec<_>>()
    );
    let advice = index_advice(&report);
    assert!(advice.notes.iter().any(|n| n.contains("4 indexes on partitions are left out")), "{:?}", advice.notes);
}

#[test]
fn a_foreign_key_on_a_partition_is_the_parents() {
    let mut s = snapshot();
    s.tables.push(partition(&s, "events_2026_09"));
    let mut fk = s.foreign_keys.first().expect("a foreign key").clone();
    fk.table = "events_2026_09".into();
    fk.columns = vec!["tenant_id".into()];
    fk.name = "events_2026_09_tenant_fk".into();
    s.foreign_keys.push(ForeignKey { ..fk });
    assert!(!ids(s).iter().any(|id| id.starts_with("unindexed-fk:public.events_2026_09")));
}
