//! SQL Server provider: reads Query Store, the plan cache and catalog/DMV statistics into a Culprit snapshot.
//! Every statistics query is read-only and runs with a short lock timeout and low deadlock priority.

mod actual;
mod ado;
mod advice;
mod conn;
pub mod deadlocks;
mod live;
mod operations;
mod periods;
pub mod plan_xml;
mod pressure;
mod probe;
mod recorded;
mod schema;
mod signals;
pub mod sources;
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
    Capability, CollectorNote, ColumnInfo, ConnectionSpec, Coverage, Engine, Environment, ExplainMode, ForeignKey,
    IndexStat, LiveState, MissingIndex, Plan, Provider, ResourceSample, Result, ResultSet, SNAPSHOT_FORMAT, ServerInfo,
    SessionAction, Setting, Snapshot, SnapshotOptions, StatisticsInfo, TableStat, WaitStat, Workload,
};

pub use ado::parse_ado;
use conn::{MsClient, Pool, SessionKind, connect};
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

/// What a part of the scan's sources could read: the shared account every provider keeps (`culprit_core::Coverage`).
type Outcome = Coverage;

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
        part.outcome.not_applicable(
            &sources::QUERY_STORE,
            "Query Store requires SQL Server 2016 or later and a user database.",
        );
        part.outcome.lacking(
            Capability::QueryStore,
            "Query Store is not available here; it needs SQL Server 2016 or later and a user database.",
            None,
        );
    }
    if let Some(qs) = &probe.query_store
        && !qs.readable()
    {
        part.outcome.recorded(
            &sources::QUERY_STORE,
            culprit_core::SourceState::Unavailable,
            Some(format!("Query Store is {} on this database.", qs.actual)),
        );
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
        let grant = || probe.grant_hint();
        if let Some(result) = sources::read(
            &mut part.outcome,
            &sources::QUERY_STORE,
            workload::from_query_store(client, options).await,
            grant,
        ) {
            part.outcome.completed(&sources::QUERY_STORE, result.workload.queries.len());
            part.outcome.delivered(Capability::QueryStore);
            if result.plans_parsed > 0 {
                part.outcome.delivered(Capability::QueryPlans);
            }
            if result.plan_errors > 0 {
                part.outcome.note("plans", format!("{} Query Store plans could not be parsed", result.plan_errors));
            }
            if !result.waits.is_empty() {
                part.outcome.delivered(Capability::WaitStats);
            }
            part.waits = result.waits;
            part.workload = Some(result.workload);
            return part;
        }
    }

    if !dmv {
        part.outcome.recorded(
            &sources::PLAN_CACHE,
            culprit_core::SourceState::Unavailable,
            Some("This login cannot read plan cache statistics.".into()),
        );
        part.outcome.lacking(
            Capability::PlanCache,
            "This login cannot read plan cache statistics.",
            Some(probe.grant_hint()),
        );
        return part;
    }
    let plan_cache = workload::from_plan_cache(client, options).await;
    if let Some(result) = sources::read(&mut part.outcome, &sources::PLAN_CACHE, plan_cache, || probe.grant_hint()) {
        part.outcome.completed(&sources::PLAN_CACHE, result.workload.queries.len());
        part.outcome.delivered(Capability::PlanCache);
        if result.plans_parsed > 0 {
            part.outcome.delivered(Capability::QueryPlans);
        }
        part.workload = Some(result.workload);
    }
    part
}

/// The latest this database's index usage counters may have started, when later than the server's start (the age every
/// counter has otherwise): a database restored or created since then starts its counters with it (sys.databases'
/// create_date), and one with AUTO_CLOSE on starts them again each time it opens, so as far as anyone knows, now.
async fn usage_counted_since(client: &mut MsClient, probe: &Probe) -> Option<String> {
    let r = conn::query_one(
        client,
        "SELECT is_auto_close_on AS auto_close, CONVERT(varchar(19), DATEADD(minute, DATEDIFF(minute, GETDATE(), GETUTCDATE()), create_date), 126) + 'Z' AS created FROM sys.databases WHERE database_id = DB_ID()",
    )
    .await
    .ok()?;
    if r.bool("auto_close") {
        return Some(chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string());
    }
    let created = r.str("created")?;
    let epoch = |t: &str| chrono::DateTime::parse_from_rfc3339(t).ok().map(|d| d.timestamp());
    match probe.started_at.as_deref().and_then(epoch) {
        Some(started) if epoch(&created).is_some_and(|c| c <= started) => None,
        _ => Some(created),
    }
}

