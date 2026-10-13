//! SQL Server provider: reads Query Store, the plan cache and catalog/DMV statistics into a Culprit snapshot.
//! Every statistics query is read-only and runs with a short lock timeout and low deadlock priority.

mod actual;
mod ado;
mod advice;
mod conn;
pub mod deadlocks;
mod live;
mod periods;
pub mod plan_xml;
mod pressure;
mod probe;
mod recorded;
mod schema;
mod signals;
mod spread;
mod statement;
mod timeline;
mod value;
mod workload;

use std::collections::HashMap;
use std::future::Future;
use std::time::Instant;

use async_trait::async_trait;
use culprit_core::{
    Capability, CollectorNote, ColumnInfo, ConnectionSpec, Engine, Environment, ExplainMode, ForeignKey, IndexStat,
    LiveState, MissingCapability, MissingIndex, Plan, Provider, ResourceSample, Result, ResultSet, SNAPSHOT_FORMAT,
    ServerInfo, SessionAction, Setting, Snapshot, SnapshotOptions, StatisticsInfo, TableStat, WaitStat, Workload,
};

pub use ado::parse_ado;
use conn::{MsClient, Pool, SessionKind, connect, is_permission_error, map_error};
use probe::Probe;

pub struct MssqlProvider {
    environment: Environment,
    pool: Pool,
}

impl MssqlProvider {
    pub fn new(spec: &ConnectionSpec) -> Result<Self> {
        Ok(MssqlProvider { environment: spec.environment, pool: Pool::new(conn::build_config(spec)?) })
    }

    async fn user_session(&self) -> Result<MsClient> {
        connect(self.pool.config(), SessionKind::User).await
    }
}

/// What the collectors found out about the server beyond the data itself.
#[derive(Default)]
struct Outcome {
    have: Vec<Capability>,
    missing: Vec<MissingCapability>,
    notes: Vec<CollectorNote>,
}

impl Outcome {
    fn ok(&mut self, capability: Capability) {
        if !self.have.contains(&capability) {
            self.have.push(capability);
        }
    }

    fn lacking(&mut self, capability: Capability, reason: impl Into<String>, fix: Option<String>) {
        if self.missing.iter().any(|m| m.capability == capability) {
            return;
        }
        self.missing.push(MissingCapability { capability, reason: reason.into(), fix });
    }

    fn failed(&mut self, collector: &str, capability: Capability, err: &tiberius::error::Error, probe: &Probe) {
        let mapped = map_error(err.clone());
        self.notes.push(CollectorNote { collector: collector.into(), message: mapped.message.clone() });
        let fix = is_permission_error(err).then(|| probe.grant_hint());
        let reason = if is_permission_error(err) {
            "This login lacks the permission to read it.".to_string()
        } else {
            format!("Reading it failed: {}", mapped.message)
        };
        self.lacking(capability, reason, fix);
    }

    fn merge(&mut self, other: Outcome) {
        for c in other.have {
            self.ok(c);
        }
        for m in other.missing {
            if !self.missing.iter().any(|x| x.capability == m.capability) {
                self.missing.push(m);
            }
        }
        self.notes.extend(other.notes);
    }
}

#[derive(Default)]
struct WorkloadPart {
    workload: Option<Workload>,
    waits: Vec<WaitStat>,
    outcome: Outcome,
}

#[derive(Default)]
struct SchemaPart {
    tables: Vec<TableStat>,
    indexes: Vec<IndexStat>,
    columns: Vec<ColumnInfo>,
    foreign_keys: Vec<ForeignKey>,
    statistics: Vec<StatisticsInfo>,
    outcome: Outcome,
}

#[derive(Default)]
struct MiscPart {
    pressure: Option<culprit_core::Pressure>,
    missing_indexes: Vec<MissingIndex>,
    forwarded: HashMap<(String, String), f64>,
    query_families: Vec<culprit_core::QueryFamily>,
    plan_cache: Option<culprit_core::PlanCacheUse>,
    tuning: Option<culprit_core::TuningAdvice>,
    waits: Vec<WaitStat>,
    resources: Vec<ResourceSample>,
    settings: Vec<Setting>,
    outcome: Outcome,
}

