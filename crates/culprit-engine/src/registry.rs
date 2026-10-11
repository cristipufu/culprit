//! Maps an engine to its provider. New engines plug in here behind a cargo feature.

use culprit_core::{ConnectionSpec, Engine, Error, ErrorCode, Provider, Result};

pub fn provider(spec: &ConnectionSpec) -> Result<Box<dyn Provider>> {
    let engine = spec.engine.ok_or_else(|| {
        Error::invalid("Can't tell which database engine this is")
            .with_hint("Use a postgres:// or mssql:// URL, or pass --engine")
    })?;
    match engine {
        #[cfg(feature = "mssql")]
        Engine::Mssql => Ok(Box::new(culprit_mssql::MssqlProvider::new(spec)?)),
        #[cfg(feature = "postgres")]
        Engine::Postgres => Ok(Box::new(culprit_postgres::PostgresProvider::new(spec)?)),
        #[cfg(feature = "mysql")]
        Engine::Mysql => Ok(Box::new(culprit_mysql::MysqlProvider::new(spec)?)),
        #[allow(unreachable_patterns)]
        other => Err(Error::new(ErrorCode::Unsupported, format!("This build of Culprit has no {other} support"))),
    }
}
