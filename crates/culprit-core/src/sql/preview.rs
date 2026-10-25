use serde::{Deserialize, Serialize};
use ts_rs::TS;

use super::lexer::{Token, readings};
use crate::{Engine, Error, Result, ResultSet, SqlSyntax};

#[derive(Serialize, Deserialize, TS, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
#[ts(export)]
pub enum SortDirection {
    Asc,
    Desc,
}

#[derive(Serialize, Deserialize, TS, Clone, Debug, PartialEq, Eq)]
#[ts(export)]
pub struct ColumnSort {
    pub column: String,
    pub direction: SortDirection,
}

/// An explicitly requested, bounded table read. The filter is a WHERE expression, never a statement or clause.
#[derive(Serialize, Deserialize, TS, Clone, Debug)]
#[ts(export)]
pub struct TablePreviewOptions {
    pub schema: String,
    pub table: String,
    pub filter: Option<String>,
    pub order: Option<ColumnSort>,
    pub limit: usize,
}

#[derive(Serialize, Deserialize, TS, Clone, Debug)]
#[ts(export)]
pub struct TablePreview {
    pub sql: String,
    pub result: ResultSet,
}

/// Keep a predicate inside its parentheses under every supported quoting mode. The full query still passes the
/// normal read-only classifier and server checks before execution; the database validates expression semantics.
fn predicate(text: &str, engine: Engine) -> Result<&str> {
    let text = text.trim();
    if text.len() > 16 * 1024 {
        return Err(Error::invalid("The quick filter is too long").with_hint("Use the SQL editor for larger queries"));
    }
    for tokens in readings(text, engine).map_err(Error::invalid)? {
        let mut depth = 0;
        for token in tokens {
            let invalid = match token {
                Token::Punct('(') => {
                    depth += 1;
                    false
                }
                Token::Punct(')') => {
                    depth -= 1;
                    depth < 0
                }
                Token::Semicolon(_) => true,
                Token::Word(word) if depth == 0 => [
                    "SELECT",
                    "WITH",
                    "FROM",
                    "WHERE",
                    "UNION",
                    "INTERSECT",
                    "EXCEPT",
                    "ORDER",
                    "GROUP",
                    "HAVING",
                    "LIMIT",
                    "OFFSET",
                    "FETCH",
                    "INTO",
                    "FOR",
                    "WINDOW",
                    "OPTION",
                    "RETURNING",
                ]
                .iter()
                .any(|keyword| word.eq_ignore_ascii_case(keyword)),
                _ => false,
            };
            if invalid {
                return Err(filter_error());
            }
        }
        if depth != 0 {
            return Err(filter_error());
        }
    }
    Ok(text)
}

fn filter_error() -> Error {
    Error::invalid("Enter a WHERE expression, without query clauses or statements")
        .with_hint("For example: status = 'open' AND amount > 100. Use the column headers to sort.")
}

pub fn table_preview_sql(engine: Engine, options: &TablePreviewOptions) -> Result<String> {
    if options.schema.is_empty() || options.table.is_empty() {
        return Err(Error::invalid("Choose a schema and table to preview"));
    }
    let limit = options.limit.clamp(1, crate::limits::MAX_RESULT_ROWS);
    // Always quote catalog names, including lowercase reserved words. The advice dialect may omit simple quotes.
    let quote = |name: &str| match engine.sql_syntax() {
        SqlSyntax::Mssql => format!("[{}]", name.replace(']', "]]")),
        SqlSyntax::Mysql => format!("`{}`", name.replace('`', "``")),
        SqlSyntax::Postgres | SqlSyntax::Sqlite => format!("\"{}\"", name.replace('"', "\"\"")),
    };
    let name = format!("{}.{}", quote(&options.schema), quote(&options.table));
    let mut sql = match engine.sql_syntax() {
        SqlSyntax::Mssql => format!("SELECT TOP ({limit}) * FROM {name}"),
        _ => format!("SELECT * FROM {name}"),
    };
    if let Some(filter) = options.filter.as_deref().filter(|filter| !filter.trim().is_empty()) {
        let filter = predicate(filter, engine)?;
        // A trailing line comment cannot swallow the closing parenthesis or result limit.
        sql.push_str(&format!(" WHERE (\n{filter}\n)"));
    }
    if let Some(order) = &options.order {
        let direction = match order.direction {
            SortDirection::Asc => "ASC",
            SortDirection::Desc => "DESC",
        };
        sql.push_str(&format!(" ORDER BY {} {direction}", quote(&order.column)));
    }
    if engine.sql_syntax() != SqlSyntax::Mssql {
        sql.push_str(&format!(" LIMIT {limit}"));
    }
    Ok(sql)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options(filter: &str) -> TablePreviewOptions {
        TablePreviewOptions {
            schema: "app".into(),
            table: "orders".into(),
            filter: Some(filter.into()),
            order: None,
            limit: 100,
        }
    }

    #[test]
    fn every_engine_keeps_filters_inside_a_bounded_read() {
        for engine in Engine::ALL {
            for filter in [
                "status = 'open' AND amount > 100",
                "id IN (1, 2, 3)",
                "note = '; UNION SELECT'",
                "id > 1 -- a comment",
                "(id > 1 OR id IS NULL)",
            ] {
                let sql = table_preview_sql(engine, &options(filter)).unwrap();
                assert!(crate::sql::reads_only(&sql, engine), "{engine:?}: {sql}");
                assert!(crate::sql::single_statement(&sql, engine).is_ok());
                assert!(sql.contains("TOP (100)") || sql.ends_with("LIMIT 100"));
            }
        }
    }

    #[test]
    fn predicates_cannot_escape_into_statements_or_remove_the_limit() {
        for engine in Engine::ALL {
            for filter in [
                "1=1); DELETE FROM orders; --",
                "1=1) UNION ALL SELECT * FROM orders WHERE (1=1",
                "id = 1 LIMIT 1000000000",
                "id = 1 ORDER BY id",
                "(id = 1",
                "id = 1 /*",
            ] {
                assert!(table_preview_sql(engine, &options(filter)).is_err(), "{engine:?} accepted {filter}");
            }
        }
    }

    #[test]
    fn identifiers_are_quoted_and_limits_are_bounded() {
        for engine in Engine::ALL {
            let mut options = options("");
            options.table = "odd\"`]table".into();
            options.order = Some(ColumnSort { column: "odd\"`]column".into(), direction: SortDirection::Desc });
            options.limit = usize::MAX;
            let sql = table_preview_sql(engine, &options).unwrap();
            assert!(sql.contains(&engine.dialect().quote(&options.table)));
            assert!(sql.contains(&format!("ORDER BY {} DESC", engine.dialect().quote("odd\"`]column"))));
            assert!(!sql.contains(&usize::MAX.to_string()));
        }
    }
}
