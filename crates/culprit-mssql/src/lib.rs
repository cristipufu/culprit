//! SQL Server provider: reads Query Store, the plan cache and catalog/DMV statistics into a Culprit snapshot.
//! Every statistics query is read-only and runs with a short lock timeout and low deadlock priority.

mod ado;
mod conn;
mod live;
pub mod plan_xml;
mod probe;
mod schema;
mod statement;
mod value;
mod workload;

use std::future::Future;
use std::time::Instant;

use async_trait::async_trait;
use culprit_core::{
    Capability, CollectorNote, ConnectionSpec, Engine, Environment, ExplainMode, ForeignKey, IndexStat, LiveState,
    MissingCapability, MissingIndex, Plan, Provider, ResourceSample, Result, ResultSet, SNAPSHOT_FORMAT, ServerInfo,
    Setting, Snapshot, SnapshotOptions, StatisticsInfo, TableStat, WaitStat, Workload,
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
    foreign_keys: Vec<ForeignKey>,
    statistics: Vec<StatisticsInfo>,
    outcome: Outcome,
}

#[derive(Default)]
struct MiscPart {
    missing_indexes: Vec<MissingIndex>,
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

async fn collect_misc(client: &mut MsClient, probe: &Probe) -> MiscPart {
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
                let part = collect_misc(&mut c, probe_ref).await;
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
        Ok(Snapshot {
            format: SNAPSHOT_FORMAT,
            id: taken_at.format("%Y%m%dT%H%M%SZ").to_string(),
            connection: connection.to_string(),
            environment: self.environment,
            taken_at: taken_at.format("%Y-%m-%dT%H:%M:%SZ").to_string(),
            duration_ms: started.elapsed().as_secs_f64() * 1_000.0,
            server: probe::server_info(&probe, outcome.have, outcome.missing),
            workload: workload_part.workload,
            tables: schema_part.tables,
            indexes: schema_part.indexes,
            foreign_keys: schema_part.foreign_keys,
            missing_indexes: misc_part.missing_indexes,
            statistics: schema_part.statistics,
            waits,
            resources: misc_part.resources,
            settings: misc_part.settings,
            notes: outcome.notes,
            options: Some(options.clone()),
        })
    }

    async fn explain(&self, sql: &str, mode: ExplainMode) -> Result<Plan> {
        let mut client = self.user_session().await?;
        match mode {
            ExplainMode::Estimated => statement::estimated_plan(&mut client, sql).await,
            ExplainMode::Analyze => statement::actual_plan(&mut client, sql).await,
        }
    }

    async fn live(&self) -> Result<LiveState> {
        self.pool.with(|c| Box::pin(live::collect(c))).await
    }

    async fn run(&self, sql: &str, limit: usize) -> Result<ResultSet> {
        let mut client = self.user_session().await?;
        statement::run(&mut client, sql, limit).await
    }
}