async fn collect_workload(client: &mut MsClient, probe: &Probe, options: &SnapshotOptions) -> WorkloadPart {
    let mut part = WorkloadPart::default();
    let dmv = probe.can_read_dmvs();
    if probe.query_store.is_none() {
        part.outcome.lacking(
            Capability::QueryStore,
            "Query Store is not available here; it needs SQL Server 2016 or later and a user database.",
            None,
        );
    }
    if let Some(qs) = &probe.query_store
        && !qs.readable()
    {
        part.outcome.lacking(
            Capability::QueryStore,
            format!(
                "Query Store is {} on this database, so workload history is limited to what is still in the plan cache.",
                qs.actual.to_ascii_lowercase().replace('_', " ")
            ),
            Some("ALTER DATABASE CURRENT SET QUERY_STORE = ON (OPERATION_MODE = READ_WRITE);".into()),
        );
    }

    if probe.query_store_readable() {
        match workload::from_query_store(client, options).await {
            Ok(result) => {
                part.outcome.ok(Capability::QueryStore);
                if result.plans_parsed > 0 {
                    part.outcome.ok(Capability::QueryPlans);
                }
                if result.plan_errors > 0 {
                    part.outcome.notes.push(CollectorNote {
                        collector: "plans".into(),
                        message: format!("{} Query Store plans could not be parsed", result.plan_errors),
                    });
                }
                if !result.waits.is_empty() {
                    part.outcome.ok(Capability::WaitStats);
                }
                part.waits = result.waits;
                part.workload = Some(result.workload);
                return part;
            }
            Err(err) => part.outcome.failed("query_store", Capability::QueryStore, &err, probe),
        }
    }

    if !dmv {
        part.outcome.lacking(
            Capability::PlanCache,
            "This login cannot read plan cache statistics.",
            Some(probe.grant_hint()),
        );
        return part;
    }
    match workload::from_plan_cache(client, options).await {
        Ok(result) => {
            part.outcome.ok(Capability::PlanCache);
            if result.plans_parsed > 0 {
                part.outcome.ok(Capability::QueryPlans);
            }
            part.workload = Some(result.workload);
        }
        Err(err) => part.outcome.failed("plan_cache", Capability::PlanCache, &err, probe),
    }
    part
}

async fn collect_schema(client: &mut MsClient, probe: &Probe) -> SchemaPart {
    let mut part = SchemaPart::default();
    let dmv = probe.can_read_dmvs();
    match schema::tables(client, dmv).await {
        Ok(tables) => {
            part.tables = tables;
            part.outcome.ok(Capability::TableStats);
        }
        Err(err) => part.outcome.failed("tables", Capability::TableStats, &err, probe),
    }
    match schema::indexes(client, dmv).await {
        Ok((indexes, with_usage)) => {
            part.indexes = indexes;
            if with_usage {
                part.outcome.ok(Capability::IndexUsage);
            } else {
                part.outcome.lacking(
                    Capability::IndexUsage,
                    "Index usage counters are not readable with this login, so unused-index checks are off.",
                    Some(probe.grant_hint()),
                );
            }
        }
        Err(err) => part.outcome.failed("indexes", Capability::IndexUsage, &err, probe),
    }
    match schema::columns(client).await {
        Ok(columns) => part.columns = columns,
        Err(err) => part.outcome.failed("columns", Capability::TableStats, &err, probe),
    }
    match schema::foreign_keys(client).await {
        Ok(fks) => part.foreign_keys = fks,
        Err(err) => part.outcome.failed("foreign_keys", Capability::TableStats, &err, probe),
    }
    match schema::statistics(client).await {
        Ok(stats) => {
            part.statistics = stats;
            part.outcome.ok(Capability::Statistics);
        }
        Err(err) => part.outcome.failed("statistics", Capability::Statistics, &err, probe),
    }
    part
}

