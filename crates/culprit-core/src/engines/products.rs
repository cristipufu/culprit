use serde::{Deserialize, Serialize};
use ts_rs::TS;

/// The database product a provider identified. Product branding does not select an execution provider.
#[derive(Serialize, Deserialize, TS, Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "lowercase")]
#[ts(export)]
pub enum DatabaseProduct {
    Mssql,
    Postgres,
    Timescale,
    Yugabyte,
    Mysql,
    Mariadb,
    Sqlite,
}

/// Product identity owned by an engine specification, without per-query allocation.
#[derive(Clone, Copy, Debug)]
pub struct ProductSpec {
    pub id: DatabaseProduct,
    pub label: &'static str,
    /// The native plan formats a person can open without a connection.
    pub plan_format: &'static str,
    /// `None` identifies the provider's default product; named variants match their recorded product prefix.
    pub prefix: Option<&'static str>,
}

impl ProductSpec {
    pub const fn primary(id: DatabaseProduct, label: &'static str, plan_format: &'static str) -> Self {
        Self { id, label, plan_format, prefix: None }
    }

    pub const fn variant(
        id: DatabaseProduct,
        label: &'static str,
        prefix: &'static str,
        plan_format: &'static str,
    ) -> Self {
        Self { id, label, plan_format, prefix: Some(prefix) }
    }

    pub fn facts(self) -> ProductFacts {
        ProductFacts {
            id: self.id,
            label: self.label.into(),
            plan_format: self.plan_format.into(),
            prefix: self.prefix.map(str::to_string),
        }
    }
}

/// The same product identities and recognition rules consumed by the UI.
#[derive(Serialize, Deserialize, TS, Clone, Debug)]
#[ts(export)]
pub struct ProductFacts {
    pub id: DatabaseProduct,
    pub label: String,
    pub plan_format: String,
    pub prefix: Option<String>,
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;
    use crate::{Engine, ServerInfo};

    #[test]
    fn every_provider_declares_one_default_and_distinct_products() {
        let mut ids = BTreeSet::new();
        for engine in Engine::ALL {
            let products = engine.spec().products();
            assert!(!products.is_empty(), "{engine:?}");
            assert_eq!(products.iter().filter(|p| p.prefix.is_none()).count(), 1, "{engine:?}");
            assert!(products[0].prefix.is_none(), "{engine:?}");
            assert_eq!(products[0].label, engine.label());
            for product in products {
                assert!(ids.insert(product.id), "duplicate product {:?}", product.id);
                assert!(!product.label.is_empty());
                assert!(!product.plan_format.trim().is_empty(), "{:?} needs a native plan description", product.id);
            }
        }
    }

    #[test]
    fn product_identity_does_not_change_execution_family() {
        for (engine, product, expected, label) in [
            (Engine::Mssql, "SQL Server 2025", DatabaseProduct::Mssql, "SQL Server"),
            (Engine::Postgres, "PostgreSQL 18.6", DatabaseProduct::Postgres, "PostgreSQL"),
            (Engine::Postgres, "TimescaleDB 2.31.0 (PostgreSQL 17.11)", DatabaseProduct::Timescale, "TimescaleDB"),
            (Engine::Yugabyte, "YugabyteDB 2026.1", DatabaseProduct::Yugabyte, "YugabyteDB"),
            (Engine::Mysql, "MySQL 8.4", DatabaseProduct::Mysql, "MySQL"),
            (Engine::Mysql, "MariaDB 11.8.9", DatabaseProduct::Mariadb, "MariaDB"),
            (Engine::Sqlite, "SQLite", DatabaseProduct::Sqlite, "SQLite"),
            (Engine::Postgres, "MariaDB 11.8.9", DatabaseProduct::Postgres, "PostgreSQL"),
            (Engine::Mysql, "TimescaleDB 2.31", DatabaseProduct::Mysql, "MySQL"),
        ] {
            let server = ServerInfo {
                engine,
                product: product.into(),
                version: String::new(),
                edition: None,
                cloud: None,
                platform: None,
                database: "test".into(),
                database_size_bytes: None,
                started_at: None,
                read_only_session: true,
                capabilities: vec![],
                missing: vec![],
            };
            assert_eq!(server.database_product(), Some(expected), "{product}");
            assert_eq!(server.name(), label, "{product}");
            assert_eq!(server.is_mariadb(), expected == DatabaseProduct::Mariadb);
            assert_eq!(server.engine, engine);
        }
    }

    #[test]
    fn exported_facts_retain_the_core_product_rules() {
        for engine in Engine::ALL {
            let facts = engine.facts();
            let json = serde_json::to_string(&facts).unwrap();
            let decoded: super::super::EngineFacts = serde_json::from_str(&json).unwrap();
            for (spec, fact) in engine.spec().products().iter().zip(&decoded.products) {
                assert_eq!(spec.id, fact.id);
                assert_eq!(spec.label, fact.label);
                assert_eq!(spec.plan_format, fact.plan_format);
                assert_eq!(spec.prefix, fact.prefix.as_deref());
            }
        }
    }
}
