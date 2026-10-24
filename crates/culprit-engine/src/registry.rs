//! Maps an engine to its provider. New engines plug in here behind a cargo feature.

use culprit_core::{ConnectionSpec, Engine, Error, ErrorCode, Provider, Result, Source};

/// Every source a scan reads on `engine`, as its provider declares them (`<provider>/src/sources.rs`); empty for an
/// engine this build has no provider for.
pub fn sources(engine: Engine) -> &'static [Source] {
    match engine {
        #[cfg(feature = "mssql")]
        Engine::Mssql => culprit_mssql::sources::ALL,
        #[cfg(feature = "postgres")]
        Engine::Postgres => culprit_postgres::sources::ALL,
        #[cfg(feature = "yugabyte")]
        Engine::Yugabyte => culprit_yugabyte::sources::ALL,
        #[cfg(feature = "mysql")]
        Engine::Mysql => culprit_mysql::sources::ALL,
        #[cfg(feature = "sqlite")]
        Engine::Sqlite => culprit_sqlite::sources::ALL,
        #[allow(unreachable_patterns)]
        _ => &[],
    }
}

pub fn provider(spec: &ConnectionSpec) -> Result<Box<dyn Provider>> {
    let engine = spec.engine.ok_or_else(|| {
        Error::invalid("Can't tell which database engine this is")
            .with_hint("Use a postgres://, mssql://, mysql:// or sqlite:// URL, or pass --engine")
    })?;
    match engine {
        #[cfg(feature = "mssql")]
        Engine::Mssql => Ok(Box::new(culprit_mssql::MssqlProvider::new(spec)?)),
        #[cfg(feature = "postgres")]
        Engine::Postgres => Ok(Box::new(culprit_postgres::PostgresProvider::new(spec)?)),
        #[cfg(feature = "yugabyte")]
        Engine::Yugabyte => Ok(Box::new(culprit_yugabyte::YugabyteProvider::new(spec)?)),
        #[cfg(feature = "mysql")]
        Engine::Mysql => Ok(Box::new(culprit_mysql::MysqlProvider::new(spec)?)),
        #[cfg(feature = "sqlite")]
        Engine::Sqlite => Ok(Box::new(culprit_sqlite::SqliteProvider::new(spec)?)),
        #[allow(unreachable_patterns)]
        other => Err(Error::new(ErrorCode::Unsupported, format!("This build of Culprit has no {other} support"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Each engine's sources are one list anyone can read (`culprit sources`): every engine this build serves has one,
    // and a source's name, which its notes carry, says which one it was.
    #[test]
    fn every_engine_declares_its_sources_once() {
        for engine in Engine::ALL {
            let list = sources(engine);
            assert!(!list.is_empty(), "{engine:?} declares no sources");
            let mut names: Vec<&str> = list.iter().map(|s| s.name).collect();
            names.sort();
            let before = names.len();
            names.dedup();
            assert_eq!(names.len(), before, "{engine:?} names two sources the same");
        }
    }
}