async fn collect_misc(client: &mut MsClient, probe: &Probe, options: &SnapshotOptions) -> MiscPart {
    let mut part = MiscPart::default();
    let dmv = probe.can_read_dmvs();
    if dmv {
        match schema::missing_indexes(client).await {
            Ok(missing) => {
                part.missing_indexes = missing;
                part.outcome.ok(Capability::MissingIndexes);
            }
            Err(err) => part.outcome.failed("missing_indexes", Capability::MissingIndexes, &err, probe),
        }
        match schema::waits(client, probe.is_azure_sql_database()).await {
            Ok(waits) => {
                part.waits = waits;
                part.outcome.ok(Capability::WaitStats);
            }
            Err(err) => part.outcome.failed("waits", Capability::WaitStats, &err, probe),
        }
        let (pressure, notes) = pressure::collect(client, probe).await;
        part.outcome
            .notes
            .extend(notes.into_iter().map(|message| CollectorNote { collector: "pressure".into(), message }));
        part.pressure = Some(pressure);
    } else {
        for capability in [Capability::MissingIndexes, Capability::WaitStats] {
            part.outcome.lacking(
                capability,
                "This login lacks VIEW DATABASE STATE / VIEW SERVER STATE.",
                Some(probe.grant_hint()),
            );
        }
    }
    if probe.is_azure_sql_database() {
        match schema::resources(client).await {
            Ok(resources) => {
                part.resources = resources;
                part.outcome.ok(Capability::ResourceUsage);
            }
            Err(err) => part.outcome.failed("resources", Capability::ResourceUsage, &err, probe),
        }
    }
    if dmv {
        // The index advisor and tuning checks: each is optional, a failure is only a note.
        match advice::forwarded_fetches(client).await {
            Ok(forwarded) => part.forwarded = forwarded,
            Err(err) => part.notes_only("forwarded_fetches", &err),
        }
        let query_store = probe.query_store_readable();
        match advice::query_families(client, options.window_in_minutes(), query_store).await {
            Ok(families) => part.query_families = families,
            Err(err) => part.notes_only("query_families", &err),
        }
        match advice::plan_cache(client).await {
            Ok(cache) => part.plan_cache = cache,
            Err(err) => part.notes_only("plan_cache", &err),
        }
        match advice::tuning(client, query_store).await {
            Ok(tuning) => part.tuning = Some(tuning),
            Err(err) => part.notes_only("tuning", &err),
        }
    }
    let query_store_options = probe.query_store.as_ref().map(|qs| &qs.options);
    match schema::settings(client, probe.is_azure_sql_database(), query_store_options).await {
        Ok(settings) => part.settings = settings,
        Err(err) => part.notes_only("settings", &err),
    }
    part
}

impl MiscPart {
    fn notes_only(&mut self, collector: &str, err: &tiberius::error::Error) {
        self.outcome.notes.push(CollectorNote { collector: collector.into(), message: map_error(err.clone()).message });
    }
}

impl MssqlProvider {
    /// Runs `f` on its own pooled session so collector groups proceed concurrently.
    async fn on_session<T: Default, F, Fut>(&self, f: F) -> (T, Option<CollectorNote>)
    where
        F: FnOnce(MsClient) -> Fut,
        Fut: Future<Output = (T, MsClient)>,
    {
        match self.pool.get().await {
            Ok(client) => {
                let (value, client) = f(client).await;
                self.pool.put(client);
                (value, None)
            }
            Err(err) => (T::default(), Some(CollectorNote { collector: "connection".into(), message: err.message })),
        }
    }
}

#[async_trait]
impl Provider for MssqlProvider {
    fn engine(&self) -> Engine {
        Engine::Mssql
    }

    async fn probe(&self) -> Result<ServerInfo> {
        let probe = self.pool.with(|c| Box::pin(probe::run(c))).await?;
        let (have, missing) = probe::expected_capabilities(&probe);
        Ok(probe::server_info(&probe, have, missing))
    }

