//! How each engine spells the SQL Culprit writes for a person to copy: a quoted name, a qualified one, an index built
//! or dropped without blocking writes where the engine can. Checks and plan insights ask the engine's dialect
//! (`Engine::dialect`) instead of matching the engine, so a new engine is one more implementation here. Culprit never
//! runs this SQL.

use crate::model::Engine;

/// Whether an index can be built while the table stays writable, as far as the caller knows. SQL Server needs an
/// edition that has it; the other engines always can.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Online {
    Yes,
    No,
    /// The edition isn't known (a plan seen without its server): say how, don't assume.
    Unknown,
}

/// An index to suggest. `target` is the table, already qualified with [`Dialect::qualified`]; `name` is used as given;
/// a key ending in " DESC" is descending, as `IndexStat::columns` writes it.
pub struct NewIndex<'a> {
    pub target: &'a str,
    pub unique: bool,
    pub name: &'a str,
    pub keys: &'a [String],
    pub include: &'a [String],
    /// The rows it keeps (a filtered or partial index), written as the engine keeps filters.
    pub filter: Option<&'a str>,
    pub online: Online,
}

pub trait Dialect: Send + Sync {
    /// A name quoted for a script, with its closing delimiter doubled so a name can never end the quoting early.
    fn quote(&self, ident: &str) -> String;

    /// Names joined with dots, each quoted: `[dbo].[orders]`, `public.orders`.
    fn qualified(&self, parts: &[&str]) -> String {
        parts.iter().map(|p| self.quote(p)).collect::<Vec<_>>().join(".")
    }

    /// `CREATE INDEX`, built without blocking writes where the engine (and, for SQL Server, the edition) can.
    fn create_index(&self, index: &NewIndex) -> String;

    /// `DROP INDEX`, without blocking where the engine can.
    fn drop_index(&self, schema: &str, table: &str, index: &str) -> String;

    /// Recompute a table's optimizer statistics, with the engine's defaults (`target` already qualified).
    fn refresh_statistics(&self, target: &str) -> String;

    /// End another session (rolling its transaction back), by the id the engine shows for it.
    fn end_session(&self, session: &str) -> String;
}

impl Engine {
    /// How this engine spells the SQL Culprit suggests.
    pub fn dialect(self) -> &'static dyn Dialect {
        match self {
            Engine::Mssql => &Mssql,
            Engine::Postgres => &Postgres,
            Engine::Mysql => &Mysql,
            Engine::Sqlite => &Sqlite,
        }
    }
}

/// The included columns that aren't keys already, quoted.
fn included(d: &dyn Dialect, index: &NewIndex) -> Vec<String> {
    index.include.iter().filter(|c| !index.keys.iter().any(|k| k.eq_ignore_ascii_case(c))).map(|c| d.quote(c)).collect()
}

