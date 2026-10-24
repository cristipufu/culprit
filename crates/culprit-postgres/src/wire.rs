//! Shared PostgreSQL wire transport and SQL execution for YSQL-compatible engines. This boundary exposes no
//! PostgreSQL snapshot, storage collector or connection probe: each engine owns its measurements and native plans.

use culprit_core::{ConnectionSpec, Engine, ExplainMode, Plan, Provider, Result, ResultSet};

pub use crate::connect::{COLLECTOR, Connector, EXECUTION, INTERACTIVE, ObservedSession, SessionLimits, map_error};
pub use crate::rows::{Row, iso, query, query_bounded, query_one, quote_ident, tagged};
pub use crate::trust::{elevated as elevated_login, not_its_own as foreign_statement, own_ysql_text};

/// SQL execution with the same authentication, TLS, planner trust restrictions, read-only transactions, rollback,
/// deadlines and streaming result budgets as the PostgreSQL provider. The caller interprets the native plan.
pub struct SqlExecutor {
    inner: crate::PostgresProvider,
}

impl SqlExecutor {
    pub fn new(spec: &ConnectionSpec) -> Result<Self> {
        let mut wire = spec.clone();
        wire.engine = Some(Engine::Postgres);
        Ok(Self { inner: crate::PostgresProvider::new(&wire)? })
    }

    /// YSQL's distributed storage has no PostgreSQL heap pages. Do not enrich it with the heap-based catalog reader;
    /// measured plans request DIST rather than meaningless PostgreSQL buffer counters.
    pub async fn explain_ysql(&self, sql: &str, mode: ExplainMode) -> Result<Plan> {
        self.inner.explain_wire(sql, mode, false, true).await
    }

    pub async fn recorded_text(&self, sql: &str, query_id: Option<&str>) -> Result<String> {
        let client = self.inner.connector.connect(INTERACTIVE).await?;
        if !crate::trust::elevated(&client).await? {
            return Ok(sql.into());
        }
        match query_id {
            Some(id) => crate::trust::own_text(&client, id).await?.ok_or_else(crate::trust::not_its_own),
            None => Err(crate::trust::not_its_own()),
        }
    }

    pub async fn run(&self, sql: &str, limit: usize) -> Result<ResultSet> {
        self.inner.run(sql, limit).await
    }

    pub async fn execute(&self, sql: &str, limit: usize) -> Result<ResultSet> {
        self.inner.execute(sql, limit).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Opt-in against dev/yugabyte's disposable loopback database. No PostgreSQL probe or heap collector is called.
    #[tokio::test]
    #[ignore = "requires dev/yugabyte and CULPRIT_YSQL_TEST_PORT"]
    async fn live_ysql_execution_retains_native_metrics_notices_and_transaction_boundaries() {
        let port = std::env::var("CULPRIT_YSQL_TEST_PORT").expect("set CULPRIT_YSQL_TEST_PORT").parse().unwrap();
        let mut spec = ConnectionSpec {
            host: "127.0.0.1".into(),
            port: Some(port),
            database: Some("culprit_yugabyte".into()),
            user: Some("yugabyte".into()),
            environment: culprit_core::Environment::Local,
            ..Default::default()
        };
        spec.options.insert("sslmode".into(), "disable".into());
        let session = Connector::new(&spec).unwrap().connect_observed(INTERACTIVE).await.unwrap();
        let before = session.notices();
        session
            .client
            .batch_execute(&tagged("DO $$ BEGIN RAISE WARNING 'Culprit partial-source test'; END $$"))
            .await
            .unwrap();
        assert_eq!(session.notices() - before, 1, "server warnings must not disappear behind a successful response");

        let executor = SqlExecutor::new(&spec).unwrap();
        // Application stimulus: intentionally recorded as workload, like dev/yugabyte/seed.sql.
        session
            .client
            .simple_query("SELECT name FROM public.customers WHERE id = 42 /* ysql_native_test */")
            .await
            .unwrap();
        let recorded = query_one(
            &session.client,
            "SELECT queryid::text AS id FROM pg_stat_statements WHERE query LIKE '%ysql_native_test%'
             AND userid = (SELECT oid FROM pg_roles WHERE rolname = current_user) LIMIT 1",
        )
        .await
        .unwrap()
        .unwrap();
        let own = executor
            .recorded_text("SELECT name FROM public.customers WHERE id = $1", recorded.text("id").as_deref())
            .await
            .unwrap();
        assert!(own.contains("ysql_native_test"));
        assert_eq!(
            executor.explain_ysql(&own, ExplainMode::Estimated).await.unwrap().kind,
            culprit_core::PlanKind::Generic
        );
        let rows = executor.run("SELECT n FROM generate_series(1, 20) n", 3).await.unwrap();
        assert_eq!(rows.rows.len(), 3);
        assert!(rows.truncated);
        assert!(
            executor.execute("SELECT 100 / (20 - n) FROM generate_series(1, 20) n", 3).await.is_err(),
            "a late error after the display budget is still an error"
        );
        let old = executor.run("SELECT total FROM public.orders WHERE id = 42", 1).await.unwrap();
        let measured = executor
            .explain_ysql("UPDATE public.orders SET total = total + 1 WHERE id = 42", ExplainMode::Analyze)
            .await
            .unwrap();
        assert_eq!(measured.kind, culprit_core::PlanKind::Actual);
        assert_eq!(
            executor.run("SELECT total FROM public.orders WHERE id = 42", 1).await.unwrap().rows,
            old.rows,
            "measured writes must roll back"
        );
        let plan = executor
            .explain_ysql("SELECT * FROM public.orders WHERE customer_id = 42 LIMIT 10", ExplainMode::Analyze)
            .await
            .unwrap();
        assert!(
            plan.raw.as_deref().unwrap().contains("Storage Table Rows Scanned"),
            "measured YSQL plans need DocDB DIST metrics"
        );
        assert!(
            plan.nodes().iter().all(|n| n.table_rows.is_none()),
            "YSQL must not receive heap-based row-count enrichment"
        );
    }
}
