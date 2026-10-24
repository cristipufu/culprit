use culprit_core::{Capability, Source, source_id};
pub const TABLES: Source = Source {
    name: source_id::TABLES,
    reads: "YSQL relation catalog and optimizer row estimates",
    delivers: &[Capability::TableStats, Capability::Statistics],
};
pub const INDEXES: Source =
    Source { name: source_id::INDEXES, reads: "YSQL index definitions, LSM keys and backfill state", delivers: &[] };
pub const SIZES: Source =
    Source { name: source_id::SIZES, reads: "DocDB table/index allocation size APIs", delivers: &[] };
pub const STORAGE: Source = Source {
    name: source_id::TABLE_STORAGE,
    reads: "yb_table_properties: tablets, hash keys and colocation",
    delivers: &[],
};
pub const COLUMNS: Source = Source { name: source_id::COLUMNS, reads: "YSQL columns and defaults", delivers: &[] };
pub const FOREIGN_KEYS: Source =
    Source { name: source_id::FOREIGN_KEYS, reads: "YSQL foreign-key catalog", delivers: &[] };
pub const CONSTRAINTS: Source = Source {
    name: source_id::CONSTRAINTS,
    reads: "YSQL CHECK and foreign-key constraints awaiting validation",
    delivers: &[],
};
pub const WORKLOAD: Source = Source {
    name: source_id::WORKLOAD,
    reads: "YSQL pg_stat_statements and native DocDB costs, in node/cluster scope",
    delivers: &[Capability::PgStatStatements],
};
pub const COSTS: Source = Source {
    name: source_id::QUERY_COSTS,
    reads: "native statement metrics with collection settings verified on every contributing node",
    delivers: &[],
};
pub const EPOCHS: Source = Source {
    name: source_id::DOCDB_EPOCHS,
    reads: "statement reset epochs for every contributing YB-TServer",
    delivers: &[],
};
pub const SETTINGS: Source =
    Source { name: source_id::SETTINGS, reads: "YSQL and DocDB supported configuration", delivers: &[] };
pub const INDEX_USAGE: Source = Source {
    name: source_id::DOCDB_INDEX_USAGE,
    reads: "complete cluster-wide index usage and its reset epochs",
    delivers: &[Capability::IndexUsage],
};
pub const ALL: &[Source] = &[
    TABLES,
    INDEXES,
    SIZES,
    STORAGE,
    COLUMNS,
    FOREIGN_KEYS,
    CONSTRAINTS,
    WORKLOAD,
    COSTS,
    EPOCHS,
    SETTINGS,
    INDEX_USAGE,
];
