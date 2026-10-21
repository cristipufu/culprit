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
    /// How far back to read workload history where the engine keeps it (Query Store), in whole days. A window that
    /// isn't whole days sets `window_minutes` too, and this is that window rounded up.
    pub window_days: u32,
    /// The window in minutes when it isn't a whole number of days (`--window 6h`). Absent from scans before windows
    /// could be hours, which read `window_days`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub window_minutes: Option<u32>,
    /// How many of the heaviest queries to keep in the snapshot.
    pub top_queries: u32,
    /// How many plans to fetch and parse, heaviest queries first.
    pub plans: u32,
}

impl Default for SnapshotOptions {
    fn default() -> Self {
        SnapshotOptions { window_days: 7, window_minutes: None, top_queries: 60, plans: 25 }
    }
}

impl SnapshotOptions {
    /// The history window in minutes, whether it was given in days or not.
    pub fn window_in_minutes(&self) -> u32 {
        self.window_minutes.unwrap_or_else(|| self.window_days.saturating_mul(crate::MINUTES_PER_DAY)).max(1)
    }

    /// These options with a window of `minutes`: whole days keep `window_days` alone, as every scan did before.
    pub fn with_window_minutes(self, minutes: u32) -> Self {
        let minutes = minutes.max(1);
        let days = minutes.div_ceil(crate::MINUTES_PER_DAY);
        let exact = minutes.is_multiple_of(crate::MINUTES_PER_DAY);
        SnapshotOptions { window_days: days, window_minutes: (!exact).then_some(minutes), ..self }
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

    /// The collector that actually supplied this snapshot. Runtime wrappers need not claim a builtin's version.
    fn collector_version(&self) -> crate::ComponentVersion {
        crate::ComponentVersion::unknown("external provider")
    }

    /// Connect, identify the server and report which capabilities this login can use. Cheap; used by `conn test` and `doctor`.
    async fn probe(&self) -> Result<ServerInfo>;

    /// Collect a normalized snapshot. Reads statistics views only; must be safe on production.
    async fn snapshot(&self, connection: &str, options: &SnapshotOptions) -> Result<Snapshot>;

    async fn explain(&self, sql: &str, mode: ExplainMode) -> Result<Plan>;

    /// The estimated plan of a statement someone else's session recorded (a query from a scan, a session's current
    /// statement), never run. Unlike [`Provider::explain`], whoever asks didn't write its text, so an engine whose
    /// planner runs the code a statement reaches may plan the login's own copy instead (`query_id`, its id in the
    /// engine's statistics) or refuse. Everyone else plans it as `explain` would.
    async fn explain_recorded(&self, sql: &str, _query_id: Option<&str>) -> Result<Plan> {
        self.explain(sql, ExplainMode::Estimated).await
    }

    async fn live(&self) -> Result<LiveState>;

    /// Run a statement that reads and return up to `limit` rows, inside a read-only transaction where the engine has one.
    /// Callers enforce environment rules before calling.
    async fn run(&self, sql: &str, limit: usize) -> Result<ResultSet>;

    /// Run a statement that changes data (or that Culprit can't prove only reads) in a read-write session and commit it.
    /// Callers have already asked for confirmation where the environment requires it.
    async fn execute(&self, _sql: &str, _limit: usize) -> Result<ResultSet> {
        Err(self.engine().refused(crate::model::Ability::Writes))
    }

    /// The statements the server itself would run for `sql`, by type (`SELECT`, `COMMIT TRANSACTION`, `CREATE TABLE`…),
    /// read without running anything; `None` when the engine (or this login) can't tell. Confirms Culprit's own
    /// classifier with the server's parser before a read or a measured run, so a lexing difference can't hide a
    /// second statement.
    async fn statement_types(&self, _sql: &str) -> Result<Option<Vec<String>>> {
        Ok(None)
    }

    /// The lock each waiting session asked for (object, index, mode), to explain blocking chains. Read only when someone
    /// is blocked. Empty when the engine can't tell.
    async fn lock_waits(&self) -> Result<Vec<crate::blocking::LockWait>> {
        Ok(Vec::new())
    }

    /// Recent deadlocks from wherever the engine keeps them, newest first, already classified.
    async fn deadlocks(&self, _since_hours: f64) -> Result<crate::deadlock::DeadlockReport> {
        Err(self.engine().refused(crate::model::Ability::Deadlocks))
    }

    /// What the server waits on right now: two readings `seconds` apart (or sessions sampled every second). Reads
    /// statistics views only; safe on production.
    async fn wait_sample(&self, _seconds: u32) -> Result<crate::waits::WaitSample> {
        Err(self.engine().refused(crate::model::Ability::WaitSample))
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
        Err(self.engine().refused(crate::model::Ability::LastActualPlan))
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

    /// What `interrupt_session` supports on this engine: the engine's own (`EngineSpec::session_actions`) unless this
    /// provider can do less.
    fn session_actions(&self) -> Vec<SessionAction> {
        self.engine().spec().session_actions()
    }

    /// Cancel another session's statement or end the session. `session` is an id from `live()`. Implementations refuse
    /// Culprit's own sessions and system sessions; callers enforce environment rules (never on production).
    async fn interrupt_session(&self, _session: &str, _action: SessionAction) -> Result<()> {
        Err(Error::new(ErrorCode::Unsupported, format!("{} sessions can't be ended from Culprit", self.engine())))
    }

    /// Changes the database timestamps itself since `since` (UTC, ISO 8601): tables, procedures and views created or
    /// altered, settings changed at runtime, the configuration reloaded. Catalog views only; safe on production. Changes
    /// the engine doesn't date are found by comparing Culprit's scans instead.
    async fn recorded_changes(&self, _since: &str) -> Result<Vec<crate::events::ChangeEvent>> {
        Err(Error::new(ErrorCode::Unsupported, format!("{} doesn't date schema changes", self.engine())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_window_of_hours_keeps_its_minutes_and_rounds_its_days_up() {
        let six_hours = SnapshotOptions::default().with_window_minutes(360);
        assert_eq!((six_hours.window_days, six_hours.window_minutes), (1, Some(360)));
        assert_eq!(six_hours.window_in_minutes(), 360);

        let two_days = SnapshotOptions::default().with_window_minutes(2 * crate::MINUTES_PER_DAY);
        assert_eq!((two_days.window_days, two_days.window_minutes), (2, None));
        assert_eq!(two_days.window_in_minutes(), 2 * crate::MINUTES_PER_DAY);
    }

    #[test]
    fn options_saved_before_hour_windows_still_read() {
        let old: SnapshotOptions =
            serde_json::from_str(r#"{"window_days":7,"top_queries":60,"plans":25}"#).expect("old options");
        assert_eq!(old.window_minutes, None);
        assert_eq!(old.window_in_minutes(), 7 * crate::MINUTES_PER_DAY);
        let json = serde_json::to_string(&SnapshotOptions::default()).expect("serialize");
        assert!(!json.contains("window_minutes"), "{json}");
    }
}
