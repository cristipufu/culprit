//! Native YSQL catalogs and DocDB measurements, sharing only PostgreSQL wire transport and SQL execution.
mod ash;
mod catalog;
#[cfg(test)]
mod cluster_tests;
mod collect;
mod global;
mod history;
mod index_usage;
pub mod plan;
mod probe;
mod progress;
mod resources;
mod sessions;
pub mod sources;
mod terminations;
#[cfg(test)]
mod tests;
mod waits;
mod workload;

use async_trait::async_trait;
use culprit_core::limits::{OBSERVE_TIMEOUT, QUERY_TIMEOUT};
use culprit_core::{
    AuthKind, ConnectionSpec, Engine, Error, ExplainMode, LiveState, Plan, Provider, Result, ResultSet, ServerInfo,
    Snapshot, SnapshotOptions,
};
use culprit_postgres::wire::{COLLECTOR, Connector, SqlExecutor};

pub fn component_version() -> culprit_core::ComponentVersion {
    culprit_core::ComponentVersion::package(
        "culprit-yugabyte",
        env!("CARGO_PKG_VERSION"),
        option_env!("CULPRIT_BUILD_REVISION").or(option_env!("GITHUB_SHA")),
    )
}

pub struct YugabyteProvider {
    connector: Connector,
    execution: SqlExecutor,
    spec: ConnectionSpec,
}

impl YugabyteProvider {
    pub fn new(spec: &ConnectionSpec) -> Result<Self> {
        if spec.engine.is_some_and(|e| e != Engine::Yugabyte) {
            return Err(Error::invalid("This connection does not select YugabyteDB"));
        }
        if !matches!(spec.auth, AuthKind::Password | AuthKind::None) {
            return Err(Error::invalid(
                "Culprit supports password or trust authentication for YSQL; cloud bearer tokens are not sent to YugabyteDB",
            ));
        }
        let mut spec = spec.clone();
        spec.port = Some(spec.port.unwrap_or(5433));
        if spec.user.is_none() {
            spec.user = Some("yugabyte".into());
        }
        if spec.database.is_none() {
            spec.database = Some("yugabyte".into());
        }
        Ok(Self { connector: Connector::new(&spec)?, execution: SqlExecutor::new(&spec)?, spec })
    }
}