async fn collect_schema(client: &mut MsClient, probe: &Probe) -> SchemaPart {
    let mut part = SchemaPart::default();
    let dmv = probe.can_read_dmvs();
    let grant = || probe.grant_hint();
    let out = &mut part.outcome;
    if let Some(tables) = sources::read_rows(out, &sources::TABLES, schema::tables(client, dmv).await, grant) {
        part.tables = tables;
        out.delivered(Capability::TableStats);
    }
    if let Some((indexes, with_usage)) =
        sources::read(out, &sources::INDEXES, schema::indexes(client, dmv).await, grant)
    {
        out.completed(&sources::INDEXES, indexes.len());
        part.indexes = indexes;
        if with_usage {
            // Index usage is counted per database, from when it came online: no older than the server's start.
            let since = usage_counted_since(client, probe).await;
            part.indexes.iter_mut().for_each(|i| i.counters_since = since.clone());
            out.delivered(Capability::IndexUsage);
        } else {
            out.lacking(
                Capability::IndexUsage,
                "Index usage counters are not readable with this login, so unused-index checks are off.",
                Some(probe.grant_hint()),
            );
        }
    }
    part.columns = sources::read_rows(out, &sources::COLUMNS, schema::columns(client).await, grant).unwrap_or_default();
    part.foreign_keys =
        sources::read_rows(out, &sources::FOREIGN_KEYS, schema::foreign_keys(client).await, grant).unwrap_or_default();
    if let Some(stats) = sources::read_rows(out, &sources::STATISTICS, schema::statistics(client).await, grant) {
        part.statistics = stats;
        out.delivered(Capability::Statistics);
    }
    part
}

async fn collect_misc(client: &mut MsClient, probe: &Probe, options: &SnapshotOptions) -> MiscPart {
    let mut part = MiscPart::default();
    let dmv = probe.can_read_dmvs();
    let grant = || probe.grant_hint();
    if dmv {
        let missing = schema::missing_indexes(client).await;
        if let Some(missing) = sources::read_rows(&mut part.outcome, &sources::MISSING_INDEXES, missing, grant) {
            part.missing_indexes = missing;
            part.outcome.delivered(Capability::MissingIndexes);
        }
        let waits = schema::waits(client, probe.is_azure_sql_database()).await;
        if let Some(waits) = sources::read_rows(&mut part.outcome, &sources::WAITS, waits, grant) {
            part.waits = waits;
            part.outcome.delivered(Capability::WaitStats);
        }
        let (pressure, read) = pressure::collect(client, probe).await;
        part.outcome.merge(read);
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
        let resources = schema::resources(client).await;
        if let Some(resources) = sources::read_rows(&mut part.outcome, &sources::RESOURCES, resources, grant) {
            part.resources = resources;
            part.outcome.delivered(Capability::ResourceUsage);
        }
    }
    if dmv {
        // The index advisor and tuning checks: each adds detail, and a failure is a note.
        let out = &mut part.outcome;
        part.forwarded =
            sources::read(out, &sources::FORWARDED_FETCHES, advice::forwarded_fetches(client).await, grant)
                .unwrap_or_default();
        let query_store = probe.query_store_readable();
        let families = advice::query_families(client, options.window_in_minutes(), query_store).await;
        part.query_families = sources::read_rows(out, &sources::QUERY_FAMILIES, families, grant).unwrap_or_default();
        part.plan_cache =
            sources::read(out, &sources::PLAN_CACHE_USE, advice::plan_cache(client).await, grant).flatten();
        part.tuning = sources::read(out, &sources::TUNING, advice::tuning(client, query_store).await, grant);
    }
    let query_store_options = probe.query_store.as_ref().map(|qs| &qs.options);
    let settings = schema::settings(client, probe.is_azure_sql_database(), query_store_options).await;
    part.settings = sources::read_rows(&mut part.outcome, &sources::SETTINGS, settings, grant).unwrap_or_default();
    part
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
                outcome.delivered(capability);
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
        if let Some(note) = &schema_note {
            for source in
                [&sources::TABLES, &sources::INDEXES, &sources::COLUMNS, &sources::FOREIGN_KEYS, &sources::STATISTICS]
            {
                outcome.source_failed(source, &culprit_core::Error::internal(note.message.clone()));
            }
        }
        for note in schema_note.into_iter().chain(misc_note) {
            outcome.note(&note.collector, note.message);
        }
        let sources = outcome.outcomes();
        let (have, missing, notes) = outcome.finish();

        let waits = if workload_part.waits.is_empty() { misc_part.waits } else { workload_part.waits };
        let mut tables = schema_part.tables;
        for t in &mut tables {
            t.forwarded_fetches = misc_part.forwarded.get(&(t.schema.to_lowercase(), t.name.to_lowercase())).copied();
        }
        let mut snapshot = Snapshot {
            sources,
            format: SNAPSHOT_FORMAT,
            id: taken_at.format("%Y%m%dT%H%M%SZ").to_string(),
            connection: connection.to_string(),
            environment: self.environment,
            taken_at: taken_at.format("%Y-%m-%dT%H:%M:%SZ").to_string(),
            duration_ms: started.elapsed().as_secs_f64() * 1_000.0,
            server: probe::server_info(&probe, have, missing),
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
            notes,
            options: Some(options.clone()),
            query_families: misc_part.query_families,
            plan_cache: misc_part.plan_cache,
            tuning: misc_part.tuning,
            deadlocks: None,
            constraints: Vec::new(),
            histograms: None,
            escalations_before: None,
            contention_before: None,
            evictions_before: None,
            sizes_before: None,
            operations: Default::default(),
        };
        match self.pool.get().await {
            Ok(mut client) => {
                signals::collect(&mut client, &probe, options, &mut snapshot).await;
                operations::collect(&mut client, &probe, &mut snapshot).await;
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
