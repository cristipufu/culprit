use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::actual::QueryProgress;
use crate::error::{Error, ErrorCode, Result};
use crate::model::{Engine, LiveState, ResultSet, ServerInfo, SessionAction, Snapshot};
use crate::plan::Plan;

#[derive(Serialize, Deserialize, TS, Clone, Debug)]
#[ts(export)]
pub struct SnapshotOptions {
    /// How far back to read workload history where the engine keeps it (Query Store).
    pub window_days: u32,
    /// How many of the heaviest queries to keep in the snapshot.
    pub top_queries: u32,
    /// How many plans to fetch and parse, heaviest queries first.
    pub plans: u32,
}

impl Default for SnapshotOptions {
    fn default() -> Self {
        SnapshotOptions { window_days: 7, top_queries: 60, plans: 25 }
    }
}

#[derive(Serialize, Deserialize, TS, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
#[ts(export)]
pub enum ExplainMode {
    /// Plan only. Never executes the statement.
    Estimated,
    /// Executes the statement and reports what actually happened.
    Analyze,
}

/// One database engine. Implementations open their own connections from the configuration they were built with.
#[async_trait]
pub trait Provider: Send + Sync {
    fn engine(&self) -> Engine;

    /// Connect, identify the server and report which capabilities this login can use. Cheap; used by `conn test` and `doctor`.
    async fn probe(&self) -> Result<ServerInfo>;

    /// Collect a normalized snapshot. Reads statistics views only; must be safe on production.
    async fn snapshot(&self, connection: &str, options: &SnapshotOptions) -> Result<Snapshot>;

    async fn explain(&self, sql: &str, mode: ExplainMode) -> Result<Plan>;

    async fn live(&self) -> Result<LiveState>;

    /// Run a statement that reads and return up to `limit` rows, inside a read-only transaction where the engine has one.
    /// Callers enforce environment rules before calling.
    async fn run(&self, sql: &str, limit: usize) -> Result<ResultSet>;

    /// Run a statement that changes data (or that Culprit can't prove only reads) in a read-write session and commit it.
    /// Callers have already asked for confirmation where the environment requires it.
    async fn execute(&self, _sql: &str, _limit: usize) -> Result<ResultSet> {
        Err(Error::new(ErrorCode::Unsupported, "This engine can't run statements that change data"))
    }

    /// The lock each waiting session asked for (object, index, mode), to explain blocking chains. Read only when someone
    /// is blocked. Empty when the engine can't tell.
    async fn lock_waits(&self) -> Result<Vec<crate::blocking::LockWait>> {
        Ok(Vec::new())
    }

    /// Recent deadlocks from wherever the engine keeps them, newest first, already classified.
    async fn deadlocks(&self, _since_hours: f64) -> Result<crate::deadlock::DeadlockReport> {
        Err(Error::new(ErrorCode::Unsupported, "This engine doesn't expose deadlocks"))
    }

    /// What the server waits on right now: two readings `seconds` apart (or sessions sampled every second). Reads
    /// statistics views only; safe on production.
    async fn wait_sample(&self, _seconds: u32) -> Result<crate::waits::WaitSample> {
        Err(Error::new(ErrorCode::Unsupported, "This engine can't sample waits"))
    }

    /// Every query's runs and time in two periods, from a source that keeps history by interval (Query Store).
    /// `None` when the engine only keeps running totals; callers then compare Culprit's own scans.
    async fn period_stats(
        &self,
        _baseline: &crate::changes::Period,
        _recent: &crate::changes::Period,
    ) -> Result<Option<crate::changes::PeriodData>> {
        Ok(None)
    }

    /// A plan the engine kept for a query (Query Store keeps every plan a query used). `None` when the engine keeps no plans.
    async fn stored_plan(&self, _query_id: &str, _plan_id: &str) -> Result<Option<Plan>> {
        Ok(None)
    }

    /// The actual plan of a query's most recent run as the engine kept it, without running the query (SQL Server 2019+
    /// with LAST_QUERY_PLAN_STATS). Ids are the workload's query and plan ids.
    async fn last_actual_plan(&self, _query_id: &str, _plan_id: Option<&str>) -> Result<Plan> {
        Err(Error::new(ErrorCode::Unsupported, format!("{} doesn't keep the actual plan of past runs", self.engine())))
    }

    /// How far along another session's running statement is. `with_plan` asks for the plan as well; engines whose
    /// plan doesn't change while it runs (Postgres, MySQL) only send it when asked, SQL Server always sends its live
    /// counters.
    async fn query_progress(&self, _session: &str, _with_plan: bool) -> Result<QueryProgress> {
        Err(Error::new(
            ErrorCode::Unsupported,
            format!("{} can't report a running statement's progress", self.engine()),
        ))
    }

    /// What `interrupt_session` supports on this engine.
    fn session_actions(&self) -> Vec<SessionAction> {
        Vec::new()
    }

    /// Cancel another session's statement or end the session. `session` is an id from `live()`. Implementations refuse
    /// Culprit's own sessions and system sessions; callers enforce environment rules (never on production).
    async fn interrupt_session(&self, _session: &str, _action: SessionAction) -> Result<()> {
        Err(Error::new(ErrorCode::Unsupported, format!("{} sessions can't be ended from Culprit", self.engine())))
    }
}