fn keys(d: &dyn Dialect, index: &NewIndex) -> String {
    index
        .keys
        .iter()
        .map(|k| {
            let name = crate::model::strip_direction(k);
            if name.len() == k.len() { d.quote(name) } else { format!("{} DESC", d.quote(name)) }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn unique(index: &NewIndex) -> &'static str {
    if index.unique { "UNIQUE " } else { "" }
}

struct Mssql;
struct Postgres;
struct Mysql;
struct Sqlite;

impl Dialect for Mssql {
    fn quote(&self, ident: &str) -> String {
        format!("[{}]", ident.replace(']', "]]"))
    }

    fn create_index(&self, index: &NewIndex) -> String {
        let mut sql = format!(
            "CREATE {}NONCLUSTERED INDEX {}\n    ON {} ({})",
            unique(index),
            self.quote(index.name),
            index.target,
            keys(self, index)
        );
        let include = included(self, index);
        if !include.is_empty() {
            sql.push_str(&format!("\n    INCLUDE ({})", include.join(", ")));
        }
        if let Some(filter) = index.filter {
            sql.push_str(&format!("\n    WHERE {filter}"));
        }
        match index.online {
            Online::Yes => sql.push_str("\n    WITH (ONLINE = ON);"),
            Online::No => sql.push(';'),
            Online::Unknown => sql.push_str(
                ";\n-- Add WITH (ONLINE = ON) on Enterprise or Azure SQL to keep the table available while it builds.",
            ),
        }
        sql
    }

    fn drop_index(&self, schema: &str, table: &str, index: &str) -> String {
        format!("DROP INDEX {} ON {};", self.quote(index), self.qualified(&[schema, table]))
    }

    fn refresh_statistics(&self, target: &str) -> String {
        format!("UPDATE STATISTICS {target};")
    }

    fn end_session(&self, session: &str) -> String {
        format!("KILL {session};")
    }
}

impl Dialect for Postgres {
    fn quote(&self, ident: &str) -> String {
        let simple = ident.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
            && !ident.starts_with(|c: char| c.is_ascii_digit());
        if simple { ident.to_string() } else { format!("\"{}\"", ident.replace('"', "\"\"")) }
    }

    fn create_index(&self, index: &NewIndex) -> String {
        let mut sql = format!(
            "CREATE {}INDEX CONCURRENTLY {}\n    ON {} ({})",
            unique(index),
            self.quote(index.name),
            index.target,
            keys(self, index)
        );
        let include = included(self, index);
        if !include.is_empty() {
            sql.push_str(&format!("\n    INCLUDE ({})", include.join(", ")));
        }
        if let Some(filter) = index.filter {
            sql.push_str(&format!("\n    WHERE {filter}"));
        }
        sql.push(';');
        sql
    }

    fn drop_index(&self, schema: &str, _table: &str, index: &str) -> String {
        format!("DROP INDEX CONCURRENTLY {};", self.qualified(&[schema, index]))
    }

    fn refresh_statistics(&self, target: &str) -> String {
        format!("ANALYZE {target};")
    }

    fn end_session(&self, session: &str) -> String {
        format!("SELECT pg_terminate_backend({session});")
    }
}

impl Dialect for Mysql {
    fn quote(&self, ident: &str) -> String {
        format!("`{}`", ident.replace('`', "``"))
    }

    fn create_index(&self, index: &NewIndex) -> String {
        // InnoDB has no INCLUDE: secondary indexes already carry the primary key, so extra columns would have to become
        // keys. No partial indexes either.
        format!(
            "CREATE {}INDEX {}\n    ON {} ({})\n    ALGORITHM=INPLACE LOCK=NONE;",
            unique(index),
            self.quote(index.name),
            index.target,
            keys(self, index)
        )
    }

    fn drop_index(&self, schema: &str, table: &str, index: &str) -> String {
        // MariaDB's DROP INDEX takes no ALGORITHM/LOCK options; ALTER TABLE takes them (comma-separated) on both.
        format!(
            "ALTER TABLE {} DROP INDEX {}, ALGORITHM=INPLACE, LOCK=NONE;",
            self.qualified(&[schema, table]),
            self.quote(index)
        )
    }

    fn refresh_statistics(&self, target: &str) -> String {
        format!("ANALYZE TABLE {target};")
    }

    fn end_session(&self, session: &str) -> String {
        format!("KILL {session};")
    }
}

impl Dialect for Sqlite {
    fn quote(&self, ident: &str) -> String {
        format!("\"{}\"", ident.replace('"', "\"\""))
    }

    fn create_index(&self, index: &NewIndex) -> String {
        // No INCLUDE and no online build: SQLite builds an index under the database's write lock, so writers wait.
        let mut sql = format!(
            "CREATE {}INDEX IF NOT EXISTS {}\n    ON {} ({})",
            unique(index),
            self.quote(index.name),
            index.target,
            keys(self, index)
        );
        if let Some(filter) = index.filter {
            sql.push_str(&format!("\n    WHERE {filter}"));
        }
        sql.push_str(";\n-- SQLite builds an index under the database's write lock: writers wait until it finishes.");
        sql
    }

    fn drop_index(&self, schema: &str, _table: &str, index: &str) -> String {
        format!("DROP INDEX IF EXISTS {};", self.qualified(&[schema, index]))
    }

    fn refresh_statistics(&self, target: &str) -> String {
        format!("ANALYZE {target};")
    }

    fn end_session(&self, _session: &str) -> String {
        "-- SQLite has no sessions to end: the application that holds the write lock has to finish its transaction."
            .into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_name_can_never_end_its_quoting() {
        assert_eq!(Engine::Mssql.dialect().quote("a]b"), "[a]]b]");
        assert_eq!(Engine::Mysql.dialect().quote("a`b"), "`a``b`");
        assert_eq!(Engine::Postgres.dialect().quote("Orders"), "\"Orders\"");
        assert_eq!(Engine::Postgres.dialect().quote("orders"), "orders");
        assert_eq!(Engine::Postgres.dialect().quote("1st"), "\"1st\"");
    }

    #[test]
    fn indexes_are_built_without_blocking_where_the_engine_can() {
        let keys = vec!["customer_id".to_string()];
        let include = vec!["total".to_string(), "customer_id".to_string()];
        let index = |online| NewIndex {
            target: "x",
            unique: false,
            name: "ix_t_customer_id",
            keys: &keys,
            include: &include,
            filter: None,
            online,
        };
        let ms = Engine::Mssql.dialect();
        assert!(ms.create_index(&index(Online::Yes)).ends_with("INCLUDE ([total])\n    WITH (ONLINE = ON);"));
        assert!(ms.create_index(&index(Online::Unknown)).contains("-- Add WITH (ONLINE = ON)"));
        assert!(Engine::Postgres.dialect().create_index(&index(Online::No)).starts_with("CREATE INDEX CONCURRENTLY"));
        assert!(Engine::Mysql.dialect().create_index(&index(Online::No)).ends_with("ALGORITHM=INPLACE LOCK=NONE;"));
        let filtered = NewIndex { filter: Some("([is_deleted]=(0))"), ..index(Online::Yes) };
        assert_eq!(Engine::Postgres.dialect().end_session("42"), "SELECT pg_terminate_backend(42);");
        assert_eq!(Engine::Mysql.dialect().refresh_statistics("`shop`.`orders`"), "ANALYZE TABLE `shop`.`orders`;");
        assert!(ms.create_index(&filtered).contains("\n    WHERE ([is_deleted]=(0))\n    WITH (ONLINE = ON);"));
    }
}