    async fn snapshot(&self, connection: &str, options: &SnapshotOptions) -> Result<Snapshot> {
        let started = Instant::now();
        let taken_at = chrono::Utc::now();
        let probe = self.pool.with(|c| Box::pin(probe::run(c))).await?;
        let probe_ref = &probe;

        let (workload_part, (schema_part, schema_note), (misc_part, misc_note)) = tokio::join!(
            async {
                match self.pool.get().await {
                    Ok(mut client) => {
                        let part = collect_workload(&mut client, &probe, options).await;
                        self.pool.put(client);
                        Ok(part)
                    }
                    Err(err) => Err(err),
                }
            },
            self.on_session(move |mut c| async move {
                let part = collect_schema(&mut c, probe_ref).await;
                (part, c)
            }),
            self.on_session(move |mut c| async move {
                let part = collect_misc(&mut c, probe_ref, options).await;
                (part, c)
            }),
        );
        let workload_part = workload_part?;

        let mut outcome = Outcome::default();
        let (expected_have, expected_missing) = probe::expected_capabilities(&probe);
        for capability in [Capability::Showplan, Capability::Sessions] {
            if expected_have.contains(&capability) {
                outcome.ok(capability);
            }
        }
        for m in expected_missing {
            if matches!(m.capability, Capability::Showplan | Capability::Sessions) {
                outcome.lacking(m.capability, m.reason, m.fix);
            }
        }
        outcome.merge(workload_part.outcome);
        outcome.merge(schema_part.outcome);
        outcome.merge(misc_part.outcome);
        outcome.notes.extend(schema_note);
        outcome.notes.extend(misc_note);
        outcome.missing.retain(|m| !outcome.have.contains(&m.capability));
        outcome.have.sort();

        let waits = if workload_part.waits.is_empty() { misc_part.waits } else { workload_part.waits };
        let mut tables = schema_part.tables;
        for t in &mut tables {
            t.forwarded_fetches = misc_part.forwarded.get(&(t.schema.to_lowercase(), t.name.to_lowercase())).copied();
        }
        let mut snapshot = Snapshot {
            format: SNAPSHOT_FORMAT,
            id: taken_at.format("%Y%m%dT%H%M%SZ").to_string(),
            connection: connection.to_string(),
            environment: self.environment,
            taken_at: taken_at.format("%Y-%m-%dT%H:%M:%SZ").to_string(),
            duration_ms: started.elapsed().as_secs_f64() * 1_000.0,
            server: probe::server_info(&probe, outcome.have, outcome.missing),
            workload: workload_part.workload,
            tables,
            indexes: schema_part.indexes,
            columns: schema_part.columns,
            foreign_keys: schema_part.foreign_keys,
            missing_indexes: misc_part.missing_indexes,
            statistics: schema_part.statistics,
            waits,
            resources: misc_part.resources,
            settings: misc_part.settings,
            pressure: misc_part.pressure,
            notes: outcome.notes,
            options: Some(options.clone()),
            query_families: misc_part.query_families,
            plan_cache: misc_part.plan_cache,
            tuning: misc_part.tuning,
            deadlocks: None,
            constraints: Vec::new(),
            histograms: None,
        };
        match self.pool.get().await {
            Ok(mut client) => {
                signals::collect(&mut client, &probe, options, &mut snapshot).await;
                self.pool.put(client);
            }
            Err(err) => snapshot.notes.push(CollectorNote { collector: "signals".into(), message: err.message }),
        }
        snapshot.duration_ms = started.elapsed().as_secs_f64() * 1_000.0;
        Ok(snapshot)
    }

    async fn explain(&self, sql: &str, mode: ExplainMode) -> Result<Plan> {
        let mut client = self.user_session().await?;
        match mode {
            ExplainMode::Estimated => statement::estimated_plan(&mut client, sql).await,
            ExplainMode::Analyze => statement::actual_plan(&mut client, sql).await,
        }
        .map_err(statement::parameter_error)
    }

    async fn live(&self) -> Result<LiveState> {
        self.pool.with(|c| Box::pin(live::collect(c))).await
    }

    async fn lock_waits(&self) -> Result<Vec<culprit_core::LockWait>> {
        self.pool.with(|c| Box::pin(deadlocks::lock_waits(c))).await
    }

    async fn deadlocks(&self, since_hours: f64) -> Result<culprit_core::DeadlockReport> {
        self.pool.with(move |c| Box::pin(deadlocks::recent(c, since_hours))).await
    }

