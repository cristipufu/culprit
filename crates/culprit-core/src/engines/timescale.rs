//! DDL for TimescaleDB-managed hypertables. Ordinary PostgreSQL tables keep the PostgreSQL dialect.

use super::postgres::Postgres;
use crate::dialect::{Dialect, NewIndex, included, keys, unique};

pub(super) struct Timescale {
    pub per_chunk: bool,
}

impl Dialect for Timescale {
    fn quote(&self, ident: &str) -> String {
        Postgres.quote(ident)
    }

    fn create_index(&self, index: &NewIndex) -> String {
        let mut sql = format!(
            "CREATE {}INDEX {}\n    ON {} ({})",
            unique(index),
            self.quote(index.name),
            index.target,
            keys(self, index)
        );
        let include = included(self, index);
        if !include.is_empty() {
            sql.push_str(&format!("\n    INCLUDE ({})", include.join(", ")));
        }
        // CONCURRENTLY is unsupported on hypertables. Per-chunk transactions are supported for non-unique indexes.
        if self.per_chunk && !index.unique {
            sql.push_str("\n    WITH (timescaledb.transaction_per_chunk)");
        }
        if let Some(filter) = index.filter {
            sql.push_str(&format!("\n    WHERE {filter}"));
        }
        sql.push(';');
        sql
    }

    fn drop_index(&self, schema: &str, _table: &str, index: &str) -> String {
        format!("DROP INDEX {};", self.qualified(&[schema, index]))
    }

    fn refresh_statistics(&self, target: &str) -> String {
        Postgres.refresh_statistics(target)
    }

    fn end_session(&self, session: &str) -> String {
        Postgres.end_session(session)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Engine, Online};

    #[test]
    fn managed_indexes_use_per_chunk_builds_while_regular_tables_keep_concurrent_builds() {
        let keys = vec!["device_id".into()];
        let index = NewIndex {
            target: "public.sensor_readings",
            unique: false,
            name: "sensor_device_idx",
            keys: &keys,
            include: &[],
            filter: Some("device_id > 0"),
            online: Online::Unknown,
        };
        let engine = Engine::Postgres.spec();
        let sql = engine.table_dialect(Some("hypertable")).create_index(&index);
        assert!(!sql.contains("CONCURRENTLY"));
        assert!(sql.contains("WITH (timescaledb.transaction_per_chunk)\n    WHERE device_id > 0"));
        let unique = engine.table_dialect(Some("hypertable")).create_index(&NewIndex { unique: true, ..index });
        assert!(!unique.contains("transaction_per_chunk") && unique.starts_with("CREATE UNIQUE INDEX"));
        assert!(engine.table_dialect(None).create_index(&index).contains("CONCURRENTLY"));
        let unknown = engine.table_dialect(Some("timescale-unresolved")).create_index(&index);
        assert!(!unknown.contains("CONCURRENTLY") && !unknown.contains("transaction_per_chunk"));
        assert!(
            !engine
                .table_dialect(Some("hypertable"))
                .drop_index("public", "sensor_readings", "sensor_device_idx")
                .contains("CONCURRENTLY")
        );
    }
}
