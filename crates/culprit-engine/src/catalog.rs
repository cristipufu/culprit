//! The live catalog for editor completion: databases, tables and views with their columns, read from the standard
//! information schema through the read-only run path. Cheap, and it works on connections that were never scanned.

use culprit_core::{Engine, ResultSet};
use serde::{Deserialize, Serialize};
use ts_rs::TS;

#[derive(Serialize, Deserialize, TS, Clone, Debug, Default, PartialEq)]
#[ts(export)]
pub struct Catalog {
    /// Other databases on the server (schemas, for MySQL), for `database.` completion.
    pub databases: Vec<String>,
    pub tables: Vec<CatalogTable>,
    /// The catalog stopped at the row cap; completion covers the first tables only.
    pub truncated: bool,
}

#[derive(Serialize, Deserialize, TS, Clone, Debug, Default, PartialEq)]
#[ts(export)]
pub struct CatalogTable {
    pub schema: String,
    pub name: String,
    pub view: bool,
    pub columns: Vec<CatalogColumn>,
}

#[derive(Serialize, Deserialize, TS, Clone, Debug, Default, PartialEq)]
#[ts(export)]
pub struct CatalogColumn {
    pub name: String,
    pub data_type: String,
}

/// Columns of every user table and view, in table order.
pub(crate) fn columns_sql(engine: Engine) -> &'static str {
    match engine {
        Engine::Mssql => {
            "SELECT c.TABLE_SCHEMA, c.TABLE_NAME, t.TABLE_TYPE, c.COLUMN_NAME, c.DATA_TYPE
               FROM INFORMATION_SCHEMA.COLUMNS c
               JOIN INFORMATION_SCHEMA.TABLES t ON t.TABLE_SCHEMA = c.TABLE_SCHEMA AND t.TABLE_NAME = c.TABLE_NAME
              ORDER BY c.TABLE_SCHEMA, c.TABLE_NAME, c.ORDINAL_POSITION"
        }
        Engine::Postgres => {
            "SELECT c.table_schema, c.table_name, t.table_type, c.column_name, c.data_type
               FROM information_schema.columns c
               JOIN information_schema.tables t ON t.table_schema = c.table_schema AND t.table_name = c.table_name
              WHERE c.table_schema NOT IN ('pg_catalog', 'information_schema') AND c.table_schema NOT LIKE 'pg_toast%'
              ORDER BY c.table_schema, c.table_name, c.ordinal_position"
        }
        Engine::Mysql => {
            "SELECT c.TABLE_SCHEMA, c.TABLE_NAME, t.TABLE_TYPE, c.COLUMN_NAME, c.COLUMN_TYPE
               FROM information_schema.COLUMNS c
               JOIN information_schema.TABLES t ON t.TABLE_SCHEMA = c.TABLE_SCHEMA AND t.TABLE_NAME = c.TABLE_NAME
              WHERE c.TABLE_SCHEMA = DATABASE()
              ORDER BY c.TABLE_SCHEMA, c.TABLE_NAME, c.ORDINAL_POSITION"
        }
    }
}

pub(crate) fn databases_sql(engine: Engine) -> &'static str {
    match engine {
        Engine::Mssql => "SELECT name FROM sys.databases WHERE state = 0 ORDER BY name",
        Engine::Postgres => "SELECT datname FROM pg_database WHERE NOT datistemplate ORDER BY datname",
        Engine::Mysql => {
            "SELECT SCHEMA_NAME FROM information_schema.SCHEMATA
              WHERE SCHEMA_NAME NOT IN ('mysql', 'sys', 'performance_schema', 'information_schema') ORDER BY SCHEMA_NAME"
        }
    }
}

fn text(value: Option<&serde_json::Value>) -> String {
    match value {
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(serde_json::Value::Null) | None => String::new(),
        Some(other) => other.to_string(),
    }
}

/// Fold column rows (schema, table, type, column, data type) into tables.
pub(crate) fn fold(columns: ResultSet, databases: Option<ResultSet>) -> Catalog {
    let mut tables: Vec<CatalogTable> = Vec::new();
    for row in &columns.rows {
        let (schema, name) = (text(row.first()), text(row.get(1)));
        let column = CatalogColumn { name: text(row.get(3)), data_type: text(row.get(4)) };
        match tables.last_mut() {
            Some(last) if last.schema == schema && last.name == name => last.columns.push(column),
            _ => tables.push(CatalogTable {
                schema,
                name,
                view: text(row.get(2)).to_ascii_uppercase().contains("VIEW"),
                columns: vec![column],
            }),
        }
    }
    let databases = databases.map(|d| d.rows.iter().map(|r| text(r.first())).collect()).unwrap_or_default();
    Catalog { databases, tables, truncated: columns.truncated }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn rows_fold_into_tables() {
        let columns = ResultSet {
            columns: vec![],
            rows: vec![
                vec![json!("shop"), json!("orders"), json!("BASE TABLE"), json!("id"), json!("int")],
                vec![json!("shop"), json!("orders"), json!("BASE TABLE"), json!("note"), json!("text")],
                vec![json!("shop"), json!("open_orders"), json!("VIEW"), json!("id"), json!("int")],
            ],
            truncated: false,
            elapsed_ms: 0.0,
            messages: vec![],
        };
        let catalog = fold(columns, None);
        assert_eq!(catalog.tables.len(), 2);
        assert_eq!(catalog.tables[0].columns.len(), 2);
        assert!(catalog.tables[1].view);
    }
}