    async fn recorded_changes(&self, since: &str) -> Result<Vec<culprit_core::ChangeEvent>> {
        let since = since.to_string();
        self.pool
            .with(move |c| {
                let since = since.clone();
                Box::pin(async move { recorded::changes(c, &since).await })
            })
            .await
    }

    async fn wait_sample(&self, seconds: u32) -> Result<culprit_core::WaitSample> {
        let probe = self.pool.with(|c| Box::pin(probe::run(c))).await?;
        if !probe.can_read_dmvs() {
            return Err(culprit_core::Error::new(
                culprit_core::ErrorCode::PermissionDenied,
                "Sampling waits needs permission to read the server's wait statistics",
            )
            .with_hint(probe.grant_hint()));
        }
        let azure = probe.is_azure_sql_database();
        self.pool.with(move |c| Box::pin(pressure::sample(c, azure, seconds))).await
    }

    fn session_actions(&self) -> Vec<SessionAction> {
        vec![SessionAction::Terminate]
    }

    async fn interrupt_session(&self, session: &str, action: SessionAction) -> Result<()> {
        live::interrupt(&self.pool, session, action).await
    }

    async fn period_stats(
        &self,
        baseline: &culprit_core::changes::Period,
        recent: &culprit_core::changes::Period,
    ) -> Result<Option<culprit_core::changes::PeriodData>> {
        self.pool
            .with(|c| {
                let (baseline, recent) = (baseline.clone(), recent.clone());
                Box::pin(async move { periods::collect(c, &baseline, &recent).await })
            })
            .await
    }

    async fn stored_plan(&self, query_id: &str, plan_id: &str) -> Result<Option<Plan>> {
        let numeric = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_digit());
        if !numeric(query_id) || !numeric(plan_id) {
            return Err(culprit_core::Error::invalid("Query Store ids are numbers"));
        }
        let sql = format!(
            "SELECT query_plan FROM sys.query_store_plan WHERE plan_id = {plan_id} AND query_id = {query_id} /* culprit */"
        );
        let xml = self
            .pool
            .with(|c| {
                let sql = sql.clone();
                Box::pin(async move { conn::query(c, &sql).await })
            })
            .await?
            .into_iter()
            .next()
            .and_then(|r| r.str("query_plan"));
        xml.map(|x| plan_xml::parse(&x)).transpose()
    }

    async fn last_actual_plan(&self, query_id: &str, plan_id: Option<&str>) -> Result<Plan> {
        actual::last_actual_plan(&self.pool, query_id, plan_id).await
    }

    async fn query_progress(&self, session: &str, _with_plan: bool) -> Result<culprit_core::QueryProgress> {
        actual::query_progress(&self.pool, session).await
    }

    async fn run(&self, sql: &str, limit: usize) -> Result<ResultSet> {
        let mut client = self.user_session().await?;
        statement::run(&mut client, sql, limit).await.map_err(statement::parameter_error)
    }

    async fn statement_types(&self, sql: &str) -> Result<Option<Vec<String>>> {
        let mut client = self.user_session().await?;
        match statement::statement_types(&mut client, sql).await {
            Ok(types) => Ok(Some(types)),
            // A login without SHOWPLAN permission can't ask; Culprit's own classifier has already passed the text.
            Err(e) if e.message.to_ascii_lowercase().contains("showplan permission") => Ok(None),
            Err(e) => Err(statement::parameter_error(e)),
        }
    }

    /// SQL Server has no read-only transactions, so reads and writes share one path (autocommit); the Workspace only
    /// sends statements here that the classifier proved read, or that the person confirmed.
    async fn execute(&self, sql: &str, limit: usize) -> Result<ResultSet> {
        let mut client = self.user_session().await?;
        let mut result = if statement::returns_rows(sql) {
            statement::run(&mut client, sql, limit).await
        } else {
            statement::run_changes(&mut client, sql).await
        }
        .map_err(statement::parameter_error)?;
        // "2 rows changed, committed." — the same words MySQL and PostgreSQL use.
        match result.messages.last_mut() {
            Some(last) if last.ends_with("changed") => last.push_str(", committed."),
            _ => result.messages.push("Committed.".to_string()),
        }
        Ok(result)
    }
}
