use super::*;
use crate::evidence::{Observation, Requirement as Evidence};
pub fn sequence_capacity_info() -> CheckInfo {
    CheckInfo { id: "sequence-capacity".into(), title: "Identity and sequence allocation limits".into(), category: Category::Maintenance, engines: vec![Engine::Mssql,Engine::Postgres,Engine::Mysql], requires: vec![], description: "Non-cycling identities, sequences and auto-increment columns approaching their numeric bounds; cache allocation is not a forecast of remaining requests.".into() }
}
check!(
    SequenceCapacityCheck: sequence_capacity_info, sequence_capacity,
    &[Evidence::Observed(Observation::Sequences, &[culprit_core::source_id::SEQUENCE_CAPACITY])]
);
pub fn sequence_capacity(s: &Snapshot) -> Vec<Finding> {
    let mut out: Vec<Finding> = s.operations.sequences.iter().filter_map(|seq| {
        let (remaining,total) = seq.remaining()?;
        if total == 0 { return None; }
        let fraction = remaining as f64 / total as f64;
        if fraction > 0.01 && (remaining > 10_000 || fraction > 0.1) { return None; }
        let mut f = finding("sequence-capacity", &seq.name, Category::Maintenance, subject(SubjectKind::Database, &seq.name));
        f.title = format!("{} is near its numeric limit ({} recorded steps remain)",seq.name,format::count(remaining as f64));
        f.summary = format!("The recorded value is {}, increment {}, with bounds {} to {} and cycling off. {} Caching, skipped values and per-connection allocation rules mean this is not the number of application requests left or a time-to-failure estimate. Reaching the bound can reject inserts or allocations; reusing old numbers can violate uniqueness.",seq.current,seq.increment,seq.minimum,seq.maximum,if seq.allocated { "The source may record reserved cache space or the next auto-increment value rather than the last value returned to a caller." } else { "This is the allocation state observed by the source." });
        f.severity = if remaining <= 10_000 { Severity::Medium } else { Severity::Low };
        f.confidence = Confidence::High; f.impact.score=base_score(f.severity);f.impact.label=format!("{} steps to bound",format::count(remaining as f64));
        f.metrics=vec![Metric::text("Recorded value",&seq.current),Metric::text("Minimum",&seq.minimum),Metric::text("Maximum",&seq.maximum),Metric::new("Steps after recorded allocation",remaining as f64,Unit::Count)];
        if let Some(table) = &seq.table {
            f.metrics.push(Metric::text("Table", table));
        }
        f.fix=Some(Fix { summary:"Plan a migration to a larger numeric range, including referencing columns, constraints, indexes and application types. Validate cache/increment behavior and rollout locking first. Do not reseed, restart or enable cycling without proving values cannot collide.".into(),sql:None,cost:Some("Changing column types and dependent keys may rewrite data and require locks; plan and test the migration.".into()) });
        f.verify=vec!["Check the new type and allocation bounds, confirm foreign keys and application values remain compatible, and verify inserts or sequence allocations under representative load.".into()];Some(f)
    }).collect();
    if s.server.engine != Engine::Postgres {
        out.extend(super::table_bloat::expiring_range_partitions(s));
    }
    out.extend(int32_primary_keys_near_limit(s));
    out
}

/// Large tables (`>= 100M` rows) whose single-column primary key is a 32-bit signed integer (`int`, `integer`, `int4`,
/// `serial`), when not already covered by an explicit `operations.sequences` entry.
fn int32_primary_keys_near_limit(s: &Snapshot) -> Vec<Finding> {
    if s.server.engine == Engine::Sqlite || crate::evidence::index_catalog(s).is_some() || s.columns.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    for t in &s.tables {
        if t.partition_of.is_some() {
            continue;
        }
        let Some(rows) =
            t.rows.or_else(|| t.storage.as_ref().and_then(|st| st.planner_rows)).filter(|r| *r >= 100_000_000.0)
        else {
            continue;
        };
        let qual = t.qualified();
        let covered_by_sequence = s.operations.sequences.iter().any(|seq| {
            seq.table.as_deref().is_some_and(|tbl| {
                tbl.eq_ignore_ascii_case(&qual) || tbl.rsplit('.').next().unwrap_or(tbl).eq_ignore_ascii_case(&t.name)
            })
        });
        if covered_by_sequence {
            continue;
        }
        let Some(pk) = s.indexes_on(&t.schema, &t.name).find(|ix| ix.is_primary_key && ix.columns.len() == 1) else {
            continue;
        };
        let pk_col_name = &pk.columns[0];
        let Some(col) = s.columns.iter().find(|c| {
            c.schema.eq_ignore_ascii_case(&t.schema)
                && c.table.eq_ignore_ascii_case(&t.name)
                && c.name.eq_ignore_ascii_case(pk_col_name)
                && is_signed_int32(&c.data_type)
        }) else {
            continue;
        };
        let id_key = format!("int32-pk:{qual}.{}", col.name);
        let mut f = finding("sequence-capacity", &id_key, Category::Maintenance, subject(SubjectKind::Table, &qual));
        f.severity = if rows >= 500_000_000.0 { Severity::High } else { Severity::Medium };
        f.confidence = Confidence::High;
        f.impact.score = base_score(f.severity);
        f.impact.label = format!("{} rows on 32-bit PK", format::count(rows));
        f.title =
            format!("{}.{} is a 32-bit integer primary key on a {}-row table", t.name, col.name, format::count(rows));
        f.summary = format!(
            "Primary key {qual}.{} is declared as `{}` (signed 32-bit maximum 2,147,483,647) and {qual} already holds \
             {} live rows. Once the sequence or auto-increment counter reaches 2,147,483,647 — which happens earlier \
             than the live row count when rows have been deleted or sequence caches skipped values — every new INSERT \
             into {qual} fails with an integer overflow error.",
            col.name,
            col.data_type,
            format::count(rows)
        );
        f.metrics = vec![
            Metric::new("Rows in table", rows, Unit::Rows),
            Metric::text("Primary key column", format!("{}.{} ({})", t.name, col.name, col.data_type)),
            Metric::text("Signed 32-bit limit", "2,147,483,647"),
        ];
        f.fix = Some(Fix {
            summary: format!(
                "Plan a multi-step migration of {qual}.{} (and any foreign-key columns referencing it) to 64-bit \
                 `bigint` before the 32-bit range is exhausted.",
                col.name
            ),
            sql: None,
            cost: Some("Widening a primary key column to bigint rewrites the table or requires a dual-column backfill and swap.".into()),
        });
        f.verify = vec![format!(
            "Re-scan after migrating {qual}.{} to `bigint`: its type is `bigint` and this finding is gone.",
            col.name
        )];
        out.push(f);
    }
    out
}

fn is_signed_int32(data_type: &str) -> bool {
    let lower = data_type.trim().to_ascii_lowercase();
    if lower.contains("unsigned") {
        return false;
    }
    let base = lower.split('(').next().unwrap_or(&lower).trim();
    matches!(base, "int" | "integer" | "int4" | "serial" | "serial4")
}
