//! The live catalog for editor completion: databases, tables and views with their columns, read from the standard
//! information schema through the read-only run path. Cheap, and it works on connections that were never scanned.

use culprit_core::{Engine, Provider, Result, ResultSet};
use serde::{Deserialize, Serialize};
use ts_rs::TS;

#[derive(Serialize, Deserialize, TS, Clone, Debug, Default, PartialEq)]
#[ts(export)]
pub struct Catalog {
    /// Other databases on the server (schemas, for MySQL), for `database.` completion.
    pub databases: Vec<String>,
    pub tables: Vec<CatalogTable>,
    /// The catalog reached its collection limit; completion may omit tables or columns.
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
        Engine::Postgres | Engine::Yugabyte => {
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
        Engine::Sqlite => {
            "/* culprit */ SELECT 'main', m.name, CASE m.type WHEN 'view' THEN 'VIEW' ELSE 'BASE TABLE' END, p.name, p.type
               FROM main.sqlite_schema m
               JOIN pragma_table_list t ON t.schema = 'main' AND t.name = m.name AND t.type <> 'shadow'
               JOIN pragma_table_xinfo(CASE WHEN t.type = 'virtual' THEN NULL ELSE m.name END) p
              WHERE m.type IN ('table', 'view') AND m.name NOT LIKE 'sqlite\\_%' ESCAPE '\\' AND p.hidden <> 1
              ORDER BY m.name, p.cid"
        }
    }
}

/// Virtual tables can need modules linked only into the application. Read their columns separately so one missing
/// module cannot lose completion for the rest of the file. Both the result size and the time spent listing them are
/// bounded; every call goes through the provider's read-only, timed path.
pub(crate) async fn columns(provider: &dyn Provider, engine: Engine) -> Result<ResultSet> {
    const LIMIT: usize = 50_000;
    let mut columns = provider.run(columns_sql(engine), LIMIT).await?;
    if engine != Engine::Sqlite {
        return Ok(columns);
    }
    let started = std::time::Instant::now();
    let tables = provider
        .run(
            "/* culprit */ SELECT name FROM pragma_table_list WHERE schema = 'main' AND type = 'virtual' ORDER BY name",
            LIMIT,
        )
        .await?;
    columns.truncated |= tables.truncated;
    for row in tables.rows {
        if columns.rows.len() >= LIMIT || started.elapsed() >= std::time::Duration::from_secs(2) {
            columns.truncated = true;
            break;
        }
        let Some(name) = row.first().and_then(serde_json::Value::as_str) else { continue };
        let quoted = format!("'{}'", name.replace('\'', "''"));
        let sql = format!(
            "/* culprit */ SELECT 'main', {quoted}, 'BASE TABLE', name, type FROM pragma_table_xinfo({quoted}) WHERE hidden <> 1 ORDER BY cid"
        );
        if let Ok(part) = provider.run(&sql, LIMIT - columns.rows.len()).await {
            columns.rows.extend(part.rows);
            columns.truncated |= part.truncated;
        }
    }
    // Folding expects all of a table's columns together, as the single catalog query returns on server engines.
    columns.rows.sort_by(|a, b| a.get(1).and_then(|v| v.as_str()).cmp(&b.get(1).and_then(|v| v.as_str())));
    Ok(columns)
}

pub(crate) fn databases_sql(engine: Engine) -> &'static str {
    match engine {
        Engine::Mssql => "SELECT name FROM sys.databases WHERE state = 0 ORDER BY name",
        Engine::Postgres | Engine::Yugabyte => "SELECT datname FROM pg_database WHERE NOT datistemplate ORDER BY datname",
        Engine::Sqlite => "SELECT name FROM pragma_database_list ORDER BY seq",
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