#[async_trait]
impl Provider for YugabyteProvider {
    fn engine(&self) -> Engine {
        Engine::Yugabyte
    }
    fn collector_version(&self) -> culprit_core::ComponentVersion {
        component_version()
    }
    async fn probe(&self) -> Result<ServerInfo> {
        deadline(OBSERVE_TIMEOUT, async {
            let session = self.connector.connect_observed(COLLECTOR).await?;
            Ok(probe::read(&session.client).await?.server())
        })
        .await
    }
    async fn snapshot(&self, connection: &str, options: &SnapshotOptions) -> Result<Snapshot> {
        collect::snapshot(self, connection, options).await
    }
    async fn explain(&self, sql: &str, mode: ExplainMode) -> Result<Plan> {
        if mode == ExplainMode::Analyze
            && self.spec.environment.is_production()
            && !culprit_core::sql::reads_only(sql, Engine::Yugabyte)
        {
            return Err(Error::new(
                culprit_core::ErrorCode::PermissionDenied,
                "Production measurements require a proven read-only YSQL statement",
            ));
        }
        deadline(
            match mode {
                ExplainMode::Estimated => OBSERVE_TIMEOUT,
                ExplainMode::Analyze => QUERY_TIMEOUT,
            },
            async { plan::native(self.execution.explain_ysql(sql, mode).await?) },
        )
        .await
    }
    async fn explain_recorded(&self, sql: &str, id: Option<&str>) -> Result<Plan> {
        deadline(OBSERVE_TIMEOUT, async {
            let session = self.connector.connect_observed(COLLECTOR).await?;
            let probe = probe::read(&session.client).await?;
            let text = if culprit_postgres::wire::elevated_login(&session.client).await? {
                let id = id.ok_or_else(culprit_postgres::wire::foreign_statement)?;
                culprit_postgres::wire::own_ysql_text(&session.client, id, probe.cluster)
                    .await?
                    .ok_or_else(culprit_postgres::wire::foreign_statement)?
            } else {
                sql.into()
            };
            self.explain(&text, ExplainMode::Estimated).await
        })
        .await
    }
    async fn stored_plan(&self, query_id: &str, plan_id: &str) -> Result<Option<Plan>> {
        // Validate untrusted ids before opening a session; signed 64-bit ids are native QPM hashes.
        if query_id.parse::<i64>().is_err() || plan_id.parse::<i64>().is_err() {
            return Err(Error::invalid("YSQL query and plan identifiers must be signed 64-bit integers"));
        }
        deadline(OBSERVE_TIMEOUT, async {
            let session = self.connector.connect_observed(COLLECTOR).await?;
            let probe = probe::read(&session.client).await?;
            history::stored(&session, &probe, query_id, plan_id).await
        })
        .await
    }
    async fn run(&self, sql: &str, limit: usize) -> Result<ResultSet> {
        deadline(QUERY_TIMEOUT, async {
            if !culprit_core::sql::reads_only(sql, Engine::Yugabyte) {
                return Err(Error::invalid(
                    "This YSQL statement is not proven read-only; choose an explicit write action to run it",
                ));
            }
            self.execution.run(sql, limit).await
        })
        .await
    }
    async fn execute(&self, sql: &str, limit: usize) -> Result<ResultSet> {
        deadline(QUERY_TIMEOUT, async { self.execution.execute(sql, limit).await }).await
    }
    async fn live(&self) -> Result<LiveState> {
        deadline(OBSERVE_TIMEOUT, async { sessions::live(&self.connector, self.spec.environment).await }).await
    }
    async fn wait_sample(&self, seconds: u32) -> Result<culprit_core::WaitSample> {
        deadline(std::time::Duration::from_secs(seconds.clamp(1, 60).into()) + OBSERVE_TIMEOUT, async {
            waits::sample(&self.connector, seconds).await
        })
        .await
    }
    async fn query_progress(&self, id: &str, with_plan: bool) -> Result<culprit_core::QueryProgress> {
        deadline(OBSERVE_TIMEOUT, async {
            let (mut progress, allowed) = progress::read(&self.connector, id, with_plan).await?;
            if allowed && progress.phase.is_none() {
                if let Some(statement) = &progress.statement {
                    match self.explain(statement,ExplainMode::Estimated).await {
                        Ok(plan) => progress.plan = Some(plan),
                        Err(_) => progress.note = Some(format!("{} Current activity was read, but this login could not safely plan the statement. No estimated operator progress is presented as measured work.", progress.note.as_deref().unwrap_or_default())),
                    }
                }
            } else if with_plan && progress.running && progress.phase.is_none() {
                progress.note = Some(format!("{} Current activity was read; this backend's ownership or visibility could not authorize planning its text with this login's privileges.", progress.note.as_deref().unwrap_or_default()));
            }
            Ok(progress)
        }).await
    }
    async fn interrupt_session(&self, id: &str, action: culprit_core::SessionAction) -> Result<()> {
        deadline(OBSERVE_TIMEOUT, async {
            if self.spec.environment.is_production() {
                return Err(Error::new(
                    culprit_core::ErrorCode::PermissionDenied,
                    "Culprit does not cancel or end sessions on a production connection",
                ));
            }
            sessions::interrupt(&self.connector, id, action).await
        })
        .await
    }
}

async fn deadline<T>(budget: std::time::Duration, work: impl std::future::Future<Output = Result<T>>) -> Result<T> {
    tokio::time::timeout(budget, work).await.unwrap_or_else(|_| Err(culprit_core::limits::timeout_error()))
}
